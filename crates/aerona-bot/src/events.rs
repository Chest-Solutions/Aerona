//! Gateway events: the only place in Aerona where Discord talks to us.
//!
//! Three rules govern every handler here, and they are what separates a bot that
//! survives a raid from one that becomes part of it:
//!
//! 1. cheap checks first, database second — an unmanaged guild, the bot's own message or
//!    a 4 000-character paste must cost nothing before it costs a query;
//! 2. a handler never returns an error to serenity and never lets a panic escape: the
//!    worst outcome is a logged warning and one dropped event, not a dead gateway for
//!    every server the bot is in;
//! 3. anything that mutates Discord carries an audit-log reason, so an operator can read
//!    in the audit trail what changed and why.
//!
//! Each event is handled on a spawned task. That is not decoration: `Message::reply` on
//! a slow connection is a hundred milliseconds, and a handler that runs inline on the
//! dispatch loop turns a hundred messages a second into a hundred-fold slowdown of the
//! whole shard.

use std::sync::Arc;

use serenity::all::{
    ButtonStyle, ChannelId, ChannelType, Color, CommandDataResolved, ComponentInteraction, Context,
    CreateActionRow, CreateAllowedMentions, CreateButton, CreateChannel, CreateEmbed,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, EditChannel,
    EditMessage, EventHandler, Guild, GuildId, Interaction, InteractionResponseFlags, Member,
    Message, MessageId, MessageUpdateEvent, PermissionOverwrite, PermissionOverwriteType,
    Permissions, Ready, RoleId, UnavailableGuild, UserId,
};

use aerona_core::identity::Snowflake;
use aerona_core::models::{AutoConfig, GuildSettings, RaidConfig, TicketConfig, TicketStatus};
use aerona_core::time::now;

use crate::commands::{self, Actor};
use crate::gate;
use crate::state::{BotState, Rate};
use crate::{clamp_chars, tasks};

pub struct Aerona {
    pub state: Arc<BotState>,
}

impl Aerona {
    pub fn new(state: Arc<BotState>) -> Self {
        Aerona { state }
    }
}

/// Guild ids become manageable the moment the gateway says they exist; the *names* and
/// member counts arrive with the `guild_create` events serenity fires immediately
/// after, which is also the only moment they are guaranteed fresh.
fn note_guild(state: &BotState, guild: GuildId) {
    let id = Snowflake(guild.get());
    state.managed.insert(id);
}

#[serenity::async_trait]
impl EventHandler for Aerona {
    async fn ready(&self, ctx: Context, ready: Ready) {
        self.state.attach(&ctx);
        let present: Vec<Snowflake> = ready.guilds.iter().map(|g| Snowflake(g.id.get())).collect();
        for id in &present {
            state_note(&self.state, *id);
        }
        // A guild in the database that the gateway does not report is one the bot has
        // been kicked from. Without this the dashboard would say "connected" forever.
        if let Ok(rows) = self.state.db.guilds() {
            for row in rows {
                if !present.contains(&row.id) {
                    if let Err(e) = self.state.db.mark_guild_absent(row.id) {
                        tracing::warn!(guild = %row.id, error = %e, "could not record that the bot is gone");
                    }
                    self.state.managed.remove(&row.id);
                    self.state.forget_guild_windows(row.id);
                }
            }
        }
        let _ = self.state.db.prune_bot_status(&present);
        tracing::info!(user = %ready.user.name, guilds = ready.guilds.len(), "gateway ready");
        // Drain the queue immediately: rows that accumulated while the bot was down
        // should start moving before anything else happens.
        crate::outbox::drain(&self.state).await;
        tasks::reregister_commands(&self.state, &ctx).await;
    }

    async fn shards_ready(&self, ctx: Context, total_shards: u32) {
        *self.state.shards_total.lock() = total_shards;
        *self.state.shards_ready.lock() = total_shards;
        let _ = ctx;
    }

    async fn guild_create(&self, ctx: Context, guild: Guild, _is_new: Option<bool>) {
        let _ = &ctx;
        let id = Snowflake(guild.id.get());
        if let Err(e) = self.state.db.sync_guild(
            id,
            &guild.name,
            guild.icon.as_ref().map(|i| i.to_string()).as_deref(),
            guild.member_count as i64,
        ) {
            // A failed sync is not a reason to stop serving the guild: the next
            // `guild_create` or the periodic status pass will write it again.
            tracing::warn!(guild = %id, error = %e, "guild sync failed");
        }
        self.state.managed.insert(id);
        note_guild(&self.state, guild.id);
        tracing::info!(guild = %id, name = %guild.name, "guild available");
    }

    async fn guild_delete(&self, ctx: Context, incomplete: UnavailableGuild, _full: Option<Guild>) {
        let _ = &ctx;
        let id = Snowflake(incomplete.id.get());
        let _ = self.state.db.mark_guild_absent(id);
        self.state.managed.remove(&id);
        self.state.forget_guild_windows(id);
        tracing::info!(guild = %id, "guild left or unavailable");
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let state = Arc::clone(&self.state);
        // `JoinError` means the task panicked; a panic in an interaction handler must not
        // reach serenity's dispatch loop, so it is caught here by construction.
        if tokio::spawn(async move { handle_interaction(&state, &ctx, interaction).await })
            .await
            .is_err()
        {
            tracing::error!("an interaction handler panicked; the gateway is still running");
        }
    }

    async fn message(&self, ctx: Context, msg: Message) {
        let state = Arc::clone(&self.state);
        let _ = tokio::spawn(async move { handle_message(&state, &ctx, &msg).await }).await;
    }

    async fn message_update(
        &self,
        ctx: Context,
        _old: Option<Message>,
        _new: Option<Message>,
        event: MessageUpdateEvent,
    ) {
        let state = Arc::clone(&self.state);
        let _ = tokio::spawn(async move { handle_edit(&state, &ctx, event).await }).await;
    }

    async fn message_delete(
        &self,
        ctx: Context,
        channel_id: ChannelId,
        deleted: MessageId,
        guild_id: Option<GuildId>,
    ) {
        let state = Arc::clone(&self.state);
        let _ = tokio::spawn(async move {
            handle_delete(&state, &ctx, guild_id, channel_id, deleted).await
        })
        .await;
    }

    async fn guild_member_addition(&self, ctx: Context, member: Member) {
        let state = Arc::clone(&self.state);
        let _ = tokio::spawn(async move { handle_join(&state, &ctx, &member).await }).await;
    }

