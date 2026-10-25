//! Credential resolution: dashboard session cookie, API key, CSRF check, and the
//! guild-scope rule that decides whether a credential may touch a given guild.
//!
//! The whole point of this module is that handlers never parse headers themselves.
//! A handler asks for `Authed` and gets either an identity or a rejection, so
//! forgetting auth on a new route is impossible.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::header::{HeaderMap, HeaderValue};
use axum::http::request::Parts;
use axum::http::Method;

use aerona_core::config::Config;
use aerona_core::db::security::{ApiKeyRecord, Session};
use aerona_core::error::{Error, Result};
use aerona_core::identity::Snowflake;
use aerona_core::security;

use crate::app::AppState;

pub const SESSION_COOKIE: &str = "aerona_session";
/// The socket peer, when the server was configured to hand it over.
pub fn peer_of(extensions: &axum::http::Extensions) -> Option<SocketAddr> {
    extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0)
}

pub const CSRF_HEADER: &str = "x-aerona-csrf";
pub const API_KEY_PREFIXES: [&str; 3] = ["aerona ", "Aerona ", "Bearer "];

/// Who is calling. Both variants are "an operator", but the difference matters for
/// audit (`actor_type`) and for scoping: a key can be limited to one guild.
#[derive(Debug, Clone)]
pub enum Principal {
    Session {
        session: Session,
        /// The raw token, needed only to rotate/clear the cookie.
        token: String,
    },
    Key {
        key: ApiKeyRecord,
    },
}

impl Principal {
    pub fn user_id(&self) -> Snowflake {
        match self {
            Principal::Session { session, .. } => session.user_id,
            Principal::Key { key } => key.created_by,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Principal::Session { session, .. } => session.username.clone(),
            Principal::Key { key } => format!("key:{}", key.label),
        }
    }

    pub fn actor_type(&self) -> aerona_core::db::security::ActorType {
        match self {
            Principal::Session { .. } => aerona_core::db::security::ActorType::User,
            Principal::Key { .. } => aerona_core::db::security::ActorType::ApiKey,
        }
    }

    /// A key minted for guild A can never act on guild B. A session is not
    /// per-guild (the dashboard switches servers), so the guild membership check
    /// happens in `crate::app::require_guild`.
    pub fn allows_guild(&self, guild: Option<Snowflake>) -> bool {
        match self {
            Principal::Session { .. } => true,
            Principal::Key { key } => match (key.guild_id, guild) {
                (None, _) => true,
                (Some(scope), Some(g)) => scope == g,
                (Some(_), None) => false,
            },
        }
    }

    pub fn scopes(&self) -> Option<&[String]> {
        match self {
            Principal::Session { .. } => None,
            Principal::Key { key } => Some(&key.scopes),
        }
    }

    pub fn has_scope(&self, scope: &str) -> bool {
        match self {
            // Sessions are the dashboard: full access for a user who owns the install.
            Principal::Session { .. } => true,
            Principal::Key { key } => key
                .scopes
                .iter()
                .any(|s| s == scope || s == "config" && scope != "read"),
        }
    }
}

/// An authenticated principal plus the client IP for the audit trail.
#[derive(Debug, Clone)]
pub struct Authed {
    pub principal: Principal,
    pub ip: String,
}

impl Authed {
    pub fn user_id(&self) -> Snowflake {
        self.principal.user_id()
    }

    pub fn label(&self) -> String {
        self.principal.label()
    }
}

impl FromRequestParts<AppState> for Authed {
    type Rejection = crate::respond::Invalid;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> std::result::Result<Self, Self::Rejection> {
        let ip = client_ip(
            &parts.headers,
            state.cfg.trusted_proxy,
            peer_of(&parts.extensions),
        );
        let principal = authenticate(state, &parts.headers).await?;
        // A key bound to one server must not reach the install-wide surfaces:
        // `/keys`, `/sessions`, `/stats` and an unfiltered `/audit` all describe the
        // whole install. Guild routes apply the binding themselves; this is the
        // complement, and it lives in the extractor so a new route cannot forget it.
        // Paths here are relative to `/api/v1`: the `nest` prefix is stripped before
        // extractors run, so a test against "/api/v1/…" would never match.
        const GUILD_BOUND_ALLOWED: &[&str] = &["/me"];
        if let Principal::Key { key } = &principal {
            let path = parts.uri.path();
            let guild_route = path.contains("/guilds/") || GUILD_BOUND_ALLOWED.contains(&path);
            if key.guild_id.is_some() && !guild_route {
                return Err(crate::respond::Invalid(Error::Forbidden(
                    "this api key is bound to one server and may only be used on /guilds/{guild} routes",
                )));
            }
        }
        Ok(Authed { principal, ip })
    }
}

