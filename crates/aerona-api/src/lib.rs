//! Aerona dashboard API.
//!
//! One binary, one public port: it serves `/api/v1/*` *and* the built dashboard, so
//! a self-hosted install exposes exactly one socket. The bot process has no port at
//! all — the two talk through the SQLite file (config epochs, `bot_status`) and the
//! `action_outbox` queue.

pub mod app;
pub mod auth;
pub mod oauth;
pub mod respond;
pub mod routes;
pub mod server;

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) mod testutil;

pub use app::{router, AppState};

/// Version string, also reported by `/status` and checked by the dashboard.
pub const VERSION: &str = aerona_core::VERSION;