    async fn guild_member_removal(
        &self,
        ctx: Context,
        guild_id: GuildId,
        user: serenity::all::User,
        _member_data: Option<Member>,
    ) {
        let state = Arc::clone(&self.state);
        let _ =
            tokio::spawn(async move { handle_leave(&state, &ctx, guild_id, &user).await }).await;
    }
}

fn state_note(state: &BotState, id: Snowflake) {
    state.managed.insert(id);
}

// ----------------------------------------------------------------- interactions

async fn handle_interaction(state: &BotState, ctx: &Context, interaction: Interaction) {
    match interaction {
        Interaction::Command(cmd) => {
            let Some(guild_id) = cmd.data.guild_id.or(cmd.guild_id) else {
                reply(
                    ctx,
                    &cmd.id,
                    &cmd.token,
                    "Aerona's commands run in a server.",
                )
                .await;
                return;
            };
            let guild = Snowflake(guild_id.get());
            if !state.managed.contains(&guild) {
                reply(
                    ctx,
                    &cmd.id,
                    &cmd.token,
                    "this server is not set up for Aerona",
                )
                .await;
                return;
            }
            let member = cmd.member.as_deref();
            let actor = Actor {
                id: Snowflake(cmd.user.id.get()),
                label: cmd.user.name.clone(),
                staff: cmd
                    .app_permissions
                    .is_some_and(|p| p.contains(Permissions::MANAGE_MESSAGES))
                    || staff_from_member(state, guild, member).await,
                admin: cmd
                    .app_permissions
                    .is_some_and(|p| p.contains(Permissions::ADMINISTRATOR)),
            };
            let options = serde_json::to_value(&cmd.data.options).unwrap_or_default();
            let Some(spec) = commands::spec(&cmd.data.name) else {
                reply(
                    ctx,
                    &cmd.id,
                    &cmd.token,
                    "that command is not part of Aerona",
                )
                .await;
                return;
            };
            let mut argv = commands::argv_from_options(&options, spec, false);
            if argv.is_empty() {
                if let Some((id, _)) = cmd.data.resolved.users.iter().next() {
                    argv.push(id.get().to_string());
                }
            }
            let text = match commands::run(
                state,
                ctx,
                guild,
                actor,
                cmd.channel_id,
                None,
                &cmd.data.name,
                &argv,
            )
            .await
            {
                Ok(reply) => reply,
                Err(e) => format!("✗ {e}"),
            };
            reply(ctx, &cmd.id, &cmd.token, &text).await;
        }
        Interaction::Component(button) => {
            handle_button(state, ctx, button).await;
        }
        _ => {}
    }
}

async fn handle_button(state: &BotState, ctx: &Context, button: ComponentInteraction) {
    let Some(guild_id) = button.guild_id else {
        return;
    };
    let guild = Snowflake(guild_id.get());
    let channel = Snowflake(button.message.channel_id.get());
    let actor = Actor {
        id: Snowflake(button.user.id.get()),
        label: button.user.name.clone(),
        staff: staff_from_member(state, guild, button.member.as_ref()).await,
        admin: false,
    };
    let ack = || reply(ctx, &button.id, &button.token, "");
    match button.data.custom_id.as_str() {
        "aerona:ticket:open" => {
            let args = serde_json::json!({ "panelMessage": button.message.id.get().to_string() });
            match open_ticket(state, ctx, guild, &args, Some(actor.id), &actor.label).await {
                Ok(()) => {
                    let _ = ack().await;
                }
                Err(e) => {
                    reply(ctx, &button.id, &button.token, &format!("✗ {e}")).await;
                }
            }
        }
        "aerona:ticket:close" => {
            let Ok(Some(ticket)) = state.db.ticket_by_channel(guild, channel) else {
                reply(
                    ctx,
                    &button.id,
                    &button.token,
                    "this channel is not a ticket any more",
                )
                .await;
                return;
            };
            close_ticket(state, ctx, guild, ticket.id).await;
            let _ = reply(ctx, &button.id, &button.token, "closing this ticket…").await;
        }
        "aerona:ticket:claim" => {
            let Ok(Some(ticket)) = state.db.ticket_by_channel(guild, channel) else {
                reply(
                    ctx,
                    &button.id,
                    &button.token,
                    "this channel is not a ticket any more",
                )
                .await;
                return;
            };
            match state.db.claim_ticket(ticket.id, actor.id, false) {
                Ok(true) => {
                    let _ = button
                        .channel_id
                        .send_message(
                            &ctx.http,
                            CreateMessage::new()
                                .content(format!("<@{}> claimed this ticket.", actor.id.as_u64())),
                        )
                        .await;
                    let _ = reply(ctx, &button.id, &button.token, "claimed").await;
                }
                Ok(false) => {
                    let _ = reply(
                        ctx,
                        &button.id,
                        &button.token,
                        "somebody else already has this one",
                    )
                    .await;
                }
                Err(e) => {
                    let _ = reply(ctx, &button.id, &button.token, &format!("✗ {e}")).await;
                }
            }
        }
        "aerona:ticket:transcript" => {
            let Ok(Some(ticket)) = state.db.ticket_by_channel(guild, channel) else {
                reply(
                    ctx,
                    &button.id,
                    &button.token,
                    "this channel is not a ticket any more",
                )
                .await;
                return;
            };
            let rows = state
                .db
                .ticket_transcript(ticket.id, actor.staff || actor.admin)
                .unwrap_or_default();
            let text = render_transcript(&ticket, rows);
            let file = serenity::all::CreateAttachment::bytes(
                text.into_bytes(),
                format!("{}.txt", ticket.reference),
            );
            let _ = button
                .channel_id
                .send_message(
                    &ctx.http,
                    CreateMessage::new()
                        .content("Transcript of this ticket.")
                        .add_file(file),
                )
                .await;
            let _ = reply(
                ctx,
                &button.id,
                &button.token,
                "posted the transcript in this channel",
            )
            .await;
        }
        other => {
            tracing::debug!(custom_id = other, "ignored an unknown button");
        }
    }
}

