//! What `aerona` does: load once, serve forever, stop together.
//!
//! The interesting part is the failure policy, so that is what the comments are for.
//! Short version: the dashboard is the thing a restart is painful for, and the gateway
//! is the thing that breaks on its own — so the API decides when the *process* exits,
//! and the worker is retried inside this process for as long as the process lives.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use aerona_core::config::Config;
use aerona_core::db::Db;
use aerona_core::error::{Error, Result};

use crate::cli::{Command, Options};
use crate::stop::Stop;

/// How long the worker gets to say goodbye after the dashboard has finished draining.
/// It is on top of the API's own drain, and a claim left unfinished is requeued by the
/// outbox claim timeout, so a hard abort here costs a retried action, not a lost one.
const BOT_GRACE: Duration = Duration::from_secs(10);

/// The exit code `doctor` uses when Discord said no to the token. Distinct from the
/// generic `2` so a panel's startup script can tell "misconfigured" from "wrong token".
pub const DOCTOR_TOKEN_REJECTED: i32 = 6;

/// The process entry point: returns the exit code, and never panics on its own account.
pub async fn dispatch(opts: &Options) -> Result<i32> {
    // `--help` and `--version` answer from constants, *before* the config is read. That
    // ordering is a contract, not a nicety: `main.py` decides whether a candidate binary
    // is usable for this platform by running `--version` in an empty environment in a
    // directory that may have no `.env` and no token. A config error there reads as
    // "wrong binary", and the launcher would then download a toolchain it did not need.
    match opts.command {
        Command::Help => {
            print!("{}", Options::usage());
            return Ok(0);
        }
        Command::Version => {
            // `API_VERSION` is "v1" already; the prefix used to be doubled here, and a
            // launcher that greps its own probe output should not have to know that.
            println!(
                "aerona {version} (api {api}, worker protocol v{worker})",
                version = crate::VERSION,
                api = aerona_core::API_VERSION,
                worker = aerona_bot::WORKER_PROTOCOL,
            );
            return Ok(0);
        }
        _ => {}
    }

    // Flags next: they are written into the environment so that `Config` remains the
    // only place precedence between a flag, the environment and `.env` is decided.
    opts.apply_to_env();
    let mut cfg = Config::load(&opts.root, opts.dev)?;
    init_tracing(&cfg.log_level, opts.verbose);

    // Before the database: a dashboard-only install has no token to derive the
    // session pepper from, and this is what gives it one that survives a restart.
    note_pepper_secret(cfg.ensure_pepper_secret()?);
    let db = Db::open(&cfg)?;

    match opts.command {
        Command::Check => {
            print!("{}", aerona_api::server::report(&cfg, &db)?);
            print!("{}", worker_summary(&cfg));
            print!("{}", queue_summary(&db));
            Ok(0)
        }
        Command::Doctor => doctor(&cfg, &db).await,
        // `Help`/`Version` returned above; the wildcard exists so that adding a
        // command to the enum makes this match fail to compile rather than quietly
        // meaning "run".
        Command::Run => serve(cfg, db).await,
        Command::Help | Command::Version => Ok(0),
    }
}

/// Bind, connect, and wait for the dashboard to finish.
async fn serve(cfg: Config, db: Db) -> Result<i32> {
    let stop = Stop::new();
    stop.install_signals();
    let cfg = Arc::new(cfg);

    let state = aerona_api::AppState::new(Arc::clone(&cfg), db.clone());
    let api_stop = stop.clone();
    let api = tokio::spawn(async move {
        // The owned latch-and-sleep future (`Stop::wait_drain`): the signal arrives, the
        // socket stays open for the drain window, then axum closes it. It has to be
        // owned because `start` keeps the future across tasks.
        let drained = api_stop.wait_drain(aerona_api::server::DRAIN_SECS);
        aerona_api::server::start(state, drained).await
    });

    let bot = if cfg.bot_enabled {
        Some(tokio::spawn(supervise(
            Arc::clone(&cfg),
            db.clone(),
            stop.clone(),
        )))
    } else {
        tracing::info!(
            "the Discord worker is off (AERONA_BOT=off): the dashboard is serving, and \
             anything queued in action_outbox waits for a worker to show up"
        );
        None
    };

    // The API's completion is the process's completion, in both directions: it cannot
    // come back with `Ok` unless somebody stopped it, and if it comes back with `Err`
    // there is nothing for a supervisor to look at.
    let api_result = api
        .await
        .map_err(|e| Error::Config(format!("the dashboard task died: {e}")))?;

    stop.fire();
    if let Some(mut handle) = bot {
        if tokio::time::timeout(BOT_GRACE, &mut handle).await.is_err() {
            tracing::warn!(
                "the worker did not finish within {}s of the drain; abandoning it — any \
                 action it had claimed is requeued by the claim timeout",
                BOT_GRACE.as_secs()
            );
            handle.abort();
        }
    }

    api_result?;
    tracing::info!("aerona stopped");
    Ok(0)
}

