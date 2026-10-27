//! `aerona-api` - the HTTP surface: `/api/v1/*` and the built dashboard, on one port.
//!
//! This is the API-alone entry point. The thing you normally run is `aerona`, which
//! serves this *and* holds the Discord connection in one process; `main.py` prefers
//! that binary and falls back to this one. What is left here is argument handling:
//! the lifecycle lives in [`aerona_api::server`] so both entry points share it.
//!
//! Startup is deliberately loud about the two things an operator must act on (a
//! pairing code, an unusable database) and quiet about everything else. A supervisor
//! owns restarts, so this process exits non-zero when it cannot serve rather than
//! limping along.
//!
//! Flags:
//!   --check    prove the config, the database and the dashboard, then exit
//!   --pair     mint a fresh pairing code, print it, exit
//!   --version  print the version and exit

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use aerona_core::config::Config;
use aerona_core::db::Db;
use aerona_core::error::Error;

use aerona_api::server::{self, DRAIN_SECS};
use aerona_api::AppState;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let dev = std::env::var("AERONA_ENV")
        .map(|v| v.eq_ignore_ascii_case("development"))
        .unwrap_or(false);
    match run(&root, dev, &args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Safe to print: `Config`/`Db` failures name a missing variable or a
            // path, never a secret value.
            eprintln!("aerona-api: {e}");
            ExitCode::from(2)
        }
    }
}

async fn run(root: &std::path::Path, dev: bool, args: &[String]) -> Result<(), Error> {
    if args.iter().any(|a| a == "--version") {
        println!(
            "aerona-api {} (api v{})",
            aerona_api::VERSION,
            aerona_core::API_VERSION
        );
        return Ok(());
    }

    let mut cfg = Config::load(root, dev)?;
    init_tracing(&cfg.log_level);
    // A dashboard-only install has no token to derive the pepper from; this is what
    // keeps that case from sharing one global pepper. It is a no-op when either a
    // token or AERONA_MASTER_KEY exists.
    note_pepper_secret(cfg.ensure_pepper_secret()?);
    let db = Db::open(&cfg)?;

    // `--check`: everything provable without binding a port, including the integrity
    // check that `start` would otherwise run first.
    if args.iter().any(|a| a == "--check") {
        println!("{}", server::report(&cfg, &db)?);
        return Ok(());
    }

    let state = AppState::new(Arc::new(cfg), db);
    server::start(state, drain()).await
}

/// Ctrl-C plus SIGTERM (systemd, docker, `main.py`). Then a short drain so a PUT in
/// flight is not cut off mid-write - axum keeps serving until this future resolves.
async fn drain() {
    match signal().await {
        Ok(name) => tracing::info!("{name}: draining for {DRAIN_SECS}s"),
        Err(e) => tracing::warn!(%e, "could not install a signal handler; SIGINT still works"),
    }
    tokio::time::sleep(std::time::Duration::from_secs(DRAIN_SECS)).await;
}

/// Resolve when the process is told to stop. Errors instead of hanging forever if the
/// signal handlers cannot be installed (a stripped kernel, a strange container).
async fn signal() -> Result<&'static str, std::io::Error> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal as watch, SignalKind};
        let mut term = watch(SignalKind::terminate())?;
        tokio::select! {
            res = tokio::signal::ctrl_c() => {
                res.map_err(std::io::Error::other)?;
                Ok("SIGINT")
            }
            _ = term.recv() => Ok("SIGTERM"),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok("SIGINT")
    }
}

fn note_pepper_secret(created: Option<PathBuf>) {
    let Some(path) = created else { return };
    tracing::warn!(
        "no bot token and no AERONA_MASTER_KEY: wrote a persistent pepper to {} - \
         keep it with your database backup, sessions and API keys are keyed with it",
        path.display()
    );
}

fn init_tracing(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        // stdout, because `main.py` captures it into `data/logs/api.log`; a second
        // file appender here would race the supervisor for the same path.
        .with_writer(std::io::stdout)
        .init();
}