/// Ephemeral, because a command reply is for the person who ran it; the channel only
/// ever sees the message the command itself chose to post.
async fn reply(ctx: &Context, id: &serenity::all::InteractionId, token: &str, text: &str) {
    let body = if text.is_empty() {
        "👍".to_string()
    } else {
        clamp_chars(text, 1900)
    };
    let message = CreateInteractionResponseMessage::new()
        .content(body)
        .flags(InteractionResponseFlags::EPHEMERAL);
    let response = CreateInteractionResponse::Message(message);
    let _ = ctx
        .http
        .create_interaction_response(*id, token, &response, vec![])
        .await;
}

/// "Is this a moderator" has one answer for the whole product: the guild's own role
/// lists, or the `Manage Messages` permission when serenity has the member object.
async fn staff_from_member(
    state: &BotState,
    guild: Snowflake,
    member: Option<&serenity::all::Member>,
) -> bool {
    let Some(member) = member else { return false };
    if member.roles.is_empty() {
        return false;
    }
    let allowed = staff_role_ids(state, guild);
    member
        .roles
        .iter()
        .any(|r| allowed.contains(&Snowflake(r.get())))
}

fn is_admin_by_role(
    state: &BotState,
    guild: Snowflake,
    roles: Option<&Vec<serenity::all::RoleId>>,
) -> bool {
    let Some(roles) = roles else { return false };
    let Ok(settings) = state.db.guild_settings(guild) else {
        return false;
    };
    roles
        .iter()
        .any(|r| settings.admin_role_ids.contains(&Snowflake(r.get())))
}

fn staff_role_ids(state: &BotState, guild: Snowflake) -> Vec<Snowflake> {
    let mut out = state
        .db
        .mod_config(guild)
        .ok()
        .and_then(|c| c.mod_role_id)
        .into_iter()
        .collect::<Vec<_>>();
    if let Ok(settings) = state.db.guild_settings(guild) {
        out.extend(settings.mod_role_ids);
        out.extend(settings.admin_role_ids);
    }
    out
}

// -------------------------------------------------------------------- messages

async fn handle_message(state: &BotState, ctx: &Context, msg: &Message) {
    state.bump(&state.events_min);
    let Some(guild_id) = msg.guild_id else { return };
    let guild = Snowflake(guild_id.get());
    if !state.managed.contains(&guild) {
        return;
    }
    if msg.author.id == ctx.cache.current_user().id || msg.author.bot {
        // A bot's message is never XP, never a command, and never filtered: two bots
        // replying to each other is the classic self-inflicted raid, and the only fix
        // that has ever worked is not reading your own output.
        return;
    }
    let author = Snowflake(msg.author.id.get());
    let channel = Snowflake(msg.channel_id.get());
    let content = msg.content.clone();

    // A ticket channel is a conversation with staff, not with the world: the filter, XP
    // and stickies do not apply, and everything said in it becomes transcript.
    if let Ok(Some(ticket)) = state.db.ticket_by_channel(guild, channel) {
        let _ =
            state
                .db
                .append_ticket_message(ticket.id, author, &msg.author.name, &content, false);
        if ticket.status == TicketStatus::Closed && author == ticket.owner_id {
            let _ = state.db.set_ticket_status(ticket.id, TicketStatus::Open);
        }
        return;
    }

    let settings = state.db.guild_settings(guild).unwrap_or_default();
    let auto = state.db.auto_config(guild).unwrap_or_default();
    let ai = state.db.ai_config(guild).unwrap_or_default();
    let roles: Vec<Snowflake> = msg
        .member
        .as_ref()
        .map(|m| m.roles.iter().map(|r| Snowflake(r.get())).collect())
        .unwrap_or_default();

    // Cache the content *before* the filter may delete the message: a deleted message
    // is exactly the one a log needs to quote, and afterwards it is gone.
    let logs = state.db.log_config(guild).unwrap_or_default();
    if logs.enabled && logs.channel_id.is_some() && logs.max_cached_messages > 0 {
        let _ =
            state
                .db
                .remember_message(guild, channel, Snowflake(msg.id.get()), author, &content);
    }

    if let Some(outcome) = gate::evaluate(
        state,
        ctx,
        &ai,
        guild,
        author,
        channel,
        Snowflake(msg.id.get()),
        &content,
        &roles,
        &settings,
    )
    .await
    {
        if outcome.deleted {
            return;
        }
    }

    let (flood_count, user_count) = state.note_message(guild, author, 10);
    if let Ok(raid) = state.db.raid_config(guild) {
        if raid.enabled && raid.detect_msg_flood {
            if flood_count >= raid.msg_threshold.max(1) as usize {
                let _ = state.db.raid_event_begin(
                    guild,
                    "message flood",
                    flood_count as i64,
                    i64::from(raid.msg_window_secs),
                );
                alert_raid(
                    state,
                    ctx,
                    guild,
                    &raid,
                    &format!(
                        "message flood: {flood_count} messages in {}s",
                        raid.msg_window_secs
                    ),
                )
                .await;
            }
            if user_count > raid.per_user_msgs_per_sec.max(1) as usize
                && state.lockdown_until.contains_key(&guild)
                && raid.auto_kick
            {
                let _ = gid(guild).kick(&ctx.http, msg.author.id).await;
                let _ = state.db.add_strike(
                    guild,
                    author,
                    "anti-raid",
                    Some("flood"),
                    "posted too fast during a lockdown",
                    1,
                );
            }
        }
    }

    // Level-up detection by comparing the two reads, rather than trusting an outcome
    // struct: the pair is the truth whatever `add_xp` decides internally.
    if settings.levels_enabled {
        let before = state.db.level(guild, author).map(|l| l.0).unwrap_or(0);
        let _ = state.db.add_xp(guild, author, 1);
        let after = state.db.level(guild, author).map(|l| l.0).unwrap_or(0);
        if after > before {
            announce_level(state, ctx, author, after).await;
        }
    }

    stickies_on_message(state, ctx, guild, channel).await;

    let prefix = if settings.prefix.is_empty() {
        "!"
    } else {
        settings.prefix.as_str()
    };
    let Some(body) = content.strip_prefix(prefix) else {
        return;
    };
    let mut parts = body.split_whitespace();
    let Some(name) = parts.next().map(str::to_lowercase) else {
        return;
    };
    if commands::spec(&name).is_none() {
        return;
    }
    if let Rate::Throttled(secs) =
        state.rate_limit_cmd(guild, author, &name, auto.command_cooldown_secs)
    {
        let _ = msg
            .reply(ctx, format!("slow down — try again in {secs}s"))
            .await;
        return;
    }
    let argv: Vec<String> = parts.map(str::to_string).collect();
    let actor = Actor {
        id: author,
        label: msg.author.name.clone(),
        // The gateway does not always send the resolved permission set with a message,
        // so the guild's own role lists decide. That is the same list the dashboard
        // writes, which is the only reason the two paths cannot disagree.
        staff: is_staff_by_role(state, guild, msg.member.as_ref().map(|m| &m.roles)),
        admin: is_admin_by_role(state, guild, msg.member.as_ref().map(|m| &m.roles)),
    };
    let text = match commands::run(
        state,
        ctx,
        guild,
        actor,
        msg.channel_id,
        Some(Snowflake(msg.id.get())),
        &name,
        &argv,
    )
    .await
    {
        Ok(reply) => reply,
        Err(e) => format!("✗ {e}"),
    };
    let _ = msg.reply(ctx, text).await;
}

