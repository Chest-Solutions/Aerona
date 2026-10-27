//! Lockdown: the one action that changes a whole server at once, so it lives in one place.
//!
//! Both callers go through [`apply`]: the dashboard's button, which queues it through the outbox,
//! and `!freeze` / `!unfreeze`, which run it inline. Before this module the dashboard path had the
//! loop and the commands had nothing, which is how a server could be frozen from the panel and
//! thawed only by waiting for the timer.
//!
//! Slowmode at the maximum, on every channel that accepts messages, is what Discord's own "pause
//! activity" does. It is not a permission overwrite on every channel, which would be a second thing
//! to remember how to undo: a channel that already had a slowmode of its own cannot be given back
//! what the bot overwrote, because the bot cannot know what it was. `0` is therefore the honest
//! release, and the guild's own per-channel settings are the operator's to restore if they had any.

use serenity::all::{
    ChannelId, ChannelType, Context, CreateMessage, EditChannel, EditGuild, GuildId,
    VerificationLevel,
};

use aerona_core::identity::Snowflake;
use aerona_core::time::now;

use crate::clamp_chars;
use crate::state::BotState;

/// What was asked for. `Default` is the shape a raid response wants: the longest slowmode
/// Discord offers, fifteen minutes of it, and no verification change nobody asked for.
#[derive(Debug, Clone)]
pub struct Plan {
    pub active: bool,
    pub minutes: i64,
    pub slowmode_secs: u16,
    pub raise_verification: bool,
    /// Posted in the alert channel when set, which is the only reason a lockdown is not just
    /// a mystery to the people it is for.
    pub notice: Option<String>,
}

impl Default for Plan {
    fn default() -> Self {
        Self {
            active: true,
            minutes: 15,
            slowmode_secs: 21_600,
            raise_verification: false,
            notice: None,
        }
    }
}

/// Which channels take a rate limit. Categories do not hold messages, and a stage or forum does,
/// so the set is "everything a member can post in".
fn slowable(kind: ChannelType) -> bool {
    matches!(
        kind,
        ChannelType::Text | ChannelType::News | ChannelType::Voice | ChannelType::Stage |
        // A forum and its media twin both take a rate limit on the channel itself; the number is
        // how this serenity names the newer one.
        ChannelType::Forum | ChannelType::Unknown(16)
    )
}

/// The report key a lockdown writes under. The panel reads it to say "locked for 12 more
/// minutes", and the release path reads it to undo what the lock did, which is the only reason an
/// expiry timer and a button cannot disagree about what "unfrozen" means.
pub const REPORT: &str = "lockdown";

/// What a release has to remember: whether verification was raised, and to what.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct Saved {
    pub raised: bool,
    pub previous_verification: u64,
    pub minutes: i64,
    pub had_notice: bool,
}

pub fn saved(state: &BotState, guild: Snowflake) -> Saved {
    state
        .db
        .guild_report(guild, REPORT)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value::<Saved>(v).ok())
        .unwrap_or_default()
}