/// An authenticated caller that has also been checked against one guild.
///
/// This exists so a handler cannot forget the authorization step: asking for
/// `Guild` *is* the check. It resolves the `{guild}` path segment, verifies the
/// credential may act on it (key scope, session binding) and that Aerona is
/// actually present in that server, and only then hands the id over.
pub struct Guild {
    pub id: Snowflake,
    pub auth: Authed,
    /// Every path parameter of the request (`{feature}`, `{id}`, `{user}` …). It is
    /// carried here rather than extracted per-handler so a handler cannot read a path
    /// segment without also having passed the guild authorization above.
    pub params: std::collections::HashMap<String, String>,
}

impl Guild {
    pub fn actor(&self) -> Snowflake {
        self.auth.user_id()
    }

    pub fn label(&self) -> String {
        self.auth.label()
    }

    pub fn ip(&self) -> &str {
        &self.auth.ip
    }

    pub fn principal(&self) -> &Principal {
        &self.auth.principal
    }

    /// A required path segment. A missing one is a routing bug, not client input, so
    /// it is reported as `not found` on the address rather than a validation error.
    pub fn param(&self, key: &str) -> Result<String> {
        self.params
            .get(key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or(Error::NotFound("path segment"))
    }

    pub fn param_i64(&self, key: &str) -> Result<i64> {
        let raw = self.param(key)?;
        raw.parse::<i64>()
            .map_err(|_| Error::Invalid(format!("{key} must be a number")))
    }

    pub fn param_feature(&self) -> Result<aerona_core::models::Feature> {
        let raw = self.param("feature")?;
        // One parser for both the path segment and the query, so the accepted
        // spelling of a feature name can never drift between them.
        crate::routes::feature(&raw)
    }

    pub fn param_user(&self) -> Result<Snowflake> {
        let raw = self.param("user")?;
        parse_guild(&raw)
    }
}

impl FromRequestParts<AppState> for Guild {
    type Rejection = crate::respond::Invalid;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> std::result::Result<Self, Self::Rejection> {
        let auth = Authed::from_request_parts(parts, state).await?;
        let params =
            <axum::extract::Path<std::collections::HashMap<String, String>> as FromRequestParts<
                AppState,
            >>::from_request_parts(parts, state)
            .await
            .map_err(|_| crate::respond::Invalid(Error::Invalid("missing guild id".into())))?;
        let raw = params.0.get("guild").cloned().unwrap_or_default();
        let id = parse_guild(&raw).map_err(crate::respond::Invalid)?;
        crate::app::require_guild(state, &auth.principal, id).map_err(crate::respond::Invalid)?;
        Ok(Guild {
            id,
            auth,
            params: params.0,
        })
    }
}

/// Resolve headers into a principal. Session cookie first (the dashboard is the
/// common case), then an API key.
pub async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Principal> {
    let pepper = state.pepper();
    if let Some(cookie) = cookie_value(headers, SESSION_COOKIE) {
        if let Some(session) = state
            .db
            .lookup_session(&pepper, &cookie)
            .map_err(auth_reject)?
        {
            // Sliding expiry, throttled inside the DB layer.
            let _ = state.db.touch_session(session.id, state.cfg.session_hours);
            return Ok(Principal::Session {
                token: cookie,
                session,
            });
        }
        return Err(Error::Unauthorized);
    }
    if let Some(raw) = header_str(headers, "authorization") {
        let candidate = API_KEY_PREFIXES
            .iter()
            .find_map(|p| raw.strip_prefix(p))
            .unwrap_or(raw)
            .trim();
        if candidate.is_empty() {
            return Err(Error::Unauthorized);
        }
        if let Some(key) = state
            .db
            .lookup_api_key(&pepper, candidate)
            .map_err(auth_reject)?
        {
            let _ = state.db.touch_api_key(key.id, true);
            return Ok(Principal::Key { key });
        }
        return Err(Error::Unauthorized);
    }
    Err(Error::Unauthorized)
}

/// A DB error while authenticating must not turn into a 500 with a path in it, and
/// must not tell the caller *why* the credential failed.
fn auth_reject(e: Error) -> Error {
    match e {
        Error::Db(_) | Error::Io(_) | Error::Internal => {
            tracing::error!(%e, "auth lookup failed");
            Error::Internal
        }
        other => other,
    }
}

/// Double-submit CSRF: the value must equal the session's own secret. A cross-site
/// form can send the cookie (browser-managed) but cannot read the secret, so it
/// cannot produce this header. API keys are exempt — they are never sent by a
/// browser automatically, which is why the dashboard's fetch also uses `SameSite`.
pub fn check_csrf(state: &AppState, headers: &HeaderMap) -> Result<()> {
    let cookie = match cookie_value(headers, SESSION_COOKIE) {
        // Key-authenticated callers have no cookie to forge against.
        None => return Ok(()),
        Some(c) => c,
    };
    let Some(sent) = header_str(headers, CSRF_HEADER).map(|s| s.to_string()) else {
        return Err(Error::Invalid("missing csrf header".into()));
    };
    let session = state
        .db
        .lookup_session(&state.pepper(), &cookie)
        .map_err(auth_reject)?
        .ok_or(Error::Unauthorized)?;
    if security::ct_eq(&sent, &session.csrf_secret) {
        Ok(())
    } else {
        Err(Error::Invalid(
            "csrf header does not match the session".into(),
        ))
    }
}

/// Methods that mutate state. `GET`/`HEAD`/`OPTIONS` are safe by HTTP definition.
pub fn is_mutating(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// `Set-Cookie` for a fresh session. `httpOnly` (JS cannot read it), `SameSite=Lax`
/// (top-level navigation still carries it, so the OAuth redirect works), and
/// `Secure` whenever the install's own base URL is HTTPS.
pub fn session_cookie(config: &Config, token: &str, max_age: i64) -> HeaderValue {
    let secure = if config.cookies_secure() {
        "; Secure"
    } else {
        ""
    };
    let value = format!(
        "{SESSION_COOKIE}={token}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Lax{secure}"
    );
    HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static(""))
}

pub fn cleared_cookie(config: &Config) -> HeaderValue {
    let secure = if config.cookies_secure() {
        "; Secure"
    } else {
        ""
    };
    let value = format!("{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{secure}");
    HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static(""))
}

pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(';'))
        .map(str::trim)
        // Match the *name* first: `find_map` on `split_once` would stop at the first
        // cookie that merely has an `=`, so a session cookie behind any other cookie
        // would read as "not signed in".
        .find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == name).then(|| v.to_string())
        })
}

pub fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Best-effort client address. Behind a reverse proxy the socket peer is `127.0.0.1`,
/// so `X-Forwarded-For` is the only useful value — but it is attacker-controlled
/// unless the operator says a proxy sits in front, hence the explicit opt-in.
/// The address to bill and to log.
///
/// `X-Forwarded-For` is believed only when the operator said a proxy sits in front
/// (`AERONA_TRUSTED_PROXY`); otherwise the socket peer is the only source that cannot
/// be forged. `peer` comes from `ConnectInfo`, which is why both the real server and
/// the test harness serve through `into_make_service_with_connect_info` — without it
/// every client would share one rate-limit bucket and every audit row would be
/// anonymous.
pub fn client_ip(headers: &HeaderMap, trusted_proxy: bool, peer: Option<SocketAddr>) -> String {
    if trusted_proxy {
        if let Some(xff) = header_str(headers, "x-forwarded-for") {
            if let Some(first) = xff.split(',').next() {
                let ip = first.trim().trim_start_matches("(inline)");
                if !ip.is_empty() && ip.len() <= 64 {
                    return security::normalize_ip(ip);
                }
            }
        }
    }
    // No proxy believed: the socket it came from. "unknown" only appears when the
    // server was built without connect info, which is a misconfiguration we want to
    // see in the audit log rather than a plausible-looking 0.0.0.0.
    peer.map(|p| p.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// `Origin`/`Referer` check for credentialed mutations. Same-origin by default;
/// `AERONA_CORS_ORIGINS` widens it. Without this, a malicious page on any port of
/// the same host could hit the API with the dashboard's cookie.
pub fn check_origin(state: &AppState, headers: &HeaderMap) -> Result<()> {
    let Some(origin) = header_str(headers, "origin").map(|s| s.to_string()) else {
        // Non-browser clients (curl, CLI, service-to-service) send no Origin.
        // They also send no cookie, so there is nothing to protect here.
        return Ok(());
    };
    if state.cfg.allows_origin(&origin) {
        return Ok(());
    }
    // The same-host fallback, which is what makes an install reached by IP:port —
    // a game panel, a VPS before anybody points a domain at it — usable while
    // `AERONA_BASE_URL` is still unset. It accepts only an IP literal or
    // `localhost`, and it is never the only line of defence: `check_csrf` still
    // demands the double-submit header, which an attacker's page cannot read.
    let host = header_str(headers, "host").unwrap_or("");
    let same_host = state.cfg.allows_same_host(&origin, host);
    if same_host {
        return Ok(());
    }
    if let Some(referer) = header_str(headers, "referer").map(|s| s.to_string()) {
        if origin_from_url(&referer).as_deref() == Some(origin.trim_end_matches('/'))
            && (state.cfg.allows_origin(&referer) || same_host)
        {
            return Ok(());
        }
    }
    Err(Error::Forbidden(
        "cross-origin request blocked — if this is your own dashboard, set AERONA_BASE_URL \
         to the URL your browser uses (see .env.example)",
    ))
}

pub fn origin_from_url(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r)?;
    let host = rest.split('/').next()?;
    Some(format!("{}://{}", &url[..url.find("://").unwrap()], host))
}

/// A `?guild=` / path id is the only way to address a server; anything that is not
/// a plain digit string is rejected rather than parsed leniently.
pub fn parse_guild(raw: &str) -> Result<Snowflake> {
    // Strictly digits, no surrounding whitespace: `Snowflake::parse` is lenient
    // about padding (it is used on form input), and a path segment with a space in
    // it is a bug in the caller, not something to accept quietly.
    if raw.is_empty() || raw.len() > 20 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Invalid(
            "guild id must be a discord snowflake".into(),
        ));
    }
    let parsed = Snowflake::parse(raw)
        .map_err(|_| Error::Invalid("guild id must be a discord snowflake".into()))?;
    if parsed.is_zero() {
        return Err(Error::Invalid(
            "guild id must be a discord snowflake".into(),
        ));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Names are `'static` because `HeaderMap::append` needs `IntoHeaderName`, which
    // is implemented for `&'static str`; values may be borrowed, since only the
    // header *name* has to be interned.
    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn cookie_parsing_is_exact() {
        let h = headers(&[("cookie", "other=1; aerona_session=abc.def; theme=dark")]);
        assert_eq!(cookie_value(&h, SESSION_COOKIE).as_deref(), Some("abc.def"));
        let h2 = headers(&[("cookie", "aerona_sessionX=no")]);
        assert_eq!(cookie_value(&h2, SESSION_COOKIE), None);
        let h3 = headers(&[("cookie", "aerona_session=")]);
        assert_eq!(cookie_value(&h3, SESSION_COOKIE).as_deref(), Some(""));
        assert_eq!(cookie_value(&HeaderMap::new(), SESSION_COOKIE), None);
        // Two cookie headers are legally merged; we must find the right one.
        let mut h4 = HeaderMap::new();
        h4.append("cookie", HeaderValue::from_static("a=1"));
        h4.append("cookie", HeaderValue::from_static("aerona_session=z"));
        assert_eq!(cookie_value(&h4, SESSION_COOKIE).as_deref(), Some("z"));
    }

    #[test]
    fn only_mutating_methods_need_csrf() {
        assert!(!is_mutating(&Method::GET));
        assert!(!is_mutating(&Method::HEAD));
        assert!(!is_mutating(&Method::OPTIONS));
        assert!(is_mutating(&Method::POST));
        assert!(is_mutating(&Method::PATCH));
        assert!(is_mutating(&Method::PUT));
        assert!(is_mutating(&Method::DELETE));
    }

    #[test]
    fn forwarded_for_is_used_only_when_a_proxy_is_trusted() {
        let peer: Option<SocketAddr> = "203.0.113.9:5555".parse().ok();
        let h = headers(&[("x-forwarded-for", "203.0.113.7:1234, 10.0.0.1")]);
        assert_eq!(
            client_ip(&h, false, peer),
            "203.0.113.9",
            "untrusted: the socket wins, the header is ignored entirely"
        );
        assert_eq!(
            client_ip(&h, true, peer),
            "203.0.113.7",
            "trusted: first hop, port dropped"
        );
        let empty = headers(&[("x-forwarded-for", "   ")]);
        assert_eq!(
            client_ip(&empty, true, peer),
            "203.0.113.9",
            "an empty list falls back to the socket"
        );
        assert_eq!(
            client_ip(&empty, true, None),
            "unknown",
            "no connect info is a misconfiguration, visible as such"
        );
        assert_eq!(client_ip(&empty, false, None), "unknown");
        let huge = format!("{}{}", "a".repeat(200), ", x");
        let h2 = headers(&[("x-forwarded-for", huge.as_str())]);
        assert_eq!(
            client_ip(&h2, true, peer),
            "203.0.113.9",
            "oversize is spoofing, not an address"
        );
    }

    /// A `Config` + state for the origin gate only: nothing here touches the database,
    /// and the *absence* of `AERONA_BASE_URL` is the thing under test.
    fn state_at(extra: &[(&'static str, &'static str)]) -> crate::AppState {
        use std::collections::HashMap;
        use std::sync::Arc;

        let mut env: HashMap<String, String> = extra
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        env.insert("AERONA_TOKEN".into(), crate::testutil::FAKE_TOKEN.into());
        env.insert("AERONA_PORT".into(), "8791".into());
        let cfg = aerona_core::config::Config::parse_env(&env, std::path::Path::new("/tmp"), true)
            .expect("the test environment is valid");
        crate::AppState::new(Arc::new(cfg), crate::testutil::test_db())
    }

    /// The game-panel case: nobody set `AERONA_BASE_URL`, and the browser dialled an IP
    /// with a port. Without reading the request's own `Host`, every mutation — the
    /// pairing login included — is a 403 forever.
    #[test]
    fn an_install_that_never_named_its_origin_accepts_its_own_host() {
        let state = state_at(&[]);
        assert!(
            !state.cfg.base_url_explicit,
            "this test is about the unset case"
        );
        let same = headers(&[
            ("origin", "http://10.0.0.5:25565"),
            ("host", "10.0.0.5:25565"),
        ]);
        assert!(
            check_origin(&state, &same).is_ok(),
            "the origin the browser dialled is its own: {}",
            state.cfg.base_url
        );

        // A name that merely happens to resolve to this box is not the same thing — that
        // is DNS rebinding, and it is why the fallback is limited to IPs and localhost.
        let rebind = headers(&[
            ("origin", "http://evil.example:25565"),
            ("host", "evil.example:25565"),
        ]);
        assert!(
            check_origin(&state, &rebind).is_err(),
            "a hostile name must not pass by matching its own Host"
        );

        // Another port on the same box is a different origin, and so is another scheme
        // with a port that does not appear in Host.
        for (origin, host) in [
            ("http://10.0.0.5:1", "10.0.0.5:25565"),
            ("http://10.0.0.5", "10.0.0.5:25565"),
            // Deliberately avoids `http://localhost:8791`, which passes for a different
            // reason: that is what `base_url` falls back to, and the exact match is the
            // primary rule. These fail both rules.
            ("http://localhost:9999", "127.0.0.1:8791"),
            ("http://127.0.0.1:8791", "127.0.0.1:9999"),
        ] {
            let h = headers(&[("origin", origin), ("host", host)]);
            assert!(
                check_origin(&state, &h).is_err(),
                "{origin} vs {host} must fail"
            );
        }

        // No `Origin` at all is a CLI: it has no cookie, so there is nothing to ride.
        assert!(check_origin(&state, &headers(&[("host", "10.0.0.5:25565")])).is_ok());
        // A spoofed-looking Host (credentials in it) is refused, not normalised.
        let creds = headers(&[
            ("origin", "http://127.0.0.1:8791"),
            ("host", "admin@127.0.0.1:8791"),
        ]);
        assert!(check_origin(&state, &creds).is_err());
    }

    /// Naming the origin is the switch that turns the fallback off: with
    /// `AERONA_BASE_URL` set, only that origin (and `AERONA_CORS_ORIGINS`) counts.
    #[test]
    fn a_named_origin_switches_the_fallback_off() {
        let state = state_at(&[("AERONA_BASE_URL", "http://127.0.0.1:8791")]);
        let ip_origin = headers(&[
            ("origin", "http://127.0.0.1:8791"),
            ("host", "127.0.0.1:8791"),
        ]);
        assert!(
            check_origin(&state, &ip_origin).is_ok(),
            "the named origin passes"
        );
        let other_host = headers(&[
            ("origin", "http://10.0.0.5:8791"),
            ("host", "10.0.0.5:8791"),
        ]);
        assert!(
            check_origin(&state, &other_host).is_err(),
            "the fallback must be inert once the origin is named"
        );
        // …and the widened list still works, which is the documented answer for a
        // deployment behind a proxy that rewrites Host.
        let state2 = state_at(&[
            ("AERONA_BASE_URL", "https://aerona.example"),
            ("AERONA_CORS_ORIGINS", "http://10.0.0.5:8791"),
        ]);
        assert!(check_origin(&state2, &other_host).is_ok());
    }

    #[test]
    fn origin_extraction_matches_browsers() {
        assert_eq!(
            origin_from_url("https://aerona.example.com/guild/1?x=1").as_deref(),
            Some("https://aerona.example.com")
        );
        assert_eq!(
            origin_from_url("http://localhost:8080/").as_deref(),
            Some("http://localhost:8080")
        );
        assert_eq!(origin_from_url("not-a-url"), None);
    }

    #[test]
    fn guild_ids_are_strict() {
        assert_eq!(parse_guild("123").unwrap(), Snowflake(123));
        for bad in [
            "",
            " 123 ",
            "0x1",
            "-5",
            "1e3",
            "007",
            &"1".repeat(21),
            "12a",
        ] {
            assert!(parse_guild(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn key_scopes_gate_guilds_but_sessions_do_not() {
        let key = ApiKeyRecord {
            id: 1,
            label: "ci".into(),
            hint: "aer_live_x".into(),
            guild_id: Some(Snowflake(5)),
            scopes: vec!["read".into()],
            created_by: Snowflake(9),
            created_at: 0,
            last_used_at: None,
            expires_at: 0,
            revoked_at: None,
            request_count: 0,
        };
        let p = Principal::Key { key: key.clone() };
        assert!(p.allows_guild(Some(Snowflake(5))));
        assert!(
            !p.allows_guild(Some(Snowflake(6))),
            "a scoped key stays in its guild"
        );
        assert!(
            !p.allows_guild(None),
            "install-wide routes need a global key"
        );
        assert!(p.has_scope("read"));
        assert!(!p.has_scope("moderate"));
        let global = Principal::Key {
            key: ApiKeyRecord {
                guild_id: None,
                scopes: vec!["read".into(), "moderate".into()],
                ..key
            },
        };
        assert!(global.allows_guild(Some(Snowflake(6))));
        assert!(global.allows_guild(None));
        assert!(global.has_scope("moderate"));
        assert_eq!(global.label(), "key:ci");
        assert_eq!(global.user_id(), Snowflake(9));
    }

    #[test]
    fn session_cookie_attributes_are_right() {
        fn cfg(base: &str) -> Config {
            let mut env = std::collections::HashMap::new();
            env.insert(
                "AERONA_TOKEN".into(),
                "MTIzNDU2Nzg5MDEyMzQ1Njc4OQ.Ga1b2c.dH1SvAkeThAtIsHaSisVErS2NyM3Nw".into(),
            );
            env.insert("AERONA_PORT".into(), "8080".into());
            env.insert("AERONA_BASE_URL".into(), base.into());
            Config::parse_env(&env, std::path::Path::new("/x"), false).unwrap()
        }
        let https = cfg("https://aerona.example.com");
        let c = session_cookie(&https, "tok", 3600);
        let s = c.to_str().unwrap().to_string();
        assert!(
            s.starts_with(
                "aerona_session=tok; Path=/; Max-Age=3600; HttpOnly; SameSite=Lax; Secure"
            ),
            "{s}"
        );
        assert!(
            !s.contains("Domain="),
            "never widen the cookie to subdomains"
        );

        let local = cfg("http://localhost:8080");
        let lc = session_cookie(&local, "t", 1).to_str().unwrap().to_string();
        assert!(
            lc.contains("HttpOnly") && lc.contains("SameSite=Lax") && !lc.contains("Secure"),
            "an http origin must not advertise itself as secure: a spec-following client \
             would then never send the cookie back: {lc}"
        );

        let plain = cfg("http://192.168.1.20:8080");
        let pc = session_cookie(&plain, "t", 1).to_str().unwrap().to_string();
        assert!(
            !pc.contains("Secure"),
            "a LAN http origin must not be told it is secure: {pc}"
        );

        let cleared = cleared_cookie(&https).to_str().unwrap().to_string();
        assert!(cleared.contains("aerona_session=;"), "{cleared}");
        assert!(
            cleared.contains("Max-Age=0"),
            "logout must actually expire it"
        );
    }
}
