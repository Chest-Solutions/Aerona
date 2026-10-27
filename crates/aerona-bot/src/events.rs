//! Gateway events: the only place in Aerona where Discord talks to us.
//!
//! Three rules govern every handler here, and they are what separates a bot that
//! survives a raid from one that becomes part of it:
//!
//! 1. cheap checks first, database second - an unmanaged guild, the bot's own message or
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
    CreateEmbedFooter, CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage,
    EditChannel, EditMessage, EventHandler, Guild, GuildChannel, GuildId, Interaction,
    InteractionResponseFlags, Member, Message, MessageId, MessageUpdateEvent, PermissionOverwrite,
    PermissionOverwriteType, Permissions, Ready, Role, RoleId, UnavailableGuild, UserId,
};

use aerona_core::identity::Snowflake;
use aerona_core::models::{AutoConfig, GuildSettings, RaidConfig, TicketConfig, TicketStatus};
use aerona_core::time::now;

use aerona_core::db::logging::LogEntry;

use crate::commands::{self, Actor};
use crate::gate;
pub(crate) use crate::logger;
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
        // A panel that has never been given a channel list waits on an hour-long slow pass, which
        // is how "pick a channel" looked broken on a fresh install. Mark every guild the bot came
        // back to that has no report at all: the status loop fills them in within a tick, and a
        // guild that already has one costs nothing here.
        for id in &present {
            if self
                .state
                .db
                .guild_report(*id, "picker")
                .ok()
                .flatten()
                .is_none()
            {
                self.state.picker_dirty.insert(*id);
            }
        }
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

    async fn guild_member_removal(
        &self,
        ctx: Context,
        guild_id: GuildId,
        user: serenity::all::User,
        member_data: Option<Member>,
    ) {
        let state = Arc::clone(&self.state);
        let _ = tokio::spawn(async move {
            // Membership news is logged whether or not the welcomer is switched on: the audit
            // trail is not a feature you can afford to lose by turning off a greeting.
            let guild = Snowflake(guild_id.get());
            let age = member_data
                .as_ref()
                .and_then(|m| m.joined_at)
                .map(|t| (now() - t.unix_timestamp()).max(0))
                .map(|secs| {
                    let days = secs / 86_400;
                    if days > 0 {
                        format!("{days} day(s)")
                    } else {
                        format!("{} hour(s)", secs / 3600)
                    }
                });
            // The member object is only in the payload when the guild was cached, so the facts
            // are optional and an unknown past stays out of the card.
            let facts = member_data.as_ref().map(member_facts);
            let e = LogEntry::new(
                "member_leave",
                match &age {
                    Some(age) => format!("left after {age}"),
                    None => "left".to_string(),
                },
            )
            .user(Snowflake(user.id.get()))
            .bot(user.bot)
            .was_opt(facts)
            .is_now("no longer a member");
            logger::note(&state, &ctx, guild, e).await;
            handle_leave(&state, &ctx, guild_id, &user).await;
        })
        .await;
    }

    async fn guild_member_addition(&self, ctx: Context, member: Member) {
        let state = Arc::clone(&self.state);
        let _ = tokio::spawn(async move {
            let guild = Snowflake(member.guild_id.get());
            logger::note(
                &state,
                &ctx,
                guild,
                LogEntry::new(
                    "member_join",
                    format!("joined with {} role(s)", member.roles.len()),
                )
                .user(Snowflake(member.user.id.get()))
                .bot(member.user.bot)
                .was("not a member")
                .is_now(member_facts(&member)),
            )
            .await;
            handle_join(&state, &ctx, &member).await;
        })
        .await;
    }

    async fn message_delete_bulk(
        &self,
        ctx: Context,
        channel_id: ChannelId,
        ids: Vec<MessageId>,
        guild_id: Option<GuildId>,
    ) {
        let state = Arc::clone(&self.state);
        let _ = tokio::spawn(async move {
            handle_bulk_delete(&state, &ctx, guild_id, channel_id, ids).await
        })
        .await;
    }

    async fn guild_member_update(
        &self,
        ctx: Context,
        old: Option<Member>,
        new: Option<Member>,
        event: serenity::all::GuildMemberUpdateEvent,
    ) {
        let state = Arc::clone(&self.state);
        let _ = tokio::spawn(async move {
            handle_member_update(&state, &ctx, old, new, event).await;
        })
        .await;
    }

    async fn channel_create(&self, ctx: Context, channel: GuildChannel) {
        let state = Arc::clone(&self.state);
        let _ = tokio::spawn(async move {
            handle_channel_change(&state, &ctx, &channel, "created").await;
        })
        .await;
    }

    async fn channel_update(&self, ctx: Context, _old: Option<GuildChannel>, new: GuildChannel) {
        let state = Arc::clone(&self.state);
        let _ = tokio::spawn(async move {
            handle_channel_change(&state, &ctx, &new, "changed").await;
        })
        .await;
    }

    async fn channel_delete(
        &self,
        ctx: Context,
        channel: GuildChannel,
        _removed: Option<Vec<Message>>,
    ) {
        let state = Arc::clone(&self.state);
        let _ = tokio::spawn(async move {
            handle_channel_change(&state, &ctx, &channel, "deleted").await;
        })
        .await;
    }

    async fn guild_role_create(&self, ctx: Context, role: Role) {
        let state = Arc::clone(&self.state);
        let _ = tokio::spawn(async move {
            handle_role_change(&state, &ctx, &role, "created").await;
        })
        .await;
    }

    async fn guild_role_update(&self, ctx: Context, _old: Option<Role>, role: Role) {
        let state = Arc::clone(&self.state);
        let _ = tokio::spawn(async move {
            handle_role_change(&state, &ctx, &role, "changed").await;
        })
        .await;
    }

    async fn guild_role_delete(
        &self,
        ctx: Context,
        guild_id: GuildId,
        role_id: RoleId,
        _gone: Option<Role>,
    ) {
        let state = Arc::clone(&self.state);
        let _ = tokio::spawn(async move {
            let guild = Snowflake(guild_id.get());
            tasks::dirty_picker(&state, guild);
            logger::note(
                &state,
                &ctx,
                guild,
                LogEntry::new("role_change", format!("role <@&{}> deleted", role_id.get()))
                    .bot(true),
            )
            .await;
        })
        .await;
    }
}

