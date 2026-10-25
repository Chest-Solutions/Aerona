//! # aerona-core
//!
//! Shared foundation for the two Aerona binaries:
//!
//! * `aerona-bot` — the Discord gateway worker (moderation, tickets, stickies,
//!   anti-raid, AI text filter).
//! * `aerona-api` — the dashboard HTTP API mounted at `/api/v1` that also hosts
//!   the built dashboard.
//!
//! Everything that both sides need — configuration, the SQLite schema, domain
//! models, crypto helpers, rate limiting — lives here so there is exactly one
//! definition of every rule in the product.
//!
//! ## Design constraints
//!
//! * **No JSON files on disk.** All persistence is SQLite (`.db`) with WAL.
//! * **Small footprint.** Only the std + a tight set of deps are used; the DB is
//!   synchronous and pool-backed instead of pulling in an async ORM.
//! * **Fail closed on security, fail open on availability.** A broken Sightengine
//!   call must never brick a server; a missing signature on an API mutation must
//!   always reject.

// The repository layer in `db/` maps HTTP/queue-shaped calls onto SQL, so its
// functions legitimately take one argument per column. Grouping them into
// structs for the sake of the lint would add a type per table without adding any
// safety, so the crate opts out of `too_many_arguments` rather than fight it.
#![allow(clippy::too_many_arguments)]

pub mod config;
pub mod db;
pub mod error;
pub mod identity;
pub mod models;
pub mod ratelimit;
pub mod security;
pub mod text;
pub mod time;

/// Wire format version of the public API. Bump on breaking route changes; the
/// dashboard refuses to run against a mismatched backend.
pub const API_VERSION: &str = "v1";

/// Product version, injected by cargo.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The single word the whole product is named after.
pub const NAME: &str = "Aerona";
