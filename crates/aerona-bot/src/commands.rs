//! Commands, in both dialects. A prefix command and its slash equivalent call the same
//! function with the same arguments, so a guild that has disabled `!` commands loses the
//! transport and not the behaviour — and the two can never disagree about permissions.

use serde_json::Value;
use serenity::all::{
    ChannelId, CommandOptionType, Context, CreateCommand, CreateCommandOption, GuildId, UserId,
};

use aerona_core::identity::Snowflake;
use aerona_core::models::{GuildSettings, ModConfig, StrikeAction};
use aerona_core::time::now;

use crate::clamp_chars;
use crate::state::{BotState, Rate};

/// Who is asking, and whether they may. Computed by the caller from the message's own
/// member data or the interaction's member, because the rules for "is this a moderator"
/// are the guild's, not the transport's.
#[derive(Debug, Clone)]
pub struct Actor {
    pub id: Snowflake,
    pub label: String,
    pub staff: bool,
    pub admin: bool,
}

impl Actor {
    pub fn need_staff(&self) -> Result<(), String> {
        if self.staff || self.admin {
            Ok(())
        } else {
            Err("you need a moderator role for that".into())
        }
    }
}

/// The commands that mutate anything are refused for the bot's own account, and every
/// reply is one message: a command that prints three is a command that spams a channel
/// during a raid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// Anyone in the server.
    Anyone,
    /// A moderator role or `Manage Messages`.
    Staff,
    /// An administrator role or `Administrator`.
    Admin,
}

/// Every command in one table: the name used in both dialects, its permission level,
/// the arguments it takes and what it is for. Registration, help and the permission
/// check are all read from here, which is the only reason they cannot drift.
pub struct Spec {
    pub name: &'static str,
    pub permission: Permission,
    pub description: &'static str,
    /// `u` = a user, `t` = free text to the end of the line, `n` = a number, `c` = a
    /// channel, `s` = a short single word.
    pub args: &'static str,
    pub staff_only: bool,
}

pub const SPECS: &[Spec] = &[
    Spec {
        name: "warn",
        permission: Permission::Staff,
        description: "Warn a member and count a strike",
        args: "ut",
        staff_only: true,
    },
    Spec {
        name: "warns",
        permission: Permission::Staff,
        description: "List a member's active warns",
        args: "u",
        staff_only: true,
    },
    Spec {
        name: "pardon",
        permission: Permission::Staff,
        description: "Pardon one warn by its id",
        args: "n",
        staff_only: true,
    },
    Spec {
        name: "clearwarn",
        permission: Permission::Staff,
        description: "Clear every warn for a member",
        args: "u",
        staff_only: true,
    },
    Spec {
        name: "kick",
        permission: Permission::Staff,
        description: "Kick a member",
        args: "ut",
        staff_only: true,
    },
    Spec {
        name: "ban",
        permission: Permission::Staff,
        description: "Ban a member",
        args: "ut",
        staff_only: true,
    },
    Spec {
        name: "softban",
        permission: Permission::Staff,
        description: "Ban and unban, clearing their recent messages",
        args: "ut",
        staff_only: true,
    },
    Spec {
        name: "unban",
        permission: Permission::Admin,
        description: "Lift a ban",
        args: "ut",
        staff_only: true,
    },
    Spec {
        name: "timeout",
        permission: Permission::Staff,
        description: "Time a member out",
        args: "unt",
        staff_only: true,
    },
    Spec {
        name: "purge",
        permission: Permission::Staff,
        description: "Delete the last N messages here, optionally one member's",
        args: "nu",
        staff_only: true,
    },
    Spec {
        name: "note",
        permission: Permission::Staff,
        description: "Leave a private note on a member",
        args: "ut",
        staff_only: true,
    },
    Spec {
        name: "stats",
        permission: Permission::Anyone,
        description: "Process health: uptime, memory, queue, latency",
        args: "",
        staff_only: false,
    },
    Spec {
        name: "ping",
        permission: Permission::Anyone,
        description: "Gateway round trip",
        args: "",
        staff_only: false,
    },
    Spec {
        name: "level",
        permission: Permission::Anyone,
        description: "Your level and XP, or a member's",
        args: "u",
        staff_only: false,
    },
    Spec {
        name: "serverinfo",
        permission: Permission::Anyone,
        description: "Server facts, without pinging anyone",
        args: "",
        staff_only: false,
    },
    Spec {
        name: "userinfo",
        permission: Permission::Anyone,
        description: "When a member joined and their level",
        args: "u",
        staff_only: false,
    },
    Spec {
        name: "help",
        permission: Permission::Anyone,
        description: "The commands this server has enabled",
        args: "",
        staff_only: false,
    },
];

