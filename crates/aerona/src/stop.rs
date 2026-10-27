//! One stop signal, many listeners.
//!
//! The API has to drain, the worker has to say goodbye to Discord, and the supervisor
//! has to stop retrying a worker that is already down - all of them on the same SIGTERM,
//! none of them allowed to miss it because they happened to start a moment later. So this
//! is a *latched* flag, not a notification: `stopped()` is still true after the event, and
//! a task that begins waiting late sees it immediately.
//!
//! The latch is an `AtomicBool` and the wakeup is a `watch<u64>` counter that only ever
//! increments. That split is deliberate. A `watch<bool>` looks like the same thing and is
//! not: `send` reports the channel as closed once every receiver is gone, and a closed
//! `watch` silently stops storing values - which for a shutdown latch means "the process
//! that was asked to stop never notices", discovered by whoever had to `kill -9` it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

/// Cheap to clone; every clone is the same latch.
#[derive(Clone)]
pub struct Stop {
    inner: Arc<Inner>,
}

struct Inner {
    stopped: AtomicBool,
    ticks: AtomicU64,
    tx: watch::Sender<u64>,
    /// Kept alive so the channel is never "closed": a shutdown latch that can silently
    /// stop recording is worse than no latch at all.
    _anchor: watch::Receiver<u64>,
}

impl Default for Stop {
    fn default() -> Self {
        Self::new()
    }
}

impl Stop {
    pub fn new() -> Self {
        let (tx, anchor) = watch::channel(0);
        Self {
            inner: Arc::new(Inner {
                stopped: AtomicBool::new(false),
                ticks: AtomicU64::new(0),
                tx,
                _anchor: anchor,
            }),
        }
    }

    /// Latch it. Idempotent, and safe to call from a signal handler: nothing here locks.
    pub fn fire(&self) {
        if self.inner.stopped.swap(true, Ordering::SeqCst) {
            return; // already fired; a second wake-up would only churn the log
        }
        let tick = self.inner.ticks.fetch_add(1, Ordering::SeqCst) + 1;
        // The anchor above makes the only realistic `Err` impossible, and it would be
        // harmless anyway: the latch is set, which is the part that must persist.
        let _ = self.inner.tx.send(tick);
    }

    pub fn stopped(&self) -> bool {
        self.inner.stopped.load(Ordering::SeqCst)
    }

    /// Resolve as soon as the latch is set - immediately, if it already is.
    pub async fn wait(&self) {
        let mut rx = self.inner.tx.subscribe();
        loop {
            if self.stopped() {
                return;
            }
            if rx.changed().await.is_err() {
                // Every sender is gone, which cannot happen while an `Arc<Inner>` is
                // alive. Treat it as a stop rather than hanging forever.
                return;
            }
        }
    }

    /// `wait` as an owned future. This is the form a spawner wants: `start()` and
    /// `connect()` take a shutdown future that must be `Send + 'static`, and a borrow of
    /// a local `Stop` is neither, however briefly it lives.
    pub fn wait_signal(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let stop = self.clone();
        async move { stop.wait().await }
    }

    /// `wait`, then hold for `secs` - as an owned future, so it can be handed to a
    /// listener that keeps it across tasks. What a server needs: the latch says stop
    /// accepting, the sleep is what lets the requests already in flight finish.
    pub fn wait_drain(&self, secs: u64) -> impl std::future::Future<Output = ()> + Send + 'static {
        let stop = self.clone();
        async move {
            stop.wait().await;
            if secs > 0 {
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
        }
    }

    /// `wait`, then hold for `secs`, in this task.
    pub async fn wait_then(&self, secs: u64) {
        self.wait().await;
        if secs > 0 {
            tokio::time::sleep(Duration::from_secs(secs)).await;
        }
    }

