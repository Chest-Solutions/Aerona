//! `aerona-bot` — the gateway worker, as its own process.
//!
//! This is the split-process entry point: use it when something else (systemd, docker,
//! your own supervisor) is going to keep two children alive and you want a Discord outage
//! to be *someone else's* restart problem. The thing you normally run is `aerona`, which
//! holds this worker and the dashboard in one process and retries the worker internally.
//!
//! Either way the worker's rules are the same and live in [`aerona_bot::worker`]: exit
//! non-zero on a failure the supervisor should hear about, never loop internally.
//!
//! Flags:
//!   --check    prove the config, the database and the intents, then exit
//!   --version  print the version and exit

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use aerona_core::config::Config;
use aerona_core::db::Db;
use aerona_core::error::Error;

use aerona_bot::{state::BotState, worker};

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
            eprintln!("aerona-bot: {e}");
            ExitCode::from(2)
        }
    }
}

async fn run(root: &Path, dev: bool, args: &[String]) -> Result<(), Error> {
    if args.iter().any(|a| a == "--version") {
        println!(
            "aerona-bot {} (worker protocol v{})",
            aerona_core::VERSION,
            aerona_bot::WORKER_PROTOCOL
        );
        return Ok(());
    }

    let mut cfg = Config::load(root, dev)?;
    init_tracing(&cfg.log_level);

    // This binary has exactly one job, and `AERONA_BOT=off` says not to do it. Running
    // anyway would mean connecting with no token, so say what to run instead.
    if !cfg.bot_enabled {
        return Err(Error::Config(
            "this is the gateway worker but AERONA_BOT=off — run `aerona` (dashboard and \
             worker in one process) or `aerona-api` for the dashboard alone"
                .into(),
        ));
    }

    // A no-op here in the normal case: this process cannot start without a token, and a
    // token is what the pepper is normally derived from.
    note_pepper_secret(cfg.ensure_pepper_secret()?);
    let db = Db::open(&cfg)?;

    // `--check`: the questions `main.py check` asks about this binary. No gateway
    // connection, because a token that cannot reach Discord is a network fact, not a
    // configuration one, and the operator needs to be able to check config offline.
    if args.iter().any(|a| a == "--check") {
        db.verify()?;
        println!("{}", cfg.summary());
        println!(
            "database: ok, {} KB, {} guild(s)",
            db.size_kb()?,
            db.row_count("guilds")?
        );
        println!("{}", worker::report(&cfg));
        return Ok(());
    }

    db.verify()?;
    let state = Arc::new(BotState::new(Arc::new(cfg), db));
    worker::connect(state, shutdown_signal()).await
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

fn note_pepper_secret(created: Option<PathBuf>) {
    let Some(path) = created else { return };
    tracing::warn!(
        "wrote a persistent pepper to {} (keep it with your database backup)",
        path.display()
    );
}

/// One subscriber per process. `RUST_LOG` still wins, because an operator debugging at
/// 2 a.m. should not have to edit a config file.
///
/// stderr here (and stdout in `aerona-api`) because a supervisor that keeps two children
/// alive merges their output, and a worker's warnings must not be lost in the API's
/// access-shaped lines. The unified `aerona` binary has one stream and uses stdout.
fn init_tracing(level: &str) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(format!("warn,aerona_bot={level},aerona_core={level}"))
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_writer(std::io::stderr),
        )
        .try_init()
        .ok();
}