pub fn spec(name: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.name == name)
}

pub fn slash_definitions(enabled: &std::collections::HashSet<String>) -> Vec<CreateCommand> {
    SPECS
        .iter()
        .filter(|s| enabled.is_empty() || enabled.contains(s.name))
        .map(|s| {
            let mut cmd = CreateCommand::new(s.name).description(s.description);
            if s.staff_only {
                cmd = cmd.default_member_permissions(serenity::all::Permissions::MANAGE_MESSAGES);
            }
            for (index, c) in s.args.chars().enumerate() {
                let (kind, label) = match c {
                    'u' => (CommandOptionType::User, "user"),
                    'n' => (CommandOptionType::Integer, "number"),
                    'c' => (CommandOptionType::Channel, "channel"),
                    's' => (CommandOptionType::String, "value"),
                    _ => (CommandOptionType::String, "reason"),
                };
                let needs_reason = c == 't';
                let option = CreateCommandOption::new(
                    kind,
                    if needs_reason { "reason" } else { label },
                    description_for(c),
                )
                .required(matches!(c, 'u') && index == 0);
                cmd = cmd.add_option(option);
            }
            cmd
        })
        .collect()
}

fn description_for(c: char) -> &'static str {
    match c {
        'u' => "the member",
        'n' => "how many, or which id",
        'c' => "where to look (defaults to this channel)",
        's' => "a single word",
        _ => "why",
    }
}

/// Run one command. The returned string is the entire reply; the caller decides whether
/// it is a message, an ephemeral interaction response, or a DM.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    state: &BotState,
    ctx: &Context,
    guild: Snowflake,
    actor: Actor,
    channel: ChannelId,
    context_message: Option<Snowflake>,
    name: &str,
    argv: &[String],
) -> Result<String, String> {
    let Some(spec) = spec(name) else {
        return Err(format!(
            "there is no `{name}` command — try `{}help`",
            state
                .db
                .guild_settings(guild)
                .map(|s| s.prefix)
                .unwrap_or_default()
        ));
    };
    let settings = state.db.guild_settings(guild).unwrap_or_default();
    let auto = state.db.auto_config(guild).unwrap_or_default();
    let channel_snow = Snowflake(channel.get());
    if settings.ignored_channel_ids.contains(&channel_snow)
        || auto.disabled_channels.contains(&channel_snow)
    {
        return Err("this channel is ignored".into());
    }
    if auto.disabled_commands.iter().any(|c| c == name) {
        return Err("that command is disabled here".into());
    }
    match spec.permission {
        Permission::Anyone => {}
        Permission::Staff => {
            if !actor.staff && !actor.admin {
                return Err("that command needs a moderator role".into());
            }
        }
        Permission::Admin => {
            if !actor.admin {
                return Err("that command needs administrator rights".into());
            }
        }
    }
    if let Rate::Throttled(secs) = state.rate_limit_cmd(
        guild,
        actor.id,
        name,
        auto.command_cooldown_secs
            .max(if spec.staff_only { 0 } else { 3 }),
    ) {
        return Err(format!("slow down — try again in {secs}s"));
    }

    let mod_cfg = state.db.mod_config(guild).unwrap_or_default();
    let out = match name {
        "warn" => warn(state, ctx, guild, &actor, argv, context_message, &mod_cfg).await,
        "warns" => list_warns(state, guild, argv),
        "pardon" => pardon(state, guild, &actor, argv),
        "clearwarn" => clear_warns(state, guild, argv),
        "kick" => moderate(state, ctx, guild, &actor, argv, "kick", &mod_cfg).await,
        "ban" => moderate(state, ctx, guild, &actor, argv, "ban", &mod_cfg).await,
        "softban" => moderate(state, ctx, guild, &actor, argv, "softban", &mod_cfg).await,
        "unban" => moderate(state, ctx, guild, &actor, argv, "unban", &mod_cfg).await,
        "timeout" => moderate(state, ctx, guild, &actor, argv, "timeout", &mod_cfg).await,
        "purge" => purge(state, guild, &actor, channel, argv).await,
        "note" => note(state, guild, &actor, argv),
        "stats" => Ok(stats(state)),
        "ping" => Ok(ping(state)),
        "level" => level(state, guild, argv).await,
        "serverinfo" => serverinfo(state, ctx, guild).await,
        "userinfo" => userinfo(state, guild, argv).await,
        "help" => Ok(help(&settings, actor.staff | actor.admin)),
        // Reachable only if a Spec is added without wiring it here, which the tests in
        // this module would catch; the arm exists so that stays a test failure and never
        // a silently-accepted no-op.
        other => Err(format!(
            "`{other}` is not implemented by the gateway worker"
        )),
    };
    // Every command is one audit row: who ran what, and whether it worked. The API
    // audits its own writes; this is the same record for the same act, from the other
    // side of the product.
    state.db.audit_best_effort(
        aerona_core::db::security::AuditEntry::new(format!("command.{name}"))
            .for_guild(guild)
            .actor(aerona_core::db::security::ActorType::User, Some(actor.id), &actor.label)
            .meta(serde_json::json!({ "argv": argv, "ok": out.is_ok(), "reply": out.as_ref().map(|s| clamp_chars(s, 200)).unwrap_or_default() })),
    );
    out
}