fn is_staff_by_role(
    state: &BotState,
    guild: Snowflake,
    roles: Option<&Vec<serenity::all::RoleId>>,
) -> bool {
    let Some(roles) = roles else { return false };
    let allowed = staff_role_ids(state, guild);
    roles.iter().any(|r| allowed.contains(&Snowflake(r.get())))
}

async fn handle_edit(state: &BotState, ctx: &Context, event: MessageUpdateEvent) {
    let Some(guild_id) = event.guild_id else {
        return;
    };
    let guild = Snowflake(guild_id.get());
    if !state.managed.contains(&guild) {
        return;
    }
    state.bump(&state.events_min);
    let ai = state.db.ai_config(guild).unwrap_or_default();
    if !ai.enabled || !ai.apply_to_edits {
        return;
    }
    let Some(new_content) = event.content.clone() else {
        return;
    };
    let author = event
        .author
        .as_ref()
        .map(|u| Snowflake(u.id.get()))
        .unwrap_or(Snowflake(0));
    // 0.12 sends `Option<Option<Box<PartialMember>>>` here: the outer option is whether
    // the field was in the payload at all, the inner one whether the member was in the
    // guild when the edit happened. Both are "no roles", and no roles must not become an
    // empty allow-list downstream.
    let roles: Vec<Snowflake> = event
        .member
        .as_ref()
        .and_then(|m| m.as_ref())
        .map(|m| m.roles.iter().map(|r| Snowflake(r.get())).collect())
        .unwrap_or_default();
    let settings = state.db.guild_settings(guild).unwrap_or_default();
    let channel = Snowflake(event.channel_id.get());
    if let Some(outcome) = gate::evaluate(
        state,
        ctx,
        &ai,
        guild,
        author,
        channel,
        Snowflake(event.id.get()),
        &new_content,
        &roles,
        &settings,
    )
    .await
    {
        if outcome.deleted {
            tracing::info!(guild = %guild, "an edited message was removed by the filter");
        }
    }
}

async fn handle_delete(
    state: &BotState,
    ctx: &Context,
    guild_id: Option<GuildId>,
    channel: ChannelId,
    message: MessageId,
) {
    let Some(guild_id) = guild_id else { return };
    let guild = Snowflake(guild_id.get());
    if !state.managed.contains(&guild) {
        return;
    }
    let logs = state.db.log_config(guild).unwrap_or_default();
    if !logs.enabled || !logs.events.iter().any(|e| e == "message_delete") {
        return;
    }
    let Some(target) = logs.channel_id else {
        return;
    };
    // The content is only recoverable because it was cached on arrival; a deletion of a
    // message we never saw is reported as such rather than guessed at.
    let Some((text, author, at)) = state
        .db
        .recalled_message(guild, Snowflake(message.get()))
        .ok()
        .flatten()
    else {
        return;
    };
    let _ = state.db.forget_message(guild, Snowflake(message.get()));
    let body = format!(
        "🗑 Message deleted in <#{}> by <@{}> at {}\n{}",
        channel.get(),
        author.as_u64(),
        gate::rfc3339(at),
        clamp_chars(&text, 800)
    );
    let _ = ChannelId::new(target.as_u64())
        .send_message(&ctx.http, CreateMessage::new().content(body))
        .await;
}

// ------------------------------------------------------------------- membership