/// Apply or release, and say what happened in numbers an operator can believe.
pub async fn apply(
    state: &BotState,
    ctx: &Context,
    guild: Snowflake,
    plan: Plan,
) -> Result<String, String> {
    let g = GuildId::new(guild.as_u64());
    // The gateway cache first: it holds every channel of every guild the bot is in, and reading
    // it costs nothing. The REST call is the fallback for a guild that was just joined, where the
    // cache may not have caught up yet, and it is one request either way.
    let cached: Vec<(ChannelId, ChannelType)> = ctx
        .cache
        .guild(guild.as_u64())
        .map(|gu| {
            gu.channels
                .iter()
                .map(|(id, c)| (ChannelId::new(id.get()), c.kind))
                .collect()
        })
        .unwrap_or_default();
    let channels: Vec<(ChannelId, ChannelType)> = if cached.is_empty() {
        g.channels(&ctx.http)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|(id, c)| (id, c.kind))
            .collect()
    } else {
        cached
    };

    let mut touched = 0usize;
    let mut refused = 0usize;
    for (id, kind) in &channels {
        if !slowable(*kind) {
            continue;
        }
        let edit = EditChannel::new().rate_limit_per_user(if plan.active {
            plan.slowmode_secs
        } else {
            0
        });
        // One channel refusing (no permission, archived, a thread's parent elsewhere) must not
        // strand the rest of the server: that is how you end up half frozen and unable to thaw.
        match id.edit(&ctx.http, edit).await {
            Ok(_) => touched += 1,
            Err(e) if crate::outbox::is_transient(&e) => {
                refused += 1;
                tracing::debug!(channel = %id, "slowmode deferred to the next pass");
            }
            Err(e) => {
                refused += 1;
                tracing::debug!(channel = %id, error = %e, "channel refused the rate limit");
            }
        }
    }

    if plan.active {
        let previous = ctx
            .cache
            .guild(guild.as_u64())
            .map(|gu| gu.verification_level)
            .unwrap_or(VerificationLevel::None);
        // Carried as a number through serde rather than as a cast: serenity's enum is `u8` on the
        // wire, and the version of "unknown" it uses for a level newer than this crate is its
        // business, not ours.
        let previous_number = serde_json::to_value(previous)
            .ok()
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if let Some(until) = now().checked_add(plan.minutes * 60) {
            state.lockdown_until.insert(guild, until);
        }
        if plan.raise_verification {
            let _ = g
                .edit(
                    &ctx.http,
                    EditGuild::new().verification_level(VerificationLevel::High),
                )
                .await;
        }
        // Written after the change, before the reply: the panel shows this to decide whether the
        // button says "lock" or "release", and a release reads it back to know what to restore.
        let saved = Saved {
            raised: plan.raise_verification,
            previous_verification: previous_number,
            minutes: plan.minutes,
            had_notice: plan.notice.is_some(),
        };
        if let Ok(value) = serde_json::to_value(&saved) {
            let _ = state.db.put_guild_report(guild, REPORT, &value);
        }
    } else {
        // Release is release: the timer and the report go whether or not this pass touched a single
        // channel. Half of the old failure was a loop that stopped early; the other half was an
        // expiry path that never looked at what the lock had changed.
        state.lockdown_until.remove(&guild);
        let saved = saved(state, guild);
        if saved.raised {
            let level: VerificationLevel =
                serde_json::from_value(serde_json::json!(saved.previous_verification))
                    .unwrap_or(VerificationLevel::None);
            let _ = g
                .edit(&ctx.http, EditGuild::new().verification_level(level))
                .await;
        }
        let _ = state.db.delete_guild_report(guild, REPORT);
    }

    // Locking says what the operator wrote. Releasing says "unlocked" only when the lock said
    // something, so a server that was never told it was frozen is not told it stopped being frozen.
    let announced = plan.notice.is_some() || (!plan.active && saved(state, guild).had_notice);
    if announced {
        if let Some(channel) = notice_channel(state, ctx, guild) {
            let body = if plan.active {
                format!(
                    "🔒 **Server locked** for {} minutes.\n{}",
                    plan.minutes,
                    clamp_chars(plan.notice.as_deref().unwrap_or_default(), 900)
                )
            } else {
                "🔓 **Server unlocked.** Normal activity has resumed.".to_string()
            };
            let _ = ChannelId::new(channel.as_u64())
                .send_message(&ctx.http, CreateMessage::new().content(body))
                .await;
        }
    }

    let verb = if plan.active { "locked" } else { "released" };
    // A release that found the Saved row already gone still succeeded: the report is a memory, not
    // a precondition.
    let mut out = format!("{verb} {touched} channel(s)");
    if refused > 0 {
        out.push_str(&format!(", {refused} refused"));
    }
    if plan.active {
        out.push_str(&format!(" for {} minutes", plan.minutes));
    }
    tracing::info!(%guild, active = plan.active, touched, refused, "lockdown pass finished");
    Ok(out)
}

/// The channel a raid notice goes to: the configured alert channel, else the system channel, else
/// nothing. A bot that cannot find a channel stays quiet rather than posting into a guess.
pub fn notice_channel(state: &BotState, ctx: &Context, guild: Snowflake) -> Option<Snowflake> {
    if let Ok(cfg) = state.db.raid_config(guild) {
        if let Some(c) = cfg.alert_channel_id {
            return Some(c);
        }
    }
    ctx.cache
        .guild(guild.as_u64())
        .and_then(|g| g.system_channel_id)
        .map(|c| Snowflake(c.get()))
    // No third resort: a bot that cannot find a channel stays quiet rather than posting into
    // whichever channel it guessed at.
    //
    // `state` is only here for the config read, which is why it is a parameter and not a method.
}