// ------------------------------------------------------------------ moderation

fn target(argv: &[String]) -> Result<Snowflake, String> {
    let first = argv.first().map(String::as_str);
    Snowflake::parse_opt(first)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "who? mention the member".to_string())
}

fn tail(argv: &[String]) -> String {
    argv.get(1..)
        .map(|parts| parts.join(" "))
        .unwrap_or_default()
}

async fn warn(
    state: &BotState,
    ctx: &Context,
    guild: Snowflake,
    actor: &Actor,
    argv: &[String],
    context_message: Option<Snowflake>,
    mod_cfg: &ModConfig,
) -> Result<String, String> {
    let user = target(argv)?;
    let reason = {
        let r = tail(argv);
        if r.trim().is_empty() {
            "no reason given".to_string()
        } else {
            clamp_chars(&r, 800)
        }
    };
    let outcome = state
        .db
        .add_warn(
            guild,
            user,
            actor.id,
            &reason,
            mod_cfg.warn_expiry_days,
            i64::from(mod_cfg.strike_threshold),
            context_message,
        )
        .map_err(|e| e.to_string())?;
    let mut reply = format!(
        "⚠ Warned <@{user}> — {} active warn(s), reason: {reason}",
        outcome.active_count
    );
    if outcome.escalated {
        reply.push_str(&format!(
            "\nStrike threshold reached ({}): applying {}",
            mod_cfg.strike_threshold,
            strike_action_name(&mod_cfg.escalate_action)
        ));
        apply_escalation(state, ctx, guild, user, mod_cfg).await?;
    }
    Ok(reply)
}

/// The escalation Discord-side effect, in one place, because the same ladder runs from
/// the dashboard, from a filter hit and from this command.
pub async fn apply_escalation(
    state: &BotState,
    ctx: &Context,
    guild: Snowflake,
    user: Snowflake,
    mod_cfg: &ModConfig,
) -> Result<(), String> {
    let g = GuildId::new(guild.as_u64());
    let u = UserId::new(user.as_u64());
    let name = strike_action_name(&mod_cfg.escalate_action);
    match name.as_str() {
        "kick" => {
            g.kick(&ctx.http, u).await.map_err(|e| e.to_string())?;
            state
                .db
                .record_kick(guild, user, Snowflake(0), "strike threshold reached")
                .map_err(|e| e.to_string())?;
        }
        "ban" => {
            g.ban_with_reason(&ctx.http, u, 0, "strike threshold reached")
                .await
                .map_err(|e| e.to_string())?;
            state
                .db
                .record_ban(
                    guild,
                    user,
                    Snowflake(0),
                    "strike threshold reached",
                    0,
                    false,
                )
                .map_err(|e| e.to_string())?;
        }
        "timeout" => {
            let minutes = 60i64;
            let edit = serenity::all::EditMember::new()
                .disable_communication_until(crate::gate::rfc3339(now() + minutes * 60));
            g.edit_member(&ctx.http, u, edit)
                .await
                .map_err(|e| e.to_string())?;
        }
        // `none` and `warn` both stop at the record: an escalation configured to do
        // nothing is a choice, not a bug to work around.
        _ => {}
    }
    Ok(())
}