async fn handle_join(state: &BotState, ctx: &Context, member: &Member) {
    let guild = Snowflake(member.guild_id.get());
    if !state.managed.contains(&guild) {
        return;
    }
    state.bump(&state.events_min);
    let user = &member.user;
    let _ = state.db.upsert_user(
        Snowflake(user.id.get()),
        &user.name,
        &user
            .discriminator
            .map(|d| d.to_string())
            .unwrap_or_else(|| "0".into()),
        user.global_name.as_deref(),
        user.avatar.map(|a| a.to_string()).as_deref(),
        user.bot,
    );

    let auto = state.db.auto_config(guild).unwrap_or_default();
    let raid = state.db.raid_config(guild).unwrap_or_default();
    let count = state.note_join(guild, raid.join_window_secs.max(1));
    let author = Snowflake(user.id.get());
    let fresh = user.created_at().unix_timestamp();
    let account_age_days = (now() - fresh) / 86_400;

    if raid.enabled {
        let mut flags = Vec::new();
        if raid.account_max_age_days > 0 && account_age_days < i64::from(raid.account_max_age_days)
        {
            flags.push("new account");
        }
        if raid.require_avatar && member.avatar.is_none() && user.avatar.is_none() {
            flags.push("no avatar");
        }
        if raid.require_username_letters && !user.name.chars().any(char::is_alphabetic) {
            flags.push("username without letters");
        }
        let whitelisted = raid.whitelist_user_ids.contains(&author)
            || member
                .roles
                .iter()
                .any(|r| raid.whitelist_role_ids.contains(&Snowflake(r.get())));
        if count >= raid.join_threshold.max(1) as usize {
            let _ = state.db.raid_event_begin(
                guild,
                "join burst",
                count as i64,
                i64::from(raid.join_window_secs),
            );
            alert_raid(
                state,
                ctx,
                guild,
                &raid,
                &format!("{count} members joined in {}s", raid.join_window_secs),
            )
            .await;
        }
        if !flags.is_empty() && !whitelisted {
            let note = format!("join heuristics: {}", flags.join(", "));
            let _ = state.db.add_note(guild, author, Snowflake(0), &note);
            // Auto-kick only while a raid is active: a heuristic that is right 90 % of
            // the time is a fine alarm and a terrible expulsion when nothing is happening.
            if raid.auto_kick && state.lockdown_until.contains_key(&guild) {
                let _ = gid(guild).kick(&ctx.http, user.id).await;
                let _ = state.db.record_kick(guild, author, Snowflake(0), &note);
            }
        }
    }

    if auto.autoroles_enabled && !auto.autorole_ids.is_empty() {
        let delay = u64::from(auto.autorole_delay_mins) * 60;
        let g = member.guild_id;
        let uid = user.id;
        let roles: Vec<RoleId> = auto
            .autorole_ids
            .iter()
            .map(|r| RoleId::new(r.as_u64()))
            .collect();
        let http = Arc::clone(&ctx.http);
        // Delayed on purpose: an instant role grant hands a raid's throwaway accounts
        // the permissions to post. The sleep lives in a detached task so a join storm
        // cannot occupy the handlers.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;

            for role in roles {
                let _ = http.add_member_role(g, uid, role, None).await;
            }
        });
    }

    if auto.welcome_enabled && !(user.bot && auto.welcome_ignore_bots) {
        // Read from the gateway cache rather than the API: a join storm must not turn
        // into a rate-limit problem, and a name that is a minute stale is still the
        // right name. Both placeholders used to be wrong — `{members}` printed the
        // guild id and `{server}` was replaced with nothing, so a greeting read
        // "welcome to , you are member 1043287654321".
        //
        // The block is load-bearing: a cache reference is deliberately `!Send` and
        // `handle_join` is spawned, so the borrow must be gone before the send below.
        // The channel is picked in the order an operator expects from the panel's hint:
        // what they chose, then the server's own system-channel channel, then nothing.
        // Reading it from the cache rather than the API is deliberate — a join storm must
        // not turn into a rate-limit problem, and a name a minute stale is still the right
        // name. The block is load-bearing for the same reason as before: a cache reference
        // is `!Send` and this whole handler runs in a spawned task, so the borrow has to
        // be gone before the first await below.
        let (name, members, system) = match ctx.cache.guild(member.guild_id.get()) {
            Some(g) => (
                g.name.clone(),
                g.member_count.to_string(),
                g.system_channel_id.map(|c| Snowflake(c.get())),
            ),
            None => ("the server".to_string(), "everyone".to_string(), None),
        };
        let channel = auto.welcome_channel_id.or(system);
        let facts = GreetingFacts {
            user_id: user.id.get(),
            handle: user.name.clone(),
            server: name,
            members,
            avatar: user.avatar.map(|h| h.to_string()),
        };
        let tokens = facts.tokens();
        if let Some(channel) = channel {
            let text = render_template(&auto.welcome_message, &tokens);
            let card = welcome_card(&auto, &facts);
            let mut builder = CreateMessage::new();
            if !text.is_empty() {
                builder = builder.content(text);
            }
            if let Some(embed) = card.clone() {
                builder = builder.add_embed(embed);
            }
            // The greeting is a courtesy, not a checkpoint: a channel the bot cannot post
            // in must not lose the join roles, the audit row or anything else after it.
            if let Ok(sent) = ChannelId::new(channel.as_u64())
                .send_message(&ctx.http, builder)
                .await
            {
                let after = auto.welcome_delete_after_secs;
                if after > 0 {
                    let (http, ch, msg) = (
                        Arc::clone(&ctx.http),
                        ChannelId::new(channel.as_u64()),
                        sent.id,
                    );
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(u64::from(after))).await;
                        let _ = ch.delete_message(&http, msg).await;
                    });
                }
            }
            if auto.welcome_direct_dm {
                // Same words, second door. A DM is the one place a greeting is guaranteed to
                // be read, which is also why it is off by default and stays a text message:
                // a card in a DM reads as a bot advertisement.
                let dm_text = render_template(
                    if auto.welcome_message.is_empty() {
                        &auto.welcome_embed_description
                    } else {
                        &auto.welcome_message
                    },
                    &tokens,
                );
                if !dm_text.is_empty() {
                    if let Ok(dm) = user.create_dm_channel(&ctx.http).await {
                        let _ = dm
                            .send_message(&ctx.http, CreateMessage::new().content(dm_text))
                            .await;
                    }
                }
            }
        }
    }
}

/// What a join or leave template may talk about.
///
/// A struct rather than six arguments, because the panel's hints advertise this same list and the
/// two halves have to move together: a placeholder the bot never replaces is a greeting that reads
/// `{count}` to the one person it was for, which is precisely what the default message used to do.
#[derive(Debug, Clone, Default)]
pub(crate) struct GreetingFacts {
    pub user_id: u64,
    pub handle: String,
    pub server: String,
    pub members: String,
    pub avatar: Option<String>,
}

impl GreetingFacts {
    pub(crate) fn tokens(&self) -> Vec<(&'static str, String)> {
        vec![
            ("{user}", format!("<@{}>", self.user_id)),
            ("{tag}", self.handle.clone()),
            ("{server}", self.server.clone()),
            ("{members}", self.members.clone()),
            ("{count}", self.members.clone()),
            ("{id}", self.user_id.to_string()),
        ]
    }
}

/// The greeting card, or `None` when the server has not asked for one.
fn welcome_card(auto: &AutoConfig, facts: &GreetingFacts) -> Option<CreateEmbed> {
    if auto.welcome_embed_title.is_empty() && auto.welcome_embed_description.is_empty() {
        return None;
    }
    let c = auto.welcome_embed_color;
    let mut embed = CreateEmbed::new().color(Color::from_rgb(
        ((c >> 16) & 0xff) as u8,
        ((c >> 8) & 0xff) as u8,
        (c & 0xff) as u8,
    ));
    let tokens = facts.tokens();
    if !auto.welcome_embed_title.is_empty() {
        embed = embed.title(render_template(&auto.welcome_embed_title, &tokens));
    }
    if !auto.welcome_embed_description.is_empty() {
        embed = embed.description(render_template(&auto.welcome_embed_description, &tokens));
    }
    if auto.welcome_show_avatar {
        // Composed from the snowflake and hash rather than asked of the user object: the CDN URL is
        // stable, costs no request, and a member with no avatar simply gets no thumbnail.
        if let Some(hash) = &facts.avatar {
            embed = embed.thumbnail(format!(
                "https://cdn.discordapp.com/avatars/{}/{}.png?size=128",
                facts.user_id, hash
            ));
        }
    }
    Some(embed)
}

