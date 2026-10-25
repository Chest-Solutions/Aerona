//! Configuration: `.env` loading, parsing and validation.
//!
//! Contract with the operator:
//!
//! * Exactly two variables are **required** — `AERONA_TOKEN` and `AERONA_PORT`.
//!   Everything else has a safe default. `AERONA_BOT=off` drops the token
//!   requirement: the dashboard then runs on its own, which is what a first boot on
//!   a fresh box looks like (site up, bot added later).
//! * Values from the process environment win over the file, because that is how
//!   containers and systemd units are usually configured.
//! * Secrets are parsed into [`Secret`] and never included in an error message.
//!   A misconfiguration error says *which* variable is wrong, never what its
//!   value was.
//! * Parsing is a pure function over a `HashMap`, so the whole policy — including
//!   every rejection — is unit-testable without touching the filesystem.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::security::Secret;

/// Everything both binaries need to start.
///
/// Deliberately **not** `Clone`: secrets must have exactly one owner in memory,
/// so both binaries share an `Arc<Config>` instead of copying tokens around.
#[derive(Debug)]
pub struct Config {
    /// Discord bot token (never logged).
    pub token: Secret,
    /// Public port: hosts the dashboard **and** `/api/v1` on `0.0.0.0`.
    pub port: u16,
    /// Sightengine credentials (`user:secret`), optional at runtime.
    pub sightengine: Option<(String, Secret)>,
    pub db_path: PathBuf,
    pub data_dir: PathBuf,
    /// Where the built dashboard lives. Served by the same port as the API, so a
    /// self-hosted install exposes one socket. Relative paths resolve against the
    /// process working directory, which `main.py` sets to the repo root.
    pub static_dir: PathBuf,
    /// Canonical origin, e.g. `https://aerona.example` — used for OAuth redirect
    /// and to decide whether `Secure` cookies are safe to set.
    pub base_url: String,
    /// Whether `base_url` came from `AERONA_BASE_URL` or from the localhost default.
    ///
    /// It is the switch behind [`Config::allows_same_host`]: an install that never
    /// named its origin may trust the `Host` header, one that did must match it
    /// exactly.
    pub base_url_explicit: bool,
    pub discord_client_id: Option<String>,
    /// The `redirect_uri` to send Discord, verbatim, from `AERONA_OAUTH_REDIRECT_URI`.
    ///
    /// This exists because Discord matches that value against the application's
    /// OAuth2 → Redirects list **character for character** — scheme, host, port, path,
    /// trailing slash, query — and there is no prefix or wildcard matching. Deriving it
    /// from the public origin is right almost always, and hopeless in the cases that are
    /// worth naming: a portal entry registered as `http://localhost:25154/…` while the
    /// browser dials the box by IP; a TLS terminator in front that changes the scheme; a
    /// second install sharing one application; or a panel port that changed once and was
    /// never re-registered. Pin the exact string Discord expects and the mismatch cannot
    /// drift, because both halves of the flow (authorize and token exchange) read the same
    /// field. Leave it empty and the origin is derived, which is what a fresh install wants.
    pub oauth_redirect_uri: Option<String>,
    pub discord_client_secret: Option<Secret>,
    /// Pepper for token/session MACs. Generated and persisted if not provided.
    pub master_key: Option<Secret>,
    pub session_hours: i64,
    pub rate_rps: f64,
    pub rate_burst: f64,
    pub log_level: String,
    /// Message-content intent is opt-in; see [`Config::content_intent`].
    pub content_intent: bool,
    /// Whether this process should hold a Discord gateway connection.
    ///
    /// `AERONA_BOT=off` (also `false`, `0`, `no`) turns it off, which is the only
    /// supported way to run the dashboard with no bot at all: nothing else in the
    /// product changes, and the outbox simply stays queued until a worker shows up.
    pub bot_enabled: bool,
    pub cors_origins: Vec<String>,
    /// Set only when something *you* control (nginx/caddy) terminates TLS in front
    /// of the API. It is the switch that makes `X-Forwarded-For` load-bearing for
    /// rate limiting and audit; without it, the socket peer is used and a client
    /// cannot fake its own address.
    pub trusted_proxy: bool,
    /// Total timeout for one outbound request to Discord (login exchange, guild
    /// list) and the shorter connect timeout used to fail fast.
    pub http_timeout_secs: u64,
    pub http_connect_timeout_secs: u64,

    /// Days of events/audit/stats kept before the hourly pass deletes them.
    pub retention_days: i64,
    pub dev: bool,
}

