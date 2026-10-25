//! The HTTP server's lifecycle, as one function.
//!
//! `aerona-api`'s own `main` and the unified `aerona` binary both call [`start`], so an
//! operator cannot end up with a different startup sequence depending on which binary
//! they ran: the same integrity check, the same missed-retention catch-up, the same
//! pairing-code announcement, the same graceful drain. Anything that has to happen
//! exactly once per process belongs here, not in a `main`.

use std::fmt::Write as _;
use std::future::Future;

use aerona_core::config::Config;
use aerona_core::db::Db;
use aerona_core::error::{Error, Result};

use crate::app::{self, AppState};

/// Seconds in-flight requests get after a stop signal, before the listener closes.
/// Long enough for a `PUT` that is mid-body, short enough that a stuck client never
/// holds a shutdown hostage.
pub const DRAIN_SECS: u64 = 5;

/// Everything provable without binding a port. Both `--check` paths print this, so
/// `aerona check` and `aerona-api --check` cannot drift apart.
pub fn report(cfg: &Config, db: &Db) -> Result<String> {
    let mut s = cfg.summary();
    let _ = writeln!(
        s,
        "database: ok, {} KB, {} guild(s)",
        db.size_kb()?,
        db.row_count("guilds")?
    );
    let _ = writeln!(s, "install id: {}", db.install_id()?);
    let _ = writeln!(
        s,
        "dashboard: {}",
        if cfg.static_dir.join("index.html").is_file() {
            "built"
        } else {
            "not built (api-only mode)"
        }
    );
    Ok(s)
}

/// The public socket. Binding is separated from serving because `aerona doctor` wants to
/// be able to say "the port is taken" without also deciding to quit.
pub async fn bind(port: u16) -> Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port))
        .await
        .map_err(|e| Error::Config(format!("could not bind 0.0.0.0:{port}: {e}")))
}

/// Where the built dashboard lives, if it was built at all.
pub fn assets(cfg: &Config) -> Option<app::StaticAssets> {
    cfg.static_dir
        .join("index.html")
        .is_file()
        .then(|| app::StaticAssets {
            dist_dir: cfg.static_dir.clone(),
        })
}

/// Serve `/api/v1` and the dashboard until `shutdown` resolves.
///
/// `shutdown` is the caller's business: `aerona-api` waits for its own signal, the
/// unified binary waits for a shared stop handle. By the time it resolves, draining has
/// already been given its `DRAIN_SECS`, which is what the caller sleeps for — axum keeps
/// answering requests until then, so a browser tab that was mid-request finishes.
pub async fn start(
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    // Integrity before anything is exposed: a corrupted WAL should stop the process
    // here, not be discovered by the first request that touches a bad page.
    state.db.verify()?;

    // There is no bootstrap credential to print any more: the only way into the dashboard is
    // a Discord login, and `require_guild`/`finish_login` decide who gets one. An install that
    // cannot be signed in to is therefore a *configuration* problem (no client id/secret, or a
    // name the operator never set), and `report`/`main.py check` say which.

    // Retention that was skipped while the process was down runs before the first
    // request, so a box that was off for a week does not spend its first minute in
    // `integrity_error` on the audit table.
    match state.db.housekeeping_due(3600) {
        Ok(true) => match state.db.housekeeping(state.cfg.retention_days, 20_000) {
            Ok(removed) => {
                let _ = state.db.mark_housekeeping();
                tracing::info!(?removed, "startup retention pass");
            }
            Err(e) => {
                tracing::warn!(%e, "startup retention pass failed; the hourly task will retry")
            }
        },
        Ok(false) => {}
        Err(e) => tracing::warn!(%e, "could not check whether retention is due"),
    }

    let listener = bind(state.cfg.port).await?;
    let addr = listener.local_addr()?;
    let assets = assets(&state.cfg);
    let dashboard_built = assets.is_some();
    let router = app::router(state.clone(), assets);

    // The stats ticker is part of *this* process's job, not a separate binary's: it is
    // what `/status` reads, and `main.py healthcheck` is useless without it.
    let stats = tokio::spawn(app::spawn_stats_task(state.clone()));

    let guilds = state.db.guilds().unwrap_or_default();
    tracing::info!(
        guilds = guilds.len(),
        absent = guilds.iter().filter(|g| !g.bot_present).count(),
        config_epochs = ?state.db.config_epochs().unwrap_or_default(),
        "starting"
    );
    // Connect info is not optional here: the rate limiter and every audit row key on
    // the peer address.
    tracing::info!(
        %addr,
        version = crate::VERSION,
        dashboard = dashboard_built,
        "aerona-api listening"
    );

    let served = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await;
    // Requests are drained; stop the ticker and leave the database tidy for the next
    // process (`main.py` runs `vacuum` after the child is down).
    stats.abort();
    let _ = state.db.optimize();
    served.map_err(|e| Error::Io(e.to_string()))?;
    tracing::info!("aerona-api stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `report` is what an operator reads when nothing works, so it must never be the
    /// thing that fails — and it must never contain a secret.
    #[test]
    fn report_never_leaks_a_secret() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = std::collections::HashMap::new();
        env.insert(
            "AERONA_TOKEN".into(),
            "MTk4MTA5NzA5NTA4Njk0MDgw.GsxABc.abcdefghijklmnop".into(),
        );
        env.insert("AERONA_PORT".into(), "8080".into());
        env.insert("AERONA_MASTER_KEY".into(), "sup3rsecre1-mATERIAL".into());
        env.insert(
            "AERONA_DATA_DIR".into(),
            dir.path().join("data").display().to_string(),
        );
        env.insert("SIGHTENGINE_TOKEN".into(), "user:topsecretvalue".into());
        let cfg = Config::parse_env(&env, dir.path(), true).unwrap();
        // `open_path` is the low-level constructor: it does not create a directory,
        // which is what `Db::open` (the one the binaries use) does for you.
        std::fs::create_dir_all(cfg.db_path.parent().unwrap()).unwrap();
        let db = Db::open_path(&cfg.db_path).unwrap();
        let text = report(&cfg, &db).unwrap();
        assert!(text.contains("port            8080"), "{text}");
        assert!(text.contains("install id"), "{text}");
        assert!(
            !text.contains("topsecretvalue")
                && !text.contains("sup3rsecre1")
                && !text.contains("GsxABc"),
            "the check report is echoed to a supervisor's stdout: {text}"
        );
    }

    // The lint wants a constant compared with a constant to be deleted. That is the
    // entire point: this is a guard rail on a number someone will edit in a year, and
    // the second comparison is against a *different* constant in another module.
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn drain_is_longer_than_a_slow_write_but_shorter_than_a_timeout() {
        // The API's own per-request timeout is 30 s; a drain that outlasted it would
        // mean the client saw a timeout while we were still serving it.
        assert!(DRAIN_SECS >= 1 && DRAIN_SECS <= 10);
        assert!(DRAIN_SECS < app::REQUEST_TIMEOUT.as_secs());
    }
}