async fn handle_leave(
    state: &BotState,
    ctx: &Context,
    guild_id: GuildId,
    user: &serenity::all::User,
) {
    let guild = Snowflake(guild_id.get());
    if !state.managed.contains(&guild) {
        return;
    }
    let auto = state.db.auto_config(guild).unwrap_or_default();
    // One decision for both directions: `welcome_ignore_bots` says the thing it means, and a
    // verification app leaving is not news either.
    if auto.goodbye_enabled && !(user.bot && auto.welcome_ignore_bots) {
        // The goodbye used to know two tokens while the greeting knew six, so the `{server}` and
        // `{members}` the panel advertises in its own placeholder printed the placeholder. One
        // list for both directions, and the same system-channel fallback the hint promises —
        // without it, a server that never set a system channel had a goodbye box that did nothing.
        let (name, members, system) = match ctx.cache.guild(guild_id.get()) {
            Some(g) => (
                g.name.clone(),
                g.member_count.to_string(),
                g.system_channel_id.map(|c| Snowflake(c.get())),
            ),
            None => ("the server".to_string(), "everyone".to_string(), None),
        };
        let facts = GreetingFacts {
            user_id: user.id.get(),
            handle: user.name.clone(),
            server: name,
            members,
            avatar: user.avatar.map(|h| h.to_string()),
        };
        if let Some(channel) = auto.goodbye_channel_id.or(system) {
            let text = render_template(&auto.goodbye_message, &facts.tokens());
            let _ = ChannelId::new(channel.as_u64())
                .send_message(&ctx.http, CreateMessage::new().content(text))
                .await;
        }
    }
}

fn gid(guild: Snowflake) -> GuildId {
    GuildId::new(guild.as_u64())
}

// ---------------------------------------------------------------------- tickets

/// Create the channel for a ticket and record it. Called from a panel button and from
/// the outbox (`ticket.create`), which is why it takes loose JSON rather than a struct:
/// the two callers know different things.
pub async fn open_ticket(
    state: &BotState,
    ctx: &Context,
    guild: Snowflake,
    args: &serde_json::Value,
    by: Option<Snowflake>,
    actor_label: &str,
) -> Result<(), String> {
    let cfg: TicketConfig = state.db.ticket_config(guild).map_err(|e| e.to_string())?;
    if !cfg.enabled {
        return Err("tickets are not enabled in this server".into());
    }
    let owner = args
        .get("user")
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_u64().map(|u| u.to_string()))
        })
        .and_then(|s| {
            s.chars()
                .filter(char::is_ascii_digit)
                .collect::<String>()
                .parse::<u64>()
                .ok()
        })
        .or_else(|| by.map(|b| b.as_u64()))
        .ok_or("nobody to open a ticket for")?;
    let subject = args
        .get("subject")
        .and_then(|v| v.as_str())
        .unwrap_or("Help request")
        .to_string();
    let owner_sf = Snowflake(owner);
    if cfg.max_per_user > 0
        && state
            .db
            .open_ticket_count(guild, Some(owner_sf))
            .map_err(|e| e.to_string())?
            >= i64::from(cfg.max_per_user)
    {
        return Err(format!(
            "you already have {} open ticket(s)",
            cfg.max_per_user
        ));
    }

    let everyone_view = Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::ATTACH_FILES
        | Permissions::EMBED_LINKS;
    let empty = Permissions::empty();
    let mut overwrites = vec![
        // @everyone is denied *at creation*: a private conversation whose privacy is
        // applied by a second request has a window in which it is public, and that
        // window is a data breach if the second request fails.
        PermissionOverwrite {
            kind: PermissionOverwriteType::Role(RoleId::new(guild.as_u64())),
            allow: empty,
            deny: everyone_view,
        },
        PermissionOverwrite {
            kind: PermissionOverwriteType::Member(UserId::new(owner)),
            allow: everyone_view,
            deny: empty,
        },
    ];
    if let Some(role) = cfg.support_role_id {
        overwrites.push(PermissionOverwrite {
            kind: PermissionOverwriteType::Role(RoleId::new(role.as_u64())),
            allow: everyone_view,
            deny: empty,
        });
    }
    let mut builder = CreateChannel::new("ticket")
        .kind(ChannelType::Text)
        .permissions(overwrites);
    if let Some(category) = cfg.category_id {
        builder = builder.category(ChannelId::new(category.as_u64()));
    }
    let channel = gid(guild)
        .create_channel(&ctx.http, builder)
        .await
        .map_err(|e| e.to_string())?;

    let tag = state
        .db
        .user(owner_sf)
        .ok()
        .flatten()
        .and_then(|v| {
            v.get("username")
                .and_then(|u| u.as_str().map(str::to_string))
        })
        .unwrap_or_else(|| actor_label.to_string());
    let record = state
        .db
        .create_ticket(
            &aerona_core::db::tickets::NewTicket {
                guild_id: guild,
                channel_id: Snowflake(channel.id.get()),
                user_channel: None,
                owner_id: owner_sf,
                subject: clamp_chars(&subject, 120),
                panel_id: args.get("panelId").and_then(|v| v.as_i64()),
            },
            &cfg.name_format,
            &tag,
        )
        .map_err(|e| e.to_string())?;
    let _ = channel
        .id
        .edit(&ctx.http, EditChannel::new().name(record.name.clone()))
        .await;

    let intro = if cfg.default_message.is_empty() {
        format!(
            "Ticket {} for <@{owner}>. Write here and a moderator will reply.",
            record.reference
        )
    } else {
        render_template(
            &cfg.default_message,
            &[
                ("{user}", format!("<@{owner}>")),
                ("{reference}", record.reference.clone()),
            ],
        )
    };
    let mut buttons = Vec::new();
    if cfg.show_close {
        buttons.push(
            CreateButton::new("aerona:ticket:close")
                .style(ButtonStyle::Danger)
                .label("Close ticket"),
        );
    }
    if cfg.show_claim {
        buttons.push(
            CreateButton::new("aerona:ticket:claim")
                .style(ButtonStyle::Secondary)
                .label("Claim"),
        );
    }
    buttons.push(
        CreateButton::new("aerona:ticket:transcript")
            .style(ButtonStyle::Secondary)
            .label("Transcript"),
    );
    let message = CreateMessage::new()
        .content(clamp_chars(&intro, 1800))
        .components(vec![CreateActionRow::Buttons(buttons)])
        .allowed_mentions(CreateAllowedMentions::new());
    let _ = channel.id.send_message(&ctx.http, message).await;
    if cfg.ping_support_role {
        if let Some(role) = cfg.support_role_id {
            let _ = channel
                .id
                .send_message(
                    &ctx.http,
                    CreateMessage::new()
                        .content(format!("<@&{}>", role.as_u64()))
                        .allowed_mentions(
                            CreateAllowedMentions::new().roles(vec![RoleId::new(role.as_u64())]),
                        ),
                )
                .await;
        }
    }
    tracing::info!(guild = %guild, ticket = %record.reference, "ticket opened");
    Ok(())
}