/// Nickname and role changes, from the one event that carries both. `old` is only present when the
/// member was in the cache, and a diff against nothing is not a fact worth logging, so that case
/// reports the change without pretending to know what changed.
async fn handle_member_update(
    state: &BotState,
    ctx: &Context,
    old: Option<Member>,
    new: Option<Member>,
    event: serenity::all::GuildMemberUpdateEvent,
) {
    // `GuildMemberUpdateEvent` carries a plain `GuildId` and a plain `User`: the field that is
    // optional here is `member`, because the member may not have been in the cache.
    let guild = Snowflake(event.guild_id.get());
    if !state.managed.contains(&guild) {
        return;
    }
    state.bump(&state.events_min);
    let user = Snowflake(
        new.as_ref()
            .map(|m| m.user.id.get())
            .unwrap_or_else(|| event.user.id.get()),
    );
    let bot = new.as_ref().map(|m| m.user.bot).unwrap_or(event.user.bot);

    let before_nick = old.as_ref().and_then(|m| m.nick.clone());
    let after_nick = new.as_ref().and_then(|m| m.nick.clone());
    if before_nick != after_nick {
        let entry = LogEntry::new(
            "nickname_change",
            format!(
                "nickname is now {}",
                after_nick
                    .clone()
                    .unwrap_or_else(|| "their username".into())
            ),
        )
        .user(user)
        .bot(bot);
        let entry = match (before_nick, after_nick) {
            (Some(b), Some(a)) => entry.was(b).is_now(a),
            (Some(b), None) => entry.was(b),
            (None, Some(a)) => entry.is_now(a),
            _ => entry,
        };
        logger::note(state, ctx, guild, entry).await;
    }

    let diff = match (&old, &new) {
        (Some(o), Some(n)) => {
            let before: Vec<u64> = o.roles.iter().map(|r| r.get()).collect();
            let after: Vec<u64> = n.roles.iter().map(|r| r.get()).collect();
            let added: Vec<String> = after
                .iter()
                .filter(|r| !before.contains(r))
                .map(|r| format!("<@&{r}>"))
                .collect();
            let removed: Vec<String> = before
                .iter()
                .filter(|r| !after.contains(r))
                .map(|r| format!("<@&{r}>"))
                .collect();
            let names = |ids: &Vec<u64>| {
                if ids.is_empty() {
                    return "no roles".to_string();
                }
                ids.iter()
                    .map(|r| {
                        ctx.cache
                            .guild(guild.as_u64())
                            .and_then(|g| {
                                g.roles.get(&RoleId::new(*r)).map(|role| role.name.clone())
                            })
                            .unwrap_or_else(|| format!("<@&{r}>"))
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let (before_text, after_text) = (names(&before), names(&after));
            if added.is_empty() && removed.is_empty() {
                None
            } else {
                let mut bits = Vec::new();
                if !added.is_empty() {
                    bits.push(format!("+{}", added.join(" ")));
                }
                if !removed.is_empty() {
                    bits.push(format!("-{}", removed.join(" ")));
                }
                Some((bits.join("  "), before_text, after_text))
            }
        }
        _ => None,
    };
    if let Some((diff, before_text, after_text)) = diff {
        let entry = LogEntry::new("role_change", diff)
            .user(user)
            .bot(bot)
            .was(before_text)
            .is_now(after_text);
        logger::note(state, ctx, guild, entry).await;
    }
}

/// A channel moved, so the pickers that list channels are stale, and the guild's audit trail wants
/// the fact. One cache read for each, and neither is a REST call.
async fn handle_channel_change(
    state: &BotState,
    ctx: &Context,
    channel: &GuildChannel,
    verb: &str,
) {
    let guild = Snowflake(channel.guild_id.get());
    if !state.managed.contains(&guild) {
        return;
    }
    tasks::dirty_picker(state, guild);
    logger::note(state, ctx, guild, {
        // "Created" and "deleted" are the fact; the settings are the detail a moderator asks
        // for next, and the only reason a channel event earns a card. Which half they belong
        // to follows the verb: a channel that is gone has its facts in the past.
        let entry = LogEntry::new(
            "channel_change",
            format!("#{} was {verb} (kind {:?})", channel.name, channel.kind),
        )
        .channel(Snowflake(channel.id.get()));
        let facts = channel_facts(channel);
        let entry = if verb == "deleted" {
            entry.was(facts)
        } else {
            entry.is_now(facts)
        };
        // A channel change is made by a human with permission or by nobody, and
        // `ignore_bots` is about message noise; `bot(true)` means "do not treat this as a
        // member's post".
        entry.bot(true)
    })
    .await;
}

async fn handle_role_change(state: &BotState, ctx: &Context, role: &Role, verb: &str) {
    let guild = Snowflake(role.guild_id.get());
    if !state.managed.contains(&guild) {
        return;
    }
    tasks::dirty_picker(state, guild);
    logger::note(
        state,
        ctx,
        guild,
        LogEntry::new(
            "role_change",
            format!(
                "{} role <@&{}> ({verb})",
                if role.hoist { "a pinned" } else { "a" },
                role.id.get()
            ),
        )
        .bot(true),
    )
    .await;
}

/// What a member was, in the handful of numbers a mod log is read for. Ages rather than
/// timestamps: `1699` is not information, and the panel has no clock to convert it with.
fn member_facts(m: &Member) -> String {
    let now = now();
    let since = |unix: i64| {
        let days = (now - unix).max(0) / 86_400;
        if days == 0 {
            "today".to_string()
        } else if days == 1 {
            "1 day ago".to_string()
        } else {
            format!("{days} days ago")
        }
    };
    let joined = m
        .joined_at
        .map(|t| format!("joined {}", since(t.unix_timestamp())))
        .unwrap_or_else(|| "joined now".into());
    let account = format!(
        "account created {}",
        since(m.user.created_at().unix_timestamp())
    );
    let nick = m
        .nick
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .map(|n| format!("nick \"{n}\""))
        .unwrap_or_else(|| "no nickname".into());
    format!(
        "{joined} · {account} · {} role(s) · {nick}{}",
        m.roles.len(),
        if m.user.bot { " · bot" } else { "" }
    )
}

/// What a channel is, in the same spirit: the settings, not the ids.
fn channel_facts(c: &GuildChannel) -> String {
    let mut bits = vec![
        format!("kind {:?}", c.kind),
        format!("position {}", c.position),
        if c.nsfw {
            "nsfw".to_string()
        } else {
            "sfw".to_string()
        },
        format!("slowmode {}s", c.rate_limit_per_user.unwrap_or(0)),
    ];
    if !c.permission_overwrites.is_empty() {
        // The count is the useful part: "why can't anybody post here" is answered by "there are
        // nine overrides on this channel", and the list itself is a Discord screen away.
        bits.push(format!("{} override(s)", c.permission_overwrites.len()));
    }
    if let Some(topic) = c.topic.as_deref().filter(|t| !t.trim().is_empty()) {
        let short: String = topic.chars().take(80).collect();
        bits.push(format!("topic: {short}"));
    }
    if let Some(parent) = c.parent_id {
        bits.push(format!("under category <#{parent}>"));
    }
    bits.join(" · ")
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
        id if id.starts_with("aerona:ticket:open") => {
            // A board carries one button per ticket type on a single message, so the button says
            // which row it is. `panelMessage` stays as the fallback for a panel posted before
            // custom ids named a type, where the message itself is the only clue.
            let panel_id = id
                .strip_prefix("aerona:ticket:open:")
                .and_then(|rest| rest.parse::<i64>().ok());
            let args = serde_json::json!({
                "panelMessage": button.message.id.get().to_string(),
                "panelId": panel_id,
            });
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
    let author = Snowflake(msg.author.id.get());
    let channel = Snowflake(msg.channel_id.get());
    let content = msg.content.clone();

    // The text is kept *here*, before anything else decides what happens to this message: the
    // filter may delete it, a ticket channel returns early, and either way the log still owes
    // somebody the words. A deleted message is exactly the one a log needs to quote, and after
    // the fact the text is gone.
    //
    // This sits above the bot check on purpose. A moderator deleting a spam bot's message, or
    // Aerona's own, is a delete worth quoting, and the reason a delete card used to read
    // "(deleted)" with nothing else was that the messages most likely to be deleted were the ones
    // this handler refused to look at.
    let logs = state.db.log_config(guild).unwrap_or_default();
    if logs.caches() && worth_caching(&content, msg.attachments.len()) {
        let _ =
            state
                .db
                .remember_message(guild, channel, Snowflake(msg.id.get()), author, &content);
    }

    if msg.author.id == ctx.cache.current_user().id || msg.author.bot {
        // A bot's message is never XP, never a command, and never filtered: two bots
        // replying to each other is the classic self-inflicted raid, and the only fix
        // that has ever worked is not reading your own output.
        return;
    }

    // A ticket channel is a conversation with staff, not with the world: the filter, XP
    // and stickies do not apply, and everything said in it becomes transcript.
    if let Ok(Some(ticket)) = state.db.ticket_by_channel(guild, channel) {
        // Files ride with the row. A transcript of a bug report without the screenshot is a
        // transcript in which somebody described having a screenshot.
        let attachments = attachment_list(msg);
        let _ = state.db.append_ticket_message(
            ticket.id,
            author,
            &msg.author.name,
            &content,
            false,
            &attachments,
        );
        if ticket.status == TicketStatus::Closed && author == ticket.owner_id {
            let _ = state.db.set_ticket_status(ticket.id, TicketStatus::Open);
            // Talking again cancels the deadline. A channel scheduled for deletion while its owner
            // is still answering staff would delete a live conversation, which is the one thing
            // this feature must never do.
            let _ = state.db.schedule_ticket_delete(ticket.id, None);
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
            .reply(ctx, format!("slow down - try again in {secs}s"))
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
    let channel = Snowflake(event.channel_id.get());
    // Discord does not promise `guild_id` on a message update, and serenity says so out loud: the
    // payload may or may not carry fields that cannot change, and this is one of them. The channel
    // is asked instead, which is the same fact from a source that has it. Reading only the payload
    // is what made edits look like they were never logged at all: an update that arrived without
    // the field returned before anything was built.
    let Some(guild) = guild_of(
        state,
        event.guild_id,
        guild_of_channel(ctx, event.channel_id),
    ) else {
        return;
    };
    state.bump(&state.events_min);
    let Some(new_content) = event.content.clone() else {
        return;
    };
    let bot = event
        .author
        .as_ref()
        .map(|u| u.bot)
        .or_else(|| {
            event
                .member
                .as_ref()
                .and_then(|m| m.as_ref())
                .and_then(|m| m.user.as_ref())
                .map(|u| u.bot)
        })
        .unwrap_or(false);
    // The log comes before the filter, and unconditionally: an edit is worth recording whether or
    // not the AI gate is switched on, and the old order meant the logger ran only inside the
    // feature that deletes messages.
    let message = Snowflake(event.id.get());
    let quote = quote_of(state, ctx, guild, event.channel_id, message).await;
    if let Some(entry) = edit_entry(channel, message, quote.as_ref(), &new_content, bot) {
        // Keep the cache current, or the next edit of the same message compares against text
        // nobody has seen since the first one.
        let _ = state.db.remember_message(
            guild,
            channel,
            message,
            entry.user.unwrap_or(Snowflake(0)),
            &new_content,
        );
        logger::note(state, ctx, guild, entry).await;
    }
    let ai = state.db.ai_config(guild).unwrap_or_default();
    if !ai.enabled || !ai.apply_to_edits {
        return;
    }
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
    let Some(guild) = guild_of(state, guild_id, guild_of_channel(ctx, channel)) else {
        return;
    };
    let message_id = Snowflake(message.get());
    let channel_id = Snowflake(channel.get());
    let quote = quote_of(state, ctx, guild, channel, message_id).await;
    let entry = delete_entry(
        channel_id,
        message_id,
        quote.as_ref(),
        state.cfg.content_intent,
    );
    if quote.is_some() {
        let _ = state.db.forget_message(guild, message_id);
    }
    logger::note(state, ctx, guild, entry).await;
}

/// Which guild a channel belongs to, from the cache serenity keeps.
///
/// Serenity deprecates this accessor in favour of cloning the whole guild and searching its
/// channels, which is the more expensive way to ask a smaller question on a path that runs once
/// per event. So the deprecated call is made here, once, and nowhere else.
#[allow(deprecated)]
fn guild_of_channel(ctx: &Context, channel: ChannelId) -> Option<GuildId> {
    ctx.cache.channel(channel).map(|c| c.guild_id)
}

/// Which guild an event belongs to, or `None` when it is not a guild this install serves.
///
/// Both halves are optional by necessity: a message update may omit the guild, and the channel is
/// only in the cache for as long as the cache holds it. The managed set is the filter either way,
/// because an id belonging to a server nobody added here is not ours to log.
pub(crate) fn guild_of(
    state: &BotState,
    payload: Option<GuildId>,
    from_channel: Option<GuildId>,
) -> Option<Snowflake> {
    let id = payload.or(from_channel)?;
    let guild = Snowflake(id.get());
    state.managed.contains(&guild).then_some(guild)
}

/// What the message said, from the two places that can still know.
///
/// The bot's own cache is asked first because it is the one bounded on purpose (the guild's own
/// window) and it holds the text as it arrived. Serenity's cache is the fallback: it keeps the
/// newest messages per channel whether or not the log was asked to remember them, so a delete of
/// something the window had already forgotten is still quotable.
pub(crate) async fn quote_of(
    state: &BotState,
    ctx: &Context,
    guild: Snowflake,
    channel: ChannelId,
    message: Snowflake,
) -> Option<(String, Snowflake)> {
    if let Ok(Some((text, author, _at))) = state.db.recalled_message(guild, message) {
        return Some((text, author));
    }
    ctx.cache
        .message(channel, MessageId::new(message.as_u64()))
        .map(|m| {
            (
                aerona_core::security::clean(&m.content, 1500),
                Snowflake(m.author.id.get()),
            )
        })
}

/// The row for a delete. There is always one: a message vanishing is news even when the bot never
/// got to read it, and a log that only records what it could quote teaches nobody that a purge
/// happened.
pub(crate) fn delete_entry(
    channel: Snowflake,
    message: Snowflake,
    quote: Option<&(String, Snowflake)>,
    text_visible: bool,
) -> LogEntry {
    // `text_visible` is whether this deployment can read message text at all - the
    // MESSAGE_CONTENT intent - and it is the difference between two sentences that look alike in a
    // log and mean different things to an operator: "nobody said anything" and "I was not allowed
    // to hear it". Both used to be reported as a delete with nothing quoted, which is how a guild
    // could run for months with a mod log that never said what was removed.
    let mut e = match quote {
        // The text is what *was* there, so it is the `before` half and the `after` half stays
        // empty. The old card filled it with "(deleted)": the one thing a moderator could already
        // see for themselves, and the reason the kept text read like a footnote to it.
        Some((text, author)) if !text.trim().is_empty() => LogEntry::new(
            "message_delete",
            "message removed; the text was kept from when it arrived",
        )
        .user(*author)
        .was(text.clone()),
        Some((_, author)) if text_visible => LogEntry::new(
            "message_delete",
            "message removed with nothing written on it - a file, a sticker or an embed",
        )
        .user(*author),
        Some((_, author)) => LogEntry::new(
            "message_delete",
            "message removed; Aerona cannot read message text while the MESSAGE_CONTENT intent is off",
        )
        .user(*author),
        None if text_visible => LogEntry::new(
            "message_delete",
            "message deleted before its text could be kept - it is older than this guild's cache window, or it arrived before the bot was reading",
        ),
        None => LogEntry::new(
            "message_delete",
            "message deleted; Aerona cannot read message text while the MESSAGE_CONTENT intent is off",
        ),
    };
    e = e.channel(channel).message(message);
    e
}

/// The edit entry, or `None` when there is nothing to say about it. Split out from the handler,
/// and given the quote as an argument rather than fetching it, so the test can prove the
/// before/after pair without a gateway, a cache, or a network.
fn edit_entry(
    channel: Snowflake,
    message: Snowflake,
    quote: Option<&(String, Snowflake)>,
    new_content: &str,
    bot: bool,
) -> Option<LogEntry> {
    let mut entry = match quote {
        Some((old, _)) if old == new_content => return None,
        Some((old, author)) => LogEntry::new("message_edit", "the member edited their message")
            .user(*author)
            .was(clamp_chars(old, 900))
            .is_now(clamp_chars(new_content, 900)),
        None => LogEntry::new("message_edit", "edited; the original text was never kept")
            .is_now(clamp_chars(new_content, 900)),
    };
    entry = entry.channel(channel).message(message).bot(bot);
    Some(entry)
}

async fn handle_bulk_delete(
    state: &BotState,
    ctx: &Context,
    guild_id: Option<GuildId>,
    channel: ChannelId,
    ids: Vec<MessageId>,
) {
    let Some(guild_id) = guild_id else { return };
    let guild = Snowflake(guild_id.get());
    if !state.managed.contains(&guild) || ids.is_empty() {
        return;
    }
    let who = ids
        .first()
        .and_then(|m| state.db.recalled_message(guild, Snowflake(m.get())).ok())
        .flatten()
        .map(|(_, author, _)| author);
    let mut e = LogEntry::new(
        "message_bulk_delete",
        format!("{} messages removed at once", ids.len()),
    )
    .channel(Snowflake(channel.get()));
    if let Some(author) = who {
        e = e.user(author);
    }
    logger::note(state, ctx, guild, e).await;
}

/// Is there anything worth remembering about this message? A message with no text and no file has
/// nothing for a delete log to quote, and the cache window is a count (`max_cached_messages`), so
/// letting empty rows into it pushes out the rows somebody would actually read.
fn worth_caching(content: &str, files: usize) -> bool {
    !content.trim().is_empty() || files > 0
}

/// What the transcript keeps about a message's files. The direct URL is what renders; the proxy
/// URL is kept because it is the link that still resolves when Discord rotates the first.
fn attachment_list(msg: &Message) -> String {
    let items: Vec<serde_json::Value> = msg
        .attachments
        .iter()
        .map(|a| {
            let ctype = a.content_type.clone().unwrap_or_default();
            let kind = if ctype.starts_with("image/") {
                "image"
            } else if ctype.starts_with("video/") {
                "video"
            } else if ctype.starts_with("audio/") {
                "audio"
            } else {
                "file"
            };
            serde_json::json!({
                "url": a.url.to_string(),
                "proxyUrl": a.proxy_url.to_string(),
                "name": a.filename,
                "kind": kind,
                "contentType": ctype,
                "size": a.size,
                "width": a.width,
                "height": a.height,
            })
        })
        .collect();
    serde_json::to_string(&items).unwrap_or_else(|_| "[]".into())
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
        // right name. Both placeholders used to be wrong - `{members}` printed the
        // guild id and `{server}` was replaced with nothing, so a greeting read
        // "welcome to , you are member 1043287654321".
        //
        // The block is load-bearing: a cache reference is deliberately `!Send` and
        // `handle_join` is spawned, so the borrow must be gone before the send below.
        // The channel is picked in the order an operator expects from the panel's hint:
        // what they chose, then the server's own system-channel channel, then nothing.
        // Reading it from the cache rather than the API is deliberate - a join storm must
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
        // list for both directions, and the same system-channel fallback the hint promises -
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
    // Which ticket type this is: the row a board button named, or the row whose message the
    // button was on. `None` is `/ticket` from a person, which is not a type at all.
    let panel = args
        .get("panelId")
        .and_then(|v| v.as_i64())
        .and_then(|id| state.db.panel_by_id(guild, id).ok().flatten())
        .or_else(|| {
            args.get("panelMessage")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .and_then(|m| {
                    state
                        .db
                        .panel_for_message(guild, Snowflake(m))
                        .ok()
                        .flatten()
                })
        });
    let subject = args
        .get("subject")
        .and_then(|v| v.as_str())
        // A type that says what it is for needs no subject typed: "Report a player" *is* the
        // subject, and the member only has to press the button.
        .or_else(|| {
            panel
                .as_ref()
                .map(|p| p.subject.as_str())
                .filter(|t| !t.trim().is_empty())
        })
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
    // A type may name its own staff role; the guild's is the fallback, not the override, because
    // "billing" and "report a player" are answered by different people.
    let role = panel
        .as_ref()
        .and_then(|p| p.support_role_id)
        .or(cfg.support_role_id);
    if let Some(role) = role {
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
                panel_id: panel.as_ref().map(|p| p.id),
            },
            &cfg.name_format,
            &tag,
        )
        .map_err(|e| e.to_string())?;
    let _ = channel
        .id
        .edit(&ctx.http, EditChannel::new().name(record.name.clone()))
        .await;

    let server = ctx
        .cache
        .guild(guild.as_u64())
        .map(|g| g.name.clone())
        .unwrap_or_default();
    let intro = ticket_intro(
        &cfg,
        owner,
        &record.reference,
        &subject,
        &server,
        panel.as_ref(),
    );
    let mut components = vec![CreateActionRow::Buttons(ticket_buttons(&cfg))];
    // Link buttons get their own row: Discord caps a row at five buttons, and the three the guild
    // toggles are already on the first one.
    let links = ticket_links(&cfg);
    if !links.is_empty() {
        components.push(CreateActionRow::Buttons(links));
    }
    let message = CreateMessage::new()
        .embed(intro)
        .components(components)
        .allowed_mentions(CreateAllowedMentions::new());
    let _ = channel.id.send_message(&ctx.http, message).await;
    if state
        .db
        .guild_settings(guild)
        .map(|s| s.dms_enabled)
        .unwrap_or(true)
    {
        // The other door, for the member who will not have this channel open in the morning:
        // "your ticket is number 41 and staff were told" is the whole reason a ticket system is
        // calmer than a DM inbox.
        let dm = CreateMessage::new().embed(
            CreateEmbed::new()
                .color(cfg.embed_color)
                .title("Ticket opened")
                .description(clamp_chars(
                    &format!(
                        "Ticket {} is open in {server}. Write in that channel and a moderator will answer there.",
                        record.reference
                    ),
                    900,
                ))
                .field("Subject", clamp_chars(&subject, 200), false),
        );
        if let Ok(user) = UserId::new(owner).to_user(&ctx.http).await {
            if let Ok(dm_channel) = user.create_dm_channel(&ctx.http).await {
                let _ = dm_channel.send_message(&ctx.http, dm).await;
            }
        }
    }
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
    let cfg = state.db.ticket_config(guild).unwrap_or_default();
    let server = ctx
        .cache
        .guild(guild.as_u64())
        .map(|g| g.name.clone())
        .unwrap_or_default();
    // The copies come first, before anything can make the originals unreachable: a guild that
    // deletes closed channels is a guild whose transcripts would otherwise lose every screenshot
    // in them on a timer. A file that cannot be fetched is a gap in one transcript, not a failed
    // close, so nothing here propagates.
    let jobs = keep_plan(&rows, cfg.keep_ticket_files);
    let files = keep_files(state, guild, ticket_id, &jobs).await;
    let (name, text) = aerona_core::transcript::render_with_files(
        &ticket,
        &rows,
        cfg.transcript_format,
        &server,
        &files,
    );

    // Where the file goes. A transcript channel is the guild's answer; without one it is posted
    // into the ticket itself, which is where the people in the conversation are. It used to be that
    // no channel meant no transcript at all, silently, which is the most likely way a guild could
    // conclude that "the transcript htmls don't work".
    let where_to = cfg.transcript_channel_id.unwrap_or(ticket.channel_id);
    let sent = ChannelId::new(where_to.as_u64())
        .send_message(
            &ctx.http,
            CreateMessage::new()
                .embed(ticket_close_card(
                    &cfg,
                    &ticket,
                    &server,
                    rows.len(),
                    &files,
                ))
                .add_file(serenity::all::CreateAttachment::bytes(
                    text.as_bytes().to_vec(),
                    name.clone(),
                )),
        )
        .await;
    if sent.is_err() && cfg.transcript_channel_id.is_some() {
        // The log channel refused the file. Say so where an operator reads, and still leave the
        // transcript with the conversation rather than dropping it.
        tracing::warn!(
            %guild,
            ticket = %ticket.reference,
            channel = %where_to,
            "the transcript channel refused the transcript; posting it in the ticket instead"
        );
        let _ = ChannelId::new(ticket.channel_id.as_u64())
            .send_message(
                &ctx.http,
                CreateMessage::new().add_file(serenity::all::CreateAttachment::bytes(
                    text.into_bytes(),
                    name,
                )),
            )
            .await;
    }
    // The people in the channel get the same ending, in the channel: a ticket that vanishes
    // without a word reads as a moderator deleting evidence.
    let ending = CreateMessage::new().embed(ticket_close_card(
        &cfg,
        &ticket,
        &server,
        rows.len(),
        &files,
    ));
    if cfg.transcript_channel_id.is_some() {
        let _ = ChannelId::new(ticket.channel_id.as_u64())
            .send_message(&ctx.http, ending)
            .await;
    }
    let _ = ChannelId::new(ticket.channel_id.as_u64())
        .edit(
            &ctx.http,
            EditChannel::new().name(format!("closed-{}", ticket.reference)),
        )
        .await;
    // The deadline is the last thing written, so a close that failed halfway leaves a channel
    // nobody scheduled for deletion.
    if let Some(secs) = cfg.channel_delete_delay() {
        let _ = state
            .db
            .schedule_ticket_delete(ticket_id, Some(aerona_core::time::now() + secs));
    } else {
        let _ = state.db.schedule_ticket_delete(ticket_id, None);
    }
    if !jobs.is_empty() && files.len() < jobs.len() {
        tracing::warn!(
            %guild,
            ticket = %ticket.reference,
            kept = files.len(),
            asked = jobs.len(),
            "a transcript went out with some of its files missing"
        );
    }
}

/// One file a transcript should hold, and where to get it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeepJob {
    pub seq: i64,
    pub idx: i64,
    pub url: String,
    pub proxy: String,
    pub name: String,
    pub content_type: String,
    pub width: u32,
    pub height: u32,
    pub size: u64,
}

/// Which files are worth copying, within the ticket's budget.
///
/// Pure, so the limit can be tested without a network. Images only, and only while the ticket has
/// room: a transcript that carried every video clip would be a gigabyte of nobody's evidence, and
/// the picture is the one thing a moderator cannot reconstruct from a sentence about it. Anything
/// else stays a link, and the transcript says so in its footer rather than looking complete.
pub(crate) fn keep_plan(rows: &[serde_json::Value], keep: bool) -> Vec<KeepJob> {
    use aerona_core::models::TicketConfig;
    let mut out = Vec::new();
    if !keep {
        return out;
    }
    let mut spent: u64 = 0;
    for row in rows {
        let seq = row
            .get("seq")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let Some(list) = row.get("attachments").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for (idx, a) in list.iter().enumerate() {
            let get = |k: &str| {
                a.get(k)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            if get("kind") != "image" {
                continue;
            }
            let url = get("url");
            if url.is_empty() {
                continue;
            }
            let size = a
                .get("size")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            // Discord told us how big it is; believe it, because a 40 MB screenshot is not going
            // inside a transcript no matter how the download is capped.
            if size > TicketConfig::FILE_MAX {
                continue;
            }
            if spent.saturating_add(size.max(1)) > TicketConfig::FILES_MAX {
                continue;
            }
            spent += size.max(1);
            out.push(KeepJob {
                seq,
                idx: idx as i64,
                proxy: get("proxyUrl"),
                name: if get("name").is_empty() {
                    "attachment".to_string()
                } else {
                    get("name")
                },
                content_type: if get("contentType").is_empty() {
                    "image/png".to_string()
                } else {
                    get("contentType")
                },
                width: a
                    .get("width")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as u32,
                height: a
                    .get("height")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as u32,
                url,
                size,
            });
        }
    }
    out
}

/// The copies themselves. `proxy` is tried after `url` because it is the link that still resolves
/// when Discord rotates the signature on the first, and both are read with a cap so a server that
/// answers with something enormous cannot spend a transcript's whole budget on one request.
async fn keep_files(
    state: &BotState,
    guild: Snowflake,
    ticket_id: i64,
    jobs: &[KeepJob],
) -> Vec<aerona_core::models::TicketFile> {
    use aerona_core::models::TicketConfig;
    let mut out = Vec::new();
    for job in jobs {
        let mut bytes = match read_capped(&state.http, &job.url, TicketConfig::FILE_MAX).await {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::debug!(%e, file = %job.name, "the direct attachment url failed");
                match read_capped(&state.http, &job.proxy, TicketConfig::FILE_MAX).await {
                    Ok(b) => Some(b),
                    Err(e2) => {
                        tracing::warn!(
                            %e2,
                            file = %job.name,
                            ticket = ticket_id,
                            "could not copy a ticket's file; the transcript will link it instead"
                        );
                        None
                    }
                }
            }
        }
        .unwrap_or_default();
        if bytes.is_empty() {
            continue;
        }
        // The type is what the browser will be told, and a `.png` that is really a jpeg would be a
        // broken picture. Sniff, and let the bytes win over the metadata.
        let ctype = sniff_image_mime(&bytes).unwrap_or(job.content_type.clone());
        if let Err(e) = state.db.put_ticket_file(
            guild, ticket_id, job.seq, job.idx, &job.name, &ctype, job.width, job.height, &bytes,
            &job.url,
        ) {
            tracing::warn!(%e, file = %job.name, "a copied file could not be stored");
            continue;
        }
        out.push(aerona_core::models::TicketFile {
            seq: job.seq,
            idx: job.idx,
            name: job.name.clone(),
            content_type: ctype,
            width: job.width,
            height: job.height,
            bytes: std::mem::take(&mut bytes),
        });
    }
    out
}

/// A GET with a hard ceiling, for a file somebody else's CDN is holding.
async fn read_capped(http: &reqwest::Client, url: &str, max: u64) -> Result<Vec<u8>, String> {
    if url.is_empty() {
        return Err("no url to read".into());
    }
    let resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| format!("the file could not be reached: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("the file server answered {status}"));
    }
    if resp.content_length().is_some_and(|n| n > max) {
        return Err(format!("the file server offered more than {max} bytes"));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("the file did not finish arriving: {e}"))?;
    if bytes.len() as u64 > max {
        return Err(format!("the file server sent more than {max} bytes"));
    }
    Ok(bytes.to_vec())
}

/// The real type of the bytes we are about to put inside a document. Only the formats a transcript
/// inlines are recognised; anything else keeps whatever the metadata said.
fn sniff_image_mime(bytes: &[u8]) -> Option<String> {
    let head = bytes.get(..12)?;
    if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png".into());
    }
    if head.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg".into());
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some("image/gif".into());
    }
    if head.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) {
        return Some("image/webp".into());
    }
    None
}

/// The tokens a ticket card may use. `{server}` is filled from the gateway cache, which is the
/// same source the greeting uses, so the two never disagree about what the server is called.
fn ticket_tokens(
    owner: u64,
    reference: &str,
    subject: &str,
    server: &str,
) -> Vec<(&'static str, String)> {
    vec![
        ("{user}", format!("<@{owner}>")),
        ("{ticket}", reference.to_string()),
        ("{subject}", subject.to_string()),
        ("{server}", server.to_string()),
    ]
}

/// The first thing in a new ticket channel. A guild that set nothing gets the sentence this
/// function carries; a guild that set a title and a body gets exactly those words, which is the
/// difference between a template and a form nobody wants to fill.
pub(crate) fn ticket_intro(
    cfg: &TicketConfig,
    owner: u64,
    reference: &str,
    subject: &str,
    server: &str,
    panel: Option<&aerona_core::models::TicketPanel>,
) -> CreateEmbed {
    // A panel is a ticket type, so it wins over the guild wherever it says something. `None` is
    // the ordinary `/ticket`, and the guild's own card.
    let color = panel.and_then(|p| p.color).unwrap_or(cfg.embed_color);
    let body = match panel
        .map(|p| p.intro.as_str())
        .filter(|t| !t.trim().is_empty())
    {
        Some(text) => text.to_string(),
        None if cfg.default_message.is_empty() => {
            "Write here and a moderator will reply.".to_string()
        }
        None => cfg.default_message.clone(),
    };
    let tokens = ticket_tokens(owner, reference, subject, server);
    let title = if cfg.open_title.is_empty() {
        format!("Ticket {reference} for <@{owner}>")
    } else {
        render_template(&cfg.open_title, &tokens)
    };
    let body = render_template(&body, &tokens);
    let mut embed = CreateEmbed::new()
        .color(color)
        .title(clamp_chars(&title, 240))
        .description(clamp_chars(&body, 1500));
    if !subject.is_empty() {
        embed = embed.field("Subject", clamp_chars(subject, 200), false);
    }
    if !server.is_empty() {
        embed = embed.footer(CreateEmbedFooter::new(format!("Aerona · {server}")));
    }
    embed
}

/// The buttons staff get inside a ticket, with this server's wording where it has any.
fn ticket_buttons(cfg: &TicketConfig) -> Vec<CreateButton> {
    let mut out = Vec::new();
    if cfg.show_close {
        out.push(
            CreateButton::new("aerona:ticket:close")
                .style(ButtonStyle::Danger)
                .label(label_or(&cfg.close_label, "Close ticket")),
        );
    }
    if cfg.show_claim {
        out.push(
            CreateButton::new("aerona:ticket:claim")
                .style(ButtonStyle::Secondary)
                .label(label_or(&cfg.claim_label, "Claim")),
        );
    }
    if cfg.show_transcript {
        out.push(
            CreateButton::new("aerona:ticket:transcript")
                .style(ButtonStyle::Secondary)
                .label(label_or(&cfg.transcript_label, "Transcript")),
        );
    }
    out
}

/// A stored empty label means "the built-in wording", which is what lets a guild go back.
fn label_or(stored: &str, fallback: &str) -> String {
    if stored.trim().is_empty() {
        fallback.to_string()
    } else {
        clamp_chars(stored, 80)
    }
}

/// The card's link buttons. A link that is not http(s) is skipped rather than sent, because one
/// bad button would make Discord refuse the whole message and the ticket would fail to open.
fn ticket_links(cfg: &TicketConfig) -> Vec<CreateButton> {
    cfg.links
        .iter()
        .filter(|l| l.is_usable())
        .take(aerona_core::models::TicketLink::MAX)
        .map(|l| CreateButton::new_link(l.url.clone()).label(label_or(&l.label, "Open")))
        .collect()
}

/// The ending, posted where the conversation was and in the transcript channel.
pub(crate) fn ticket_close_card(
    cfg: &TicketConfig,
    ticket: &aerona_core::models::TicketRecord,
    server: &str,
    count: usize,
    files: &[aerona_core::models::TicketFile],
) -> CreateEmbed {
    let tokens = ticket_tokens(
        ticket.owner_id.as_u64(),
        &ticket.reference,
        &ticket.subject,
        server,
    );
    let title = if cfg.close_title.is_empty() {
        format!("Ticket {} closed", ticket.reference)
    } else {
        render_template(&cfg.close_title, &tokens)
    };
    let body = if cfg.close_body.is_empty() {
        "The transcript is attached. Write in this channel to reopen it.".to_string()
    } else {
        render_template(&cfg.close_body, &tokens)
    };
    let mut embed = CreateEmbed::new()
        .color(cfg.embed_color)
        .title(clamp_chars(&title, 240))
        .description(clamp_chars(&body, 1500))
        .field("Messages", count.to_string(), true);
    // What happens to this channel next, in the channel, at the moment it is decided. A member who
    // was told nothing and comes back to a missing channel experiences that as a bot eating
    // evidence; the same sentence three minutes earlier removes the whole question.
    if let Some(secs) = cfg.channel_delete_delay() {
        embed = embed.field(
            "Channel",
            format!("deleted in {}", human_duration(secs)),
            true,
        );
    }
    if !files.is_empty() {
        embed = embed.field(
            "Kept",
            format!("{} file(s) copied into the transcript", files.len()),
            true,
        );
    }
    embed
}

/// A duration the way a person reads one in a chat message. Seconds below a minute, minutes below a
/// day, days after that - a card that said "10800 seconds" is a card nobody parses.
pub(crate) fn human_duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        return format!("{secs} seconds");
    }
    let mins = secs / 60;
    if mins < 60 {
        return plural(mins, "minute");
    }
    let hours = mins / 60;
    let rest = mins % 60;
    if hours < 24 {
        // The remainder is printed because "1 hour" under a deadline 90 minutes away is a card that
        // is wrong by half an hour, and people read cards as promises.
        return if rest == 0 {
            plural(hours, "hour")
        } else {
            format!("{} {}", plural(hours, "hour"), plural(rest, "minute"))
        };
    }
    let days = hours / 24;
    let rest = hours % 24;
    if rest == 0 || days >= 7 {
        plural(days, "day")
    } else {
        format!("{} {}", plural(days, "day"), plural(rest, "hour"))
    }
}