impl Config {
    /// Keys this process reads, for `python3 main.py check`. `AERONA_TOKEN` is one
    /// of them because the default deployment is a bot; `AERONA_BOT=off` waives it.
    ///
    /// Deliberately a `const` and not a runtime claim: the template test below proves
    /// every one of these appears in `DOTENV_TEMPLATE`.
    pub const REQUIRED: [&'static str; 2] = ["AERONA_TOKEN", "AERONA_PORT"];

    /// The 32-byte pepper every secret in the database is keyed with: API key MACs,
    /// session MACs, pairing-code hashes. Derived from `AERONA_MASTER_KEY` when set,
    /// otherwise from the bot token, so an install always has *some* stable secret.
    ///
    /// Rotating it (changing `AERONA_MASTER_KEY`) invalidates every session and API
    /// key at once. That is the intended behaviour of an install-wide pepper — it is
    /// the "log everyone out" button that needs no database access.
    pub fn pepper(&self) -> [u8; 32] {
        let material = self
            .master_key
            .as_ref()
            .map(|s| s.expose().to_string())
            .unwrap_or_else(|| self.token.expose().to_string());
        crate::security::derive_pepper(&material)
    }

    /// Give `pepper()` real entropy when there is no bot token to derive it from.
    ///
    /// A normal install has either `AERONA_MASTER_KEY` or a bot token, so this is a
    /// no-op. A dashboard-only install (`AERONA_BOT=off`, no token, no master key)
    /// has neither, and deriving the pepper from the empty string would let anybody
    /// who read this source forge a session for it. So the first boot writes 32
    /// random bytes to `data/install.key` (mode 0600) and uses them as the master
    /// key; every later boot reads the same file, which is also what lets a backup
    /// and restore keep its sessions.
    ///
    /// Returns the path when this call created the file, so the caller can say so.
    pub fn ensure_pepper_secret(&mut self) -> Result<Option<PathBuf>> {
        if self.master_key.is_some() || !self.token.expose().is_empty() {
            return Ok(None);
        }
        let path = self.data_dir.join("install.key");
        match std::fs::read_to_string(&path) {
            Ok(existing) => {
                let trimmed = existing.trim().to_string();
                if trimmed.len() < 32 {
                    // Truncated or half-written: refusing beats silently rotating the
                    // pepper, which would log every operator out on each restart.
                    return Err(Error::Config(format!(
                        "{} is too short to be a secret ({} bytes) - delete it and restart",
                        path.display(),
                        trimmed.len()
                    )));
                }
                self.master_key = Some(Secret::new(trimmed));
                Ok(None)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let secret = hex::encode(crate::security::random_bytes(32));
                std::fs::create_dir_all(&self.data_dir).map_err(Error::io)?;
                write_private(&path, &secret)?;
                self.master_key = Some(Secret::new(secret));
                Ok(Some(path))
            }
            Err(e) => Err(Error::io(e)),
        }
    }

    /// Where Discord must send the browser back to. `origin` is the value
    /// [`Self::request_origin`] approved for this request; a pinned
    /// `AERONA_OAUTH_REDIRECT_URI` replaces the derivation entirely.
    /// Where Discord sends the browser back to, relative to the origin. A top-level path,
    /// not one under `/api/v1`: this is the shape the portal entry has to carry, it reads as
    /// a login endpoint rather than an API endpoint, and the older
    /// `/api/v1/auth/discord/callback` is kept as a permanent alias so an install that was
    /// configured before does not silently break.
    pub const CALLBACK_PATH: &'static str = "/auth/callback";

    pub fn redirect_uri(&self, origin: &str) -> String {
        if let Some(pinned) = &self.oauth_redirect_uri {
            return pinned.clone();
        }
        format!("{}{}", origin.trim_end_matches('/'), Self::CALLBACK_PATH)
    }

    /// What the operator has to paste into the Discord Developer Portal, and where.
    ///
    /// The sentence is here rather than in the README because the number one cause of
    /// Discord's `Invalid OAuth2 redirect_uri` screen is a portal entry that differs from
    /// the one actually being sent by one character, and the only reliable way to see that
    /// is to read both back from the machine that is doing the sending.
    pub fn redirect_uri_guidance(&self, origin: Option<&str>) -> Vec<String> {
        let mut out = Vec::new();
        let shown = match (&self.oauth_redirect_uri, origin) {
            (Some(pinned), _) => {
                out.push(format!(
                    "pinned by AERONA_OAUTH_REDIRECT_URI, so it is used verbatim for both halves of the flow: {pinned}"
                ));
                pinned.clone()
            }
            (None, Some(origin)) => {
                let uri = self.redirect_uri(origin);
                out.push(format!(
                    "derived from this install's own origin ({origin}); set AERONA_OAUTH_REDIRECT_URI to pin it verbatim: {uri}"
                ));
                uri
            }
            (None, None) => {
                out.push(
                    "cannot be derived: this install is reached by a host it was never configured with. \
                     Set AERONA_BASE_URL (or AERONA_OAUTH_REDIRECT_URI) to the address in the browser's bar, port included."
                        .into(),
                );
                return out;
            }
        };
        out.extend(self.redirect_uri_shape_notes(&shown));
        out
    }

    /// The three shapes that produce Discord's error screen with an otherwise correct setup:
    /// a trailing slash, a query string, and plain `http` on a host that is not local.
    ///
    /// These sentences deliberately never repeat the value back. That is what lets the
    /// unauthenticated `GET /api/v1/status` carry them for a URI it derived from a request's own
    /// `Host`, where echoing the string would turn the endpoint into an oracle for whatever name
    /// a client asked about (`redirect_uri_guidance`, which does interpolate, is only used where
    /// the value is the operator's own).
    pub fn redirect_uri_shape_notes(&self, shown: &str) -> Vec<String> {
        let mut out = Vec::new();
        if shown.ends_with('/') {
            out.push(
                "this value ends with a slash, so the portal entry must end with one too — Discord's \
                 Add Redirect box adds one when you press Enter without checking"
                    .into(),
            );
        }
        if shown.contains('?') {
            out.push(
                "this value carries a query string, which Discord only accepts if the portal entry repeats \
                 it byte for byte; a per-login state must go in the authorize URL, never here"
                    .into(),
            );
        }
        if shown.starts_with("http://") && !redirect_uri_is_local(shown) {
            out.push(
                "plain http on a non-local host works with Discord, but the portal entry has to say http:// \
                 as well; the moment you put TLS in front, both sides must move to https:// together"
                    .into(),
            );
        }
        let _ = &self;
        out
    }

    /// The `redirect_uri` a login would send when there is no request to learn an origin
    /// from — which is the situation `check` is in. `Some` only for the two
    /// operator-authored sources, so the string printed by `check` is always one the
    /// operator wrote, and never a guess that happens to be right today.
    pub fn redirect_uri_configured(&self) -> Option<String> {
        if let Some(pinned) = self.oauth_redirect_uri.as_deref() {
            return Some(pinned.to_string());
        }
        if self.base_url_explicit {
            return Some(self.redirect_uri(&self.base_url));
        }
        None
    }

    /// Which of the three sources `redirect_uri_configured` answered from.
    pub fn redirect_source_configured(&self) -> &'static str {
        if self.oauth_redirect_uri.is_some() {
            "pinned by AERONA_OAUTH_REDIRECT_URI"
        } else if self.base_url_explicit {
            "derived from AERONA_BASE_URL"
        } else {
            "not configured — derived per request from the browser's own origin"
        }
    }

    /// The absolute origin a browser should be sent back to *for this request*.
    ///
    /// `base_url` is the operator's answer, so once it was configured it always wins:
    /// reflecting `Host` on an install that has chosen a name would let any client steer
    /// an OAuth `redirect_uri` — and therefore the authorization code — to a host of its
    /// own. Unconfigured is the game-panel case, and here the internal default is actively
    /// wrong: `http://localhost:PORT` is a *different origin* from the address in the bar,
    /// so minting an absolute link from it sends the browser to `localhost` after a login
    /// and Discord rejects the redirect before that. So reflect the request's own host, but
    /// only under [`Self::host_is_self`].
    ///
    /// `None` therefore means "a name nobody told us about", and the caller must say *set
    /// `AERONA_BASE_URL`* rather than guess one.
    pub fn request_origin(&self, host: &str, proto: &str) -> Option<String> {
        if self.base_url_explicit {
            return Some(self.base_url.trim_end_matches('/').to_string());
        }
        if !Self::host_is_self(host) {
            return None;
        }
        // Only ever two values, so a forwarded scheme cannot smuggle text into a URL.
        let proto = if proto.eq_ignore_ascii_case("https") {
            "https"
        } else {
            "http"
        };
        Some(format!("{proto}://{host}"))
    }