/// Close a ticket: write the transcript where the guild asked for it, rename the
/// channel, and leave it open for two minutes so the people in it can read the ending.
pub async fn close_ticket(state: &BotState, ctx: &Context, guild: Snowflake, ticket_id: i64) {
    let Ok(Some(ticket)) = state.db.ticket_by_id(ticket_id) else {
        return;
    };
    let _ = state.db.set_ticket_status(ticket_id, TicketStatus::Closed);
    let rows = state
        .db
        .ticket_transcript(ticket_id, true)
        .unwrap_or_default();
    let text = render_transcript(&ticket, rows);
    let cfg = state.db.ticket_config(guild).unwrap_or_default();
    if let Some(log) = cfg.transcript_channel_id {
        let file = serenity::all::CreateAttachment::bytes(
            text.into_bytes(),
            format!("{}.txt", ticket.reference),
        );
        let _ = ChannelId::new(log.as_u64())
            .send_message(
                &ctx.http,
                CreateMessage::new()
                    .content(format!("Ticket {} closed.", ticket.reference))
                    .add_file(file),
            )
            .await;
    }
    let _ = ChannelId::new(ticket.channel_id.as_u64())
        .edit(
            &ctx.http,
            EditChannel::new().name(format!("closed-{}", ticket.reference)),
        )
        .await;
}

/// A transcript as plain text: greppable, printable, and readable on a phone by a
/// moderator who has no editor. JSON would be machine-readable and human-useless.
pub fn render_transcript(
    ticket: &aerona_core::models::TicketRecord,
    rows: Vec<serde_json::Value>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("Aerona ticket {}\n", ticket.reference));
    out.push_str(&format!("server  {}\n", ticket.guild_id));
    out.push_str(&format!("opened  {}\n\n", gate::rfc3339(ticket.created_at)));
    for row in &rows {
        let at = row
            .get("at")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let author = row
            .get("author")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("someone")
            .to_string();
        let content = row
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let internal = row
            .get("internal")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        out.push_str(&format!(
            "[{}] {}{}\n",
            gate::rfc3339(at),
            if internal { "(internal) " } else { "" },
            clamp_chars(&format!("{author}: {content}"), 1800)
        ));
    }
    if rows.is_empty() {
        out.push_str("(no messages were recorded)\n");
    }
    out
}

// ---------------------------------------------------------- stickies, levels, raid

async fn stickies_on_message(
    state: &BotState,
    ctx: &Context,
    guild: Snowflake,
    channel: Snowflake,
) {
    let cfg = match state.db.sticky_config(guild) {
        Ok(c) if c.enabled => c,
        _ => return,
    };
    let key = (guild, channel);
    let mut entry = state.sticky_tally.entry(key).or_insert(0);
    *entry += 1;
    let due = match state.db.due_stickies(guild, channel) {
        Ok(list) if !list.is_empty() => list,
        _ => return,
    };
    *entry = 0;
    // One repost per channel per cooldown, however many stickies are due: three stickies
    // reposting individually is three edits in the same instant, and the point of a
    // sticky is that it is quiet.
    let since = now() - state.last_sticky_edit.get(&guild).map(|v| *v).unwrap_or(0);
    if since < crate::STICKY_MIN_SECS.max(i64::from(cfg.cooldown_secs)) {
        return;
    }
    state.last_sticky_edit.insert(guild, now());
    let ch = ChannelId::new(channel.as_u64());
    for sticky in due {
        let mut posted = None;
        if !cfg.repost {
            if let Some(existing) = sticky.sticky_message_id {
                // Editing keeps one message at the bottom rather than an endless trail.
                let edit = EditMessage::new().content(clamp_chars(&sticky.content, 1900));
                if ch
                    .edit_message(&ctx.http, MessageId::new(existing.as_u64()), edit)
                    .await
                    .is_ok()
                {
                    posted = Some(existing);
                }
            }
        }
        if posted.is_none() {
            let body = if cfg.repost {
                format!("📌 {}", clamp_chars(&sticky.content, 1800))
            } else {
                clamp_chars(&sticky.content, 1900)
            };
            posted = ch
                .send_message(
                    &ctx.http,
                    CreateMessage::new()
                        .content(body)
                        .allowed_mentions(CreateAllowedMentions::new()),
                )
                .await
                .ok()
                .map(|m| Snowflake(m.id.get()));
        }
        let _ = state.db.mark_sticky_run(sticky.id, posted);
        let _ = state.db.bump_stickies(guild, channel);
    }
}

/// A level-up is a DM, never a channel message: the alternative is a server where every
/// tenth message is a congratulations, which is how level systems get turned off.
async fn announce_level(state: &BotState, ctx: &Context, user: Snowflake, level: u32) {
    let text = format!("You reached level **{level}** in a server we share. Keep it up.");
    let Ok(resolved) = UserId::new(user.as_u64()).to_user(&ctx.http).await else {
        return;
    };
    if let Ok(dm) = resolved.create_dm_channel(&ctx.http).await {
        let _ = dm
            .send_message(&ctx.http, CreateMessage::new().content(text))
            .await;
    }
    let _ = state;
}

async fn alert_raid(
    state: &BotState,
    ctx: &Context,
    guild: Snowflake,
    raid: &RaidConfig,
    reason: &str,
) {
    let at = now();
    if let Some(last) = state.raid_alerted.get(&guild) {
        if at - *last < i64::from(raid.cooldown_secs.max(60)) {
            return;
        }
    }
    state.raid_alerted.insert(guild, at);
    let Some(channel) = raid.alert_channel_id else {
        return;
    };
    let text = format!(
        "🚨 **Raid suspected** — {reason}\nA lockdown can be lifted or applied from the dashboard."
    );
    let _ = ChannelId::new(channel.as_u64())
        .send_message(&ctx.http, CreateMessage::new().content(text))
        .await;
    state.db.audit_best_effort(
        aerona_core::db::security::AuditEntry::new("raid.suspected")
            .for_guild(guild)
            .actor(
                aerona_core::db::security::ActorType::Bot,
                None,
                "aerona-bot",
            )
            .meta(serde_json::json!({ "reason": reason })),
    );
}

