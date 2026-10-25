//! Application state, middleware stack and router assembly.
//!
//! The security model of the HTTP surface lives here, in one file, so it can be
//! audited in one read:
//!
//! * every response carries the full security-header set (CSP, HSTS, framing,
//!   CORP/COOP, referrer, permissions);
//! * every *mutating* request must pass an origin check and, when it carries a
//!   session cookie, the double-submit CSRF header;
//! * every request is metered per client address;
//! * bodies are capped before they are parsed, and handler futures are cancelled
//!   after a deadline so a slow client cannot pin a worker.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::header::{RETRY_AFTER, SET_COOKIE};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::any_service;
use axum::Router;
use serde_json::{json, Value};
use tower_http::services::{ServeDir, ServeFile};

use aerona_core::config::Config;
use aerona_core::db::Db;
use aerona_core::error::{Error, Result};
use aerona_core::identity::Snowflake;
use aerona_core::ratelimit::BucketTable;
use aerona_core::security;

use crate::auth::{self, Principal};
use crate::respond::{self, RequestId};

/// Max JSON body the API will read. Settings documents and rule lists are small;
/// anything bigger is either an abuse attempt or belongs in a file upload.
pub const MAX_BODY_BYTES: usize = 512 * 1024;

/// A handler may not run longer than this. The DB layer is synchronous with a
/// 5 s busy timeout, so 30 s is generous headroom, not a tuning knob.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Db,
    /// Per-address request budget for ordinary traffic.
    general: Arc<BucketTable<String>>,
    /// Much tighter budget for the endpoints that can mint or test credentials.
    auth: Arc<BucketTable<String>>,
    pub started_at: i64,
}

impl AppState {
    pub fn new(cfg: Arc<Config>, db: Db) -> Self {
        let general = Arc::new(BucketTable::new(
            cfg.rate_rps,
            cfg.rate_burst.max(1.0) as u32,
            900,
        ));
        // 6 requests/minute per address with a burst of 4 is enough for a normal
        // login and nowhere near enough to brute-force anything.
        let auth = Arc::new(BucketTable::new(0.1, 4, 3600));
        AppState {
            cfg,
            db,
            general,
            auth,
            started_at: aerona_core::time::now(),
        }
    }

    pub fn pepper(&self) -> [u8; 32] {
        self.cfg.pepper()
    }

    fn meter(
        &self,
        table: &BucketTable<String>,
        ip: &str,
        cost: f64,
    ) -> std::result::Result<(), u32> {
        table.check(&ip.to_string(), cost, aerona_core::time::now())
    }

    /// Public base URL without a trailing slash.
    pub fn base_url(&self) -> String {
        self.cfg.base_url.trim_end_matches('/').to_string()
    }
}

/// Every route below is mounted under `/api/v1`; the middleware therefore sees
/// paths relative to that prefix (see `api_path`).
#[derive(Debug, Clone)]
pub struct StaticAssets {
    pub dist_dir: std::path::PathBuf,
}

impl StaticAssets {
    pub fn exists(&self) -> bool {
        self.dist_dir.join("index.html").is_file()
    }
}

