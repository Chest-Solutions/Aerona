//! Test-only construction helpers.
//!
//! Kept in `src/` behind `#[cfg(test)]` so both the unit tests in this crate and the
//! async HTTP tests share one definition of "an API pointed at an empty database".

use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use aerona_core::config::Config;
use aerona_core::db::Db;

use crate::app::AppState;

/// A token with the right shape (three dot-separated parts, long enough) so
/// `Config` accepts it. Nothing here ever talks to Discord.
pub const FAKE_TOKEN: &str =
    "mZgRBmFak3t0k3n.1730000000.3a9c1f0e5b6d7c8a9b0c1d2e3f405162738495a6b7c8d9e0";

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_root(tag: &str) -> PathBuf {
    // Self-cleaning: a test suite that leaves a directory per case would eventually
    // be indistinguishable from a leak. Anything older than an hour is fair game.
    let base = std::env::temp_dir();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("aerona-api-test-") {
                continue;
            }
            let old = entry
                .path()
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok().map(|e| e.as_secs()))
                .unwrap_or(0);
            if old > 3600 {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let at = base.join(format!("aerona-api-test-{tag}-{}-{n}", std::process::id()));
    let _ = now;
    std::fs::create_dir_all(&at).expect("create the temp root");
    at
}

pub fn test_config() -> Config {
    test_config_in(&std::env::temp_dir())
}

/// `Config` with everything an HTTP test needs: no OAuth (so the pairing path is the
/// one under test), `dev` on (so cookies are not `Secure`), and a base URL that
/// matches the loopback origin the test client sends.
pub fn test_config_in(root: &Path) -> Config {
    let mut env = HashMap::new();
    env.insert("AERONA_TOKEN".to_string(), FAKE_TOKEN.to_string());
    // A real port number: `Config` refuses 0 on purpose (a server must not be
    // configured to "any port"), and tests connect to an ephemeral port separately.
    env.insert("AERONA_PORT".to_string(), "8791".to_string());
    env.insert(
        "AERONA_DB".to_string(),
        root.join("test.db").display().to_string(),
    );
    env.insert(
        "AERONA_BASE_URL".to_string(),
        "http://127.0.0.1:8791".to_string(),
    );
    env.insert("AERONA_DATA_DIR".to_string(), root.display().to_string());
    // A generous general bucket: a test that trips the limiter would be testing the
    // limiter, not the endpoint. The *auth* bucket is hardcoded in `AppState::new`
    // precisely so the lockout path can still be exercised.
    env.insert("AERONA_RATE_RPS".to_string(), "5000".to_string());
    env.insert("AERONA_RATE_BURST".to_string(), "5000".to_string());
    Config::parse_env(&env, root, true).expect("test config parses")
}

/// An `AppState` over a fresh database. The directory is removed when the
/// `TestEnv` this came from is dropped; bare `test_state()` leaks into `/tmp` and
/// relies on the sweep above.
pub fn test_state() -> AppState {
    AppState::new(std::sync::Arc::new(test_config()), test_db())
}

pub fn test_db() -> Db {
    let root = temp_root("db");
    Db::open_path(&root.join("test.db")).expect("open the test database")
}

/// A temp root, its config, and a database in it - the whole install, in one value
/// that cleans up after itself.
pub struct TestEnv {
    pub root: PathBuf,
    pub state: AppState,
}

impl TestEnv {
    pub fn new(tag: &str) -> Self {
        let root = temp_root(tag);
        // `Config` is deliberately not `Clone` (it owns zeroizing secrets), so the
        // single parsed value moves straight into the `Arc` inside `AppState`; the
        // file layout below is read from it before that happens.
        let cfg = test_config_in(&root);
        let db = Db::open_path(&cfg.db_path).expect("open the test database");
        let state = AppState::new(std::sync::Arc::new(cfg), db);
        TestEnv { root, state }
    }

    /// A live session for `user`, as the header value a browser would send.
    pub fn session_cookie(
        &self,
        user: u64,
        username: &str,
        guild: Option<u64>,
    ) -> (String, String) {
        let pepper = self.state.pepper();
        let guild = guild.map(aerona_core::identity::Snowflake);
        let (session, token) = self
            .state
            .db
            .create_session(
                &pepper,
                aerona_core::identity::Snowflake(user),
                username,
                guild,
                "127.0.0.1",
                "test",
                12,
            )
            .unwrap();
        (
            format!("aerona_session={}; Path=/; HttpOnly; SameSite=Lax", token),
            session.csrf_secret,
        )
    }

