//! Error type shared by both binaries, plus the HTTP-facing envelope.
//!
//! Rule: internals get a precise, logged message; clients get a short, safe,
//! stable code. We never echo database errors, file paths or dependency output
//! to an unauthenticated caller — those are exactly the strings an attacker
//! uses to fingerprint a stack.

use std::fmt;

/// Machine-readable error codes surfaced by `GET/PATCH` failures. The dashboard
/// switches on these, so they are part of the API contract and must stay stable.
pub mod code {
    pub const BAD_REQUEST: &str = "bad_request";
    pub const UNAUTHORIZED: &str = "unauthorized";
    pub const FORBIDDEN: &str = "forbidden";
    pub const NOT_FOUND: &str = "not_found";
    pub const CONFLICT: &str = "conflict";
    pub const UNPROCESSABLE: &str = "unprocessable";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const LOCKED: &str = "locked";
    pub const CSRF: &str = "csrf_failed";
    pub const TOO_LARGE: &str = "payload_too_large";
    pub const UNAVAILABLE: &str = "service_unavailable";
    pub const INTERNAL: &str = "internal";
    pub const DISCORD: &str = "discord_rejected";
    pub const MODERATION_API: &str = "moderation_api_error";
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config: {0}")]
    Config(String),
    #[error("database: {0}")]
    Db(String),
    #[error("migration {version} failed: {reason}")]
    Migration { version: i64, reason: String },
    #[error("not found: {0}")]
    NotFound(&'static str),
    #[error("forbidden: {0}")]
    Forbidden(&'static str),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("rate limited")]
    RateLimited,
    /// The request body exceeded the configured limit. Separate from `Invalid`
    /// because the client's fix is different (send less, not retype it) and because
    /// a 413 lets a proxy distinguish "too big" from "malformed".
    #[error("payload too large")]
    PayloadTooLarge,
    #[error("upstream discord: {status} {message}")]
    Discord { status: u16, message: String },
    #[error("upstream moderation: {0}")]
    Moderation(String),
    #[error("io: {0}")]
    Io(String),
    #[error("internal")]
    Internal,
}

impl Error {
    /// Short code for the JSON envelope + log lines.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Config(_) | Error::Internal => code::INTERNAL,
            Error::Db(_) | Error::Io(_) => code::INTERNAL,
            Error::Migration { .. } => code::INTERNAL,
            Error::NotFound(_) => code::NOT_FOUND,
            Error::Forbidden(_) => code::FORBIDDEN,
            Error::Invalid(_) => code::BAD_REQUEST,
            Error::Conflict(_) => code::CONFLICT,
            Error::Unauthorized => code::UNAUTHORIZED,
            Error::RateLimited => code::RATE_LIMITED,
            Error::PayloadTooLarge => code::TOO_LARGE,
            Error::Discord { .. } => code::DISCORD,
            Error::Moderation(_) => code::MODERATION_API,
        }
    }

    /// HTTP status for this error.
    pub fn status(&self) -> u16 {
        match self {
            Error::Invalid(_) => 400,
            Error::Unauthorized => 401,
            Error::Forbidden(_) => 403,
            Error::NotFound(_) => 404,
            Error::Conflict(_) => 409,
            Error::RateLimited => 429,
            Error::PayloadTooLarge => 413,
            Error::Discord { status, .. } => match *status {
                400..=499 => *status,
                _ => 502,
            },
            Error::Moderation(_) => 502,
            _ => 500,
        }
    }

    /// Client-safe message. Deliberately generic for anything that could leak
    /// implementation detail (DB/IO/internal).
    pub fn safe_message(&self) -> String {
        match self {
            Error::Config(_) | Error::Internal => "the request could not be completed".into(),
            Error::Db(_) | Error::Io(_) | Error::Migration { .. } => {
                "storage is temporarily unavailable".into()
            }
            Error::NotFound(what) => format!("{what} was not found"),
            Error::Forbidden(why) => (*why).to_string(),
            Error::Invalid(why) => why.clone(),
            Error::Conflict(why) => why.clone(),
            Error::Unauthorized => "authentication required".into(),
            Error::RateLimited => "too many requests".into(),
            Error::PayloadTooLarge => "request body is too large".into(),
            Error::Discord { status, message } => match *status {
                403 => "Aerona lacks the required permission in that server".into(),
                404 => "that target no longer exists on Discord".into(),
                429 => "Discord is rate limiting Aerona; retry shortly".into(),
                _ => format!("discord rejected the action ({status}): {message}"),
            },
            Error::Moderation(_) => "the moderation provider is unavailable".into(),
        }
    }

    /// Convenience for `.map_err(|e| Error::Db(e.to_string()))`.
    pub fn db<E: fmt::Display>(e: E) -> Self {
        Error::Db(e.to_string())
    }

    pub fn io<E: fmt::Display>(e: E) -> Self {
        Error::Io(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        // Constraint failures are user-facing (duplicate warn, missing row);
        // everything else is a server fault we must not describe.
        match &e {
            rusqlite::Error::SqliteFailure(f, Some(msg)) => match f.code {
                rusqlite::ErrorCode::ConstraintViolation => Error::Conflict(truncate(msg, 180)),
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                    Error::RateLimited
                }
                _ => Error::Db(msg.clone()),
            },
            rusqlite::Error::QueryReturnedNoRows => Error::NotFound("record"),
            other => Error::Db(other.to_string()),
        }
    }
}

impl From<r2d2::Error> for Error {
    fn from(e: r2d2::Error) -> Self {
        Error::Db(e.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Invalid(format!("malformed json: {}", truncate(&e.to_string(), 120)))
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut out = String::with_capacity(max + 1);
    for ch in s.chars() {
        if out.len() + ch.len_utf8() > max {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_leaks_internals_to_clients() {
        let e = Error::Db("no such table: secrets at /home/aerona/data/x.db".into());
        assert_eq!(e.status(), 500);
        assert_eq!(e.code(), code::INTERNAL);
        assert_eq!(e.safe_message(), "storage is temporarily unavailable");
        assert!(!e.safe_message().contains("secrets"));
    }

    #[test]
    fn constraint_violation_becomes_conflict() {
        let e = Error::from(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
            Some("UNIQUE constraint failed: warns.id".into()),
        ));
        assert_eq!(e.status(), 409);
        assert_eq!(e.code(), code::CONFLICT);
    }

    #[test]
    fn truncation_is_utf8_safe() {
        assert_eq!(truncate("héllo wörld", 5), "héll…");
        assert_eq!(truncate("abc", 10), "abc");
    }
}