pub fn router(state: AppState, assets: Option<StaticAssets>) -> Router {
    let api = Router::new()
        .merge(crate::routes::system::routes())
        .merge(crate::routes::guild::routes())
        .merge(crate::routes::account::routes())
        // An unmatched /api/v1 path must answer with the error envelope, never with
        // the dashboard HTML — an SPA fallback on an API path turns a typo into a
        // confusing 200.
        .fallback(api_not_found);

    // No panic-catching layer: the release profile is `panic = "abort"`, so a panic
    // ends the process and the supervisor restarts it. Wrapping a service to catch
    // an unwind that cannot happen would only hide the bug in debug builds.
    let api = api
        .layer(middleware::from_fn_with_state(state.clone(), gate))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES));

    let mut app: Router = Router::new()
        .nest("/api/v1", api)
        .merge(crate::routes::system::public_routes())
        .with_state(state.clone());

    match assets.filter(|a| a.exists()) {
        Some(assets) => {
            // An unmatched path is *not* rewritten onto `index.html`. Every route of
            // this site is prerendered, so a client-side catch-all would only turn a
            // typo, a stale link or a traversal probe into a 200 with the home page —
            // a soft 404, which is worse for a crawler than a real one and worse for
            // an operator debugging a broken link. The prerendered `/404/` page is
            // served instead, with the status kept at 404 (see `static_headers`).
            let not_found = assets.dist_dir.join("404").join("index.html");
            let fallback = if not_found.is_file() {
                not_found
            } else {
                // An older or partial build without the prerendered 404 page: the shell
                // still renders `<NotFound/>` client-side, and the status is corrected to
                // 404 below either way.
                assets.dist_dir.join("index.html")
            };
            // The client's own routes get the app *shell*: the template, an empty `#root`, none
            // of the root page's markup. `/guild/123/tickets` has no file because its URL carries
            // a server id — the router inside the bundle is what knows that page exists. Handing
            // it `index.html` instead (the previous choice, and the reason every deep link logged
            // a recovered hydration mismatch) means hydrating a guild page against the prerendered
            // server list: React notices, throws the HTML away and mounts — so the page does work,
            // but each visit pays a discarded first paint plus a console error nobody can act on.
            // The empty root is the case the client is written for. `shell.html` is the same
            // template with its slots left unfilled, so it cannot go stale the way a copy of a
            // prerendered page would; a build without it keeps the old behaviour rather than 404.
            let shell_file = if assets.dist_dir.join("shell.html").is_file() {
                assets.dist_dir.join("shell.html")
            } else {
                assets.dist_dir.join("index.html")
            };
            let shell = ServeFile::new(shell_file);
            let serve = ServeDir::new(&assets.dist_dir).fallback(ServeFile::new(fallback));
            // Static files still get the security headers: they are what a browser
            // actually renders, so CSP matters most here.
            app = app
                .route("/guild/{*rest}", any_service(shell))
                .fallback_service(serve)
                .layer(middleware::from_fn_with_state(state, static_headers));
        }
        None => {
            app = app.fallback(dist_missing);
        }
    }
    app
}

/// The API's own heartbeat and hourly retention, in one task.
///
/// The snapshot is written under process `"api"` — a *separate* record from the bot's,
/// because a stalled dashboard process must not look like a stalled bot and vice
/// versa. `wsLatencyMs` is copied from the freshest bot status so the dashboard can
/// graph "API up, gateway latency" without a second query.
pub async fn spawn_stats_task(state: AppState) {
    let mut tick: u64 = 0;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        tick = tick.wrapping_add(1);
        let guilds = state.db.guilds().unwrap_or_default();
        let bot = state.db.latest_stats("bot").ok().flatten();
        let snap = aerona_core::models::StatsSnapshot {
            at: aerona_core::time::now(),
            process: "api".to_string(),
            rss_kb: proc_rss_kb(),
            // CPU is the supervisor's job to measure (`main.py check`), and a fake
            // number here would be worse than none.
            cpu_pct: 0.0,
            guilds: guilds.len() as i64,
            users_seen: state.db.user_count().unwrap_or(0),
            ws_latency_ms: bot.as_ref().map(|b| b.ws_latency_ms).unwrap_or(0),
            messages_per_min: bot.as_ref().map(|b| b.messages_per_min).unwrap_or(0),
            db_size_kb: state.db.size_kb().unwrap_or(0),
        };
        let _ = state.db.record_stats(&snap);
        // The dashboard is the only writer of sessions and pairing state, so it is
        // also the process that has to expire them.
        if tick % 180 == 0 {
            let _ = state.db.purge_oauth_state();
            let _ = state.db.purge_pairing_codes();
            let _ = state.db.purge_sessions();
            let _ = state.db.purge_outbox(7);
            let present: Vec<_> = guilds
                .iter()
                .filter(|g| g.bot_present)
                .map(|g| g.id)
                .collect();
            let _ = state.db.prune_bot_status(&present);
        }
        if matches!(state.db.housekeeping_due(3600), Ok(true)) {
            if let Ok(removed) = state.db.housekeeping(state.cfg.retention_days, 20_000) {
                let _ = state.db.mark_housekeeping();
                tracing::info!(?removed, "retention pass");
            }
        }
    }
}

/// Resident set from `/proc`, because `sysinfo` is not worth 2 MB of binary for one
/// number. `Statm` field 2 is resident pages.
fn proc_rss_kb() -> i64 {
    match std::fs::read_to_string("/proc/self/statm") {
        Ok(s) => s
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.parse::<i64>().ok())
            .map(|pages| pages * 4)
            .unwrap_or(0),
        Err(_) => 0,
    }
}

async fn api_not_found() -> Response {
    respond::error_response(&Error::NotFound("endpoint"))
}