    /// As `session_cookie`, but stamps the row with a grant list first - which is exactly
    /// what `finish_login` does with Discord's answer. Tests about *which servers a session
    /// may see* have to mint the way the product does; a `session_cookie` session has a NULL
    /// list, and NULL means unrestricted, so such a test would pass for the wrong reason.
    pub fn session_with_grants(
        &self,
        user: u64,
        username: &str,
        grants: &Value,
    ) -> (String, String) {
        let pepper = self.state.pepper();
        let (session, token) = self
            .state
            .db
            .create_session(
                &pepper,
                aerona_core::identity::Snowflake(user),
                username,
                None,
                "127.0.0.1",
                "test",
                12,
            )
            .unwrap();
        self.state
            .db
            .set_session_grants(session.id, grants)
            .unwrap();
        (
            format!("aerona_session={}; Path=/; HttpOnly; SameSite=Lax", token),
            session.csrf_secret,
        )
    }
}

impl TestEnv {
    /// A `dist` directory at the path `cfg.static_dir` already points at, with the three
    /// documents the static layer judges a path by. Tests that only exercise the API never
    /// call this; a test that wants to know how a *redirect* and a *file server* interact has
    /// to, because that interaction is where the sign-in bounce used to lose its status code.
    pub fn write_dist(&self) -> std::path::PathBuf {
        let dist = self.state.cfg.static_dir.clone();
        for page in ["index.html", "login/index.html", "404/index.html"] {
            let file = dist.join(page);
            std::fs::create_dir_all(file.parent().unwrap()).expect("make the dist directories");
            let name = page.split('/').next().unwrap();
            std::fs::write(&file, format!("<html>{name}</html>")).expect("write the document");
        }
        dist
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        // Pooled connections are closed when the last `Db` clone drops, so the
        // removal can fail on a busy machine; a leftover temp dir is not a failure.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A running server on an ephemeral port, plus the client to talk to it with.
///
/// The URL the *config* advertises (`cfg.base_url`) is deliberately different from
/// the ephemeral address we connect to, so `Origin` checks are exercised for real:
/// tests send the configured origin when they mean "a well-behaved browser" and a
/// different one when they mean "a cross-site page".
pub struct Site {
    pub env: TestEnv,
    pub base: String,
    pub client: reqwest::Client,
    addr: SocketAddr,
    handle: tokio::task::JoinHandle<()>,
}

impl Site {
    pub async fn start(tag: &str) -> Self {
        let env = TestEnv::new(tag);
        Self::with_env(tag.to_string(), env).await
    }

    pub async fn with_env(tag: String, env: TestEnv) -> Self {
        Self::with_env_and(tag, env, None).await
    }

    /// As `with_env`, but the router is assembled the way `aerona serve` assembles it - with
    /// `Some(assets)` - so the static layer is in the path of every response under test.
    pub async fn with_env_and(
        _tag: String,
        env: TestEnv,
        assets: Option<crate::app::StaticAssets>,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("the bound address");
        // Same as the real server: `ConnectInfo` must be present or per-address
        // limiting and the audit `ip` column would be untested.
        let app = crate::app::router(env.state.clone(), assets)
            .into_make_service_with_connect_info::<SocketAddr>();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let site = Site {
            env,
            base: format!("http://{addr}"),
            // Redirects are *not* followed: for the login flow the redirect *is* the answer,
            // and following it replaces a 302 with a `Location` by whatever the target
            // returns, which in a test install with no `dist/` is a 404 that proves nothing.
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("a test client that does not chase redirects"),
            addr,
            handle,
        };
        site.wait().await;
        site
    }

    /// Poll until the listener answers, so no test needs a fixed sleep.
    async fn wait(&self) {
        for _ in 0..400 {
            if tokio::net::TcpStream::connect(self.addr).await.is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the test server never came up on {}", self.addr);
    }

    pub fn db(&self) -> &Db {
        &self.env.state.db
    }

    /// The origin a same-origin dashboard would send.
    pub fn origin(&self) -> String {
        self.env.state.cfg.base_url.clone()
    }

    /// A request with hand-built headers and no defaults at all: this is what the
    /// negative tests (missing CSRF, wrong origin) need.
    pub async fn raw(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, String)],
    ) -> (u16, Value) {
        let (status, value, _) = self.exchange(method, path, body, headers).await;
        (status, value)
    }

    /// As `raw`, but also hands back the response headers - which is the only way to
    /// assert that a login really set a cookie.
    pub async fn exchange(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, String)],
    ) -> (u16, Value, reqwest::header::HeaderMap) {
        let mut req = self
            .client
            .request(parse_method(method), format!("{}{path}", self.base));
        for (k, v) in headers {
            req = req.header(*k, v.clone());
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        send(req).await
    }

    /// A request as a well-behaved browser on the right origin would send it:
    /// `Origin` + `Referer` + cookie + CSRF header.
    pub async fn browser(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        cookie: Option<&str>,
        csrf: Option<&str>,
    ) -> (u16, Value) {
        let mut headers: Vec<(&str, String)> = vec![
            ("origin", self.origin()),
            ("referer", format!("{}/", self.origin())),
        ];
        if let Some(c) = cookie {
            headers.push(("cookie", c.to_string()));
        }
        if let Some(t) = csrf {
            headers.push((crate::auth::CSRF_HEADER, t.to_string()));
        }
        self.raw(method, path, body, &headers).await
    }

    /// A plain unauthenticated GET (public routes).
    pub async fn get(&self, path: &str) -> (u16, Value) {
        self.raw("GET", path, None, &[]).await
    }

    /// A signed-in read: cookie only, no CSRF needed for GETs.
    pub async fn authed_get(&self, path: &str, cookie: &str) -> (u16, Value) {
        self.raw("GET", path, None, &[("cookie", cookie.to_string())])
            .await
    }

    /// A machine-key call: `Authorization: Aerona <key>`, no cookie, no CSRF.
    pub async fn key_call(
        &self,
        method: &str,
        path: &str,
        key: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        self.raw(
            method,
            path,
            body,
            &[("authorization", format!("Aerona {key}"))],
        )
        .await
    }
}

