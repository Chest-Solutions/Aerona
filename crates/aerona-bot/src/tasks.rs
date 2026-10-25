//! The worker's own clocks: the queue drain, the status report the dashboard reads,
//! and the slow maintenance passes. One task with a 20-second tick and one with a
//! 60-second tick — a loop per concern would be tidier in a language with cheap threads
//! and would cost more, so the cadence is a counter instead.
//!
//! Everything here is best-effort by design. A task that cannot reach Discord logs and
//! waits for the next tick; a task that panics is restarted by the supervisor. Nothing
//! in this file may hold a lock across an `await`, and nothing may block the gateway.

use std::sync::Arc;
use std::time::Duration;

use serenity::all::{ActivityData, GuildId, OnlineStatus, UserId};

use aerona_core::identity::Snowflake;
use aerona_core::models::StatsSnapshot;
use aerona_core::time::now;

use crate::commands;
use crate::outbox;
use crate::state::BotState;

/// How often the operator's "is the bot alive" data is rewritten. The dashboard treats
/// a report older than twice this as stale, so the two numbers are a contract.
pub const STATUS_SECS: u64 = 20;
pub const SLOW_SECS: u64 = 60;

/// Start the loops. Returns the handles so a test or a caller can await them; `main`
/// drops them, which is what a supervisor wants: they run until the process ends.
pub fn spawn(state: Arc<BotState>) -> Vec<tokio::task::JoinHandle<()>> {
    let fast = {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                fast_tick(&state).await;
                tokio::time::sleep(Duration::from_secs(STATUS_SECS)).await;
            }
        })
    };
    let slow = {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut n: u64 = 0;
            loop {
                n += 1;
                slow_tick(&state, n).await;
                tokio::time::sleep(Duration::from_secs(SLOW_SECS)).await;
            }
        })
    };
    vec![fast, slow]
}

async fn fast_tick(state: &BotState) {
    if state.ctx().is_none() {
        // Before `ready` there is no HTTP handle. Draining the queue then would either
        // fail or block, and a queue exists precisely to wait.
        return;
    }
    let applied = outbox::drain(state).await;
    if applied > 0 {
        tracing::debug!(applied, "outbox drain");
    }
    report_status(state).await;
    expire_bans(state).await;
}

async fn slow_tick(state: &BotState, n: u64) {
    let Some(ctx) = state.ctx() else { return };
    let n = n.max(1);
    if n % 10 == 0 {
        // Every ~10 minutes: the config-epoch sweep. A settings change is rare, and 15
        // seconds of staleness in a role assignment is not worth a query per action.
        let epochs = state.db.config_epochs().unwrap_or_default();
        let changed = state.refresh_epochs(&epochs);
        if !changed.is_empty() {
            tracing::info!(
                guilds = changed.len(),
                "settings changed; refreshing command registrations"
            );
            reregister_commands(state, ctx).await;
        }
    }
    if n % 3 == 0 {
        record_stats(state);
    }
    let _ = state.prune_cooldowns();
    release_expired_lockdowns(state, ctx).await;
    if n % 60 == 0 {
        auto_close_stale_tickets(state, ctx).await;
    }
    if n % 30 == 0 {
        set_presence(state, ctx);
    }
}

/// The row the dashboard's live tile reads, once per guild. Written as JSON because
/// `bot_status` is a report, not a schema: a new metric should not need a migration.
async fn report_status(state: &BotState) {
    let Ok(guilds) = state.db.guilds() else {
        return;
    };
    let (pending, failed) = state.db.outbox_depth().unwrap_or((0, 0));
    let users_cached = state.db.user_count().unwrap_or(0);
    let events = state.take_rate(&state.events_min);
    let ws = *state.ws_latency_ms.lock();
    let shards_total = *state.shards_total.lock();
    let shards_ready = *state.shards_ready.lock();
    for guild in guilds {
        let open_tickets = state.db.open_ticket_count(guild.id, None).unwrap_or(0);
        let active_raid = state
            .db
            .active_raid(guild.id)
            .ok()
            .flatten()
            .map(|e| e.id)
            .unwrap_or(0);
        let status = serde_json::json!({
            "wsLatencyMs": ws,
            "shardsTotal": shards_total,
            "shardsReady": shards_ready,
            "usersCached": users_cached,
            "activeRaid": active_raid,
            "openTickets": open_tickets,
            "queueDepth": pending,
            "queueFailed": failed,
            "gatewayEventsMin": events,
            "protocol": crate::WORKER_PROTOCOL,
            "reportedAt": now(),
        });
        if let Err(e) = state.db.put_bot_status(guild.id, &status) {
            tracing::debug!(guild = %guild.id, error = %e, "status write failed");
        }
    }
}