/// Shown when the API is up but nobody has run the dashboard build yet — the
/// single most likely first-run state for someone who cloned this repo.
async fn dist_missing() -> Response {
    let body = "Aerona API is running, but the dashboard bundle was not found.\n\
                Build it with `python3 main.py build` (or `cd dashboard && npm install && npm run build`).\n";
    axum::response::Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| axum::response::Response::new(axum::body::Body::empty()))
}

/// Whether this URL names a build artifact with its content baked into its name, i.e.
/// `/assets/<stem>-<8+ alphanumerics>.<ext>` — vite's `[hash]` is base64-ish, so more than hex is
/// accepted. Deliberately strict about the rest: `logo-2.png` gets no forever-promise, because
/// that `2` is a number a human bumps rather than a digest.
fn is_fingerprinted(path: &str) -> bool {
    let Some(file) = path.strip_prefix("/assets/").filter(|f| !f.contains('/')) else {
        return false;
    };
    let stem = file.split('.').next().unwrap_or_default();
    let Some((before, hash)) = stem.rsplit_once('-') else {
        return false;
    };
    !before.is_empty() && hash.len() >= 8 && hash.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A panic inside a handler must not take the process down or leak a backtrace.
/// `panic = "abort"` in release means this only fires in dev, where the detail is
/// worth having; in release the layer is simply inert.
/// The gate: identity of the request, its meter, and the two CSRF-class checks.
async fn gate(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let id = RequestId(security::random_urlsafe(6));
    let ip = auth::client_ip(
        req.headers(),
        state.cfg.trusted_proxy,
        auth::peer_of(req.extensions()),
    );
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let is_auth_route = path.starts_with("/api/v1/auth") || path.ends_with("/pair");

    // Was there a session cookie to begin with? Answered here because `req` is moved into
    // the handler below, and the answer is needed only after the response exists.
    let carried_session = auth::cookie_value(req.headers(), auth::SESSION_COOKIE)
        .is_some_and(|v| !v.trim().is_empty());

    req.extensions_mut().insert(id.clone());

    // Metering happens before any parsing or database work so that a flood costs
    // us as little as possible.
    let metered = if is_auth_route {
        state.meter(&state.auth, &ip, 1.0)
    } else {
        // Credentials cost more than a read: they hit the sessions/keys tables.
        let cost = if method == Method::GET { 1.0 } else { 2.0 };
        state.meter(&state.general, &ip, cost)
    };
    if let Err(retry) = metered {
        tracing::warn!(ip, %path, retry, "rate limited");
        let mut resp = respond::error_response(&Error::RateLimited);
        resp.headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from(retry));
        return resp;
    }

    if auth::is_mutating(&method) {
        if let Err(e) = auth::check_origin(&state, req.headers())
            .and_then(|()| auth::check_csrf(&state, req.headers()))
        {
            // A rejected mutation is worth a warn line: either a CSRF attempt or a
            // misconfigured dashboard, and both deserve a look.
            tracing::warn!(ip, method = %method, path = %api_path(&path), code = e.code(), "rejected mutating request");
            return respond::error_response(&e);
        }
    }

    let outcome = tokio::time::timeout(REQUEST_TIMEOUT, next.run(req)).await;
    let mut resp = match outcome {
        Ok(resp) => resp,
        Err(_) => {
            tracing::warn!(path = %api_path(&path), "request timed out");
            respond::error_response(&Error::Internal)
        }
    };
    // A 401 that *carried* a session cookie means that cookie is nobody's session: expired,
    // revoked, or minted before `AERONA_MASTER_KEY` changed. The browser cannot notice —
    // `HttpOnly` is the point — so without this the install keeps refusing a request the
    // operator cannot fix except by finding "clear site data" themselves. The refusal is what
    // drops it, and the next sign-in writes a fresh one.
    if carried_session && resp.status() == StatusCode::UNAUTHORIZED {
        let cleared = auth::cleared_cookie(&state.cfg);
        if !cleared.is_empty() {
            // `append`, not `insert`: a logout that clears the cookie itself still gets to say so.
            resp.headers_mut().append(SET_COOKIE, cleared);
        }
    }
    decorate(&state, &mut resp, &id, Some(security::API_CACHE_CONTROL));
    resp
}

/// The `nest` prefix is stripped before the layer runs, so the path the middleware
/// sees is relative to the API root. Log lines quote the full one.
fn api_path(relative: &str) -> String {
    format!("/api/v1{relative}")
}