/// The worker's restart loop.
///
/// Every attempt builds a fresh [`aerona_bot::BotState`], because that state holds the
/// `Context` serenity hands out — reusing it across a reconnect would leave the outbox
/// executor talking to a dead shard set. The transient state that *is* worth keeping
/// (cooldowns, raid windows) lives only as long as a healthy connection anyway: after
/// an outage, letting somebody run `!raid` again immediately is the correct behaviour.
async fn supervise(cfg: Arc<Config>, db: Db, stop: Stop) {
    let mut attempts = 0_u32;
    loop {
        if stop.stopped() {
            return;
        }
        let state = Arc::new(aerona_bot::BotState::new(Arc::clone(&cfg), db.clone()));
        match aerona_bot::worker::connect(state, stop.wait_signal()).await {
            Ok(()) => {
                tracing::info!("the gateway worker is stopped");
                return;
            }
            Err(e) => {
                // A failure that races a stop is a shutdown, not a fault: say nothing
                // alarming about it.
                if stop.stopped() {
                    tracing::debug!(%e, "worker stopped while shutting down");
                    return;
                }
                attempts += 1;
                let rejected = is_auth_rejection(&e);
                let wait = backoff(attempts, rejected);
                if rejected {
                    tracing::error!(
                        tries = attempts,
                        %e,
                        "Discord refused this bot token. Check AERONA_TOKEN (and that the \
                         application is not archived) — the dashboard keeps serving while \
                         I keep retrying"
                    );
                } else {
                    tracing::warn!(
                        tries = attempts,
                        %e,
                        "the gateway worker failed; reconnecting on its own"
                    );
                }
                let sleep = tokio::time::sleep(Duration::from_secs(wait));
                tokio::select! {
                    _ = sleep => {}
                    _ = stop.wait() => return,
                }
            }
        }
    }
}

/// Only the shape of the message identifies a rejection: serenity's error type is a
/// string wrapper around whatever the HTTP layer saw, and a 401 must not be treated
/// like a blip.
fn is_auth_rejection(e: &Error) -> bool {
    if let Error::Discord { status, .. } = e {
        return *status == 401 || *status == 403;
    }
    let text = e.to_string();
    text.contains("401") || text.contains("Unauthorized") || text.contains("invalid token")
}

/// Reconnect delays, in seconds.
///
/// Discord's own limits make a fast retry loop actively harmful (a bot that hammers the
/// gateway gets globally rate-limited and stops working everywhere), so the floor is a
/// few seconds even for the first attempt and the ceiling is a minute. A rejected token
/// starts at a minute because no amount of retrying fixes it and the operator's log is
/// the thing that must stay readable.
fn backoff(attempts: u32, rejected: bool) -> u64 {
    const STEPS: [u64; 5] = [3, 6, 12, 30, 60];
    let index = attempts.saturating_sub(1) as usize;
    let step = STEPS[index.min(STEPS.len() - 1)];
    if rejected {
        step.max(60)
    } else {
        step
    }
}

fn worker_summary(cfg: &Config) -> String {
    if !cfg.bot_enabled {
        return "worker          off (AERONA_BOT=off — dashboard only)\n".to_string();
    }
    let mut s = String::from("worker          on\n");
    for line in aerona_bot::worker::report(cfg).lines() {
        s.push_str("  ");
        s.push_str(line);
        s.push('\n');
    }
    s
}

