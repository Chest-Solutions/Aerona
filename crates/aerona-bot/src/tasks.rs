//! The worker's own clocks: the queue drain, the status report the dashboard reads,
//! and the slow maintenance passes. One task with a 20-second tick and one with a
//! 60-second tick - a loop per concern would be tidier in a language with cheap threads
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
/// How often the bot checks whether the panel asked for something. Nothing in this loop is
/// expensive when the answer is no - one indexed query for the queue, one `SELECT` for the
/// settings epoch - and it is the number that decides how long a moderator waits after pressing
/// a button. At 20 seconds, a click reads as a broken button; at 2, it reads as a save.
pub const PUSH_SECS: u64 = 2;

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
    let queue = {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                queue_tick(&state).await;
                tokio::time::sleep(Duration::from_secs(PUSH_SECS)).await;
            }
        })
    };
    vec![fast, slow, queue]
}

/// The short loop: what the panel asked for, what the panel changed, and what has now expired.
async fn queue_tick(state: &BotState) {
    let Some(ctx) = state.ctx() else { return };
    // A queued action first, because that is a button somebody pressed.
    if outbox::drain(state).await > 0 {
        // Draining can change what the status tile reports, so let the next report see it rather
        // than a stale depth for another 20 seconds.
        report_status(state).await;
    }
    // Then the settings epoch: a save in the panel must not wait on an hour-long pass to be
    // reflected in what commands Discord offers.
    let epochs = state.db.config_epochs().unwrap_or_default();
    let changed = state.refresh_epochs(&epochs);
    if !changed.is_empty() {
        tracing::info!(
            guilds = changed.len(),
            "settings changed; refreshing command registrations"
        );
        reregister_commands(state, ctx).await;
    }
    // And the lockdown timer, so a freeze ends when it said it would and the verification level it
    // raised goes back in the same breath.
    release_expired_lockdowns(state, ctx).await;
}

async fn fast_tick(state: &BotState) {
    if state.ctx().is_none() {
        // Before `ready` there is no HTTP handle. Draining the queue then would either
        // fail or block, and a queue exists precisely to wait.
        return;
    }
    // The queue is drained by `queue_tick` every couple of seconds; this pass only reports, so
    // one loop is responsible for acting on what the panel asked for.
    report_status(state).await;
    expire_bans(state).await;
}

async fn slow_tick(state: &BotState, n: u64) {
    let Some(ctx) = state.ctx() else { return };
    let n = n.max(1);
    // The config-epoch sweep and the lockdown timer moved to `queue_tick`: a settings change and
    // the end of a freeze are both things an operator is watching happen.
    if n % 3 == 0 {
        record_stats(state);
    }
    let _ = state.prune_cooldowns();
    if n % 60 == 0 {
        auto_close_stale_tickets(state, ctx).await;
    }
    // Closed tickets whose channels have outlived their usefulness. Every tick, because the
    // deadline a moderator was told about is in minutes, and a sweep that runs hourly can be an
    // hour late on the promise.
    delete_due_ticket_channels(state, ctx).await;
    if n % 30 == 0 {
        set_presence(state, ctx);
    }
    // The cape notifier keeps its own clock inside this one: every guild is asked here, and the
    // per-guild stamp decides whether that guild is due. So the interval an operator sets is
    // honoured to the minute without a timer per server, and a bot in forty servers holds one loop.
    crate::capes::tick(state, ctx).await;
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

    // Then the guilds whose channel or role list actually moved. One REST pair each, so a server
    // that renamed nothing costs nothing, and a settings form that has just been given a new
    // channel sees it inside one status tick rather than one slow pass.
    let Some(ctx) = state.ctx() else { return };
    let mut dirty: Vec<Snowflake> = state.picker_dirty.iter().map(|e| *e).collect();
    dirty.sort();
    // Two REST calls per guild, so a bot in four hundred servers must not spend one status tick
    // hammering the API for all of them at once. The rest stay marked and are picked up by the next
    // tick, which is still inside a minute of a moderator asking.
    for guild in dirty.into_iter().take(PER_TICK) {
        state.picker_dirty.remove(&guild);
        report_picker(state, ctx, guild, true).await;
    }
}