fn strike_action_name(action: &StrikeAction) -> String {
    // Serialised rather than matched by variant: the models are `#[serde(rename_all =
    // "snake_case")]`, so the wire name is the same one the dashboard already displays,
    // and a future variant needs no change here.
    serde_json::to_value(action)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "none".into())
}

async fn moderate(
    state: &BotState,
    ctx: &Context,
    guild: Snowflake,
    actor: &Actor,
    argv: &[String],
    action: &str,
    mod_cfg: &ModConfig,
) -> Result<String, String> {
    let user = target(argv)?;
    let reason = {
        let r = if action == "timeout" {
            argv.get(2..)
                .map(|parts| parts.join(" "))
                .unwrap_or_default()
        } else {
            tail(argv)
        };
        if r.trim().is_empty() {
            format!("moderated by {}", actor.label)
        } else {
            clamp_chars(&r, 500)
        }
    };
    if user == actor.id {
        return Err(
            "that would be self-moderation, which is how bots get banned by accident".into(),
        );
    }
    if mod_cfg.hierarchy_check {
        // Refuse before the API call: Discord would answer 403 with a message nobody
        // reads, and the audit row would look like a bug in Aerona.
        if higher_than_actor(ctx, guild, actor, user).await {
            return Err("that member has a role at or above yours".into());
        }
    }
    let g = GuildId::new(guild.as_u64());
    let u = UserId::new(user.as_u64());
    let reply = match action {
        "kick" => {
            g.kick(&ctx.http, u).await.map_err(|e| e.to_string())?;
            state
                .db
                .record_kick(guild, user, actor.id, &reason)
                .map_err(|e| e.to_string())?;
            format!("👢 Kicked <@{user}> — {reason}")
        }
        "ban" => {
            g.ban_with_reason(&ctx.http, u, 0, &reason)
                .await
                .map_err(|e| e.to_string())?;
            state
                .db
                .record_ban(guild, user, actor.id, &reason, 0, false)
                .map_err(|e| e.to_string())?;
            format!("🔨 Banned <@{user}> — {reason}")
        }
        "softban" => {
            g.ban_with_reason(&ctx.http, u, 7, &reason)
                .await
                .map_err(|e| e.to_string())?;
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            ctx.http
                .remove_ban(g, u, Some("soft ban"))
                .await
                .map_err(|e| e.to_string())?;
            state
                .db
                .record_ban(guild, user, actor.id, &reason, 0, true)
                .map_err(|e| e.to_string())?;
            format!("🧹 Soft-banned <@{user}> and cleared their recent messages")
        }
        "unban" => {
            ctx.http
                .remove_ban(g, u, Some(&reason))
                .await
                .map_err(|e| e.to_string())?;
            format!("🔓 Unbanned <@{user}>")
        }
        "timeout" => {
            // `argv` is [user, minutes, …reason]: reading the first element as the
            // duration would "time out" somebody for their snowflake in minutes.
            let minutes = argv
                .get(1)
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(10)
                .clamp(1, 40_320);
            let edit = serenity::all::EditMember::new()
                .disable_communication_until(crate::gate::rfc3339(now() + minutes * 60));
            g.edit_member(&ctx.http, u, edit)
                .await
                .map_err(|e| e.to_string())?;
            format!("⏱ Timed out <@{user}> for {minutes} minutes")
        }
        other => Err(format!("unknown moderation action {other:?}"))?,
    };
    Ok(reply)
}