/// `{user}`-style substitution, shared by welcome, goodbye and the ticket default
/// message. Unknown placeholders are left alone: an operator who typos a template should
/// see the typo, not a silent blank.
pub fn render_template(template: &str, pairs: &[(&str, String)]) -> String {
    let mut out = template.to_string();
    for (needle, value) in pairs {
        out = out.replace(needle, value);
    }
    out = out.replace("{server.name}", "{server}");
    clamp_chars(&out, 1900)
}

#[allow(dead_code)]
fn reserved(
    _: &CommandDataResolved,
    _: &AutoConfig,
    _: &GuildSettings,
    _: &UnavailableGuild,
    _: &CreateEmbed,
    _: &Guild,
    _: &MessageUpdateEvent,
) {
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_substitute_and_survive_an_unknown_placeholder() {
        let out = render_template("hi {user} {nope}", &[("{user}", "KEVIN".into())]);
        assert_eq!(out, "hi KEVIN {nope}");
    }

    #[test]
    fn the_default_greeting_leaves_no_placeholder_behind() {
        let auto = AutoConfig::default();
        let facts = GreetingFacts {
            user_id: 7,
            handle: "kevin".into(),
            server: "Test Guild".into(),
            members: "1204".into(),
            avatar: None,
        };
        let out = render_template(&auto.welcome_message, &facts.tokens());
        assert!(
            !out.contains('{') && !out.contains('}'),
            "unfilled token in {out:?}"
        );
        // `{count}` and `{members}` are the same number by design: the sentence is about how many
        // members there are, whichever word the operator reached for.
        assert!(out.contains("#1204") && out.contains("<@7>"), "{out}");
    }

    #[test]
    fn a_goodbye_knows_every_token_a_greeting_knows() {
        let facts = GreetingFacts {
            user_id: 7,
            handle: "kevin".into(),
            server: "Test Guild".into(),
            members: "1203".into(),
            avatar: None,
        };
        let out = render_template(
            "{user} left {server}. {members} now, #{count}.",
            &facts.tokens(),
        );
        assert_eq!(out, "<@7> left Test Guild. 1203 now, #1203.");
    }

    #[test]
    fn a_card_exists_only_when_one_is_asked_for() {
        let facts = GreetingFacts {
            user_id: 11,
            handle: "ann".into(),
            server: "S".into(),
            members: "2".into(),
            avatar: Some("abc".into()),
        };
        let mut auto = AutoConfig::default();
        assert!(
            welcome_card(&auto, &facts).is_none(),
            "no title and no body is no card"
        );
        auto.welcome_embed_title = "Hello, {tag}!".into();
        let embed = welcome_card(&auto, &facts).expect("a title is a card");
        let sent = serde_json::to_value(&embed).expect("an embed serializes");
        assert_eq!(sent["title"], "Hello, ann!");
        assert_eq!(
            sent["color"], 0x58_65_F2,
            "blurple, from the number in the document"
        );
        assert_eq!(
            sent["thumbnail"]["url"],
            "https://cdn.discordapp.com/avatars/11/abc.png?size=128"
        );
        auto.welcome_show_avatar = false;
        let off = serde_json::to_value(welcome_card(&auto, &facts).unwrap()).unwrap();
        assert!(
            off.get("thumbnail").is_none(),
            "no thumbnail when the switch is off"
        );
    }

    #[test]
    fn the_old_server_placeholder_still_works() {
        let out = render_template("welcome to {server.name}", &[]);
        assert_eq!(out, "welcome to {server}");
    }

    #[test]
    fn a_long_template_is_clamped_to_what_discord_accepts() {
        let out = render_template(&"a".repeat(5000), &[]);
        assert_eq!(out.chars().count(), 1900);
    }

    #[test]
    fn a_transcript_reads_like_one_and_is_empty_safe() {
        let ticket = aerona_core::models::TicketRecord {
            id: 1,
            guild_id: Snowflake(10),
            channel_id: Snowflake(11),
            owner_id: Snowflake(12),
            claimed_by: None,
            status: TicketStatus::Open,
            subject: "missing role".into(),
            reference: "AER-ABC123".into(),
            created_at: 1_700_000_000,
            closed_at: None,
            message_count: 2,
        };
        let rows = vec![
            serde_json::json!({ "at": 1_700_000_100, "author": "kevin", "content": "hello?", "internal": false }),
            serde_json::json!({ "at": 1_700_000_200, "author": "mod", "content": "looking into it", "internal": true }),
        ];
        let text = render_transcript(&ticket, rows);
        assert!(text.contains("AER-ABC123"), "{text}");
        assert!(
            text.contains("[2023-11-14T22:15:00Z] kevin: hello?"),
            "{text}"
        );
        assert!(
            text.contains("(internal) mod"),
            "an internal note must be marked as one"
        );
        let empty = render_transcript(&ticket, vec![]);
        assert!(empty.contains("no messages were recorded"));
    }

    #[test]
    fn staff_role_ids_are_empty_for_a_fresh_guild_and_never_panic() {
        // The whole permission model of the gateway path depends on "no data means no
        // authority", so this is the assertion that matters most.
        let dir = tempfile::TempDir::new().expect("a temp dir for the test database");
        let mut env = std::collections::HashMap::new();
        env.insert(
            "AERONA_TOKEN".to_string(),
            "MTIzNDU2Nzg5.ABCDEFGHIJKLMNOPQRSTUVWXYZ.abcdefghijklmnopqrstuvwxyz01".to_string(),
        );
        env.insert("AERONA_PORT".to_string(), "8798".to_string());
        let cfg = Arc::new(aerona_core::config::Config::parse_env(&env, dir.path(), true).unwrap());
        let db = aerona_core::db::Db::open_path(&dir.path().join("t.db")).unwrap();
        let state = Arc::new(BotState::new(cfg, db));
        assert!(staff_role_ids(&state, Snowflake(404)).is_empty());
    }
}