fn queue_summary(db: &Db) -> String {
    // `outbox_depth` counts `pending` *and* `claimed`: a claimed row is one a worker is
    // holding, and on a box with a dead worker it is exactly what the operator needs to
    // see. Which is the point of printing it here at all.
    match db.outbox_depth() {
        Ok((queued, failed)) => {
            format!("outbox          {queued} queued, {failed} failed\n")
        }
        Err(e) => format!("outbox          could not be read: {e}\n"),
    }
}

/// The checks that need the network or the filesystem, printed as one line each.
async fn doctor(cfg: &Config, db: &Db) -> Result<i32> {
    print!("{}", aerona_api::server::report(cfg, db)?);
    print!("{}", worker_summary(cfg));

    // 1. Can we bind? `report` proved config and storage; this proves the port.
    match aerona_api::server::bind(cfg.port).await {
        Ok(_listener) => println!("port            {port} is free", port = cfg.port),
        Err(e) => {
            println!("port            CANNOT BE BOUND: {e}");
            return Ok(3);
        }
    }

    // 2. Is the dashboard built, and does it look like a prerendered build rather than
    //    a stale `npm run build`? A directory with one index.html and no assets/ is the
    //    half-copy that produces a blank page.
    let dist = &cfg.static_dir;
    if dist.join("index.html").is_file() {
        let routes = count_documents(dist);
        let assets = dist.join("assets").is_dir();
        println!(
            "dashboard       {index} (shell {shell} KB, {routes} prerendered document(s){assets})",
            index = dist.display(),
            shell = dist
                .join("index.html")
                .metadata()
                .map(|m| m.len() / 1024)
                .unwrap_or(0),
            routes = routes.saturating_sub(1),
            assets = if assets {
                ", assets present"
            } else {
                ", NO assets/ directory"
            },
        );
        if !assets {
            println!("dashboard       WARNING: assets/ is missing — the page will render unstyled");
        }
    } else {
        println!(
            "dashboard       not built (looked for {}): the API will serve JSON only",
            dist.join("index.html").display()
        );
    }

    // 3. Who will the browser be told this site is?
    if !cfg.base_url_explicit {
        println!(
            "origin          AERONA_BASE_URL is unset: mutations from an IP or localhost \
             origin are accepted, anything else (a domain) needs it set"
        );
    }

    // 3b. The exact `redirect_uri` a Discord login sends. Discord's own failure screen says
    //     only "Invalid OAuth2 redirect_uri", and it happens in *their* browser before any
    //     request reaches this install, so there is no log line to read afterwards: the only
    //     usable diagnosis is this string next to the one in the Developer Portal.
    if cfg.oauth_configured() {
        match cfg.redirect_uri_configured() {
            Some(uri) => {
                // `redirect_uri_guidance` opens with where the value came from and the value
                // itself, so no second line here: a report that says a thing twice is a report
                // people stop reading.
                println!("oauth redirect  {uri}");
                for line in cfg.redirect_uri_guidance(Some(cfg.base_url.as_str())) {
                    println!("                  · {line}");
                }
                println!(
                    "                  paste the value above into Discord → OAuth2 → Redirects, then press Save Changes"
                );
            }
            None => {
                println!("oauth redirect  per request, from the browser's own origin");
                // No second line naming the source: when nothing is configured, "per request"
                // *is* the source, and `redirect_source_configured` would only repeat it.
                println!(
                    "                  the portal needs one entry per address people actually \
                     type, e.g. http://127.0.0.1:25154/auth/callback"
                );
            }
        }
    }

    // 4. Does Discord accept this token? The only question `check` cannot answer, and
    //    the one that explains most "the bot is not doing anything" reports.
    if cfg.bot_enabled {
        match whoami(cfg).await {
            Ok(who) => println!("discord         token accepted, authenticated as {who}"),
            Err(e) if e.to_string().contains("offline") => {
                println!("discord         could not be reached (no egress, DNS or TLS trust): {e}");
            }
            Err(e) => {
                println!("discord         REJECTED THIS TOKEN: {e}");
                return Ok(DOCTOR_TOKEN_REJECTED);
            }
        }
    } else {
        println!("discord         not asked (worker is off)");
    }

    println!(
        "database        {} KB, install {}",
        db.size_kb()?,
        db.install_id()?
    );
    Ok(0)
}