fn plural(n: i64, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// The text transcript, kept as a name because the API and the tests both reach for it. The
/// renderer lives in `transcript.rs`, next to the HTML and JSON halves that share its row shape.
pub fn render_transcript(
    ticket: &aerona_core::models::TicketRecord,
    rows: Vec<serde_json::Value>,
) -> String {
    aerona_core::transcript::text(ticket, &rows)
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
        if let Err(e) = apply_sticky(state, ctx, &cfg, guild, ch, &sticky).await {
            tracing::debug!(guild = %guild, sticky = sticky.id, error = %e, "sticky not posted");
        }
    }
}

/// Put one sticky where it belongs and record what the bot posted.
///
/// One shape only, because there is only one thing a sticky is for: the message at the bottom of
/// the channel, right now. So the fresh copy is sent, and the old one is deleted after it lands -
/// in that order, because a send that fails leaves the server holding the copy it already has,
/// while a delete first leaves it holding nothing. The switch that used to choose an in-place edit
/// instead is gone from the config (see `StickyConfig::RETIRED_KEYS`), and the payload is built by
/// one function so a sticky written in the dashboard and a sticky anchored to somebody's message
/// cannot drift apart.
pub(crate) async fn apply_sticky(
    state: &BotState,
    ctx: &Context,
    _cfg: &aerona_core::models::StickyConfig,
    guild: Snowflake,
    ch: ChannelId,
    sticky: &aerona_core::models::StickyRecord,
) -> Result<(), String> {
    let previous = sticky.sticky_message_id;
    let sent = ch
        .send_message(&ctx.http, sticky_message(sticky))
        .await
        .map(|m| Snowflake(m.id.get()))
        .map_err(|e| e.to_string())?;
    // `filter` on the id, not on the option: a retry that happens to post the same message it is
    // replacing must not delete the copy it just made.
    if let Some(old) = previous.filter(|old| *old != sent) {
        let _ = ch
            .delete_message(&ctx.http, MessageId::new(old.as_u64()))
            .await;
    }
    let _ = state.db.mark_sticky_run(sticky.id, Some(sent));
    let _ = state.db.bump_stickies(guild, sticky.channel_id);
    Ok(())
}

