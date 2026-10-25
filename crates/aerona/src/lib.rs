//! `aerona` — the whole product in one process: the dashboard, the API and the
//! Discord worker.
//!
//! # Why this crate exists
//!
//! `aerona-api` and `aerona-bot` are complete programs on their own, and running them
//! as two processes is the most robust shape there is (a worker that dies cannot take
//! the dashboard with it). But it means an operator has to supervise two things, and
//! the most common host for this project — a game panel — is bad at exactly that: it
//! runs one command, and a container that exits looks like a crash.
//!
//! So this binary holds both halves and does the supervising inside itself:
//!
//! * one port, one process, one `Arc<Config>`, one SQLite pool;
//! * the API is the load-bearing part — if it cannot bind, the process exits non-zero;
//! * the worker is the *optional* part — a bad token, a Discord outage or even a panic
//!   in the shard must never take the dashboard down. It is retried with backoff in a
//!   task, and the outbox absorbs everything queued in the meantime;
//! * the two halves still never call each other. They talk through the database
//!   exactly as they do in the two-process shape, which is why the same tests cover
//!   both: [`aerona_api::server::start`] and [`aerona_bot::worker::connect`] are the
//!   same entry points the standalone binaries use.
//!
//! Nothing here is a second implementation of anything: this crate owns argument
//! handling, the process's signal story, the worker's restart policy, and the log
//! subscriber. Every rule about who may do what lives in the API.

pub mod cli;
pub mod serve;
pub mod stop;

/// Version string, so `aerona --version` and `/status` agree with the rest of the
/// workspace.
pub const VERSION: &str = aerona_core::VERSION;

/// What the binary calls itself in a usage line.
pub const PROG: &str = "aerona";