fn record_stats(state: &BotState) {
    let snapshot = StatsSnapshot {
        at: now(),
        process: "bot".into(),
        rss_kb: crate::state::rss_kb(),
        cpu_pct: crate::state::cpu_pct(),
        guilds: state.managed.len() as i64,
        users_seen: state.db.user_count().unwrap_or(0),
        ws_latency_ms: *state.ws_latency_ms.lock(),
        messages_per_min: state.take_rate(&state.msgs_min) as i64,
        db_size_kb: state.db.size_kb().unwrap_or(0),
    };
    let _ = state.db.record_stats(&snapshot);
}

/// A temporary ban is a row with an expiry; unban it here rather than leaving a member
/// out of the server forever because the API never runs this pass.
async fn expire_bans(state: &BotState) {
    let Some(ctx) = state.ctx() else { return };
    let due = state.db.due_unbans(64).unwrap_or_default();
    for (ban_id, guild, user) in due {
        match ctx
            .http
            .remove_ban(
                GuildId::new(guild.as_u64()),
                UserId::new(user.as_u64()),
                Some("temporary ban expired"),
            )
            .await
        {
            Ok(()) => {
                let _ = state.db.mark_unbanned(guild, ban_id);
                state.db.audit_best_effort(
                    aerona_core::db::security::AuditEntry::new("moderation.temp_ban_expired")
                        .for_guild(guild)
                        .actor(
                            aerona_core::db::security::ActorType::Bot,
                            None,
                            "aerona-bot",
                        )
                        .maybe_target(Some(user.as_str())),
                );
            }
            Err(e) => {
                tracing::debug!(guild = %guild, %e, "the unban was refused; the row stays open")
            }
        }
    }
}

/// A lockdown is a promise about time, so the worker keeps the promise even if nobody
/// clicks "release".
async fn release_expired_lockdowns(state: &BotState, ctx: &serenity::all::Context) {
    let at = now();
    let expired: Vec<_> = state
        .lockdown_until
        .iter()
        .filter(|e| *e.value() <= at)
        .map(|e| *e.key())
        .collect();
    for guild in expired {
        state.lockdown_until.remove(&guild);
        let Ok(channels) = GuildId::new(guild.as_u64()).channels(&ctx.http).await else {
            continue;
        };
        for (id, _) in channels {
            let _ = id
                .edit(
                    &ctx.http,
                    serenity::all::EditChannel::new().rate_limit_per_user(0),
                )
                .await;
        }
        state.db.audit_best_effort(
            aerona_core::db::security::AuditEntry::new("raid.lockdown_expired")
                .for_guild(guild)
                .actor(
                    aerona_core::db::security::ActorType::Bot,
                    None,
                    "aerona-bot",
                ),
        );
        tracing::info!(guild = %guild, "lockdown released automatically");
    }
}

async fn auto_close_stale_tickets(state: &BotState, ctx: &serenity::all::Context) {
    let Ok(guilds) = state.db.guilds() else {
        return;
    };
    for guild in guilds {
        let Ok(cfg) = state.db.ticket_config(guild.id) else {
            continue;
        };
        if cfg.auto_close_days <= 0 {
            continue;
        }
        let Ok(stale) = state.db.stale_tickets(guild.id, cfg.auto_close_days) else {
            continue;
        };
        for ticket in stale {
            let body = format!(
                "Closing this ticket after {} days of quiet. Reply here to reopen it.",
                cfg.auto_close_days
            );
            let _ = serenity::all::ChannelId::new(ticket.channel_id.as_u64())
                .send_message(&ctx.http, serenity::all::CreateMessage::new().content(body))
                .await;
            crate::events::close_ticket(state, ctx, guild.id, ticket.id).await;
        }
    }
}

fn set_presence(state: &BotState, ctx: &serenity::all::Context) {
    let text = format!(
        "/warn · {} server(s) · up {}",
        state.managed.len(),
        crate::commands::human_words(state.uptime_secs())
    );
    ctx.set_presence(Some(ActivityData::listening(text)), OnlineStatus::Online);
}

/// Register the slash commands a guild is allowed to have. Guild-scoped on purpose: a
/// global command is cached by Discord for up to an hour, which makes testing a new
/// command an exercise in patience, and it would advertise Aerona in servers that never
/// opted in.
pub async fn reregister_commands(state: &BotState, ctx: &serenity::all::Context) {
    let Ok(guilds) = state.db.guilds() else {
        return;
    };
    for guild in guilds {
        if !guild.bot_present {
            continue;
        }
        match reregister_guild(state, ctx, guild.id).await {
            Ok(n) => tracing::debug!(guild = %guild.id, commands = n, "slash commands registered"),
            // A server where we lost the applications role will refuse this forever; that
            // is a warning, not a crash, and the dashboard already shows the guild as
            // needing attention.
            Err(e) => tracing::warn!(guild = %guild.id, error = %e, "could not register commands"),
        }
        report_picker(state, ctx, guild.id);
    }
}