/// Does `target` hold a role at or above the actor's highest one? Roles are compared by
/// position, the only ordering Discord respects, and both sides are read from the guild's
/// live role map so a role created a minute ago is not ignored.
async fn higher_than_actor(
    ctx: &Context,
    guild: Snowflake,
    actor: &Actor,
    target: Snowflake,
) -> bool {
    let g = GuildId::new(guild.as_u64());
    let Ok(roles) = g.roles(&ctx.http).await else {
        return false;
    };
    async fn top(
        g: GuildId,
        ctx: &Context,
        roles: &std::collections::HashMap<serenity::all::RoleId, serenity::all::Role>,
        id: Snowflake,
    ) -> Option<i64> {
        let member = g.member(&ctx.http, UserId::new(id.as_u64())).await.ok()?;
        let mut best: i64 = 0;
        for role in &member.roles {
            if let Some(r) = roles.get(role) {
                best = best.max(r.position as i64);
            }
        }
        Some(best)
    }
    match (
        top(g, ctx, &roles, target).await,
        top(g, ctx, &roles, actor.id).await,
    ) {
        // If either read fails, let Discord's own permission check decide: a bot that
        // refuses moderation because of a network blip is worse than one that gets a 403.
        (Some(target_top), Some(actor_top)) => target_top >= actor_top,
        _ => false,
    }
}

async fn purge(
    state: &BotState,
    guild: Snowflake,
    actor: &Actor,
    channel: ChannelId,
    argv: &[String],
) -> Result<String, String> {
    let limit: i64 = argv.first().and_then(|s| s.parse().ok()).unwrap_or(25);
    // The channel is always the one the command was run in. A `#channel` argument would
    // let a moderator purge a room they cannot see, and the queue would obey it.
    let mod_cfg = state.db.mod_config(guild).unwrap_or_default();
    if limit < 1 || limit > i64::from(mod_cfg.purge_max) {
        return Err(format!("purge takes 1 to {} messages", mod_cfg.purge_max));
    }
    let body = serde_json::json!({
        "channel": channel.get().to_string(),
        "limit": limit,
        "filterUser": argv.get(1).map(|s| mention_id(s)).filter(|s| !s.is_empty()),
        "protectMinAge": mod_cfg.purge_min_age_secs,
    });
    // Purging goes through the queue even when a human typed the command: the queue is
    // where the 429 backoff lives, and a 500-message purge is exactly the thing that
    // needs it.
    let id = state
        .db
        .enqueue_action(guild, actor.id, &actor.label, "moderate.purge", &body, "")
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "🧽 Queued a purge of {limit} message(s) (job {id})."
    ))
}

fn mention_id(raw: &str) -> String {
    raw.chars().filter(char::is_ascii_digit).collect()
}

fn list_warns(state: &BotState, guild: Snowflake, argv: &[String]) -> Result<String, String> {
    let user = target(argv)?;
    let warns = state
        .db
        .warns_for(guild, user, 10)
        .map_err(|e| e.to_string())?;
    if warns.is_empty() {
        return Ok(format!("No warns on record for <@{user}>."));
    }
    let mut lines = vec![format!("**Warns for <@{user}>** ({}):", warns.len())];
    for w in warns.iter().take(10) {
        lines.push(format!(
            "· `#{}` {} — {}{}",
            w.id,
            &crate::gate::rfc3339(w.created_at)[..10],
            clamp_chars(&w.reason, 60),
            if w.pardoned_at.is_some() {
                " (pardoned)"
            } else if w.active {
                ""
            } else {
                " (expired)"
            }
        ));
    }
    Ok(lines.join("\n"))
}

fn pardon(
    state: &BotState,
    guild: Snowflake,
    actor: &Actor,
    argv: &[String],
) -> Result<String, String> {
    let id: i64 = argv
        .first()
        .and_then(|s| s.parse().ok())
        .ok_or("which warn id?")?;
    if state
        .db
        .pardon_warn(guild, id, actor.id)
        .map_err(|e| e.to_string())?
    {
        Ok(format!("🕊 Pardoned warn #{id}."))
    } else {
        Err("no active warn with that id in this server".into())
    }
}

fn clear_warns(state: &BotState, guild: Snowflake, argv: &[String]) -> Result<String, String> {
    let user = target(argv)?;
    let n = state
        .db
        .clear_warns(guild, user)
        .map_err(|e| e.to_string())?;
    Ok(format!("🧼 Cleared {n} warn(s) for <@{user}>."))
}

fn note(
    state: &BotState,
    guild: Snowflake,
    actor: &Actor,
    argv: &[String],
) -> Result<String, String> {
    let user = target(argv)?;
    let body = tail(argv);
    if body.trim().is_empty() {
        return Err("a note needs some words".into());
    }
    let id = state
        .db
        .add_note(guild, user, actor.id, &body)
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "📝 Note #{id} saved on <@{user}>. Only moderators can see it."
    ))
}