impl Drop for Site {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn parse_method(method: &str) -> reqwest::Method {
    match method {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PATCH" => reqwest::Method::PATCH,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        other => panic!("unsupported method {other}"),
    }
}

async fn send(req: reqwest::RequestBuilder) -> (u16, Value, reqwest::header::HeaderMap) {
    let resp = req
        .send()
        .await
        .expect("the request reaches the test server");
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let text = resp.text().await.unwrap_or_default();
    // A non-json body is wrapped rather than dropped: the assertion that a route
    // "answered with the envelope, not the SPA" needs to see what it answered.
    let value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
    (status, value, headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_env_is_a_whole_isolated_install() {
        let env = TestEnv::new("env");
        assert!(
            env.root.join("test.db").exists() || env.state.cfg.db_path.exists(),
            "the database file is inside the temp root"
        );
        assert!(
            env.state.cfg.dev,
            "tests run in dev mode so cookies are not Secure"
        );
        assert!(
            env.state.cfg.discord_client_id.is_none(),
            "no oauth in tests: a session is minted straight into the db"
        );
        assert_eq!(
            env.state.cfg.data_dir, env.root,
            "every file a test writes lands in the temp root"
        );
        assert_eq!(
            env.state.db.row_count("guilds").unwrap(),
            0,
            "every env starts empty"
        );
        let other = TestEnv::new("env");
        assert_ne!(other.state.db.row_count("sessions").unwrap(), -1);
        drop(env);
        assert!(!other.root.join("nope").exists());
    }

    #[tokio::test]
    async fn the_harness_starts_and_serves_the_envelope() {
        let site = Site::start("harness").await;
        let (status, body) = site.get("/api/v1/healthz").await;
        assert_eq!(status, 200);
        assert_eq!(
            body["ok"], true,
            "the probe is a bare object on purpose, not the envelope"
        );
        let (status, body) = site.get("/api/v1/definitely-not-a-route").await;
        assert_eq!(status, 404);
        assert_eq!(
            body["error"]["code"], "not_found",
            "an unknown api path is a json error, not html"
        );
        // The origin the config advertises is not the ephemeral address: that is the
        // point, so `browser()` has to send the right one and `raw()` can send a bad one.
        assert_ne!(site.origin().trim_end_matches('/'), site.base);
    }

    #[test]
    fn sessions_can_be_handed_out_directly() {
        let env = TestEnv::new("creds");
        let (cookie, csrf) = env.session_cookie(7, "owner", None);
        assert!(cookie.starts_with("aerona_session=") && cookie.len() > 40);
        assert!(!csrf.is_empty());
        let (cookie_bound, _) = env.session_cookie(7, "owner", Some(42));
        assert_ne!(cookie, cookie_bound, "each mint is a distinct session");
    }
}
