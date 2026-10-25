//! The Aerona gateway worker: the only process in this product that talks to Discord.
//!
//! Split of responsibility, stated once because it explains every design decision
//! below: `aerona-api` owns *intent* (who is allowed to do what, what is stored, what
//! is audited) and this crate owns *execution* (the gateway, the actions, the
//! reactions to events). The two never speak over a socket — the API writes rows into
//! `action_outbox` and this worker claims them, so a restart loses nothing and a
//! Discord outage turns into a backed-up queue instead of a failed HTTP request.
//!
//! Three rules the whole crate follows:
//!
//! * **The cache is not a source of truth.** Anything the dashboard shows is read from
//!   SQLite; serenity's in-memory cache is used only to answer "what is this channel
//!   called" style questions where being one event stale is harmless. That is what
//!   keeps memory flat on a 5 000-member server.
//! * **No handler may panic.** Discord sends garbage sometimes (a field absent, a
//!   malformed mention, a payload from an older schema). Every entry point catches a
//!   panic from the future it spawned, logs it, and carries on — a panic here would
//!   take the gateway down for every server at once.
//! * **Best effort outward, strict inward.** Writes to Discord are allowed to fail
//!   silently-but-loudly (a warning, a queue entry marked failed); writes to the
//!   database go through the same validated core layer the API uses.

pub mod commands;
pub mod events;
pub mod gate;
pub mod outbox;
pub mod state;
pub mod tasks;
pub mod worker;

pub use state::BotState;

/// How many queued actions one worker tick claims. Discord's write budget per token is
/// roughly 10 requests/2 s for non-privileged calls; 32 in flight is enough to drain a
/// raid sweep without inviting a 429 on every claim.
pub const OUTBOX_BATCH: u32 = 32;

/// A sticky is re-posted at most once per this many seconds per channel, however many
/// messages pass under it. Without this, a chatty channel edits the sticky every
/// message and the edit itself becomes the spam.
pub const STICKY_MIN_SECS: i64 = 15;

/// The gateway worker's own protocol version, reported in `bot_status` so the dashboard
/// can tell "the bot is old" apart from "the bot is down".
pub const WORKER_PROTOCOL: u32 = 1;

/// Discord's hard limit for message content.
pub const MAX_MESSAGE_CHARS: usize = 2000;

/// Truncate to what Discord will accept, with an ellipsis that survives the byte
/// boundary: `&s[..n]` on a multi-byte character is a panic, and a panic in a message
/// handler means a crash loop on one bad sticker name.
pub fn clamp_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::clamp_chars;

    #[test]
    fn short_text_is_untouched() {
        assert_eq!(clamp_chars("hello", 2000), "hello");
        assert_eq!(clamp_chars("", 10), "");
    }

    #[test]
    fn long_text_keeps_its_last_character_as_the_ellipsis() {
        let big = "x".repeat(3000);
        let out = clamp_chars(&big, 200);
        assert_eq!(out.chars().count(), 200);
        assert!(out.ends_with('…'));
    }

    /// The whole point of counting chars rather than slicing bytes: a grapheme cluster
    /// must never be cut in half, or the writer panics on somebody's emoji.
    #[test]
    fn multi_byte_content_does_not_panic() {
        let family = "👨‍👩‍👧".repeat(500);
        let out = clamp_chars(&family, 10);
        assert_eq!(out.chars().count(), 10);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }
}