/// Guilds refreshed per status tick. Enough that one server's operator never waits, small enough
/// that a bot in hundreds of guilds cannot rate-limit itself into a stall.
const PER_TICK: usize = 12;

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
        // One path for both ways out of a lockdown. This used to be its own loop of slowmode
        // writes, which is how the timer released the rate limits while the verification level the
        // lock had raised stayed raised forever.
        let plan = crate::raid::Plan {
            active: false,
            ..Default::default()
        };
        match crate::raid::apply(state, ctx, guild, plan).await {
            Ok(note) => {
                let mut entry = aerona_core::db::security::AuditEntry::new("raid.lockdown_expired")
                    .for_guild(guild)
                    .actor(
                        aerona_core::db::security::ActorType::Bot,
                        None,
                        "aerona-bot",
                    );
                // The count belongs in the audit row: "released" is not as useful a sentence as
                // "released, 41 channels, 2 refused".
                entry.meta = serde_json::json!({ "result": note });
                state.db.audit_best_effort(entry);
                tracing::info!(guild = %guild, "lockdown released automatically");
            }
            Err(e) => {
                // Put the deadline back an hour, or the timer would retry this every status tick
                // for a guild Discord is currently refusing to talk to.
                if let Some(later) = now().checked_add(3600) {
                    state.lockdown_until.insert(guild, later);
                }
                tracing::warn!(guild = %guild, error = %e, "automatic release failed, retrying later");
            }
        }
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

/// Delete the channels of tickets that were closed long enough ago.
///
/// Three rules, all of them learned the hard way by bots that offered this feature:
///
/// * the transcript is written first, by `close_ticket`, and the file copies with it - this sweep
///   only ever removes the container, never the record;
/// * a channel that will not delete is not marked deleted. Discord refusing (no permission, a
///   category locked down, an audit-log problem) is retried, with a six-hour backoff so a
///   permanently unreachable channel does not warn once a minute forever;
/// * a channel that is already gone - deleted by hand, or by a moderation action - is recorded as
///   gone, because leaving it in the queue means a janitor knocking on a door that is not there.
async fn delete_due_ticket_channels(state: &BotState, ctx: &serenity::all::Context) {
    let now = aerona_core::time::now();
    let due = match state.db.tickets_due_for_delete(now, 16) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(%e, "the ticket sweep could not read its queue");
            return;
        }
    };
    for ticket in due {
        let channel = serenity::all::ChannelId::new(ticket.channel_id.as_u64());
        // No audit-log reason: serenity's `delete` does not take one, and a channel that vanishes
        // with the ticket reference in its name is its own explanation.
        match channel.delete(&ctx.http).await {
            Ok(_) => {
                let _ = state
                    .db
                    .mark_ticket_channel_deleted(ticket.id, aerona_core::time::now());
                tracing::info!(
                    guild = %ticket.guild_id,
                    ticket = %ticket.reference,
                    "deleted a closed ticket's channel"
                );
            }
            Err(e) if channel_gone(&e) => {
                let _ = state
                    .db
                    .mark_ticket_channel_deleted(ticket.id, aerona_core::time::now());
                tracing::debug!(
                    guild = %ticket.guild_id,
                    ticket = %ticket.reference,
                    "the channel was already deleted by somebody else"
                );
            }
            Err(e) => {
                // Back off rather than retry every minute, and say what the operator can act on:
                // the channel, the ticket, and that the transcript is safe either way.
                let _ = state
                    .db
                    .schedule_ticket_delete(ticket.id, Some(aerona_core::time::now() + 6 * 3600));
                tracing::warn!(
                    guild = %ticket.guild_id,
                    ticket = %ticket.reference,
                    channel = %ticket.channel_id,
                    %e,
                    "could not delete a closed ticket's channel; its transcript is kept and this is retried in six hours"
                );
            }
        }
        // A guild that set a file retention gets its old copies pruned on the same pass that
        // touches tickets, which is the only place that knows what each guild asked for.
        if let Ok(cfg) = state.db.ticket_config(ticket.guild_id) {
            if let Some(secs) = cfg.file_retention_secs() {
                let _ = state
                    .db
                    .prune_ticket_files(ticket.guild_id, aerona_core::time::now() - secs);
            }
        }
    }
}

/// Did Discord say "no", or "there is nothing there"? `UnknownChannel` is serenity's typed answer;
/// a 404 off a proxy or a CDN-fronted endpoint arrives as a status, so both are treated as gone.
fn channel_gone(e: &serenity::Error) -> bool {
    match e {
        serenity::Error::Http(h) => h.status_code().map(|s| s.as_u16()) == Some(404),
        _ => false,
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
        report_picker(state, ctx, guild.id, false).await;
    }
}

