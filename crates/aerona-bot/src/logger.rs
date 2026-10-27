//! The mod log: one card per event, and the row that outlives the card.
//!
//! Before this module every handler made the same three decisions by hand: whether the guild
//! wanted the event, what the words were, and what to do when the channel refused the post. They
//! drifted, and the drift was silent, which is how a logger ended up that only deleted messages
//! reached and edits reached nobody. `note` is now the only way an event becomes a log entry, so
//! the channel copy and the dashboard list cannot describe different things.

use serenity::all::{ChannelId, Color, Context, CreateEmbed, CreateEmbedFooter, CreateMessage};

use aerona_core::db::logging::LogEntry;
use aerona_core::identity::Snowflake;

use crate::clamp_chars;
use crate::state::BotState;

/// The words and the colour for one event key. Discord colours carry meaning the same way a
/// traffic light does, so a join is green and a delete is red without a config field deciding it.
fn words(event: &str) -> (&'static str, &'static str, Color) {
    match event {
        "message_edit" => ("✏️", "Message edited", Color::from_rgb(0xF1, 0xC4, 0x0F)),
        "message_delete" => ("🗑", "Message deleted", Color::from_rgb(0xED, 0x42, 0x45)),
        "message_bulk_delete" => (
            "🗑",
            "Messages bulk deleted",
            Color::from_rgb(0xED, 0x42, 0x45),
        ),
        "member_join" => ("➡️", "Member joined", Color::from_rgb(0x57, 0xF2, 0x87)),
        "member_leave" => ("⬅️", "Member left", Color::from_rgb(0xFE, 0xE7, 0x5C)),
        "member_ban" => ("🔨", "Banned", Color::from_rgb(0xEB, 0x45, 0x9E)),
        "member_unban" => ("🔓", "Unbanned", Color::from_rgb(0x57, 0xF2, 0x87)),
        "role_change" => ("🎭", "Roles changed", Color::from_rgb(0x58, 0x65, 0xF2)),
        "nickname_change" => ("🏷", "Nickname changed", Color::from_rgb(0x58, 0x65, 0xF2)),
        "channel_change" => ("📁", "Channels changed", Color::from_rgb(0x58, 0x65, 0xF2)),
        // Reachable only if `LOG_EVENTS` grows and this table is not told: an unknown key still
        // gets logged, in blurple, rather than dropped.
        _ => ("📋", "Event", Color::from_rgb(0x58, 0x65, 0xF2)),
    }
}

/// Does this guild want this event? Kept separate from [`note`] so a caller that has to read the
/// config for another reason anyway (the welcome path needs it regardless) does not read it twice.
pub fn wanted(
    state: &BotState,
    guild: Snowflake,
    e: &LogEntry,
) -> Option<aerona_core::models::LogConfig> {
    let cfg = state.db.log_config(guild).ok()?;
    if !cfg.tracks(e.event) {
        return None;
    }
    // Two bots logging each other is a feedback loop, which is why the option exists at all.
    if e.bot && cfg.ignore_bots {
        return None;
    }
    Some(cfg)
}

/// The card, as a pure function: the test asserts the wire shape, and the handler that cannot
/// reach Discord in a test still gets to prove what it would have said.
///
/// The two halves of a change are *fields*, not sentences inside the description: an edit's text
/// and its replacement have to be read against each other, and a card that runs them together in
/// one paragraph is a card where nobody can tell which line changed. Fenced, because a message
/// that is itself markdown stops being a wall of asterisks the moment it is a code block.
pub fn card(server: &str, guild: Snowflake, e: &LogEntry) -> CreateEmbed {
    let (emoji, title, color) = words(e.event);
    // A delete is the one event whose content *is* the story, so the kept text goes in the body,
    // where a moderator reads it, and the Before/After pair is left out: nothing followed a
    // deletion, and a field that says "(deleted)" says what the title already said.
    let kept = if e.event == "message_delete" {
        e.before.as_deref().filter(|t| !t.trim().is_empty())
    } else {
        None
    };
    let description = match kept {
        Some(text) => format!("{}\n{}", clamp_chars(&e.summary, 200), quote(text)),
        None => clamp_chars(&e.summary, 600),
    };
    let mut embed = CreateEmbed::new()
        .color(color)
        .title(format!("{emoji} {title}"))
        .description(description);
    if kept.is_none() {
        if let Some(before) = e.before.as_deref().filter(|t| !t.trim().is_empty()) {
            embed = embed.field("Before", quote(before), true);
        }
        if let Some(after) = e.after.as_deref().filter(|t| !t.trim().is_empty()) {
            embed = embed.field("After", quote(after), true);
        }
    }

    if let Some(channel) = e.channel {
        embed = embed.field("Channel", format!("<#{channel}>"), true);
    }
    if let Some(user) = e.user {
        embed = embed.field("Member", format!("<@{user}>"), true);
    }
    if let (Some(channel), Some(message)) = (e.channel, e.message) {
        // A link to the message, which is the difference between a log and a lead.
        embed = embed.field(
            "Message",
            format!("[jump](https://discord.com/channels/{guild}/{channel}/{message})"),
            true,
        );
    }
    if server.is_empty() {
        embed.footer(CreateEmbedFooter::new("Aerona"))
    } else {
        embed.footer(CreateEmbedFooter::new(format!("Aerona · {server}")))
    }
}

/// Record, then post. The row is written even when the channel is not there or refuses: the log is
/// the record, the channel is a copy of it, and losing the record because a post failed is how a
/// moderator's audit trail ends up with holes in it.
pub async fn note(state: &BotState, ctx: &Context, guild: Snowflake, e: LogEntry) {
    let Some(cfg) = wanted(state, guild, &e) else {
        return;
    };
    let _ = state.db.record_log_event(guild, &e);
    let Some(channel) = cfg.channel_id else {
        return;
    };
    let server = ctx
        .cache
        .guild(guild.as_u64())
        .map(|g| g.name.clone())
        .unwrap_or_default();
    let sent = ChannelId::new(channel.as_u64())
        .send_message(
            &ctx.http,
            CreateMessage::new().embed(card(&server, guild, &e)),
        )
        .await;
    if sent.is_err() {
        // Missing access to a log channel is a setup problem, not an event problem: say so once,
        // in the log the operator reads, and keep recording.
        tracing::warn!(%guild, channel = %channel, "logged an event the channel could not receive");
    }
}

/// One side of a change, safe to put inside a card: backticks and fences are removed so quoted
/// text cannot close the block it is quoted in and start typing as the bot. The row in
/// `log_events` keeps the text exactly as it was, so nothing is lost - the card is a copy.
fn quote(text: &str) -> String {
    let safe = text.replace('`', "\u{200b}`");
    format!("```\n{}\n```", clamp_chars(&safe, 700))
}

/// A one-line summary for an event whose detail is ids rather than text.
pub fn summary(parts: &[(&str, String)]) -> String {
    let mut out = String::new();
    for (key, value) in parts {
        if !out.is_empty() {
            out.push_str(" · ");
        }
        out.push_str(&format!("{key} {value}"));
    }
    out
}