    /// Whether a `Host` value can only ever point back at this machine: an IP literal,
    /// `localhost`, or `*.localhost`. Both same-origin fallback and origin reflection use
    /// this, because both are safe for exactly the same reason — a browser-typed IP cannot
    /// be re-pointed by DNS, while a name in someone else's zone can (rebinding) — and
    /// `*.localhost` is reserved for loopback by RFC 6761. An `@` means userinfo, not a
    /// host, and is refused before anything is parsed.
    fn host_is_self(host: &str) -> bool {
        if host.is_empty() || host.contains('@') || host.contains('/') {
            return false;
        }
        // Split off the port by hand: `SocketAddr` would reject a bare `[::1]` with no
        // port, and a Host header is allowed to omit one.
        let name = if let Some(rest) = host.strip_prefix('[') {
            rest.split(']').next().unwrap_or("")
        } else {
            match host.split_once(':') {
                Some((name, port))
                    if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    name
                }
                _ => host,
            }
        };
        name.eq_ignore_ascii_case("localhost")
            || name.to_ascii_lowercase().ends_with(".localhost")
            || name.parse::<std::net::IpAddr>().is_ok()
    }

    pub fn oauth_configured(&self) -> bool {
        self.discord_client_id.is_some() && self.discord_client_secret.is_some()
    }

    /// `Secure` on the session cookie, and nothing but an `https` origin earns it.
    ///
    /// A plain-http install therefore gets a plain cookie, and `main.py` names the exposure
    /// in its startup notes rather than refusing to boot — a game panel hands out one raw TCP
    /// port with nothing in front of it, which is the ordinary shape of a deployment here.
    pub fn cookies_secure(&self) -> bool {
        self.base_url.starts_with("https://")
    }

    /// `Origin` values allowed on credentialed cross-origin requests. Empty means
    /// same-origin only, which is what a dashboard should default to.
    pub fn allows_origin(&self, origin: &str) -> bool {
        if self.cors_origins.iter().any(|o| o == origin) {
            return true;
        }
        let self_origin = self.base_url.trim_end_matches('/');
        origin.trim_end_matches('/') == self_origin
    }

    /// The same-host fallback, and the only reason a panel install works at all.
    ///
    /// `AERONA_BASE_URL` exists so the browser's `Origin` can be checked against a
    /// name the operator chose. On a game panel nobody sets it — the "domain" is an IP
    /// with a port, and the default `http://localhost:PORT` rejects every real
    /// browser, so logging in would 403 forever. So: when the origin was *never*
    /// configured, accept an `Origin` that equals this request's own `Host`.
    ///
    /// That is not a relaxation of the CSRF rule, it is the same rule read from the
    /// other side: an attacker's page cannot make a browser send its own host in
    /// `Host`. The one thing it cannot tell apart is DNS rebinding — a hostile name
    /// that resolves to your box — which is why the fallback is refused for any host
    /// that is not an IP literal or `localhost`. Deploy on a domain, set
    /// `AERONA_BASE_URL`, and the exact-match path above is the only one used.
    pub fn allows_same_host(&self, origin: &str, host: &str) -> bool {
        if self.base_url_explicit || !Self::host_is_self(host) {
            return false;
        }
        let origin = origin.trim_end_matches('/');
        // The scheme is whatever the browser says it is and both are acceptable:
        // behind a TLS-terminating proxy the page's origin is https while our own
        // default URL is http, and the host is still the same box.
        origin == format!("http://{host}") || origin == format!("https://{host}")
    }

    /// Load `.env` (from `dir`, or `AERONA_DOTENV`) and build a config.
    pub fn load(dir: &Path, dev: bool) -> Result<Self> {
        let mut env = HashMap::new();
        for candidate in dotenv_candidates(dir) {
            if candidate.is_file() {
                for (k, v) in parse_dotenv(&std::fs::read_to_string(&candidate).map_err(Error::io)?)
                {
                    env.insert(k, v);
                }
                break;
            }
        }
        // Process environment overrides the file.
        for (k, v) in std::env::vars() {
            if k.starts_with("AERONA_") || k.starts_with("SIGHTENGINE_") || k == "RUST_LOG" {
                env.insert(k, v);
            }
        }
        Self::parse_env(&env, dir, dev)
    }

    /// The whole policy, as a pure function.
    pub fn parse_env(env: &HashMap<String, String>, root: &Path, dev: bool) -> Result<Self> {
        let get = |k: &str| {
            env.get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        // The worker switch is read first because it decides whether a token is
        // mandatory. Anything that is not an explicit negative turns the bot on.
        let bot_enabled = match get("AERONA_BOT") {
            Some(v) => !matches!(
                v.to_ascii_lowercase().as_str(),
                "off" | "false" | "0" | "no" | "none" | "disabled"
            ),
            None => true,
        };
        let token = match get("AERONA_TOKEN") {
            Some(v) => {
                let secret = Secret::new(v);
                validate_token_shape(secret.expose())?;
                secret
            }
            None if bot_enabled => {
                return Err(Error::Config(
                    "AERONA_TOKEN is missing — set your Discord bot token in .env, \
                     or set AERONA_BOT=off to run the dashboard without a worker"
                        .into(),
                ))
            }
            // No worker, no token: the pepper then comes from
            // `ensure_pepper_secret`, never from an empty string.
            None => Secret::new(String::new()),
        };

        let port: u16 = get("AERONA_PORT")
            .ok_or_else(|| Error::Config("AERONA_PORT is missing — the dashboard port".into()))?
            .parse()
            .map_err(|_| {
                Error::Config("AERONA_PORT must be an integer between 1 and 65535".into())
            })?;
        if port == 0 {
            // `0` means "let the OS pick" for bind() — it would hand the dashboard
            // a random port and every OAuth/callback URL would silently break.
            return Err(Error::Config(
                "AERONA_PORT must be between 1 and 65535".into(),
            ));
        }
        if port < 1024 && !dev {
            // Binding a low port needs root; a bot dashboard should never.
            return Err(Error::Config(format!(
                "AERONA_PORT {port} is privileged; use >= 1024 (or --dev to override)"
            )));
        }

        let sightengine = match get("SIGHTENGINE_TOKEN") {
            Some(raw) => {
                let (user, secret) = crate::security::split_credential(&raw).ok_or_else(|| {
                    Error::Config(
                        "SIGHTENGINE_TOKEN must look like `user:secret` (from sightengine.com → Account)".into(),
                    )
                })?;
                Some((user, Secret::new(secret)))
            }
            None => None,
        };

        let data_dir = get("AERONA_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("data"));
        let static_dir = get("AERONA_STATIC_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("dashboard/dist"));
        let static_dir = if static_dir.is_absolute() {
            static_dir
        } else {
            root.join(static_dir)
        };
        let db_path = get("AERONA_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("aerona.db"));
        let db_path = if db_path.is_absolute() {
            db_path
        } else {
            root.join(db_path)
        };

        let port_str = port.to_string();
        let oauth_redirect_uri = match get("AERONA_OAUTH_REDIRECT_URI") {
            Some(raw) => {
                let v = raw.trim().to_string();
                if v.is_empty() {
                    None
                } else if !v.starts_with("http://") && !v.starts_with("https://") {
                    // A relative value is the intuitive thing to write and the one thing
                    // Discord can never match, so it is refused at load rather than at the
                    // first login attempt — twenty minutes later, on someone else's screen.
                    return Err(Error::Config(
                        "AERONA_OAUTH_REDIRECT_URI must be an absolute http(s) URL, e.g. http://127.0.0.1:25154/auth/callback"
                            .into(),
                    ));
                } else if v.chars().any(char::is_whitespace) {
                    return Err(Error::Config(
                        "AERONA_OAUTH_REDIRECT_URI contains whitespace — paste the URL alone, Discord compares it character for character"
                            .into(),
                    ));
                } else if v.contains('#') {
                    return Err(Error::Config(
                        "AERONA_OAUTH_REDIRECT_URI must not contain a fragment (#…) — an OAuth callback is a server endpoint and the fragment never reaches it"
                            .into(),
                    ));
                } else {
                    Some(v)
                }
            }
            None => None,
        };
        let base_url_explicit = get("AERONA_BASE_URL").is_some();
        let base_url = get("AERONA_BASE_URL").unwrap_or_else(|| {
            if dev {
                format!("http://localhost:{port_str}")
            } else {
                // Without an explicit origin we cannot mint a valid OAuth
                // redirect, so we say so loudly instead of guessing a domain.
                format!("http://localhost:{port_str}")
            }
        });
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(Error::Config(
                "AERONA_BASE_URL must start with http:// or https://".into(),
            ));
        }

        let session_hours = match get("AERONA_SESSION_HOURS") {
            Some(v) => v.parse::<i64>().map_err(|_| {
                Error::Config("AERONA_SESSION_HOURS must be an integer number of hours".into())
            })?,
            None => 12,
        };
        if !(1..=24 * 30).contains(&session_hours) {
            return Err(Error::Config(
                "AERONA_SESSION_HOURS must be between 1 and 720".into(),
            ));
        }

        let rate_rps = parse_f64(env, "AERONA_RATE_RPS", 20.0)?;
        let rate_burst = parse_f64(env, "AERONA_RATE_BURST", rate_rps * 3.0)?;
        if !(0.05..=100_000.0).contains(&rate_rps) || rate_burst < rate_rps {
            return Err(Error::Config(
                "AERONA_RATE_RPS must be 0.05..=100000 and AERONA_RATE_BURST >= AERONA_RATE_RPS"
                    .into(),
            ));
        }

        Ok(Self {
            token,
            port,
            sightengine,
            db_path,
            data_dir,
            static_dir,
            base_url,
            base_url_explicit,
            oauth_redirect_uri,
            discord_client_id: get("AERONA_DISCORD_CLIENT_ID"),
            discord_client_secret: get("AERONA_DISCORD_CLIENT_SECRET").map(Secret::new),
            master_key: get("AERONA_MASTER_KEY").map(Secret::new),
            session_hours,
            rate_rps,
            rate_burst,
            log_level: get("AERONA_LOG_LEVEL").unwrap_or_else(|| "info".into()),
            content_intent: get("AERONA_CONTENT_INTENT").as_deref() == Some("true"),
            bot_enabled,
            cors_origins: get("AERONA_CORS_ORIGINS")
                .map(|v| {
                    v.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|s| s.trim_end_matches('/').to_string())
                        .collect()
                })
                .unwrap_or_default(),
            trusted_proxy: get("AERONA_TRUSTED_PROXY").as_deref() == Some("true"),
            http_timeout_secs: parse_u64(env, "AERONA_HTTP_TIMEOUT_SECS", 10)?,
            http_connect_timeout_secs: parse_u64(env, "AERONA_HTTP_CONNECT_TIMEOUT_SECS", 5)?,

            retention_days: parse_i64(env, "AERONA_RETENTION_DAYS", 90)?.clamp(1, 3650),
            dev,
        })
    }

    /// Human summary for `main.py check` — secrets redacted by construction.
    pub fn summary(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "port            {}", self.port);
        let _ = writeln!(s, "base_url        {}", self.base_url);
        let _ = writeln!(
            s,
            "oauth_redirect  {}",
            self.oauth_redirect_uri
                .as_deref()
                .unwrap_or("(derived from the request origin)")
        );
        let _ = writeln!(s, "db              {}", self.db_path.display());
        let _ = writeln!(
            s,
            "dashboard       {}{}",
            self.static_dir.display(),
            if self.static_dir.join("index.html").is_file() {
                ""
            } else {
                "  (not built)"
            }
        );
        let _ = writeln!(
            s,
            "bot token       {}",
            match (self.bot_enabled, self.token.is_empty()) {
                (true, false) => "set",
                (true, true) => "missing",
                (false, _) => "not used (AERONA_BOT=off)",
            }
        );
        let _ = writeln!(
            s,
            "sightengine     {}",
            match &self.sightengine {
                Some((user, _)) => format!("configured (user {user})"),
                None => "not configured — AI text filter disabled".into(),
            }
        );
        let _ = writeln!(
            s,
            "discord sign-in {}",
            if self.oauth_configured() {
                "configured"
            } else {
                "not configured — nobody can sign in to the dashboard"
            }
        );
        let _ = writeln!(s, "secure cookies  {}", self.cookies_secure());
        let _ = writeln!(s, "content intent  {}", self.content_intent);
        let _ = writeln!(
            s,
            "trusted proxy   {}",
            if self.trusted_proxy {
                "X-Forwarded-For is believed"
            } else {
                "off (socket peer only)"
            }
        );
        s
    }
}