/// One guild's command list, pushed to Discord and then written down as what the dashboard
/// shows. The second half is the point: a dashboard that listed the commands from *its*
/// build would be a lie about a server where an operator has just turned four of them off,
/// so this reports what `set_commands` answered with - the list Discord accepted.
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

/// The channel and role lists a settings form needs.
///
/// `rest` asks Discord for the live lists rather than reading the gateway cache. The cache is free
/// and correct a moment after `READY`, which is what the boot pass wants; a panel that has just
/// been told "refresh" wants the answer now, and one request per guild per click is the price of
/// that. Without this parameter the picker could only ever be as fresh as the slow loop, which is
/// why a channel created in Discord took minutes to appear in the box.
///
/// Every channel kind a picker can point at is reported, with what the panel needs to draw it: the
/// kind, so a text channel can get its `#`, and for roles the colour, the icon and the position.
/// Categories are a separate list, because the ticket category is the one place a category is the
/// right answer and everywhere else it is a mistake waiting to be saved.
///
/// An empty channel list is what a bot without permission to see a private channel looks like,
/// which is why the report says how much it could see.
/// One row per decoration in use, most-worn first: `id` is the SKU a profile patch takes, `asset`
/// is Discord's hash for it, and `usedBy` says how many members here are wearing it - which is the
/// only reason anybody can pick one at all.
///
/// A pure fold over `(sku, asset)` pairs rather than a walk of the cache, so the counting is
/// testable without a guild, and so the caller decides what "seen" means for its server.
pub(crate) fn decoration_rows(
    items: impl IntoIterator<Item = (u64, String)>,
) -> Vec<serde_json::Value> {
    let mut seen: std::collections::HashMap<(u64, String), usize> =
        std::collections::HashMap::new();
    for item in items {
        *seen.entry(item).or_insert(0) += 1;
    }
    let mut rows: Vec<serde_json::Value> = seen
        .into_iter()
        .map(|((sku, asset), used)| {
            serde_json::json!({
                "id": sku.to_string(),
                "asset": asset,
                "usedBy": used,
            })
        })
        .collect();
    // Most-worn first: a moderator choosing a decoration for the bot is choosing one they have
    // seen, and an id is easier to recognise when the number behind it is familiar.
    rows.sort_by(|a, b| {
        b["usedBy"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["usedBy"].as_u64().unwrap_or(0))
            .then_with(|| {
                a["id"]
                    .as_str()
                    .unwrap_or_default()
                    .cmp(b["id"].as_str().unwrap_or_default())
            })
    });
    rows
}