/// `GET /users/@me` with the token. Deliberately not serenity: this has to work when
/// the gateway is the thing that is broken, and it must not print anything but the
/// username it was given.
async fn whoami(cfg: &Config) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(cfg.http_timeout_secs.max(5)))
        .connect_timeout(Duration::from_secs(cfg.http_connect_timeout_secs.max(3)))
        .user_agent(concat!("aerona-doctor/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| Error::Config(format!("could not build an http client: {e}")))?;
    let response = client
        .get("https://discord.com/api/v10/users/@me")
        // Discord wants its own scheme, not `Bearer`: with the wrong prefix every
        // token on earth reads as invalid.
        .header("authorization", format!("Bot {}", cfg.token.expose()))
        .send()
        .await
        .map_err(|e| {
            // "no network" must not look like "bad token" — the advice differs, so the
            // message carries that distinction and `doctor` reads it back.
            Error::Discord {
                status: 0,
                message: format!("could not be reached (offline?): {e}"),
            }
        })?;
    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(Error::Discord {
            status: status.as_u16(),
            message:
                "the token was rejected — it is wrong, revoked, or the application is archived"
                    .into(),
        });
    }
    if !status.is_success() {
        return Err(Error::Discord {
            status: status.as_u16(),
            message: "try again in a minute if you were rate limited".into(),
        });
    }
    let body: serde_json::Value = response.json().await.map_err(|e| Error::Discord {
        status: status.as_u16(),
        message: e.to_string(),
    })?;
    Ok(body
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string())
}

/// `index.html` files in the built site, to a shallow depth: one per prerendered route,
/// plus the shell. No dependency on a directory walk for three levels of a known tree.
fn count_documents(root: &Path) -> usize {
    fn scan(dir: &Path, depth: usize, out: &mut usize) {
        if depth > 2 {
            return;
        }
        if dir.join("index.html").is_file() {
            *out += 1;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                scan(&path, depth + 1, out);
            }
        }
    }
    let mut n = 0;
    scan(root, 0, &mut n);
    n
}

fn note_pepper_secret(created: Option<PathBuf>) {
    let Some(path) = created else { return };
    tracing::warn!(
        "no bot token and no AERONA_MASTER_KEY: wrote a persistent pepper to {} — keep it \
         with your database backup, sessions and API keys are keyed with it",
        path.display()
    );
}

/// One subscriber for the whole process, with every crate of ours at the configured
/// level and the world at `warn`. `RUST_LOG` still wins, because an operator debugging
/// at 2 a.m. should not have to edit a config file.
///
/// stdout, because the supervisor captures it — and in one process there is no second
/// stream to keep apart, so the bot's old habit of writing to stderr would just split a
/// story in two files.
fn init_tracing(level: &str, verbose: bool) {
    use tracing_subscriber::EnvFilter;
    let level = if verbose { "debug" } else { level };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!(
            "warn,aerona={level},aerona_api={level},aerona_bot={level},aerona_core={level}"
        ))
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_writer(std::io::stdout)
        .try_init()
        .ok();
}