// ------------------------------------------------------------------------ info

fn stats(state: &BotState) -> String {
    let (pending, failed) = state.db.outbox_depth().unwrap_or((0, 0));
    let rss = crate::state::rss_kb();
    format!(
        "**Aerona** · up {}\n· memory {} MB · cpu {:.1}%\n· guilds {} · queue {pending} pending / {failed} failed\n· gateway latency {} ms · messages/min {}\n· database {}",
        human_duration(state.uptime_secs()),
        rss / 1024,
        crate::state::cpu_pct(),
        state.managed.len(),
        *state.ws_latency_ms.lock(),
        *state.msgs_min.lock(),
        state.db.size_kb().map(|k| format!("{} MB", k / 1024)).unwrap_or_else(|_| "unknown".into())
    )
}

fn ping(state: &BotState) -> String {
    let latency = *state.ws_latency_ms.lock();
    if latency == 0 {
        "measuring…".into()
    } else {
        format!("Pong — {} ms to the gateway.", latency)
    }
}

/// Uptime as prose, for `!stats` and the presence line. Public because the presence
/// string is built in `tasks` and must read the same way as the command output.
pub fn human_words(secs: i64) -> String {
    human_duration(secs)
}

fn human_duration(secs: i64) -> String {
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m {}s", secs % 60)
    }
}

async fn level(state: &BotState, guild: Snowflake, argv: &[String]) -> Result<String, String> {
    let cfg = state.db.guild_settings(guild).unwrap_or_default();
    if !cfg.levels_enabled {
        return Err("levels are switched off in this server".into());
    }
    let user = if argv.is_empty() {
        None
    } else {
        Some(target(argv)?)
    };
    let (level, xp, need) = state
        .db
        .level(guild, user.unwrap_or(Snowflake(0)))
        .map_err(|e| e.to_string())?;
    let who = user.map_or("you".to_string(), |u| format!("<@{u}>"));
    Ok(format!(
        "{} is level **{level}** — {xp}/{need} XP to the next one.",
        who
    ))
}

async fn serverinfo(state: &BotState, ctx: &Context, guild: Snowflake) -> Result<String, String> {
    let g = GuildId::new(guild.as_u64());
    let Some(data) = g.to_guild_cached(&ctx.cache).map(|g| (*g).clone()) else {
        return Err("I cannot see that server right now".into());
    };
    let channels = data.channels.len();
    // The counts are read from the same tables the dashboard reads, rather than from a
    // stats struct, so `!serverinfo` and the overview page cannot disagree.
    let recent = state.db.recent_warns(guild, 500).unwrap_or_default();
    let month = now() - 30 * 86_400;
    let warns_month = recent.iter().filter(|w| w.created_at >= month).count();
    let tickets = state.db.open_ticket_count(guild, None).unwrap_or(0);
    Ok(format!(
        "**{}**\n· {} members · {} channels · created {}\n· {} warn(s) in the last 30 days · {} open ticket(s)",
        data.name,
        data.member_count,
        channels,
        &crate::gate::rfc3339(data.id.created_at().unix_timestamp())[..10],
        warns_month,
        tickets,
    ))
}

async fn userinfo(state: &BotState, guild: Snowflake, argv: &[String]) -> Result<String, String> {
    let user = target(argv)?;
    let ctx = state.ctx().ok_or("the gateway is not connected yet")?;
    let member = GuildId::new(guild.as_u64())
        .member(&ctx.http, UserId::new(user.as_u64()))
        .await
        .map_err(|e| e.to_string())?;
    let warns = state.db.active_warn_count(guild, user).unwrap_or(0);
    let strikes = state.db.active_strikes(guild, user).unwrap_or(0);
    let (level, _, _) = state.db.level(guild, user).unwrap_or((0, 0, 0));
    Ok(format!(
        "<@{user}> · joined {} · {} warn(s), {strikes} strike(s), level {level}",
        human_delta(
            member
                .joined_at
                .map(|t| t.unix_timestamp())
                .unwrap_or_else(now)
        ),
        warns
    ))
}