/// The message a sticky is: the text as it was written, plus a card when one was asked for.
///
/// No pin glyph and no other decoration is added to the operator's sentence. The first version
/// prefixed every repost with one, which read as the bot announcing itself and could not be
/// turned off; the fact that a fresh copy appeared is announcement enough.
fn sticky_message(sticky: &aerona_core::models::StickyRecord) -> CreateMessage {
    let mut msg = CreateMessage::new().allowed_mentions(CreateAllowedMentions::new());
    if !sticky.content.trim().is_empty() {
        msg = msg.content(clamp_chars(&sticky.content, 1900));
    }
    if let Some(e) = sticky_embed(sticky) {
        msg = msg.embed(e);
    }
    msg
}

/// The card half, on its own so a test can ask the one question that matters: does a sticky with
/// a colour and a line of text get a colour *on Discord*.
pub(crate) fn sticky_embed(sticky: &aerona_core::models::StickyRecord) -> Option<CreateEmbed> {
    sticky.has_embed().then(|| {
        let mut e = CreateEmbed::new().color(sticky.color());
        if let Some(title) = sticky.embed_title.as_deref() {
            e = e.title(clamp_chars(title, 240));
        }
        if let Some(body) = sticky.embed_body.as_deref() {
            e = e.description(clamp_chars(body, 1800));
        }
        e
    })
}