/// Map a failure to an exit code. Two numbers matter to a supervisor: `2` is "the
/// operator must do something", `0` is a clean stop.
pub fn exit_code(_error: &Error) -> ExitCode {
    // One code for every failure, on purpose: `main.py` and any systemd unit restart on
    // non-zero, and a code that distinguishes "config" from "database" would only be
    // honest if something branched on it. The message on stderr is the actionable part.
    ExitCode::from(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rejected token must not look like a network blip: the two need different
    /// advice, and this is the only line that decides which one is printed.
    #[test]
    fn auth_rejection_is_recognised_from_the_message() {
        let e = Error::Config("the gateway stopped: 401: Unauthorized".into());
        assert!(is_auth_rejection(&e));
        for unrelated in [
            "the gateway stopped: command ExecutionFailed",
            "could not build the gateway client: dns failure",
            "the gateway stopped: 502: Bad Gateway",
        ] {
            assert!(
                !is_auth_rejection(&Error::Config(unrelated.into())),
                "{unrelated} must not be reported as a bad token"
            );
        }
    }

    #[test]
    fn backoff_grows_cools_off_and_never_hammers() {
        assert_eq!(backoff(1, false), 3);
        assert!(
            backoff(2, false) > backoff(1, false),
            "the point of a backoff is that it grows"
        );
        // The ceiling holds for any number of tries, including an absurd one.
        assert_eq!(backoff(5, false), 60);
        assert_eq!(backoff(5000, false), 60);
        assert_eq!(backoff(u32::MAX, false), 60);
        // A rejected token waits at least a minute from the very first try.
        assert!(backoff(1, true) >= 60);
        assert!(backoff(2, true) >= 60);
        assert!(backoff(2, true) <= 300, "and does not sleep for an hour");
    }

    #[test]
    fn the_summaries_are_lines_and_never_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = std::collections::HashMap::new();
        env.insert("AERONA_PORT".into(), "8080".into());
        env.insert("AERONA_BOT".into(), "off".into());
        env.insert("AERONA_MASTER_KEY".into(), "test-material".into());
        env.insert(
            "SIGHTENGINE_TOKEN".into(),
            "sightengineuser:sightenginepass".into(),
        );
        let cfg = Config::parse_env(&env, dir.path(), true).unwrap();
        let worker = worker_summary(&cfg);
        assert!(worker.contains("worker          off"), "{worker}");
        assert!(!worker.contains("sightenginepass"), "{worker}");

        // The on-form summary is what an operator reads to learn which intents they
        // have to tick in the portal.
        let mut env2 = env.clone();
        env2.remove("AERONA_BOT");
        env2.insert(
            "AERONA_TOKEN".into(),
            "MTk4MTA5NzA5NTA4Njk0MDgw.GsxABc.bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        );
        let cfg2 = Config::parse_env(&env2, dir.path(), true).unwrap();
        let worker = worker_summary(&cfg2);
        assert!(worker.contains("worker          on"), "{worker}");
        assert!(worker.contains("GUILD_MEMBERS"), "{worker}");
        assert!(!worker.contains("bbbbbbbb"), "{worker}");
    }

    /// The prerender count is what tells a half-copied `dist` from a real build, so it
    /// has to count documents and not files.
    #[test]
    fn documents_are_counted_to_a_shallow_depth() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("index.html"), "shell").unwrap();
        std::fs::create_dir_all(root.join("app").join("guild")).unwrap();
        std::fs::write(root.join("app").join("index.html"), "doc").unwrap();
        std::fs::write(root.join("app").join("guild").join("index.html"), "doc").unwrap();
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(root.join("assets").join("index.js"), "not a document").unwrap();
        assert_eq!(count_documents(root), 3);
        assert_eq!(count_documents(Path::new("/definitely/not/here")), 0);
    }

    #[test]
    fn a_failure_always_means_a_non_zero_exit() {
        assert_eq!(exit_code(&Error::Config("x".into())), ExitCode::from(2));
        assert_eq!(exit_code(&Error::Db("x".into())), ExitCode::from(2));
        assert_ne!(
            DOCTOR_TOKEN_REJECTED, 2,
            "doctor's verdict must be distinguishable"
        );
    }

    /// The distinction the whole `doctor` command exists for: a box with no egress must
    /// not be told its token is wrong.
    #[test]
    fn unreachable_and_unauthorized_are_different_verdicts() {
        let offline = Error::Discord {
            status: 0,
            message: "could not be reached (offline?): dns".into(),
        };
        assert!(!is_auth_rejection(&offline));
        assert!(offline.to_string().contains("offline"));
        let rejected = Error::Discord {
            status: 401,
            message: "nope".into(),
        };
        assert!(is_auth_rejection(&rejected));
        let rate_limited = Error::Discord {
            status: 429,
            message: "slow down".into(),
        };
        assert!(!is_auth_rejection(&rate_limited), "429 is not a bad token");
    }
}