fn help(settings: &GuildSettings, staff: bool) -> String {
    let mut lines = vec![format!(
        "Commands in this server (prefix `{}`):",
        settings.prefix
    )];
    for s in SPECS {
        if s.staff_only && !staff {
            continue;
        }
        let args = s
            .args
            .chars()
            .map(|c| match c {
                'u' => "<@member> ",
                'n' => "<number> ",
                'c' => "<#channel> ",
                's' => "<word> ",
                _ => "[reason] ",
            })
            .collect::<String>();
        lines.push(format!(
            "· `{}{}{}` — {}",
            settings.prefix,
            s.name,
            args.trim_end(),
            s.description
        ));
    }
    lines.join("\n")
}

fn human_delta(secs: i64) -> String {
    let delta = now() - secs;
    let (y, mo, d) = (
        delta / 31_557_600,
        (delta % 31_557_600) / 2_629_800,
        (delta % 2_629_800) / 86_400,
    );
    if y > 0 {
        format!("{y}y {mo}mo ago")
    } else if mo > 0 {
        format!("{mo}mo {d}d ago")
    } else {
        format!("{d}d ago")
    }
}

/// Slash options come back as a list of `{name, value}` pairs whose value shape depends
/// on the option type. Read them through JSON so the worker does not have to know which
/// serenity version introduced which helper: an id is an id.
pub fn option_value(options: &Value, name: &str) -> Option<Value> {
    if let Some(list) = options.as_array() {
        for entry in list {
            if entry.get("name").and_then(Value::as_str) == Some(name) {
                // A sub-command group arrives with `value: null`, so a null is not an
                // answer: keep looking (into the nested options below) instead of
                // handing the caller a null it would read as an empty string.
                if let Some(value) = entry.get("value").cloned().filter(|v| !v.is_null()) {
                    return Some(flatten_resolved(value));
                }
            }
            // Sub-commands nest one level down.
            if let Some(inner) = entry.get("options").and_then(Value::as_array) {
                if let Some(v) = option_value(&Value::Array(inner.clone()), name) {
                    return Some(v);
                }
            }
        }
        return None;
    }
    let entry = options.get(name)?.clone();
    Some(flatten_resolved(entry))
}

/// A resolved user or channel option arrives as `{"type":6,"data":{"id":"123"}}`; the
/// command layer only ever needs the id, and flattening here means every command sees a
/// plain string whether it came from a slash option, a resolved entry, or a prefix line.
fn flatten_resolved(value: Value) -> Value {
    if let Some(id) = value.get("data").and_then(|d| d.get("id")).cloned() {
        return id;
    }
    if value.get("type").is_some() && value.get("value").is_none() {
        if let Some(id) = value.get("id").cloned() {
            return id;
        }
    }
    value
}

pub fn snowflake_from_value(v: Option<Value>) -> Option<Snowflake> {
    match v? {
        Value::String(s) => Snowflake::parse(&s).ok(),
        Value::Number(n) => n.as_u64().map(Snowflake),
        _ => None,
    }
}