fn parse_u64(env: &HashMap<String, String>, key: &str, default: u64) -> Result<u64> {
    match env.get(key).map(|v| v.trim()).filter(|v| !v.is_empty()) {
        Some(v) => v
            .parse::<u64>()
            .map_err(|_| Error::Config(format!("{key} must be a non-negative whole number"))),
        None => Ok(default),
    }
}

fn parse_i64(env: &HashMap<String, String>, key: &str, default: i64) -> Result<i64> {
    match env.get(key).map(|v| v.trim()).filter(|v| !v.is_empty()) {
        Some(v) => v
            .parse::<i64>()
            .map_err(|_| Error::Config(format!("{key} must be a whole number"))),
        None => Ok(default),
    }
}

fn parse_f64(env: &HashMap<String, String>, key: &str, default: f64) -> Result<f64> {
    match env.get(key).map(|v| v.trim()).filter(|v| !v.is_empty()) {
        Some(v) => v
            .parse::<f64>()
            .map_err(|_| Error::Config(format!("{key} must be a number"))),
        None => Ok(default),
    }
}

/// Structural check on a bot token: `base64(userId).timestamp.hmac` — three dot
/// separated parts, sane length. We deliberately do *not* validate the HMAC:
/// that would require the app secret, and Discord's `whoami` at boot is the real
/// test. But we do refuse obviously wrong input so a copy-pasted invite link
/// fails at boot with a clear message instead of an opaque 401 later.
fn validate_token_shape(token: &str) -> Result<()> {
    if token.len() < 40 || token.len() > 512 {
        // Checked first: a truncated paste is the most common mistake by far.
        return Err(Error::Config(
            "AERONA_TOKEN has an unexpected length — check for a truncated copy-paste".into(),
        ));
    }
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 || parts.len() > 4 {
        return Err(Error::Config(
            "AERONA_TOKEN does not look like a Discord bot token (expected dot-separated segments)"
                .into(),
        ));
    }
    if token.contains("://") || token.contains(' ') {
        return Err(Error::Config(
            "AERONA_TOKEN contains a URL or spaces — paste only the token".into(),
        ));
    }
    if parts[0]
        .bytes()
        .any(|b| !(b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    {
        return Err(Error::Config(
            "AERONA_TOKEN's first segment is not base64url — check for stray characters".into(),
        ));
    }
    Ok(())
}

/// Write a secret so that only the owner can read it. On unix that is a mode; on
/// windows the ACL is inherited from the data directory, which is the best a
/// standard-library-only implementation can do, so the file mode is documented as
/// "data directory readable only by you".
fn write_private(path: &Path, secret: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .and_then(|mut f| {
                use std::io::Write as _;
                f.write_all(secret.as_bytes())?;
                f.write_all(b"\n")
            })
            .map_err(Error::io)?;
        // `create_new` honours umask for the mode bits, so set them explicitly.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(Error::io)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, format!("{secret}\n")).map_err(Error::io)
    }
}

/// Whether a redirect URI points back at the machine it is served from, which is the only
/// case where `http://` is unremarkable (RFC 6761 reserves `*.localhost`, and an IP literal
/// cannot be re-pointed by DNS).
fn redirect_uri_is_local(uri: &str) -> bool {
    let Some(rest) = uri.split_once("://").map(|(_, r)| r) else {
        return false;
    };
    let host = rest.split(['/', '?']).next().unwrap_or("");
    let name = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    let name = match name.rsplit_once(':') {
        Some((n, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => n,
        _ => name,
    };
    let name = name.trim_matches(['[', ']']);
    name.eq_ignore_ascii_case("localhost")
        || name.to_ascii_lowercase().ends_with(".localhost")
        || name.eq_ignore_ascii_case("127.0.0.1")
        || name == "::1"
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn dotenv_candidates(root: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(explicit) = std::env::var("AERONA_DOTENV") {
        v.push(PathBuf::from(explicit));
    }
    v.push(root.join(".env"));
    if let Some(parent) = root.parent() {
        v.push(parent.join(".env"));
    }
    v.push(PathBuf::from(".env"));
    v
}

/// Peel a dotenv value.
///
/// Single quotes are literal, double quotes carry `\n` / `\t` / `\"` escapes, and — the
/// part that used to be wrong — a comment may follow the closing quote. Somebody who
/// writes `AERONA_DB='data/x.db' # moved` is following the shell manual, and parsing that
/// as a path containing a quote and a comment would put a nonsense file on disk.
fn unquote(value: &str) -> String {
    let Some(quote) = value.as_bytes().first().copied() else {
        return value.to_string();
    };
    if quote != b'"' && quote != b'\'' {
        // Bare value: a comment starts at " #", which is what dotenv defines — an
        // unquoted '#' inside a token (a password, a path) stays part of it.
        return match value.find(" #") {
            Some(i) => value[..i].trim_end().to_string(),
            None => value.to_string(),
        };
    }
    let closing = quote as char;
    let Some(offset) = value[1..].find(closing) else {
        // An unterminated quote is kept exactly as written: guessing which half the
        // operator meant is how a token ends up truncated 40 characters later.
        return value.to_string();
    };
    let inner = &value[1..1 + offset];
    if quote == b'"' {
        inner
            .replace("\\n", "\n")
            .replace("\\t", "\t")
            .replace("\\\"", "\"")
    } else {
        inner.to_string()
    }
}

/// Minimal dotenv: `KEY=value`, `export KEY=...`, `#` comments, single/double
/// quotes, and `\n` escapes inside double quotes. Anything malformed is
/// ignored rather than fatal — an unreadable line in someone's shell scratch
/// file should not stop a bot from starting.
pub fn parse_dotenv(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            continue;
        }
        let value = unquote(value.trim());
        out.push((key.to_string(), value));
    }
    out
}

/// Template written next to the repo so a fresh clone knows what to fill in.
pub const DOTENV_TEMPLATE: &str = "\
# Aerona — required
AERONA_TOKEN=put-your-discord-bot-token-here
AERONA_PORT=8080

# Set to `off` to run the dashboard with no Discord worker: it is also the only way
# the token becomes optional. Anything else - a blank value included - means connect.
AERONA_BOT=

# AI text moderation (sightengine.com). Format: user:secret
# Get both from https://dashboard.sightengine.com/account
SIGHTENGINE_TOKEN=

# Aerona — optional
# AERONA_BASE_URL is the exact origin your browser types, port included, no
# trailing slash. Left empty, the API accepts the host you actually reach it on
# (an IP with a port, `localhost`, a `*.localhost` name), which is what a game panel
# or a tunnel wants. A named install must set it: a name nobody configured is refused
# for browser writes - the DNS rebinding guard - and Discord sign-in stays off, since
# there is no safe redirect to build from an unverified Host. Plain http is normal
# here; https is what earns the session cookie a Secure flag.
AERONA_BASE_URL=
# AERONA_OAUTH_REDIRECT_URI is Discord's callback URL verbatim, and only needed when
# what you registered in the Developer Portal is not exactly the origin above plus
# /auth/callback - a localhost entry while you dial an IP, a port that
# changed, or one application shared by two installs. Discord matches character for
# character. main.py check prints the value in use.
AERONA_OAUTH_REDIRECT_URI=
AERONA_DISCORD_CLIENT_ID=
AERONA_DISCORD_CLIENT_SECRET=
# Pepper for session/API-key MACs. If unset it is derived from AERONA_TOKEN; a
# dashboard-only install (AERONA_BOT=off) instead generates and stores
# data/install.key on first boot. Set this explicitly if you replicate the DB.
AERONA_MASTER_KEY=
AERONA_DB=data/aerona.db
AERONA_DATA_DIR=data
# Where the built dashboard is served from (relative to this directory).
AERONA_STATIC_DIR=dashboard/dist
AERONA_SESSION_HOURS=12
AERONA_RATE_RPS=20
AERONA_RATE_BURST=60
AERONA_LOG_LEVEL=info
# Opt in to the privileged Message Content intent (needed for prefix commands).
AERONA_CONTENT_INTENT=false
# Extra origins allowed to call the API with credentials. Empty = same-origin only.
AERONA_CORS_ORIGINS=
# Set to true ONLY when a reverse proxy you control terminates TLS in front of
# Aerona. It makes X-Forwarded-For authoritative for rate limiting and audit;
# without it a client cannot spoof its address into a different rate-limit bucket.
AERONA_TRUSTED_PROXY=false
# Outbound request timeouts for the Discord login flow, in seconds.
AERONA_HTTP_TIMEOUT_SECS=10
AERONA_HTTP_CONNECT_TIMEOUT_SECS=5
# Days of audit/events/stats kept before the hourly retention pass deletes them.
AERONA_RETENTION_DAYS=90
";

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like a real token (3 segments, base64url first part, ~70 chars).
    const TOKEN: &str =
        "MTIzNDU2Nzg5MDEyMzQ1Njc4OQ.Ga1b2c.dH1SvAkeThAtIsHaSisVErS2NyM3NwXYZabcDEFghiJKLmnoPQRstu";

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn base() -> HashMap<String, String> {
        env(&[("AERONA_TOKEN", TOKEN), ("AERONA_PORT", "8080")])
    }

    #[test]
    fn minimal_config_is_valid() {
        let c = Config::parse_env(&base(), Path::new("/srv/aerona"), true).unwrap();
        assert_eq!(c.port, 8080);
        assert_eq!(c.db_path, PathBuf::from("/srv/aerona/data/aerona.db"));
        assert_eq!(c.base_url, "http://localhost:8080");
        // A plain-http origin must not mark its cookie `Secure`: a browser would store
        // it and every script client would refuse to send it back.
        assert!(
            !c.cookies_secure(),
            "http://localhost must not claim to be secure"
        );
        assert!(!c.oauth_configured());
        assert_eq!(c.session_hours, 12);
        assert!(
            (c.rate_burst - 60.0).abs() < f64::EPSILON,
            "burst defaults to 3x rps"
        );
    }

    #[test]
    fn required_vars_are_enforced() {
        let e =
            Config::parse_env(&env(&[("AERONA_PORT", "8080")]), Path::new("/x"), true).unwrap_err();
        assert!(e.to_string().contains("AERONA_TOKEN"), "{e}");
        let e =
            Config::parse_env(&env(&[("AERONA_TOKEN", TOKEN)]), Path::new("/x"), true).unwrap_err();
        assert!(e.to_string().contains("AERONA_PORT"), "{e}");
        // Empty values count as missing, not as "present but blank".
        let e = Config::parse_env(
            &env(&[("AERONA_TOKEN", ""), ("AERONA_PORT", "8080")]),
            Path::new("/x"),
            true,
        )
        .unwrap_err();
        assert!(e.to_string().contains("missing"), "{e}");
    }

    #[test]
    fn garbage_ports_rejected() {
        for bad in ["0", "abc", "8080.5", "70000", "-1", "", " 8_0 "] {
            let mut e = base();
            e.insert("AERONA_PORT".into(), bad.into());
            assert!(
                Config::parse_env(&e, Path::new("/x"), true).is_err(),
                "accepted port {bad:?}"
            );
        }
        // <1024 parses as a port; it is only *refused* outside --dev.
        let mut e = base();
        e.insert("AERONA_PORT".into(), "80".into());
        assert!(Config::parse_env(&e, Path::new("/x"), true).is_ok());
        let err = Config::parse_env(&e, Path::new("/x"), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("privileged"), "{err}");
    }

    #[test]
    fn token_shape_guardrails() {
        let mut e = base();
        // An invite link pasted into the token field must fail at boot, not at gateway.
        e.insert(
            "AERONA_TOKEN".into(),
            "https://discord.com/api/oauth2/authorize?client_id=123".into(),
        );
        let err = Config::parse_env(&e, Path::new("/x"), true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("URL or spaces"), "{err}");

        e.insert("AERONA_TOKEN".into(), "short".into());
        assert!(Config::parse_env(&e, Path::new("/x"), true)
            .unwrap_err()
            .to_string()
            .contains("length"));

        e.insert(
            "AERONA_TOKEN".into(),
            "notbase64!!!.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        );
        let err = Config::parse_env(&e, Path::new("/x"), true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("base64url"), "{err}");
    }

    #[test]
    fn secrets_never_appear_in_errors_or_summaries() {
        let mut e = base();
        e.insert("SIGHTENGINE_TOKEN".into(), "user123:sup3rsecre1".into());
        e.insert("AERONA_TOKEN".into(), "bad token".into());
        let err = Config::parse_env(&e, Path::new("/x"), true)
            .unwrap_err()
            .to_string();
        assert!(
            !err.contains("sup3rsecre1") && !err.contains("bad token"),
            "{err}"
        );

        e.insert("AERONA_TOKEN".into(), TOKEN.into());
        e.insert(
            "AERONA_DISCORD_CLIENT_SECRET".into(),
            "client-secret-value".into(),
        );
        let c = Config::parse_env(&e, Path::new("/x"), true).unwrap();
        let summary = c.summary();
        assert!(!summary.contains("client-secret-value"), "{summary}");
        assert!(!summary.contains("MTIzNDU2Nzg5"), "{summary}");
        assert!(summary.contains("configured (user user123)"), "{summary}");
        assert!(
            !summary.contains("sup3rsecre1"),
            "secret half must not leak"
        );
        assert!(format!("{c:?}").contains("redacted"), "Debug must redact");
    }

    #[test]
    fn sightengine_formats_accepted() {
        for good in ["u:s", "user:secret", "\"u:s\"", "u:s ", "a,b"] {
            let mut e = base();
            e.insert("SIGHTENGINE_TOKEN".into(), good.into());
            let c = Config::parse_env(&e, Path::new("/x"), true).unwrap();
            assert!(c.sightengine.is_some(), "rejected {good:?}");
        }
        for bad in ["onlyuser", ":x", "u:", "   "] {
            let mut e = base();
            e.insert("SIGHTENGINE_TOKEN".into(), bad.into());
            assert!(
                Config::parse_env(&e, Path::new("/x"), true).is_err() || bad.trim().is_empty(),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn https_base_url_enables_secure_cookies() {
        let mut e = base();
        e.insert("AERONA_BASE_URL".into(), "https://bot.aerona.dev/".into());
        let c = Config::parse_env(&e, Path::new("/x"), false).unwrap();
        assert!(c.cookies_secure());
        assert_eq!(c.base_url, "https://bot.aerona.dev/");
        // The operator named an origin, so no request `Host` may steer a redirect —
        // that is the Host-header injection this function used to be open to.
        let origin = c.request_origin("evil.example:8080", "http").unwrap();
        assert_eq!(origin, "https://bot.aerona.dev");
        assert_eq!(
            c.redirect_uri(&origin),
            "https://bot.aerona.dev/auth/callback"
        );
        assert!(
            c.allows_origin("https://bot.aerona.dev"),
            "trailing slash must be normalized"
        );
        assert!(!c.allows_origin("https://evil.example"));

        e.insert("AERONA_BASE_URL".into(), "bot.aerona.dev".into());
        assert!(
            Config::parse_env(&e, Path::new("/x"), false).is_err(),
            "scheme is mandatory"
        );
    }

    #[test]
    fn cors_and_rate_knobs_validated() {
        let mut e = base();
        e.insert(
            "AERONA_CORS_ORIGINS".into(),
            "https://a.example, https://b.example".into(),
        );
        let c = Config::parse_env(&e, Path::new("/x"), true).unwrap();
        assert_eq!(
            c.cors_origins,
            vec!["https://a.example", "https://b.example"]
        );
        assert!(c.allows_origin("https://a.example"));

        e.insert("AERONA_RATE_RPS".into(), "0".into());
        assert!(
            Config::parse_env(&e, Path::new("/x"), true).is_err(),
            "zero rps would disable limiting"
        );
        e.insert("AERONA_RATE_RPS".into(), "10".into());
        e.insert("AERONA_RATE_BURST".into(), "5".into());
        assert!(
            Config::parse_env(&e, Path::new("/x"), true).is_err(),
            "burst below rps is nonsense"
        );
    }

    #[test]
    fn session_hours_bounds() {
        for bad in ["0", "-5", "99999", "1.5", "yes"] {
            let mut e = base();
            e.insert("AERONA_SESSION_HOURS".into(), bad.into());
            assert!(
                Config::parse_env(&e, Path::new("/x"), true).is_err(),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn dotenv_parsing() {
        let text = "\
# comment
AERONA_TOKEN = MTIzNDU2Nzg5MDEyMzQ1Njc4OQ.Ga1b2c.dH1SvAkeThAtIsHaSisVErS2NyM3NwXYZabc
export AERONA_PORT=8080
QUOTED=\"double quoted\"
SINGLE='raw \\n value'
INLINE=value # trailing comment
QUOTED_THEN_COMMENT='data/x.db' # moved
DOUBLE_THEN_COMMENT=\"a b\" # note
UNTERMINATED=\"oops
EMPTY=
BAD KEY=x
=novalue
#AERONA_COMMENTED=nope
";
        let parsed = parse_dotenv(text);
        let get = |k: &str| parsed.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        assert_eq!(
            get("AERONA_TOKEN").as_deref(),
            Some("MTIzNDU2Nzg5MDEyMzQ1Njc4OQ.Ga1b2c.dH1SvAkeThAtIsHaSisVErS2NyM3NwXYZabc")
        );
        assert_eq!(get("AERONA_PORT").as_deref(), Some("8080"));
        assert_eq!(get("QUOTED").as_deref(), Some("double quoted"));
        // A comment after a closing quote is stripped, not absorbed into the value.
        assert_eq!(get("QUOTED_THEN_COMMENT").as_deref(), Some("data/x.db"));
        assert_eq!(get("DOUBLE_THEN_COMMENT").as_deref(), Some("a b"));
        // An unterminated quote is left alone rather than half-guessed.
        assert_eq!(get("UNTERMINATED").as_deref(), Some("\"oops"));
        assert_eq!(
            get("SINGLE").as_deref(),
            Some("raw \\n value"),
            "single quotes are literal"
        );
        assert_eq!(get("INLINE").as_deref(), Some("value"));
        assert_eq!(get("EMPTY").as_deref(), Some(""));
        assert!(get("BAD KEY").is_none() && get("AERONA_COMMENTED").is_none());
        // The file must produce a working config when combined with real values.
        let mut env = HashMap::new();
        for (k, v) in parsed {
            env.insert(k, v);
        }
        assert!(Config::parse_env(&env, Path::new("/x"), true).is_ok());
    }

    #[test]
    fn trusted_proxy_is_an_explicit_opt_in() {
        let mut e = base();
        assert!(
            !Config::parse_env(&e, Path::new("/x"), true)
                .unwrap()
                .trusted_proxy,
            "off by default"
        );
        e.insert("AERONA_TRUSTED_PROXY".into(), "true".into());
        assert!(
            Config::parse_env(&e, Path::new("/x"), true)
                .unwrap()
                .trusted_proxy
        );
        // Anything that is not exactly "true" stays off: "1"/"yes"/"TRUE" must not
        // silently enable header spoofing because an operator wrote the wrong thing.
        for v in ["1", "yes", "TRUE", "on", ""] {
            e.insert("AERONA_TRUSTED_PROXY".into(), v.into());
            assert!(
                !Config::parse_env(&e, Path::new("/x"), true)
                    .unwrap()
                    .trusted_proxy,
                "{v:?} must not enable it"
            );
        }
    }

    #[test]
    fn pepper_is_stable_derived_and_rotatable() {
        let mut e = base();
        let cfg = Config::parse_env(&e, Path::new("/x"), true).unwrap();
        let a = cfg.pepper();
        assert_eq!(
            a,
            cfg.pepper(),
            "same input must give the same pepper on every call"
        );
        assert_ne!(a, [0u8; 32], "never the empty key");
        // Without a master key the bot token is the material, so two installs with
        // different tokens cannot read each other's sessions.
        let mut e2 = base();
        e2.insert("AERONA_TOKEN".into(), format!("{TOKEN}x"));
        let other = Config::parse_env(&e2, Path::new("/x"), true).unwrap();
        assert_ne!(a, other.pepper());
        // An explicit master key wins, and changing it logs everyone out.
        e.insert("AERONA_MASTER_KEY".into(), "operator-chosen-secret".into());
        let with_key = Config::parse_env(&e, Path::new("/x"), true).unwrap();
        assert_ne!(with_key.pepper(), a, "master key must actually be used");
        assert_eq!(
            with_key.pepper(),
            Config::parse_env(&e, Path::new("/y"), true)
                .unwrap()
                .pepper()
        );
        // Changing only the master key leaves the token untouched, so the
        // difference proves the master key is the one being used.
        e.insert("AERONA_MASTER_KEY".into(), "a-different-secret".into());
        assert_ne!(
            with_key.pepper(),
            Config::parse_env(&e, Path::new("/x"), true)
                .unwrap()
                .pepper()
        );
        assert!(with_key.master_key.is_some() && a != with_key.pepper());
    }

    #[test]
    fn bot_off_makes_only_the_token_optional() {
        let mut e = base();
        e.remove("AERONA_TOKEN");
        let err = Config::parse_env(&e, Path::new("/x"), true).unwrap_err();
        assert!(
            err.to_string().contains("AERONA_BOT=off"),
            "the error must name the escape hatch: {err}"
        );

        // Every spelling of "no worker" waives the token, and only that.
        for off in ["off", "OFF", "false", "0", "no", "none", "disabled"] {
            let mut e2 = e.clone();
            e2.insert("AERONA_BOT".into(), off.into());
            let cfg = Config::parse_env(&e2, Path::new("/x"), true)
                .unwrap_or_else(|err| panic!("AERONA_BOT={off} must waive the token: {err}"));
            assert!(!cfg.bot_enabled);
            assert!(cfg.token.expose().is_empty());
            assert!(cfg.summary().contains("AERONA_BOT=off"));
        }

        // Anything else keeps the requirement, including a blank value (blank means
        // "unset" everywhere in this file) and a typo like "of".
        for on in ["on", "true", "1", "yes", "", "of", "nope"] {
            let mut e2 = e.clone();
            e2.insert("AERONA_BOT".into(), on.into());
            assert!(
                Config::parse_env(&e2, Path::new("/x"), true).is_err(),
                "AERONA_BOT={on:?} must still demand a token"
            );
            e2.insert("AERONA_TOKEN".into(), TOKEN.into());
            assert!(
                Config::parse_env(&e2, Path::new("/x"), true)
                    .unwrap()
                    .bot_enabled
            );
        }

        // Turning the worker off is not a way to skip validation of a token you did
        // set, and it never relaxes the port.
        let mut bad = base();
        bad.insert("AERONA_BOT".into(), "off".into());
        bad.insert(
            "AERONA_TOKEN".into(),
            "https://discord.com/oauth2/invite".into(),
        );
        assert!(Config::parse_env(&bad, Path::new("/x"), true).is_err());
        assert!(Config::parse_env(
            &env(&[("AERONA_PORT", "0"), ("AERONA_BOT", "off")]),
            Path::new("/x"),
            true
        )
        .is_err());
    }

    #[test]
    fn ensure_pepper_secret_creates_once_and_reuses() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut e = base();
        e.remove("AERONA_TOKEN");
        e.insert("AERONA_BOT".into(), "off".into());
        e.insert(
            "AERONA_DATA_DIR".into(),
            root.join("data").display().to_string(),
        );

        let mut cfg = Config::parse_env(&e, root, true).unwrap();
        // parse_env stays a pure function: it never touches the filesystem, so the
        // caller decides when a file may be created.
        assert!(cfg.master_key.is_none());
        let created = cfg.ensure_pepper_secret().unwrap();
        assert!(created.is_some(), "first boot must create the file");
        let path = created.unwrap();
        assert!(path.is_file());
        assert_ne!(cfg.pepper(), crate::security::derive_pepper(""));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600,
                "the pepper must not be world-readable"
            );
        }

        let mut again = Config::parse_env(&e, root, true).unwrap();
        assert!(
            again.ensure_pepper_secret().unwrap().is_none(),
            "second boot must not rewrite it"
        );
        assert_eq!(
            again.pepper(),
            cfg.pepper(),
            "a restart must not rotate the pepper"
        );

        // An explicit master key wins and leaves the file alone.
        let mut e2 = e.clone();
        e2.insert("AERONA_MASTER_KEY".into(), "operator-secret".into());
        let mut with_key = Config::parse_env(&e2, root, true).unwrap();
        assert!(with_key.ensure_pepper_secret().unwrap().is_none());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim().len(),
            64,
            "the stored secret must be untouched"
        );
        assert_ne!(with_key.pepper(), cfg.pepper());

        // A token makes the whole path a no-op, which is the normal install.
        let mut e3 = base();
        e3.insert(
            "AERONA_DATA_DIR".into(),
            root.join("data2").display().to_string(),
        );
        let mut normal = Config::parse_env(&e3, root, true).unwrap();
        assert!(normal.ensure_pepper_secret().unwrap().is_none());
        assert!(!root.join("data2").join("install.key").exists());

        // A truncated file is an error, not a silent re-roll: re-rolling would log
        // everybody out on every restart and look like a bug in the login code.
        std::fs::write(&path, "abc\n").unwrap();
        let mut broken = Config::parse_env(&e, root, true).unwrap();
        let err = broken.ensure_pepper_secret().unwrap_err().to_string();
        assert!(err.contains("too short to be a secret"), "{err}");
    }

    /// The panel fix: an install that was never given a name must build its absolute
    /// URLs from the address the browser actually used, or a login bounces to
    /// `localhost` — the origin *this* process is listening on, but not the one in the
    /// address bar. And it must refuse to reflect a name it was not given: that is the
    /// rebinding hole, and here it would also hand the OAuth code to whoever owns the name.
    /// The property the status endpoint depends on: the shape notes describe the value
    /// without ever quoting it, so they can be sent to an anonymous caller.
    #[test]
    fn the_shape_notes_never_repeat_the_value() {
        let mut env = base();
        env.insert(
            "AERONA_OAUTH_REDIRECT_URI".into(),
            "https://panel.example/auth/callback/".into(),
        );
        let c = Config::parse_env(&env, Path::new("/srv/aerona"), false).unwrap();
        let pinned = c.oauth_redirect_uri.as_ref().unwrap();
        let notes = c.redirect_uri_shape_notes(pinned);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("slash"), "{notes:?}");
        for note in &notes {
            assert!(!note.contains(pinned), "a note quoted the uri: {note}");
        }
        // A local loopback URI is the panel's ordinary case and must not be called unsafe.
        let notes = c.redirect_uri_shape_notes("http://127.0.0.1:25154/auth/callback");
        assert!(notes.is_empty(), "{notes:?}");
        // Query strings are the other way this fails: registered verbatim or not at all.
        let notes = c.redirect_uri_shape_notes("https://panel.example/callback?guild=7");
        assert!(
            notes.iter().any(|n| n.contains("query string")),
            "{notes:?}"
        );
    }

    #[test]
    fn an_unconfigured_install_derives_its_origin_from_the_request_host() {
        let c = Config::parse_env(&base(), Path::new("/srv/aerona"), false).unwrap();
        assert!(!c.base_url_explicit, "the unset case is the point");
        assert_eq!(
            c.request_origin("10.0.0.5:25565", "http").as_deref(),
            Some("http://10.0.0.5:25565")
        );
        assert_eq!(
            c.request_origin("[::1]:8080", "http").as_deref(),
            Some("http://[::1]:8080")
        );
        assert_eq!(
            c.request_origin("aerona.localhost:8080", "https")
                .as_deref(),
            Some("https://aerona.localhost:8080"),
            "a forwarded scheme is honored verbatim, but only as https or http"
        );
        for host in [
            "th-bkk03.altare.global:25154",
            "",
            "e@127.0.0.1",
            "127.0.0.1:9/x",
        ] {
            assert_eq!(
                c.request_origin(host, "http"),
                None,
                "{host} is not ours to reflect"
            );
        }
        // A scheme that is not http(s) collapses to http rather than being pasted in.
        assert_eq!(
            c.request_origin("127.0.0.1:8080", "gopher://x").as_deref(),
            Some("http://127.0.0.1:8080")
        );
    }

    #[test]
    fn same_host_fallback_is_for_ip_and_localhost_only() {
        // The panel case: no AERONA_BASE_URL, reached as an IP with a port.
        let c = Config::parse_env(&base(), Path::new("/x"), false).unwrap();
        assert!(!c.base_url_explicit);
        assert!(c.allows_same_host("http://10.0.0.5:25565", "10.0.0.5:25565"));
        assert!(c.allows_same_host("http://127.0.0.1:8080", "127.0.0.1:8080"));
        assert!(c.allows_same_host("http://localhost:8080", "localhost:8080"));
        assert!(c.allows_same_host("http://localhost", "localhost"));
        assert!(c.allows_same_host("https://[::1]:8080", "[::1]:8080"));
        // A different port on the same box is a different origin.
        assert!(!c.allows_same_host("http://127.0.0.1:9999", "127.0.0.1:8080"));
        // The rebinding case: a name that happens to resolve here is not a host we
        // will vouch for.
        assert!(!c.allows_same_host("http://evil.example", "evil.example"));
        assert!(!c.allows_same_host("http://attacker.test:8080", "attacker.test:8080"));
        // A host header carrying credentials or a bogus port is not a same-origin.
        assert!(!c.allows_same_host("http://127.0.0.1:8080", "user@127.0.0.1:8080"));
        assert!(!c.allows_same_host("http://127.0.0.1", ""));

        // `.localhost` is reserved for loopback by RFC 6761, so it is as safe as the
        // literal and it is what a browser on the box uses.
        assert!(c.allows_same_host("http://aerona.localhost:8080", "aerona.localhost:8080"));

        // Naming the origin turns the fallback off completely — that is the switch.
        let mut e = base();
        e.insert("AERONA_BASE_URL".into(), "http://127.0.0.1:8080".into());
        let named = Config::parse_env(&e, Path::new("/x"), false).unwrap();
        assert!(named.base_url_explicit);
        assert!(!named.allows_same_host("http://10.0.0.5:25565", "10.0.0.5:25565"));
        assert!(named.allows_origin("http://127.0.0.1:8080"));
        assert!(!named.allows_origin("http://10.0.0.5:25565"));
    }

    #[test]
    fn template_is_self_consistent() {
        let parsed = parse_dotenv(DOTENV_TEMPLATE);
        let keys: Vec<&str> = parsed.iter().map(|(k, _)| k.as_str()).collect();
        for k in [
            "AERONA_TOKEN",
            "AERONA_PORT",
            "SIGHTENGINE_TOKEN",
            "AERONA_BASE_URL",
            "AERONA_OAUTH_REDIRECT_URI",
            "AERONA_TRUSTED_PROXY",
        ] {
            assert!(keys.contains(&k), "{k} missing from template");
        }
        // Template placeholders must be *rejected* as a token, so nobody ships it as-is.
        let mut env = HashMap::new();
        for (k, v) in parsed {
            env.insert(k, v);
        }
        assert!(Config::parse_env(&env, Path::new("/x"), true).is_err());
        assert!(Config::REQUIRED.iter().all(|k| !k.is_empty()));
    }
}