/// Post, or repair, the ticket board in one channel: a single message with one button per type.
///
/// One message rather than one per row is the whole point. Three announcements in a channel is
/// three times the noise and no way to see the choices together, and a person reading the channel
/// wants "report, bill, suggest" side by side. Every row is re-bound to this message, which is
/// what makes a click findable: the button's custom id carries the row id.
/// Five buttons per row, because that is all Discord will take in one. Every type in a channel
/// used to go into a single row, so a sixth type turned the whole board into a request Discord
/// refused - and from the dashboard that looks like nothing happened at all.
fn button_rows(buttons: Vec<CreateButton>) -> Vec<CreateActionRow> {
    buttons
        .chunks(5)
        .map(|chunk| CreateActionRow::Buttons(chunk.to_vec()))
        .collect()
}

pub(crate) async fn draw_panels(
    state: &BotState,
    ctx: &Context,
    guild: Snowflake,
    channel: Snowflake,
) -> Result<(), String> {
    let cfg: TicketConfig = state.db.ticket_config(guild).map_err(|e| e.to_string())?;
    let rows = state
        .db
        .panels_for_channel(guild, channel)
        .map_err(|e| e.to_string())?;
    if rows.is_empty() {
        // Every type in the channel was deleted. There is nothing to say about the message that
        // used to hold them: a delete started from a queue would be the bot removing a message
        // nobody has configured any more, which is a worse surprise than a stale button.
        return Ok(());
    }
    let body = if cfg.default_message.trim().is_empty() {
        "Pick what you need help with and a channel will be made for it."
    } else {
        cfg.default_message.trim()
    };
    let mut embed = CreateEmbed::new()
        .color(rows[0].color.unwrap_or(cfg.embed_color))
        .description(clamp_chars(body, 1500));
    let mut buttons = Vec::new();
    for row in &rows {
        let name = match row.emoji.as_deref().filter(|e| !e.trim().is_empty()) {
            Some(emoji) => format!("{emoji} {}", row.label),
            None => row.label.clone(),
        };
        let note = if row.subject.trim().is_empty() {
            "-"
        } else {
            row.subject.as_str()
        };
        embed = embed.field(clamp_chars(&name, 120), clamp_chars(note, 60), true);
        buttons.push(
            CreateButton::new(format!("aerona:ticket:open:{}", row.id))
                .style(ButtonStyle::Primary)
                .label(label_or(&row.label, "Open a ticket")),
        );
    }
    let ch = ChannelId::new(channel.as_u64());
    let message = CreateMessage::new()
        .embed(embed.clone())
        .components(button_rows(buttons.clone()))
        .allowed_mentions(CreateAllowedMentions::new());
    // Reuse the board that is already there when there is one. Two boards in a channel is what a
    // redraw with a stale binding looks like, so the extras are deleted rather than left behind.
    let mut board = None;
    for row in &rows {
        if row.message_id.as_u64() == 0 {
            continue;
        }
        match board {
            None => board = Some(row.message_id),
            Some(first) if first != row.message_id => {
                let _ = ch
                    .delete_message(&ctx.http, MessageId::new(row.message_id.as_u64()))
                    .await;
            }
            _ => {}
        }
    }
    let posted = match board {
        Some(id) => {
            let edit = EditMessage::new()
                .embeds(vec![embed])
                .components(button_rows(buttons))
                .allowed_mentions(CreateAllowedMentions::new());
            match ch
                .edit_message(&ctx.http, MessageId::new(id.as_u64()), edit)
                .await
            {
                Ok(_) => id,
                Err(e) => {
                    // The bound message was deleted by hand. A board that cannot be edited is
                    // sent again, because a channel whose ticket button is gone has no ticket.
                    tracing::debug!(guild = %guild, error = %e, "ticket board is gone, re-posting");
                    ch.send_message(&ctx.http, message)
                        .await
                        .map(|m| Snowflake(m.id.get()))
                        .map_err(|e| e.to_string())?
                }
            }
        }
        None => ch
            .send_message(&ctx.http, message)
            .await
            .map(|m| Snowflake(m.id.get()))
            .map_err(|e| e.to_string())?,
    };
    for row in &rows {
        if row.message_id != posted {
            let _ = state.db.bind_panel(guild, row.id, posted);
        }
    }
    Ok(())
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
        "🚨 **Raid suspected** - {reason}\nA lockdown can be lifted or applied from the dashboard."
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

    /// A sticky with a colour and a line of text is a *card* with that colour. The first version
    /// asked whether a title or a body existed, so the natural move (type a sentence, pick white)
    /// posted the sentence with no embed at all and dropped the colour on the floor.
    #[test]
    fn a_colour_on_its_own_still_makes_a_card() {
        let mut sticky = aerona_core::models::StickyRecord {
            id: 1,
            guild_id: Snowflake(7),
            channel_id: Snowflake(3),
            message_id: Snowflake(0),
            sticky_message_id: None,
            content: "read the rules".into(),
            embed_title: None,
            embed_body: None,
            embed_color: Some(0xff_ff_ff),
            every_messages: 20,
            since_count: 0,
            created_by: Snowflake(5),
            created_at: 0,
            last_run_at: 0,
        };
        let wire =
            serde_json::to_value(sticky_message(&sticky)).expect("a message that serialises");
        let embeds = wire["embeds"].as_array().expect("an embed for the colour");
        assert_eq!(embeds.len(), 1, "one card, not none: {wire}");
        assert_eq!(embeds[0]["color"], 0xff_ff_ff, "{wire}");
        assert_eq!(
            wire["content"], "read the rules",
            "the sentence is posted as it was written"
        );

        // And a sticky that asks for nothing gets no empty card.
        sticky.embed_color = None;
        let bare =
            serde_json::to_value(sticky_message(&sticky)).expect("a message that serialises");
        assert!(
            bare.get("embeds").is_none()
                || bare["embeds"].as_array().map_or(true, |e| e.is_empty()),
            "no colour and no text in a card means no card: {bare}"
        );
    }

    /// The pair is the point of a delete: a row that says only "a message was deleted" tells a
    /// moderator nothing they can act on.
    #[test]
    fn a_deleted_message_quotes_what_the_bot_still_holds() {
        let e = delete_entry(
            Snowflake(3),
            Snowflake(9),
            Some(&("read the rules".to_string(), Snowflake(5))),
            true,
        );
        assert_eq!(e.event, "message_delete");
        assert_eq!(e.before.as_deref(), Some("read the rules"));
        assert!(
            e.after.is_none(),
            "no filler word in its place: {:?}",
            e.after
        );
        assert_eq!(e.user, Some(Snowflake(5)), "the row has to say who said it");

        let bare = delete_entry(Snowflake(3), Snowflake(9), None, true);
        assert!(
            bare.before.is_none() && bare.after.is_none(),
            "no invention"
        );
        assert!(
            bare.summary.contains("before its text"),
            "and the row says why it is empty: {}",
            bare.summary
        );
    }

    /// An edit whose original text the bot never kept is still an edit. This is the half that
    /// used to be able to swallow the event, and the half a moderator notices first.
    #[test]
    fn an_edit_is_logged_even_when_the_original_text_is_gone() {
        let e = edit_entry(Snowflake(3), Snowflake(9), None, "fixed a typo", false)
            .expect("an edit is news");
        assert_eq!(e.event, "message_edit");
        assert_eq!(e.after.as_deref(), Some("fixed a typo"));
        assert!(e.before.is_none());

        let same = edit_entry(
            Snowflake(3),
            Snowflake(9),
            Some(&("x".to_string(), Snowflake(1))),
            "x",
            false,
        );
        assert!(
            same.is_none(),
            "an edit that changed nothing is not an edit"
        );

        let pair = edit_entry(
            Snowflake(3),
            Snowflake(9),
            Some(&("the first text".to_string(), Snowflake(1))),
            "the second text",
            false,
        )
        .expect("both halves");
        assert_eq!(pair.before.as_deref(), Some("the first text"));
        assert_eq!(pair.after.as_deref(), Some("the second text"));
    }

    /// A ticket type is a button *and* a card: its own colour and opening text have to win over
    /// the guild's, or "report a player" and "billing" are the same conversation with different
    /// labels on the button.
    #[test]
    fn a_ticket_type_overrides_the_card_it_opens_with() {
        let cfg = TicketConfig {
            enabled: true,
            embed_color: 0x58_65_f2,
            open_title: "Ticket {ticket} for {server}".into(),
            default_message: "Staff are on their way.".into(),
            ..TicketConfig::default()
        };
        let panel = aerona_core::models::TicketPanel {
            id: 3,
            guild_id: Snowflake(7),
            channel_id: Snowflake(1),
            message_id: Snowflake(0),
            label: "Report a player".into(),
            emoji: Some("\u{1f6a9}".into()),
            support_role_id: Some(Snowflake(9)),
            sort: 0,
            color: Some(0xe7_3c_88),
            subject: "player report".into(),
            intro: "Say who and when, {user}. Quote {ticket}.".into(),
        };
        let v = serde_json::to_value(ticket_intro(
            &cfg,
            123,
            "AER-7",
            "player report",
            "Nine",
            Some(&panel),
        ))
        .expect("a card that serialises");
        assert_eq!(v["color"].as_i64(), Some(0xe7_3c_88), "{v}");
        let body = v["description"].as_str().unwrap_or_default().to_string();
        assert!(body.starts_with("Say who and when"), "{body}");
        assert!(
            body.contains("AER-7"),
            "the type's text gets the same tokens: {body}"
        );
        assert!(
            !body.contains('{'),
            "no placeholder is left on the card: {body}"
        );
        let title = v["title"].as_str().unwrap_or_default().to_string();
        assert_eq!(title, "Ticket AER-7 for Nine", "{v}");

        // Without a type, the guild's card is untouched - the same function, the other branch.
        let plain =
            serde_json::to_value(ticket_intro(&cfg, 123, "AER-7", "", "Nine", None)).unwrap();
        assert_eq!(plain["color"].as_i64(), Some(0x58_65_f2), "{plain}");
        assert_eq!(
            plain["description"].as_str().unwrap_or_default(),
            "Staff are on their way.",
            "{plain}"
        );
    }

    /// A board is drawn from every type in the channel, so the split into rows has to happen
    /// here rather than being somebody's hope about how many types a guild adds.
    #[test]
    fn a_board_spreads_its_buttons_over_rows_of_five() {
        let seven: Vec<CreateButton> = (0..7).map(|i| CreateButton::new(format!("x{i}"))).collect();
        let rows = serde_json::to_value(button_rows(seven)).expect("rows serialise");
        let rows = rows.as_array().expect("a list of rows");
        assert_eq!(rows.len(), 2, "five, then two: {rows:?}");
        assert_eq!(
            rows[0]["components"].as_array().unwrap().len(),
            5,
            "{rows:?}"
        );
        assert_eq!(
            rows[1]["components"].as_array().unwrap().len(),
            2,
            "{rows:?}"
        );
        assert!(button_rows(Vec::new()).is_empty(), "no types, no row");
    }

    /// The control row is built from the switches and the stored wording, and a link the guild
    /// cannot be sent is dropped rather than taking the whole message down with it.
    #[test]
    fn the_control_row_follows_the_switches_and_skips_a_link_discord_refuses() {
        let cfg = TicketConfig {
            show_close: true,
            show_claim: true,
            show_transcript: true,
            close_label: String::new(),
            claim_label: "Take it".into(),
            transcript_label: "Show me".into(),
            links: vec![
                aerona_core::models::TicketLink {
                    label: "Rules".into(),
                    url: "https://example.com/rules".into(),
                },
                aerona_core::models::TicketLink {
                    label: "Nope".into(),
                    url: "ftp://example.com/nope".into(),
                },
            ],
            ..TicketConfig::default()
        };
        let buttons = serde_json::to_string(&ticket_buttons(&cfg)).expect("buttons serialise");
        assert!(
            buttons.contains("Close ticket")
                && buttons.contains("Take it")
                && buttons.contains("Show me"),
            "a blank label means the built-in wording, not an empty button: {buttons}"
        );
        let links = serde_json::to_value(ticket_links(&cfg)).expect("links serialise");
        let kept = links.as_array().expect("a list");
        assert_eq!(kept.len(), 1, "one of two links is a button: {links}");
        assert!(
            kept[0]["url"]
                .as_str()
                .unwrap_or_default()
                .ends_with("/rules"),
            "{links}"
        );

        // Every switch off is a valid card: no row at all, rather than an empty one.
        let off = TicketConfig {
            show_close: false,
            show_claim: false,
            show_transcript: false,
            links: Vec::new(),
            ..TicketConfig::default()
        };
        assert!(
            ticket_buttons(&off).is_empty(),
            "switched off means no buttons"
        );
        assert!(ticket_links(&off).is_empty());
    }

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
            delete_at: None,
            channel_deleted_at: None,
        };
        let rows = vec![
            serde_json::json!({ "at": 1_700_000_100, "author": "kevin", "content": "hello?", "internal": false }),
            serde_json::json!({ "at": 1_700_000_200, "author": "mod", "content": "looking into it", "internal": true }),
        ];
        let text = render_transcript(&ticket, rows);
        assert!(text.contains("AER-ABC123"), "{text}");
        assert!(
            text.contains("[2023-11-14 22:15:00Z] kevin: hello?"),
            "{text}"
        );
        assert!(
            text.contains("mod: looking into it (internal)"),
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

    /// The card is where the complaint lived. The kept text used to be a `Before` field beside an
    /// `After` field reading "(deleted)", so a delete card's body said nothing about the message
    /// and the useful half sat off to the side.
    #[test]
    fn a_delete_card_puts_the_kept_words_in_the_body() {
        let e = delete_entry(
            Snowflake(3),
            Snowflake(9),
            Some(&("read the rules".to_string(), Snowflake(5))),
            true,
        );
        let card = serde_json::to_value(crate::logger::card("Nine", Snowflake(1), &e))
            .expect("embed json");
        let body = card["description"].as_str().unwrap_or_default();
        assert!(body.contains("read the rules"), "body was: {body}");
        let fields = card["fields"].as_array().cloned().unwrap_or_default();
        assert!(
            !fields.iter().any(|f| f["name"].as_str() == Some("After")),
            "a delete has no after half to show: {fields:?}"
        );
        assert!(
            fields.iter().any(|f| f["name"].as_str() == Some("Message")),
            "the jump link is still there: {fields:?}"
        );

        // An edit is the same renderer with two halves, and it must keep them: folding an edit into
        // the body would lose the comparison a moderator opens the card for.
        let edit = LogEntry::new("message_edit", "the member edited their message")
            .channel(Snowflake(3))
            .was("old")
            .is_now("new");
        let card = serde_json::to_value(crate::logger::card("Nine", Snowflake(1), &edit)).unwrap();
        let fields: Vec<serde_json::Value> = card["fields"].as_array().cloned().unwrap_or_default();
        let names: Vec<&str> = fields.iter().filter_map(|f| f["name"].as_str()).collect();
        assert!(
            names.contains(&"Before") && names.contains(&"After"),
            "an edit keeps its pair: {names:?}"
        );
    }

    /// Two ways a delete ends with nothing to quote, and they need different fixes: one is a cache
    /// window somebody set, the other is a tick box in the developer portal. Sharing one sentence
    /// for both is why this went unnoticed.
    #[test]
    fn an_empty_delete_says_which_of_the_two_reasons_it_is() {
        let no_intent = delete_entry(
            Snowflake(3),
            Snowflake(9),
            Some(&("   ".to_string(), Snowflake(5))),
            false,
        );
        assert!(
            no_intent.summary.contains("MESSAGE_CONTENT"),
            "intent off has to be named out loud: {}",
            no_intent.summary
        );
        let blank_but_reading = delete_entry(
            Snowflake(3),
            Snowflake(9),
            Some(&(String::new(), Snowflake(5))),
            true,
        );
        assert!(
            blank_but_reading
                .summary
                .contains("file, a sticker or an embed"),
            "a message with no text is not a message nobody could read: {}",
            blank_but_reading.summary
        );
        assert!(
            !blank_but_reading.summary.contains("MESSAGE_CONTENT"),
            "and it must not blame an intent that is switched on"
        );
        let gone = delete_entry(Snowflake(3), Snowflake(9), None, false);
        assert!(
            gone.summary.contains("MESSAGE_CONTENT"),
            "a missing row while the intent is off says the same true thing: {}",
            gone.summary
        );
    }

    #[test]
    fn a_message_with_no_text_and_no_file_is_not_worth_the_window() {
        assert!(worth_caching("hello", 0), "text is enough");
        assert!(worth_caching("", 1), "a file alone is still a message");
        assert!(!worth_caching("   \n ", 0), "whitespace is nothing");
        assert!(
            !worth_caching("", 0),
            "an empty row would push out a real one"
        );
    }

    /// The keep plan is the only place a close decides which of a ticket's files Aerona copies, and
    /// every one of those decisions shows up in a file a moderator downloads: too greedy and the
    /// transcript is 200 MB and the browser will not open it, too shy and the screenshot they came for
    /// is a link that expired.
    #[test]
    fn the_keep_plan_takes_the_pictures_that_fit_and_nothing_else() {
        use aerona_core::models::TicketConfig;
        let row = |seq: i64, list: serde_json::Value| serde_json::json!({ "seq": seq, "attachments": list });
        let rows = vec![
            row(
                1,
                serde_json::json!([
                    {"kind":"image","url":"https://cdn/1.png","proxyUrl":"https://cdn/proxy/1",
                     "name":"shot.png","contentType":"image/png","size":1200,"width":800,"height":600},
                    {"kind":"file","url":"https://cdn/log.txt","name":"log.txt","size":40},
                    {"kind":"image","url":"","name":"no-url.png","size":10},
                    {"kind":"image","url":"https://cdn/huge.png","name":"huge.png",
                     "size":TicketConfig::FILE_MAX + 1},
                ]),
            ),
            row(2, serde_json::json!([])),
            row(3, serde_json::json!("not a list")),
        ];
        let plan = keep_plan(&rows, true);
        assert_eq!(plan.len(), 1, "only the picture that can be kept: {plan:?}");
        assert_eq!(
            plan[0],
            KeepJob {
                seq: 1,
                idx: 0,
                proxy: "https://cdn/proxy/1".into(),
                name: "shot.png".into(),
                content_type: "image/png".into(),
                width: 800,
                height: 600,
                url: "https://cdn/1.png".into(),
                size: 1200,
            },
            "the row is addressed by (seq, idx), which is how the transcript finds it again"
        );
        assert!(keep_plan(&rows, false).is_empty(), "off means off");

        // The ceiling cuts the *later* pictures, not the whole ticket, and it is believed from
        // Discord's own size rather than discovered by downloading a 40 MB screenshot.
        let many = vec![row(
            1,
            serde_json::Value::Array(
                (0..12)
                    .map(|i| {
                        serde_json::json!({"kind":"image","url":format!("https://cdn/{i}.png"),
                                           "size": TicketConfig::FILE_MAX})
                    })
                    .collect(),
            ),
        )];
        let plan = keep_plan(&many, true);
        assert_eq!(
            plan.len(),
            (TicketConfig::FILES_MAX / TicketConfig::FILE_MAX) as usize,
            "as many as the per-ticket ceiling fits, and no more"
        );

        // A member who sent a picture with no name and no size still gets a row: the defaults are
        // what the store and the renderer need, not what Discord had.
        let bare = vec![row(
            1,
            serde_json::json!([{"kind":"image","url":"https://cdn/x"}]),
        )];
        let plan = keep_plan(&bare, true);
        assert_eq!(plan[0].name, "attachment");
        assert_eq!(plan[0].content_type, "image/png");
        assert_eq!(
            plan[0].size, 0,
            "an unknown size is capped by the download, not refused"
        );
    }

    /// The close card and the panel both say "in 3 days" rather than a timestamp, because the
    /// question a moderator is asking is how long they have, not what epoch it is.
    #[test]
    fn durations_and_counts_read_as_sentences() {
        assert_eq!(human_duration(0), "0 seconds");
        assert_eq!(human_duration(59), "59 seconds");
        assert_eq!(human_duration(60), "1 minute");
        assert_eq!(human_duration(3_600), "1 hour");
        assert_eq!(
            human_duration(5_400),
            "1 hour 30 minutes",
            "the remainder is printed: a deadline 90 minutes away is not '1 hour away'"
        );
        assert_eq!(human_duration(86_400), "1 day");
        assert_eq!(human_duration(90_061), "1 day 1 hour");
        assert_eq!(
            human_duration(30 * 86_400),
            "30 days",
            "a week or more stops naming the hours"
        );
        assert_eq!(
            human_duration(-5),
            "0 seconds",
            "a deadline that has already passed is not negative time"
        );
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(2, "file"), "2 files");
        assert_eq!(plural(0, "message"), "0 messages");
    }

    /// The stored content type comes from the bytes, because a file named `exploit.png` that is a
    /// SVG is exactly the thing an `<img>` inside a downloaded HTML document is not for.
    #[test]
    fn a_picture_is_belonged_to_its_bytes_not_its_name() {
        let png = [0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0];
        assert_eq!(sniff_image_mime(&png).as_deref(), Some("image/png"));
        let jpeg = [0xffu8, 0xd8, 0xff, 0xe0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(sniff_image_mime(&jpeg).as_deref(), Some("image/jpeg"));
        let webp: Vec<u8> = b"RIFFxxxxWEBP"
            .iter()
            .copied()
            .chain(std::iter::repeat(0))
            .take(12)
            .collect();
        assert_eq!(sniff_image_mime(&webp).as_deref(), Some("image/webp"));
        let svg: Vec<u8> = br#"<svg onload="alert(1)">"#.to_vec();
        assert_eq!(
            sniff_image_mime(&svg),
            None,
            "text that claims to be a picture is not one"
        );
        assert_eq!(
            sniff_image_mime(b"\x89PNG"),
            None,
            "too short to be a header is too short to trust"
        );
    }
}