    /// Install SIGINT/SIGTERM handling on the current runtime: whichever arrives first
    /// fires this latch. Called once, from `main`, after the runtime exists.
    ///
    /// A failure to install the SIGTERM handler is logged and survived. Ctrl-C in a
    /// foreground shell still works if only that half failed, and refusing to start a
    /// dashboard because a signal was awkward is worse than a dashboard somebody has to
    /// kill by hand.
    pub fn install_signals(&self) {
        let stop = self.clone();
        tokio::spawn(async move {
            let ctrl_c = tokio::signal::ctrl_c();
            #[cfg(unix)]
            let term = {
                use tokio::signal::unix::{signal as watch_signal, SignalKind};
                async {
                    match watch_signal(SignalKind::terminate()) {
                        Ok(mut stream) => stream.recv().await.is_some(),
                        Err(e) => {
                            tracing::warn!(%e, "no SIGTERM handler; Ctrl-C still works");
                            std::future::pending::<bool>().await
                        }
                    }
                }
            };
            #[cfg(not(unix))]
            let term = std::future::pending::<bool>();

            tokio::select! {
                res = ctrl_c => {
                    match res {
                        Ok(()) => tracing::info!("SIGINT: shutting down"),
                        Err(e) => tracing::warn!(%e, "could not listen for SIGINT"),
                    }
                }
                got = term => {
                    if got {
                        tracing::info!("SIGTERM: shutting down");
                    }
                }
            }
            stop.fire();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn firing_wakes_every_listener() {
        let stop = Stop::new();
        let waiters: Vec<_> = (0..4)
            .map(|_| {
                let s = stop.clone();
                tokio::spawn(async move {
                    s.wait().await;
                    true
                })
            })
            .collect();
        // Give the tasks a chance to register, so this is a test of waking and not of
        // the "already latched" shortcut.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!stop.stopped());
        stop.fire();
        for w in waiters {
            assert!(w.await.unwrap());
        }
        assert!(stop.stopped(), "the latch stays set");
    }

    /// The reason a latch is used instead of a broadcast: a component that starts after
    /// the signal must see it, not hang until somebody kills the process.
    #[tokio::test]
    async fn a_late_waiter_returns_immediately() {
        let stop = Stop::new();
        stop.fire();
        tokio::time::timeout(Duration::from_millis(500), stop.wait())
            .await
            .expect("an already-latched stop must not block a late listener");
        // ...including one that subscribes through the owned-future form, which is what
        // the server and the worker actually use.
        tokio::time::timeout(Duration::from_millis(500), stop.wait_signal())
            .await
            .expect("wait_signal must see a latch set before it existed");
    }

    #[test]
    fn firing_is_idempotent_and_survives_dropped_waiters() {
        let stop = Stop::new();
        // Nobody is listening when the signal arrives - the case a `watch<bool>` alone
        // gets wrong, because a channel with no receivers reports "closed" and stops
        // storing what you send to it.
        stop.fire();
        stop.fire();
        assert!(stop.stopped());
        // A clone sees the same latch, before and after.
        let later = stop.clone();
        assert!(later.stopped(), "clones share the latch");
    }

    #[tokio::test]
    async fn wait_then_adds_the_drain_window() {
        let stop = Stop::new();
        let s = stop.clone();
        let started = std::time::Instant::now();
        let drain = tokio::spawn(async move {
            s.wait_then(1).await;
            started.elapsed()
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!drain.is_finished(), "no signal yet, so no drain yet");
        stop.fire();
        let elapsed = drain.await.unwrap();
        assert!(
            elapsed >= Duration::from_millis(900),
            "the drain window was skipped: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_zero_drain_does_not_sleep() {
        let stop = Stop::new();
        stop.fire();
        let started = std::time::Instant::now();
        stop.wait_then(0).await;
        assert!(started.elapsed() < Duration::from_millis(500));
        // Same promise through the owned form, which is what serve() uses.
        let started = std::time::Instant::now();
        stop.wait_drain(0).await;
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}