pub async fn report_picker(
    state: &BotState,
    ctx: &serenity::all::Context,
    guild: Snowflake,
    rest: bool,
) -> usize {
    let gid = GuildId::new(guild.as_u64());
    let kind_word = |k: serenity::all::ChannelType| match k {
        serenity::all::ChannelType::Text => "text",
        serenity::all::ChannelType::Voice => "voice",
        serenity::all::ChannelType::Category => "category",
        // `News` is what this crate calls an announcement channel, and a media forum is newer
        // than the enum: naming the number keeps the picker honest about what it is looking at.
        serenity::all::ChannelType::News => "announcement",
        serenity::all::ChannelType::Stage => "stage",
        serenity::all::ChannelType::Forum => "forum",
        serenity::all::ChannelType::Unknown(16) => "media",
        serenity::all::ChannelType::Private => "dm",
        _ => "other",
    };

    let (channel_rows, role_rows) = if rest {
        let channels = match gid.channels(&ctx.http).await {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!(%guild, error = %e, "could not read channels for the picker");
                return 0;
            }
        };
        let roles = gid.roles(&ctx.http).await.unwrap_or_default();
        (
            channels
                .into_iter()
                .map(|(id, c)| (id.get(), c.name, c.kind, c.parent_id.map(|p| p.get())))
                .collect::<Vec<_>>(),
            roles
                .into_iter()
                .map(|(id, r)| {
                    (
                        id.get(),
                        r.name,
                        r.colour.0,
                        r.managed,
                        r.position,
                        r.icon.map(|h| h.to_string()),
                        r.unicode_emoji,
                        r.tags.bot_id.map(|b| b.get()),
                    )
                })
                .collect::<Vec<_>>(),
        )
    } else {
        let Some(g) = ctx.cache.guild(guild.as_u64()) else {
            return 0;
        };
        (
            g.channels
                .iter()
                .map(|(id, c)| {
                    (
                        id.get(),
                        c.name.clone(),
                        c.kind,
                        c.parent_id.map(|p| p.get()),
                    )
                })
                .collect(),
            g.roles
                .iter()
                .map(|(id, r)| {
                    (
                        id.get(),
                        r.name.clone(),
                        r.colour.0,
                        r.managed,
                        r.position,
                        r.icon.map(|h| h.to_string()),
                        r.unicode_emoji.clone(),
                        r.tags.bot_id.map(|b| b.get()),
                    )
                })
                .collect(),
        )
    };

    let mut categories = Vec::new();
    let mut channels = Vec::new();
    for (id, name, kind, parent) in channel_rows {
        let row = serde_json::json!({
            "id": id.to_string(),
            "name": name,
            "kind": kind_word(kind),
            "category": parent.map(|p| p.to_string()),
        });
        if matches!(kind, serenity::all::ChannelType::Category) {
            categories.push(row);
        } else {
            channels.push(row);
        }
    }
    // Most bots can see every channel, so sorting is the difference between a usable list and a
    // reshuffling one: the panel groups by category and shows them in Discord's own order.
    channels.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });
    let roles: Vec<serde_json::Value> = role_rows
        .into_iter()
        .map(|(id, name, color, managed, position, icon, emoji, bot)| {
            serde_json::json!({
                "id": id.to_string(),
                "name": name,
                // The colour as a number, so the panel can paint the chip without parsing a string.
                "color": color,
                "managed": managed,
                "position": position,
                // A role icon is a hash; the CDN path is stable and costs no request, the same
                // trick the welcome card uses for avatars.
                "iconUrl": icon.map(|h| format!("https://cdn.discordapp.com/role-icons/{id}/{h}.png?size=32")),
                "emoji": emoji,
                // A bot or integration role cannot be granted by hand, and a picker that offers it
                // is a picker that saves a role Discord will immediately strip again.
                "assignable": !managed && bot.is_none(),
            })
        })
        .collect();
    // The message list rides along with the picker because it answers the same question: what
    // exists right now. The cap is the guild's own cache window, so a server that keeps 500
    // messages gets 500 choices and one that keeps none gets an empty picker and the truth.
    let window = state
        .db
        .log_config(guild)
        .unwrap_or_default()
        .max_cached_messages;
    let messages = state.db.recent_messages(guild, window).unwrap_or_default();
    // Decorations are the one thing on this page Discord refuses to list for a bot: there is no
    // catalogue endpoint. So the report carries what can actually be seen - the decorations the
    // members of *this* server are wearing - and the panel offers those plus a box for an id typed
    // in. A refresh with an empty cache yields an empty list, which is the truth rather than a bug.
    let decorations = decoration_rows(
        ctx.cache
            .guild(guild.as_u64())
            .map(|g| {
                g.members
                    .values()
                    .filter_map(|m| m.avatar_decoration_data.as_ref())
                    .map(|d| (d.sku_id.get(), d.asset.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
    );
    let count = channels.len() + roles.len();
    let _ = state.db.put_guild_report(
        guild,
        "picker",
        &serde_json::json!({
            "reportedAt": now(),
            "channels": channels,
            "categories": categories,
            "roles": roles,
            "decorations": decorations,
            "seen": count,
        }),
    );
    // The array is the payload: the picker's own `reportedAt` above is the freshness mark for both,
    // since one call writes them from one read.
    let _ = state
        .db
        .put_guild_report(guild, "messages", &serde_json::Value::Array(messages));
    count
}

/// Mark a guild whose channels or roles just moved, so the next status pass rebuilds its picker
/// from Discord instead of the panel waiting on an hour.
pub fn dirty_picker(state: &BotState, guild: Snowflake) {
    state.picker_dirty.insert(guild);
}

#[cfg(test)]
mod tests {
    #[test]
    fn decorations_are_counted_by_who_wears_them() {
        let rows = super::decoration_rows(vec![
            (7, "aaa".to_string()),
            (9, "bbb".to_string()),
            (7, "aaa".to_string()),
            (7, "ccc".to_string()),
        ]);
        assert_eq!(
            rows.len(),
            3,
            "a SKU and an asset together name a decoration"
        );
        assert_eq!(rows[0]["id"], "7");
        assert_eq!(
            rows[0]["usedBy"], 2,
            "the most-worn one leads, whatever order the members came in"
        );
        assert_eq!(
            rows[1]["id"], "7",
            "the same SKU with a different asset is its own choice, not a merge"
        );
        assert_eq!(rows[2]["id"], "9");
        assert!(
            super::decoration_rows(vec![]).is_empty(),
            "nothing cached means nothing offered"
        );
    }
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