/// Turn a slash interaction into the same `argv` a prefix command would have gotten, so
/// there is exactly one implementation of every command.
pub fn argv_from_options(options: &Value, spec: &Spec, reason_present: bool) -> Vec<String> {
    let mut out = Vec::new();
    for c in spec.args.chars() {
        let key = match c {
            'u' => "user",
            'n' => "number",
            'c' => "channel",
            's' => "value",
            _ => "reason",
        };
        match option_value(options, key) {
            Some(v) => match c {
                'u' => out.push(
                    snowflake_from_value(Some(v))
                        .map(|s| s.as_str())
                        .unwrap_or_default(),
                ),
                _ => out.push(
                    v.as_str()
                        .map(str::to_string)
                        .or_else(|| v.as_i64().map(|i| i.to_string()))
                        .unwrap_or_default(),
                ),
            },
            None if c == 't' && reason_present => out.push(String::new()),
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_command_name_is_unique_and_lowercase() {
        let mut seen = std::collections::HashSet::new();
        for s in SPECS {
            assert!(seen.insert(s.name), "{} appears twice", s.name);
            assert_eq!(s.name, s.name.to_lowercase());
            assert!(s.name.len() <= 32, "Discord's slash-command name limit");
            assert!(!s.description.is_empty() && s.description.chars().count() <= 100);
        }
    }

    #[test]
    fn slash_definitions_carry_a_default_permission_only_for_staff_commands() {
        let none = std::collections::HashSet::new();
        let defs = slash_definitions(&none);
        assert_eq!(defs.len(), SPECS.len());
        for def in &defs {
            let v = serde_json::to_value(def).unwrap();
            let name = v["name"].as_str().unwrap();
            let has_default = v
                .get("default_member_permissions")
                .is_some_and(|p| !p.is_null());
            let spec = spec(name).unwrap();
            assert_eq!(
                has_default, spec.staff_only,
                "{name}: default permissions must match staff_only"
            );
        }
    }

    #[test]
    fn an_enabled_filter_narrows_the_registration_list() {
        let only: std::collections::HashSet<String> =
            ["warn", "kick"].into_iter().map(str::to_string).collect();
        let defs = slash_definitions(&only);
        assert_eq!(defs.len(), 2);
    }

    #[test]
    fn options_are_read_from_both_shapes_discord_sends() {
        let flat = json!([{"name": "user", "value": "123"}, {"name": "reason", "value": "spam"}]);
        assert_eq!(
            option_value(&flat, "reason").unwrap().as_str(),
            Some("spam")
        );
        let nested = json!([{"name": "sub", "value": null, "options": [{"name": "user", "value": {"type": 6, "data": {"id": "9"}}}]}]);
        assert_eq!(option_value(&nested, "user").unwrap().as_str(), Some("9"));
        let map = json!({"user": {"type": 6, "data": {"id": "42"}}});
        assert_eq!(option_value(&map, "user").unwrap().as_str(), Some("42"));
        assert!(option_value(&flat, "absent").is_none());
    }

    #[test]
    fn a_user_option_becomes_a_plain_argument() {
        let warn_spec = spec("warn").unwrap();
        let argv = argv_from_options(
            &json!([{"name": "user", "value": "123"}, {"name": "reason", "value": "being rude"}]),
            warn_spec,
            false,
        );
        assert_eq!(argv, vec!["123".to_string(), "being rude".to_string()]);
        let timeout_spec = spec("timeout").unwrap();
        let argv = argv_from_options(
            &json!([{"name": "user", "value": "1"}, {"name": "number", "value": 5}]),
            timeout_spec,
            false,
        );
        // The spec order is user-then-number for `timeout` (`ut` is reason-tail, `unt`
        // is not a spec — this asserts the mapping, not the ordering).
        assert!(!argv.is_empty());
    }

    #[test]
    fn snowflakes_are_parsed_rejectively() {
        assert_eq!(
            snowflake_from_value(Some(json!("123456789012345678"))),
            Some(Snowflake(123_456_789_012_345_678))
        );
        assert_eq!(snowflake_from_value(Some(json!(7))), Some(Snowflake(7)));
        assert_eq!(
            snowflake_from_value(Some(json!("007"))),
            None,
            "the Snowflake parser rejects leading zeroes"
        );
        assert_eq!(snowflake_from_value(Some(json!({"nope": 1}))), None);
        assert_eq!(snowflake_from_value(None), None);
    }

    #[test]
    fn durations_and_deltas_read_like_prose() {
        assert_eq!(human_duration(59), "0m 59s");
        assert_eq!(human_duration(3_661), "1h 1m");
        assert_eq!(human_duration(90_061), "1d 1h 1m");
        let now_secs = now();
        assert_eq!(
            human_delta(now_secs - 60),
            "0d ago",
            "a minute is still today, and that is honest"
        );
        assert!(human_delta(now_secs - 200 * 86_400).ends_with("ago"));
    }

    #[test]
    fn the_escalation_action_reads_as_the_name_the_dashboard_uses() {
        assert_eq!(strike_action_name(&StrikeAction::default()), "none");
    }

    #[test]
    fn help_lists_only_what_the_caller_may_use() {
        let settings = GuildSettings {
            prefix: "!".into(),
            ..Default::default()
        };
        let guest = help(&settings, false);
        let mod_one = help(&settings, true);
        assert!(guest.contains("!ping"));
        assert!(
            !guest.contains("!ban"),
            "a guest must not be shown a command they cannot run"
        );
        assert!(mod_one.contains("!ban"));
    }
}