/// One guild's command list, pushed to Discord and then written down as what the dashboard
/// shows. The second half is the point: a dashboard that listed the commands from *its*
/// build would be a lie about a server where an operator has just turned four of them off,
/// so this reports what `set_commands` answered with — the list Discord accepted.
///
/// Returns the number registered. Called both from the slow loop and from the outbox, so a
/// "Sync now" in the panel and the hourly refresh cannot drift into two code paths.
pub async fn reregister_guild(
    state: &BotState,
    ctx: &serenity::all::Context,
    guild: Snowflake,
) -> Result<usize, String> {
    let disabled: std::collections::HashSet<String> = state
        .db
        .auto_config(guild)
        .map(|c| c.disabled_commands.into_iter().collect())
        .unwrap_or_default();
    let wanted: std::collections::HashSet<String> =
        commands::SPECS.iter().map(|s| s.name.to_string()).collect();
    let enabled: std::collections::HashSet<String> =
        wanted.difference(&disabled).cloned().collect();
    let defs = commands::slash_definitions(&enabled);
    let list = GuildId::new(guild.as_u64())
        .set_commands(&ctx.http, defs)
        .await
        .map_err(|e| e.to_string())?;
    let items: Vec<serde_json::Value> = list
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id.to_string(),
                "name": c.name,
                "description": c.description,
                // Everything in this list is registered in this guild, so it is enabled;
                // the ones the operator switched off are in `disabled` instead, which is
                // what lets the panel show both halves of the truth.
                "enabled": true,
                "guildScoped": true,
                // `default_member_permissions` is what Discord kept, not what we asked for,
                // so the panel shows the truth about who can type the command: this product
                // marks a staff command by requiring Manage Messages and nothing else.
                "staffOnly": c
                    .default_member_permissions
                    .map(|p| p.contains(serenity::all::Permissions::MANAGE_MESSAGES))
                    .unwrap_or(false),
            })
        })
        .collect();
    // The key names are the dashboard's contract, not this file's: `reportedAt` is what a
    // page prints next to every row so a list is never mistaken for live data.
    let _ = state.db.put_guild_report(
        guild,
        "commands",
        &serde_json::json!({
            "reportedAt": now(),
            "count": items.len(),
            "commands": items,
            "disabled": disabled.into_iter().collect::<Vec<_>>(),
        }),
    );
    Ok(list.len())
}

/// The channel and role lists a settings form needs, from the cache the gateway already
/// keeps. Written on the slow pass rather than fetched per page load: a guild with 400
/// roles should not cost 400 roles of API calls every time somebody opens a select box, and
/// a list that is a few minutes out of date is still better than one that is a build out of
/// date. An empty channel list is what a bot without Read Channel permissions in a private
/// channel looks like, which is why the report says how many it could see.
fn report_picker(state: &BotState, ctx: &serenity::all::Context, guild: Snowflake) {
    let Some(g) = ctx.cache.guild(guild.as_u64()) else {
        return;
    };
    let channels: Vec<serde_json::Value> = g
        .channels
        .iter()
        .filter(|(_, c)| c.kind == serenity::all::ChannelType::Text)
        .map(|(id, c)| serde_json::json!({ "id": id.to_string(), "name": c.name }))
        .collect();
    let roles: Vec<serde_json::Value> = g
        .roles
        .iter()
        .map(|(id, r)| {
            serde_json::json!({
                "id": id.to_string(),
                "name": r.name,
                // The mentionable flag is what decides whether a role picker shows it, so
                // the panel can say why a role is missing instead of looking broken.
                "managed": r.managed,
            })
        })
        .collect();
    let _ = state.db.put_guild_report(
        guild,
        "picker",
        &serde_json::json!({ "reportedAt": now(), "channels": channels, "roles": roles }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // The lints about "constant assertion" and "single-pattern match" do not apply to
    // intent: these two assert a contract about the constants themselves, which is only
    // checkable at compile-adjacent time.
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn the_report_cadence_matches_what_the_dashboard_calls_stale() {
        // `aerona-api` marks a bot report stale after 90 seconds; two ticks must be
        // comfortably inside that, or the dashboard lies about a healthy bot.
        assert!(
            STATUS_SECS * 2 < 90,
            "STATUS_SECS is too slow for the staleness window"
        );
        // The slow loop is a multiple on purpose: two ticks landing together is fine because
        // neither holds a lock across an await, and a non-multiple would only make the
        // cadence harder to read in the logs.
        assert_eq!(SLOW_SECS % STATUS_SECS, 0);
        assert!(
            SLOW_SECS >= 30,
            "the slow passes must not run so often that they dominate"
        );
    }

    #[test]
    fn presence_text_is_within_discords_activity_limit() {
        let words = crate::commands::human_words(999_999_999);
        let text = format!("/warn · 5000 server(s) · up {words}");
        assert!(text.chars().count() <= 128, "{text}");
    }
}