/// `cache_control` is `Some` for API responses (which must never be stored) and
/// `None` for static files, whose value the caller decides per file type.
fn decorate(state: &AppState, resp: &mut Response, id: &RequestId, cache_control: Option<&str>) {
    let headers = resp.headers_mut();
    for (name, value) in security::security_headers(None, state.cfg.cookies_secure()) {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            headers.insert(n, v);
        }
    }
    if let Ok(v) = HeaderValue::from_str(&id.0) {
        headers.insert(HeaderName::from_static("x-request-id"), v);
    }
    // API responses are per-user and mutable; caching them is both a correctness bug
    // and an information leak through shared caches. There is deliberately no
    // "is this an /api/ path" test here: `gate` is layered on the API router only,
    // and the `nest` prefix has already been stripped from the URI by the time the
    // layer runs, so such a test would silently be false for every request.
    if let Some(value) = cache_control {
        if let Ok(v) = HeaderValue::from_str(value) {
            headers.insert(axum::http::header::CACHE_CONTROL, v);
            headers.insert(
                axum::http::header::PRAGMA,
                HeaderValue::from_static("no-cache"),
            );
        }
    }
    // Without this a shared cache could serve a credentialed response to the next
    // client. `Origin` only when the install actually allows cross-origin calls.
    if !state.cfg.cors_origins.is_empty() {
        headers.append(axum::http::header::VARY, HeaderValue::from_static("Origin"));
    }
}

