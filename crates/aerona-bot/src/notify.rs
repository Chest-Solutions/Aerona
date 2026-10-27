//! An action is not finished until the person it happened to knows about it.
//!
//! Two cards come out of one [`Action`]: a DM that says what was done and why, and a card for the
//! mod log. Both are best effort and in neither direction can they undo anything: a member with
//! DMs closed is still banned, and a log channel that refuses the post does not reopen the ban.
//! The reason is never withheld from the DM, because "contact the moderation team" is where every
//! appeal starts and a DM is where it is cheapest.

use serenity::all::{
    ChannelId, Color, Context, CreateEmbed, CreateEmbedFooter, CreateMessage, UserId,
};

use aerona_core::identity::Snowflake;

use crate::clamp_chars;
use crate::state::BotState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Warn,
    Pardon,
    Timeout,
    Kick,
    Ban,
    SoftBan,
    Unban,
    Purge,
    Note,
}

impl Kind {
    /// (what happened to you, what the record says, the card title, the colour).
    fn words(self) -> (&'static str, &'static str, &'static str, Color) {
        match self {
            Kind::Warn => (
                "warned",
                "a warning on your record",
                "Warning",
                Color::from_rgb(0xFE, 0xE7, 0x5C),
            ),
            Kind::Pardon => (
                "pardoned",
                "a warning pardoned",
                "Pardon",
                Color::from_rgb(0x57, 0xF2, 0x87),
            ),
            Kind::Timeout => (
                "timed out",
                "a timeout",
                "Timeout",
                Color::from_rgb(0xF1, 0xC4, 0x0F),
            ),
            Kind::Kick => (
                "kicked",
                "removed from the server",
                "Kick",
                Color::from_rgb(0xED, 0x42, 0x45),
            ),
            Kind::Ban => (
                "banned",
                "banned from the server",
                "Ban",
                Color::from_rgb(0xEB, 0x45, 0x9E),
            ),
            Kind::SoftBan => (
                "soft-banned",
                "banned, cleared and unbanned",
                "Soft ban",
                Color::from_rgb(0xEB, 0x45, 0x9E),
            ),
            Kind::Unban => (
                "unbanned",
                "the ban lifted",
                "Unban",
                Color::from_rgb(0x57, 0xF2, 0x87),
            ),
            Kind::Purge => (
                "messages cleared",
                "messages cleared",
                "Purge",
                Color::from_rgb(0xED, 0x42, 0x45),
            ),
            Kind::Note => (
                "noted",
                "a staff note on your record",
                "Note",
                Color::from_rgb(0x58, 0x65, 0xF2),
            ),
        }
    }

    /// A purge happens to a channel and a note is written by staff for staff: neither is a DM.
    pub fn dms(self) -> bool {
        !matches!(self, Kind::Purge | Kind::Note)
    }
}

/// Everything both cards need. Built once by the code that acted, whatever the transport was, so a
/// warn from `!warn` and a warn from the dashboard cannot disagree about what was said.
#[derive(Debug, Clone)]
pub struct Action<'a> {
    pub kind: Kind,
    pub target: Snowflake,
    /// The name to print, already resolved: a card that had to fetch a user would be a card that
    /// can fail for a reason unrelated to the action.
    pub target_name: &'a str,
    pub actor: &'a str,
    pub reason: &'a str,
    /// "45 minutes", "3 of 4 warns", "22 messages in #general".
    pub detail: Option<&'a str>,
    /// Where to post the card when the guild has no mod-log channel, so an action is never
    /// invisible. `None` for an action that happened in no channel worth posting in.
    pub fallback_channel: Option<Snowflake>,
}