/// Static responses get the same headers but no `no-store` (assets are hash-named
/// and meant to be cached forever by the browser).
async fn static_headers(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let id = RequestId(security::random_urlsafe(6));
    let path = req.uri().path().to_string();

    // The layer sits on the assembled router, so it also sees API traffic. `/api/` is
    // handed straight back: `gate` already authenticated, metered, decorated and
    // decided the status of that request, and a static-root question ("does
    // `dist/api/v1/manifest` exist?") has no bearing on it. Answering here would turn
    // every endpoint into a 404 the moment its file is not on disk — which is exactly
    // what this function's own test caught.
    if path.starts_with("/api/") {
        return next.run(req).await;
    }
    let wants_document = req
        .headers()
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html") || v.contains("*/*"));

    // A dotfile in a *document root* is never a page: `.env`, `.git/config`,
    // `.template.html` and `.well-known/acme-challenge` either do not exist here or
    // must not be readable. Refusing on the path segment is what makes the answer not
    // depend on whether some future static host decides to serve hidden files, and it
    // is why the prerender template can live in `dist` at all.
    if path.split('/').any(|seg| seg.starts_with('.')) {
        tracing::debug!(path = %path, "refused a hidden path in the static root");
        return respond::error_response(&Error::NotFound("file"));
    }

    // A miss is decided here rather than inside the static service, so the answer can
    // match what was asked for: a browser gets the styled 404 page (with a real 404
    // status, which the file server cannot set on its own) and a program gets the JSON
    // error envelope it can parse. `ServeDir`'s fallback would otherwise hand out the
    // SPA shell at 200 to anybody who mistyped a URL.
    // `ROOT_PATHS` is the exception that a file check cannot express: `/auth/callback` is a
    // route, and a route that answers with a redirect is not a missing page however little it
    // resembles a file. Everything else that is neither a file nor a router path is a miss.
    let owned_by_the_router = crate::routes::system::ROOT_PATHS.iter().any(|p| *p == path)
        || crate::routes::system::owned_by_client_router(&path);
    if !owned_by_the_router && !static_hit(&state.cfg.static_dir, &path) {
        if !wants_document {
            return respond::error_response(&Error::NotFound("path"));
        }
        let mut resp = next.run(req).await;
        decorate(&state, &mut resp, &id, None);
        *resp.status_mut() = StatusCode::NOT_FOUND;
        return cache_and_finish(&state, &path, resp);
    }

    let mut resp = next.run(req).await;
    decorate(&state, &mut resp, &id, None);
    cache_and_finish(&state, &path, resp)
}

/// A *fingerprinted* file name may be cached forever; nothing else may, and HTML never is — a
/// moderation page that went stale in a proxy shows an operator the wrong state.
///
/// The test is the name, not the folder. `/assets/Guild-DCQ76Tg3.js` cannot change content
/// without changing URL, so a year is free. A replaced `/favicon.png` or `/theme-boot.js` keeps
/// its name and would stay invisible for a year on every machine that had already loaded it: the
/// "I redeployed and nothing changed" bug, and it was ours — this function handed `immutable` to
/// any 200 that was not HTML. Un-hashed files now get no promise from us and the browser
/// revalidates against the `ETag`/`Last-Modified` that `ServeFile` sends anyway.
fn cache_and_finish(_state: &AppState, path: &str, mut resp: Response) -> Response {
    if resp.status().is_success() && is_fingerprinted(path) {
        if let Ok(v) = HeaderValue::from_str("public, max-age=31536000, immutable") {
            if resp
                .headers()
                .get(axum::http::header::CACHE_CONTROL)
                .is_none()
            {
                resp.headers_mut()
                    .insert(axum::http::header::CACHE_CONTROL, v);
            }
        }
    }
    if resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"))
    {
        resp.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        );
    }
    resp
}

/// Did the static host actually have this file? Mirrors `ServeDir`'s own rule (a path
/// ending in `/` means `<path>index.html`) so the 404 downgrade above cannot fire on a
/// real hit, and a directory without an `index.html` still counts as a miss.
fn static_hit(root: &std::path::Path, path: &str) -> bool {
    let rel = path.trim_start_matches('/');
    let candidate = root.join(rel);
    if path.ends_with('/') {
        candidate.join("index.html").is_file()
    } else {
        // A directory counts as a hit so the redirect to its slash form still happens;
        // if that directory has no `index.html`, the redirect lands on the 404 page.
        candidate.is_file() || candidate.is_dir()
    }
}

/// Resolve the caller's authority over one guild. This is the single place where
/// "may this credential touch this server?" is answered.
pub fn require_guild(state: &AppState, principal: &Principal, guild: Snowflake) -> Result<()> {
    if !principal.allows_guild(Some(guild)) {
        return Err(Error::Forbidden(
            "this credential is scoped to another server",
        ));
    }
    if let Principal::Session { session, .. } = principal {
        if let Some(allowed) = session.allowed_guild {
            if allowed != guild {
                return Err(Error::Forbidden("this session is scoped to another server"));
            }
        }
        // What Discord said at sign-in, read back from the row. Permissions change on
        // Discord's side without any event reaching us, so a session is a snapshot and the
        // answer to "my access should have stopped" is to sign out and in again — or to let
        // AERONA_SESSION_HOURS do it.
        if !state.db.session_allows_guild(session.id, guild.as_i64())? {
            return Err(Error::Forbidden(
                "this session was issued for different servers — sign in again",
            ));
        }
    }
    let state_row = state.db.guild_state(guild)?;
    match state_row {
        None => Err(Error::NotFound("guild")),
        Some(s) if !s.bot_present => Err(Error::Conflict(
            "aerona is not present in that server — re-invite it before changing its settings"
                .into(),
        )),
        Some(_) => Ok(()),
    }
}

/// Handlers that mutate Discord-visible state answer `202` and queue the action;
/// this is the single funnel, so every such action is audited the same way.
pub fn queue_action(
    state: &AppState,
    principal: &Principal,
    ip: &str,
    guild: Snowflake,
    action: &str,
    args: Value,
    target: Option<String>,
) -> Result<Response> {
    let id = state.db.enqueue_action(
        guild,
        principal.user_id(),
        &principal.label(),
        action,
        &args,
        "",
    )?;
    state.db.audit_best_effort(
        aerona_core::db::security::AuditEntry::new(format!("outbox.{action}"))
            .for_guild(guild)
            .actor(
                principal.actor_type(),
                Some(principal.user_id()),
                &principal.label(),
            )
            .maybe_target(target)
            .ip(ip)
            .meta(json!({ "args": args })),
    );
    Ok(respond::accepted(id))
}

/// The machine-readable route manifest (`GET /api/v1/manifest`). The dashboard and
/// `main.py check` both read it, so it is derived from the same table the routes
/// are declared in — not a hand-maintained copy that can drift.
pub fn manifest(cfg: &Config) -> Value {
    json!({
        "version": aerona_core::VERSION,
        "apiVersion": aerona_core::API_VERSION,
        "auth": ["session cookie", "authorization: Aerona <key>"],
        // Stated here so a key can be created with scopes the API will accept,
        // rather than by trial and error.
        "scopes": aerona_core::db::security::SCOPES,
        "csrf": "x-aerona-csrf header required on every mutating request from a session",
        "limits": {
            "bodyBytes": MAX_BODY_BYTES,
            "rateRps": cfg.rate_rps,
            "rateBurst": cfg.rate_burst,
            "timeoutSecs": REQUEST_TIMEOUT.as_secs(),
        },
        "features": {
            "discordOauth": cfg.oauth_configured(),
            "sightengine": cfg.sightengine.is_some(),
            "trustedProxy": cfg.trusted_proxy,
        },
        "routes": ROUTES.iter().map(|(m, p, a, s, d)| json!({
            "method": m, "path": p, "auth": a, "scope": s, "summary": d,
        })).collect::<Vec<_>>(),
    })
}

/// `(method, path, auth, scope, summary)` — one row per endpoint.
pub const ROUTES: &[(&str, &str, &str, &str, &str)] = &[
    (
        "GET",
        "/me",
        "session|key",
        "",
        "who the caller is, plus the csrf value for a session",
    ),
    (
        "GET",
        "/status",
        "none",
        "",
        "install status: version, uptime, guild count, feature flags",
    ),
    (
        "GET",
        "/healthz",
        "none",
        "",
        "liveness probe for the supervisor",
    ),
    (
        "GET",
        "/manifest",
        "none",
        "",
        "machine-readable route and limit manifest",
    ),
    (
        "GET",
        "/auth/discord",
        "none",
        "",
        "start the Discord OAuth2 flow (one state, no PKCE)",
    ),
    (
        "GET",
        "/auth/discord/callback",
        "none",
        "",
        "OAuth2 redirect target (legacy path; the alias of /auth/callback)",
    ),
    (
        "POST",
        "/logout",
        "session",
        "",
        "revoke the current session",
    ),
    (
        "GET",
        "/guilds",
        "session|key",
        "",
        "servers this caller administers, with an invite link where the bot is absent",
    ),
    (
        "GET",
        "/guilds/{guild}",
        "session|key",
        "read",
        "all feature configs plus live state",
    ),
    (
        "GET",
        "/guilds/{guild}/settings/{feature}",
        "session|key",
        "read",
        "the stored document for one feature",
    ),
    (
        "PATCH",
        "/guilds/{guild}/settings/{feature}",
        "session|key",
        "config",
        "partial update of one feature document",
    ),
    // What the gateway worker last saw, and the two things only it can do. Both are queued
    // rather than performed: the API holds no Discord token, so a save that cannot reach
    // Discord is a saved setting with a row waiting, never a failed request.
    (
        "GET",
        "/guilds/{guild}/report",
        "session|key",
        "read",
        "the worker's own last report: commands, channels, roles, profile result",
    ),
    (
        "POST",
        "/guilds/{guild}/commands/sync",
        "session|key",
        "config",
        "queue a re-registration of this guild's slash commands",
    ),
    (
        "POST",
        "/guilds/{guild}/identity/apply",
        "session|key",
        "config",
        "queue this guild's bot nickname, bio and avatar onto Discord",
    ),
    (
        "PUT",
        "/guilds/{guild}/identity/avatar",
        "session|key",
        "config",
        "stage one avatar image for the next apply; never stored in the settings document",
    ),
    (
        "DELETE",
        "/guilds/{guild}/identity/avatar",
        "session|key",
        "config",
        "drop the staged avatar and queue a clear of Discord's copy",
    ),
    (
        "GET",
        "/guilds/{guild}/settings/{feature}/history",
        "session|key",
        "read",
        "hash-chained config change history",
    ),
    (
        "GET",
        "/guilds/{guild}/moderation/stats",
        "session|key",
        "read",
        "warn/strike/ban counts",
    ),
    (
        "GET",
        "/guilds/{guild}/warns",
        "session|key",
        "read",
        "warns for a user, or the guild's recent warns",
    ),
    (
        "POST",
        "/guilds/{guild}/warns",
        "session|key",
        "moderate",
        "warn a user (escalates at the strike threshold)",
    ),
    (
        "POST",
        "/guilds/{guild}/warns/{id}/pardon",
        "session|key",
        "moderate",
        "pardon one warn",
    ),
    (
        "DELETE",
        "/guilds/{guild}/users/{user}/warns",
        "session|key",
        "moderate",
        "clear a user's active warns",
    ),
    (
        "GET",
        "/guilds/{guild}/users/{user}/strikes",
        "session|key",
        "read",
        "active strikes",
    ),
    (
        "GET",
        "/guilds/{guild}/users/{user}/notes",
        "session|key",
        "read",
        "moderator notes",
    ),
    (
        "POST",
        "/guilds/{guild}/users/{user}/notes",
        "session|key",
        "moderate",
        "add a moderator note on a user",
    ),
    (
        "DELETE",
        "/guilds/{guild}/notes/{id}",
        "session|key",
        "moderate",
        "delete a note",
    ),
    (
        "GET",
        "/guilds/{guild}/top-warned",
        "session|key",
        "read",
        "most-warned members",
    ),
    (
        "POST",
        "/guilds/{guild}/actions",
        "session|key",
        "moderate",
        "queue a Discord action (kick/ban/unban/timeout/purge)",
    ),
    (
        "GET",
        "/guilds/{guild}/tickets",
        "session|key",
        "read",
        "open and recent tickets, newest first",
    ),
    (
        "POST",
        "/guilds/{guild}/tickets",
        "session|key",
        "tickets",
        "open a ticket",
    ),
    (
        "GET",
        "/guilds/{guild}/tickets/{id}",
        "session|key",
        "read",
        "one ticket with its transcript counts",
    ),
    (
        "POST",
        "/guilds/{guild}/tickets/{id}/claim",
        "session|key",
        "tickets",
        "claim (force to steal)",
    ),
    (
        "POST",
        "/guilds/{guild}/tickets/{id}/unclaim",
        "session|key",
        "tickets",
        "release a claim",
    ),
    (
        "PATCH",
        "/guilds/{guild}/tickets/{id}",
        "session|key",
        "tickets",
        "change status",
    ),
    (
        "GET",
        "/guilds/{guild}/tickets/{id}/transcript",
        "session|key",
        "read",
        "transcript rows",
    ),
    (
        "POST",
        "/guilds/{guild}/tickets/{id}/messages",
        "session|key",
        "tickets",
        "append a reply or internal note",
    ),
    (
        "GET",
        "/guilds/{guild}/ticket-panels",
        "session|key",
        "read",
        "ticket panels in any channel",
    ),
    (
        "POST",
        "/guilds/{guild}/ticket-panels",
        "session|key",
        "config",
        "install a ticket panel in a channel",
    ),
    (
        "DELETE",
        "/guilds/{guild}/ticket-panels/{id}",
        "session|key",
        "config",
        "remove a panel",
    ),
    (
        "GET",
        "/guilds/{guild}/stickies",
        "session|key",
        "read",
        "sticky messages",
    ),
    (
        "POST",
        "/guilds/{guild}/stickies",
        "session|key",
        "config",
        "add or update a sticky",
    ),
    (
        "DELETE",
        "/guilds/{guild}/stickies/{id}",
        "session|key",
        "config",
        "remove a sticky",
    ),
    (
        "GET",
        "/guilds/{guild}/raid-events",
        "session|key",
        "read",
        "raid history, newest first",
    ),
    (
        "POST",
        "/guilds/{guild}/raid-lockdown",
        "session|key",
        "moderate",
        "force or lift a lockdown",
    ),
    (
        "GET",
        "/guilds/{guild}/ai-events",
        "session|key",
        "read",
        "recent filter verdicts",
    ),
    (
        "POST",
        "/guilds/{guild}/ai-filter/test",
        "session|key",
        "read",
        "dry-run the filter on sample text",
    ),
    (
        "GET",
        "/guilds/{guild}/ai-filter/rules",
        "session|key",
        "read",
        "the guild's custom filter rules",
    ),
    (
        "POST",
        "/guilds/{guild}/ai-filter/rules",
        "session|key",
        "config",
        "add or update a rule",
    ),
    (
        "PATCH",
        "/guilds/{guild}/ai-filter/rules/{id}",
        "session|key",
        "config",
        "enable/disable a rule",
    ),
    (
        "DELETE",
        "/guilds/{guild}/ai-filter/rules/{id}",
        "session|key",
        "config",
        "delete a rule",
    ),
    (
        "GET",
        "/guilds/{guild}/levels",
        "session|key",
        "read",
        "level leaderboard",
    ),
    (
        "GET",
        "/guilds/{guild}/levels/{user}",
        "session|key",
        "read",
        "one member's level",
    ),
    (
        "GET",
        "/audit",
        "session|key",
        "read",
        "audit trail (own guild for a scoped key)",
    ),
    (
        "POST",
        "/audit/verify",
        "session",
        "",
        "recompute the audit hash chain",
    ),
    (
        "GET",
        "/keys",
        "session",
        "",
        "list API keys (never the secrets)",
    ),
    ("POST", "/keys", "session", "", "mint an API key"),
    ("DELETE", "/keys/{id}", "session", "", "revoke an API key"),
    (
        "GET",
        "/sessions",
        "session",
        "",
        "this user's active sessions",
    ),
    (
        "DELETE",
        "/sessions/{id}",
        "session",
        "",
        "revoke one session",
    ),
    (
        "POST",
        "/sessions/revoke-all",
        "session",
        "",
        "sign out everywhere else",
    ),
    (
        "GET",
        "/stats",
        "session",
        "",
        "latest bot+api resource snapshot",
    ),
    (
        "GET",
        "/stats/{process}",
        "session",
        "",
        "resource history for one process",
    ),
    (
        "GET",
        "/outbox/{id}",
        "session|key",
        "read",
        "poll a queued action",
    ),
    (
        "POST",
        "/housekeeping",
        "session",
        "",
        "run the retention pass now",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_routes_are_well_formed_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for (method, path, auth, scope, summary) in ROUTES {
            assert!(
                ["GET", "POST", "PATCH", "DELETE", "PUT"].contains(method),
                "{method}"
            );
            assert!(path.starts_with('/'), "{path}");
            assert!(!path.contains("//"), "{path}");
            assert!(summary.len() > 12, "{path} summary too thin");
            assert!(!auth.is_empty(), "{path} must declare its auth");
            // Every guild-scoped route states its scope, so an omitted `scope`
            // cannot silently mean "anything".
            if path.contains("{guild}") {
                assert!(
                    !scope.is_empty(),
                    "{path} is guild-scoped but declares no scope"
                );
            }
            assert!(
                seen.insert(format!("{method} {path}")),
                "duplicate route {method} {path}"
            );
        }
    }

    #[test]
    fn manifest_json_lists_every_route() {
        let m = manifest(&crate::testutil::test_config());
        let routes = m["routes"].as_array().unwrap();
        assert_eq!(routes.len(), ROUTES.len());
        assert_eq!(routes[0]["method"].as_str().unwrap(), "GET");
        assert_eq!(m["apiVersion"].as_str().unwrap(), aerona_core::API_VERSION);
        assert_eq!(
            m["limits"]["bodyBytes"].as_u64().unwrap(),
            MAX_BODY_BYTES as u64
        );
        assert!(m["routes"][0]["path"].as_str().unwrap().starts_with('/'));
        assert_eq!(
            m["scopes"].as_array().unwrap().len(),
            4,
            "read/moderate/config/tickets"
        );
        assert_eq!(m["routes"][0]["summary"].as_str().unwrap(), ROUTES[0].4);
    }

    #[test]
    fn buckets_separate_addresses_and_punish_bursts() {
        let state = crate::testutil::test_state();
        // The auth bucket is deliberately tiny (burst 4 at 0.1 rps): enough for a
        // pairing attempt plus a retry, nowhere near enough to guess anything.
        for _ in 0..4 {
            state.meter(&state.auth, "1.1.1.1", 1.0).unwrap();
        }
        let retry = state.meter(&state.auth, "1.1.1.1", 1.0).unwrap_err();
        assert!(
            retry >= 1,
            "retry-after must be a positive number of seconds"
        );
        // A different address is unaffected — this is what stops one noisy client
        // from locking the whole dashboard.
        state.meter(&state.auth, "2.2.2.2", 1.0).unwrap();
        // The general bucket is per-method weighted but far more generous.
        for _ in 0..40 {
            assert!(state.meter(&state.general, "3.3.3.3", 1.0).is_ok());
        }
    }

    #[test]
    fn static_asset_probe_only_looks_for_index_html() {
        let dir = tempfile::tempdir().unwrap();
        let assets = StaticAssets {
            dist_dir: dir.path().to_path_buf(),
        };
        assert!(!assets.exists(), "no index.html means no static site");
        std::fs::write(dir.path().join("index.html"), "<html></html>").unwrap();
        assert!(assets.exists());
    }

    #[test]
    fn only_a_digest_in_the_name_earns_a_year() {
        // The two names below are the real vite output of `npm run build`; the list after them is
        // what must *not* be pinned: a favicon is replaced by overwriting it, and `theme-boot.js`
        // is the file that decides the colours of the page you are looking at. Pinning either for
        // a year is how a redeploy looks like it did nothing.
        for forever in ["/assets/Guild-DCQ76Tg3.js", "/assets/index-4f2b1c9a.css"] {
            assert!(is_fingerprinted(forever), "{forever} is content-addressed");
        }
        for mutable in [
            "/index.html",
            "/shell.html",
            "/favicon.png",
            "/theme-boot.js",
            "/site.webmanifest",
            "/assets/logo-2.png",
            "/assets/index.js",
            "/assets/icons/panel-DmjTb0nQ.js",
            "/api/v1/status",
        ] {
            assert!(
                !is_fingerprinted(mutable),
                "{mutable} is not content-addressed"
            );
        }
    }
}