/// The member's copy.
pub fn dm_card(a: &Action<'_>, server: &str) -> CreateEmbed {
    let (you, what, title, color) = a.kind.words();
    let place = if server.is_empty() {
        "the server"
    } else {
        server
    };
    let head = if a.kind.dms() {
        format!("{title}: you were {you} in {place}, and {what} was recorded.")
    } else {
        format!("{title}: {what} in {place}.")
    };
    let mut embed = CreateEmbed::new()
        .color(color)
        .title(clamp_chars(&head, 240))
        .field("Reason", clamp_chars(a.reason, 600), false)
        .footer(CreateEmbedFooter::new(
            "Reply to this DM if you think that is wrong.",
        ));
    if let Some(detail) = a.detail {
        embed = embed.field("Detail", clamp_chars(detail, 300), true);
    }
    embed
}

/// The staff copy: who did it, to whom, and why. The member is a mention rather than a name
/// because a name is what a renamed account lies about.
pub fn log_card(a: &Action<'_>, server: &str) -> CreateEmbed {
    let (_, _, title, color) = a.kind.words();
    let place = if server.is_empty() { "Aerona" } else { server };
    let mut embed = CreateEmbed::new()
        .color(color)
        .title(title)
        .field(
            "Member",
            format!("<@{}> ({})", a.target.as_u64(), a.target_name),
            true,
        )
        .field("By", clamp_chars(a.actor, 80), true)
        .field("Reason", clamp_chars(a.reason, 600), false)
        .footer(CreateEmbedFooter::new(format!("Aerona · {place}")));
    if let Some(detail) = a.detail {
        embed = embed.field("Detail", clamp_chars(detail, 300), true);
    }
    embed
}

/// What actually reached Discord, so a caller can say "noted, and they were told" instead of
/// claiming more than it knows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Delivered {
    pub dm: bool,
    pub logged: bool,
}

impl Delivered {
    /// The half a moderator is owed: whether anyone outside the log saw it.
    pub fn visible(&self) -> bool {
        self.logged
    }
}

/// Whether a DM goes out for this action, given what the guild allows. Two conditions, and both
/// are the guild's: an action that is not a DM kind, and a server that has DMs switched off. The
/// reason is always sent when the DM is, which is why there is no per-action reason toggle - a
/// member who is acted on gets told why, or the moderation is not finished.
pub fn wants_dm(kind: Kind, settings: &aerona_core::models::GuildSettings) -> bool {
    kind.dms() && settings.dms_enabled
}

/// Send both cards. Never fails: `deliver` cannot make an action that already happened un-happen.
pub async fn deliver(
    state: &BotState,
    ctx: &Context,
    guild: Snowflake,
    a: &Action<'_>,
) -> Delivered {
    let mut out = Delivered::default();
    let server = ctx
        .cache
        .guild(guild.as_u64())
        .map(|g| g.name.clone())
        .unwrap_or_default();

    let settings = state.db.guild_settings(guild).unwrap_or_default();
    if wants_dm(a.kind, &settings) {
        match UserId::new(a.target.as_u64()).to_user(&ctx.http).await {
            Ok(user) => match user.create_dm_channel(&ctx.http).await {
                Ok(dm) => {
                    let msg = CreateMessage::new().embed(dm_card(a, &server));
                    out.dm = dm.send_message(&ctx.http, msg).await.is_ok();
                }
                // Blocked, filtered, never met: the ban stands and the reason lives in the log.
                Err(e) => {
                    tracing::debug!(user = %a.target, error = %e, "no DM channel with that member")
                }
            },
            Err(e) => tracing::debug!(user = %a.target, error = %e, "that user id is not readable"),
        }
    }

    let cfg = state.db.mod_config(guild).unwrap_or_default();
    let target = match cfg.log_channel_id.or(a.fallback_channel) {
        Some(channel) => channel,
        None => return out,
    };
    let msg = CreateMessage::new().embed(log_card(a, &server));
    match ChannelId::new(target.as_u64())
        .send_message(&ctx.http, msg)
        .await
    {
        Ok(_) => out.logged = true,
        Err(e) => {
            tracing::warn!(%guild, channel = %target, error = %e, "the mod log refused a card")
        }
    }
    out
}
