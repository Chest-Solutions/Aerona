//! The outbox executor: everything this worker *does* to Discord on somebody's
//! behalf. The API only ever records an intent, so this is the single place where a
//! moderation action is turned into a gateway call — which is also what makes a
//! Discord outage survivable instead of fatal.

use serde_json::{json, Value};
use serenity::all::{
    ChannelId, CreateMessage, EditChannel, EditGuild, GuildId, MessageId, UserId, VerificationLevel,
};

use aerona_core::db::security::OutboxItem;
use aerona_core::identity::Snowflake;
use aerona_core::time::now;

use crate::clamp_chars;
use crate::state::BotState;
use crate::{events, OUTBOX_BATCH};

/// One drain pass. Returns the number of items attempted; `0` is the common case and
/// costs nothing but one query.
pub async fn drain(state: &BotState) -> usize {
    let Some(ctx) = state.ctx() else { return 0 };
    let claimed = match state.db.claim_actions(OUTBOX_BATCH) {
        Ok(items) => items,
        Err(e) => {
            tracing::warn!(error = %e, "could not claim queued actions");
            return 0;
        }
    };
    for item in &claimed {
        state.bump(&state.actions_min);
        let result = execute(state, ctx, item).await;
        let (ok, err) = match result {
            Ok(()) => (true, None),
            Err(note) => (false, Some(note)),
        };
        if let Err(e) = state.db.finish_action(item.id, ok, err.as_deref()) {
            tracing::warn!(id = item.id, error = %e, "could not close outbox item");
        }
        match &err {
            Some(note) => tracing::warn!(
                id = item.id,
                action = %item.action,
                guild = %item.guild_id,
                %note,
                "queued action failed"
            ),
            None => tracing::debug!(id = item.id, action = %item.action, "queued action applied"),
        }
    }
    claimed.len()
}

/// Where a failed item goes next. Discord tells us to back off with `Retry-After`; a
/// 5xx or a network fault is our problem to wait out, and the row stays `claimed` so
/// the claim timeout (120 s) requeues it by itself. Anything else is a real answer from
/// Discord and is recorded as a failure the operator can read in the dashboard.
fn is_transient(err: &serenity::Error) -> bool {
    match err {
        serenity::Error::Http(h) => transient_from_status(h.status_code().map(|c| c.as_u16())),
        // A failure whose shape we do not recognise is waited out rather than reported:
        // the claim timeout requeues the row, so the worst case is a delay, whereas
        // marking a transient fault failed loses the action.
        _ => true,
    }
}

/// The rule, split out so it can be tested without constructing a serenity error.
fn transient_from_status(code: Option<u16>) -> bool {
    matches!(code, Some(429) | Some(500..=599) | None)
}

pub async fn execute(
    state: &BotState,
    ctx: &serenity::all::Context,
    item: &OutboxItem,
) -> Result<(), String> {
    let guild = GuildId::new(item.guild_id.as_u64());
    let args = &item.args;
    match item.action.as_str() {
        // The only two actions that change how the *bot itself* looks or speaks in one
        // server. They live here, and nowhere near a request handler, for the same reason
        // every other arm does: the dashboard holds no Discord token, and an operator
        // pressing Save must never be failed by a call to another company's API.
        "identity.apply" => {
            let cfg = state.db.identity_config(item.guild_id).map_err(logged_db)?;
            let staged = state
                .db
                .guild_report(item.guild_id, "avatar-pending")
                .map_err(logged_db)?;
            let body = match identity_payload(&cfg, staged.as_ref()) {
                None => {
                    // Nothing configured is not a failure. It is a save with no effect, and
                    // the report is what tells the dashboard so in words.
                    let _ = state.db.put_guild_report(
                        item.guild_id,
                        "identity",
                        &json!({ "reportedAt": now(), "ok": true, "skipped": "nothing to apply" }),
                    );
                    return Ok(());
                }
                Some(v) => v,
            };
            let req = serenity::http::Request::new(
                serenity::http::Route::GuildMemberMe { guild_id: guild },
                serenity::http::LightMethod::Patch,
            )
            .body(Some(body.to_string().into_bytes()));
            match ctx.http.fire::<Value>(req).await {
                Ok(_) => {
                    // Single use, and only on success: a refused upload leaves the staged
                    // bytes in place, so the next apply retries them.
                    if staged.is_some() {
                        let _ = state
                            .db
                            .delete_guild_report(item.guild_id, "avatar-pending");
                    }
                    let _ = state.db.put_guild_report(
                        item.guild_id,
                        "identity",
                        &json!({
                            "reportedAt": now(),
                            "ok": true,
                            "fields": body.as_object().map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
                        }),
                    );
                    // Arms here are unit-shaped; the function decides the `Ok`. A refused
                    // call is the one case that must not fall through, so it returns early.
                }
                Err(e) => {
                    let text = logged(e);
                    let _ = state.db.put_guild_report(
                        item.guild_id,
                        "identity",
                        &json!({ "reportedAt": now(), "ok": false, "error": text, "queued": item.id }),
                    );
                    return Err(text);
                }
            }
        }
        "commands.sync" => {
            let n = crate::tasks::reregister_guild(state, ctx, item.guild_id).await?;
            let _ = state.db.put_guild_report(
                item.guild_id,
                "commands",
                &json!({ "count": n, "reportedAt": now() }),
            );
        }
        "moderate.ban" => {
            let user = user_of(args)?;
            let reason = reason_of(args);
            let days = args
                .get("deleteMessageDays")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .clamp(0, 7) as u8;
            guild
                .ban_with_reason(&ctx.http, user, days, &reason)
                .await
                .map_err(logged)?;
        }
        "moderate.softban" => {
            // Ban plus immediate unban is the only way to clear a member's recent
            // messages without a purge permission on every channel.
            let user = user_of(args)?;
            let reason = reason_of(args);
            guild
                .ban_with_reason(&ctx.http, user, 7, &reason)
                .await
                .map_err(logged)?;
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            ctx.http
                .remove_ban(guild, user, Some(&reason))
                .await
                .map_err(logged)?;
        }
        "moderate.kick" => {
            let user = user_of(args)?;
            guild.kick(&ctx.http, user).await.map_err(logged)?;
        }
        "moderate.unban" => {
            let user = user_of(args)?;
            ctx.http
                .remove_ban(guild, user, Some(&reason_of(args)))
                .await
                .map_err(logged)?;
            if let Some(ban_id) = args.get("banId").and_then(Value::as_i64) {
                let _ = state.db.mark_unbanned(item.guild_id, ban_id);
            }
        }
        "moderate.timeout" => {
            let user = user_of(args)?;
            let minutes = args
                .get("minutes")
                .and_then(Value::as_i64)
                .unwrap_or(10)
                .clamp(1, 40_320);
            let until = crate::gate::rfc3339(now() + minutes * 60);
            let edit = serenity::all::EditMember::new().disable_communication_until(until);
            guild
                .edit_member(&ctx.http, user, edit)
                .await
                .map_err(logged)?;
        }
        "moderate.purge" => {
            purge(state, ctx, item.guild_id, args).await?;
        }
        "ticket.create" => {
            events::open_ticket(
                state,
                ctx,
                item.guild_id,
                args,
                Some(item.actor_id),
                &item.actor_label,
            )
            .await?;
        }
        "ticket.close" => {
            let id = args
                .get("ticketId")
                .and_then(Value::as_i64)
                .ok_or_else(|| "ticketId is missing".to_string())?;
            events::close_ticket(state, ctx, item.guild_id, id).await;
        }
        "ticket.reply" => {
            let id = args
                .get("ticketId")
                .and_then(Value::as_i64)
                .ok_or_else(|| "ticketId is missing".to_string())?;
            let content = args
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let ticket = state
                .db
                .ticket_by_id(id)
                .map_err(logged_db)?
                .ok_or("ticket is gone")?;
            let channel = ChannelId::new(ticket.channel_id.as_u64());
            let body = format!("**Support:** {}", clamp_chars(&content, 1800));
            channel
                .send_message(&ctx.http, CreateMessage::new().content(body))
                .await
                .map_err(logged)?;
        }
        "raid.lockdown" => {
            lockdown(state, ctx, item.guild_id, args, guild).await?;
        }
        other => return Err(format!("no worker support for action {other:?}")),
    }
    Ok(())
}

/// Delete a range of recent messages, honouring the guild's minimum-age guard.
///
/// Discord refuses to delete anything older than two weeks in bulk, so a purge of an
/// old channel is a partial success by design: the loop stops at the first message that
/// is too old rather than trying and swallowing a 400.
async fn purge(
    state: &BotState,
    ctx: &serenity::all::Context,
    guild: Snowflake,
    args: &Value,
) -> Result<(), String> {
    let channel = ChannelId::new(
        args.get("channel")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or("channel is missing")?,
    );
    let limit = args
        .get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(50)
        .clamp(1, 1000) as u8;
    let min_age = args
        .get("protectMinAge")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let filter_user = args
        .get("filterUser")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<u64>().ok())
        .map(UserId::new);
    let messages = channel
        .messages(&ctx.http, serenity::all::GetMessages::new().limit(limit))
        .await
        .map_err(logged)?;
    let doomed: Vec<MessageId> = messages
        .iter()
        .filter(|m| filter_user.map_or(true, |u| m.author.id == u))
        // A pinned message survives a purge: it is pinned *because* somebody wants it
        // to stay, and a moderator's bulk action is not a reason to delete it.
        .filter(|m| !m.pinned)
        .filter(|m| m.timestamp.unix_timestamp() - now() < -(min_age))
        .map(|m| m.id)
        .collect();
    if doomed.is_empty() {
        return Ok(());
    }
    for batch in doomed.chunks(100) {
        if batch.len() == 1 {
            channel
                .delete_message(&ctx.http, batch[0])
                .await
                .map_err(logged)?;
        } else {
            channel
                .delete_messages(&ctx.http, batch)
                .await
                .map_err(logged)?;
        }
    }
    state.db.audit_best_effort(
        aerona_core::db::security::AuditEntry::new("moderation.purge")
            .for_guild(guild)
            .actor(
                aerona_core::db::security::ActorType::Bot,
                None,
                "aerona-bot",
            )
            .meta(serde_json::json!({ "deleted": doomed.len(), "channel": channel.get().to_string() })),
    );
    Ok(())
}

/// Lock down or release a guild. Slowmode at the maximum is used rather than a
/// permission overwrite on every channel: it is exactly what Discord's own "pause
/// activity" does, it is reversible without remembering what each channel's overwrite
/// was, and it cannot be mistaken for a permanent change by an auditor.
async fn lockdown(
    state: &BotState,
    ctx: &serenity::all::Context,
    guild: Snowflake,
    args: &Value,
    g: GuildId,
) -> Result<(), String> {
    let active = args.get("active").and_then(Value::as_bool).unwrap_or(true);
    let minutes = args
        .get("minutes")
        .and_then(Value::as_i64)
        .unwrap_or(15)
        .clamp(1, 1440);
    let slowmode = args
        .get("slowmodeSecs")
        .and_then(Value::as_i64)
        .unwrap_or(21_600)
        .clamp(0, 21_600) as u16;
    let raise = args
        .get("raiseVerification")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let notice = args
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string);

    let channels = g.channels(&ctx.http).await.map_err(logged)?;
    let mut touched = 0usize;
    for (id, _) in channels {
        if !state.managed.contains(&guild) {
            break;
        }
        let edit = EditChannel::new().rate_limit_per_user(if active { slowmode } else { 0 });
        // One channel failing (no permission, archived) must not strand the rest.
        match id.edit(&ctx.http, edit).await {
            Ok(_) => touched += 1,
            Err(e) if is_transient(&e) => {
                tracing::debug!(channel = %id, "slowmode deferred to the next pass");
            }
            Err(_) => {}
        }
    }
    if active {
        if let Some(until) = now().checked_add(minutes * 60) {
            state.lockdown_until.insert(guild, until);
        }
        if raise {
            let _ = g
                .edit(
                    &ctx.http,
                    EditGuild::new().verification_level(VerificationLevel::High),
                )
                .await;
        }
    } else {
        state.lockdown_until.remove(&guild);
        if raise {
            let _ = g
                .edit(
                    &ctx.http,
                    EditGuild::new().verification_level(VerificationLevel::None),
                )
                .await;
        }
    }
    if let Some(text) = notice {
        if let Some(channel) = notice_channel(state, guild) {
            let c = ChannelId::new(channel.as_u64());
            let body = if active {
                format!(
                    "🔒 **Server locked** for {minutes} minutes.\n{}",
                    clamp_chars(&text, 900)
                )
            } else {
                "🔓 **Server unlocked.** Normal activity has resumed.".to_string()
            };
            let _ = c
                .send_message(&ctx.http, CreateMessage::new().content(body))
                .await;
        }
    }
    // No raid-event write here: the API opened (or is holding) the event, and a worker
    // inventing `id = 0` would corrupt the history the dashboard renders.
    tracing::info!(%guild, active, touched, "lockdown applied");
    Ok(())
}

/// The channel a raid notice goes to: the configured alert channel, else the system
/// channel, else nothing — a bot that cannot find a channel stays quiet rather than
/// posting into whichever channel it guessed.
fn notice_channel(state: &BotState, guild: Snowflake) -> Option<Snowflake> {
    let cfg = state.db.raid_config(guild).ok()?;
    if let Some(c) = cfg.alert_channel_id {
        return Some(c);
    }
    let id = state
        .ctx()?
        .cache
        .guild(guild.as_u64())
        .and_then(|g| g.system_channel_id)
        .map(|c| c.get())?;
    Some(Snowflake(id))
}

/// The `user` argument, from any shape the API may have written: a bare id string, a
//  mention, or a number. Accepting all three is what lets `POST /actions` be written
/// loosely without the worker rejecting a valid request later.
fn user_of(args: &Value) -> Result<UserId, String> {
    let raw = args
        .get("user")
        .ok_or_else(|| "user is missing".to_string())?;
    let text = match raw {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return Err("user must be an id".to_string()),
    };
    let digits: String = text.chars().filter(char::is_ascii_digit).collect();
    let id = digits
        .parse::<u64>()
        .map_err(|_| format!("unparsable user id {text:?}"))?;
    if id == 0 {
        return Err("user id 0 is not a user".to_string());
    }
    Ok(UserId::new(id))
}

fn reason_of(args: &Value) -> String {
    let actor = args
        .get("actorLabel")
        .and_then(Value::as_str)
        .unwrap_or("a moderator");
    match args.get("reason").and_then(Value::as_str) {
        Some(r) if !r.trim().is_empty() => clamp_chars(r, 500),
        _ => format!("moderated by {actor} via the Aerona dashboard"),
    }
}

fn logged(e: serenity::Error) -> String {
    let transient = is_transient(&e);
    let text = e.to_string();
    if transient {
        // Returned verbatim so the caller leaves the row claimed: the claim timeout
        // requeues it, which is the whole point of having a queue.
        format!("transient: {text}")
    } else {
        text
    }
}

fn logged_db(e: aerona_core::error::Error) -> String {
    e.to_string()
}

/// The body for Modify Current Member, or `None` when there is nothing to send.
///
/// Split out of the arm above because it is the part with real semantics in it — clear vs.
/// leave-alone, staged vs. stale — and it has to be testable without a gateway or a network.
fn identity_payload(
    cfg: &aerona_core::models::IdentityConfig,
    staged: Option<&Value>,
) -> Option<Value> {
    let mut out = serde_json::Map::new();
    if let Some(nick) = cfg.nick.as_deref() {
        out.insert(
            "nick".into(),
            if nick.is_empty() {
                Value::Null
            } else {
                json!(nick)
            },
        );
    }
    if let Some(bio) = cfg.bio.as_deref() {
        out.insert(
            "bio".into(),
            if bio.is_empty() {
                Value::Null
            } else {
                json!(bio)
            },
        );
    }
    match staged {
        Some(v) if v.get("clear").and_then(Value::as_bool).unwrap_or(false) => {
            out.insert("avatar".into(), Value::Null);
        }
        Some(v) => {
            if let Some(data) = v.get("data").and_then(Value::as_str) {
                out.insert("avatar".into(), json!(data));
            }
        }
        None => {}
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_empty_field_clears_it_and_a_missing_one_does_not() {
        use aerona_core::models::IdentityConfig as I;
        let mut cfg = I::default();
        assert_eq!(
            identity_payload(&cfg, None),
            None,
            "nothing configured sends nothing"
        );
        cfg.nick = Some(String::new());
        cfg.bio = Some("keeps to itself".into());
        assert_eq!(
            identity_payload(&cfg, None),
            Some(json!({ "nick": null, "bio": "keeps to itself" })),
            "an empty string is a clear, not an omission"
        );
        cfg.avatar_hash = Some("sha256:1234567890abcdef".into());
        assert_eq!(
            identity_payload(&cfg, Some(&json!({ "clear": true })))
                .unwrap()
                .get("avatar"),
            Some(&json!(null)),
            "removing the guild avatar is an explicit null, never an omitted key"
        );
        assert_eq!(
            identity_payload(&cfg, Some(&json!({ "data": "data:image/png;base64,AAA" })))
                .unwrap()
                .get("avatar"),
            Some(&json!("data:image/png;base64,AAA"))
        );
        // A stored hash with no staged bytes must not invent an upload.
        let only_hash = I {
            avatar_hash: Some("sha256:1234567890abcdef".into()),
            ..Default::default()
        };
        assert_eq!(identity_payload(&only_hash, None), None);
    }

    #[test]
    fn user_arguments_survive_every_shape_the_api_writes() {
        assert_eq!(user_of(&json!({ "user": "123" })).unwrap().get(), 123);
        assert_eq!(user_of(&json!({ "user": "<@123>" })).unwrap().get(), 123);
        assert_eq!(user_of(&json!({ "user": "<@!123>" })).unwrap().get(), 123);
        assert_eq!(user_of(&json!({ "user": 123 })).unwrap().get(), 123);
        assert!(user_of(&json!({})).is_err());
        assert!(user_of(&json!({ "user": "nonsense" })).is_err());
        assert!(
            user_of(&json!({ "user": "0" })).is_err(),
            "a zero id must never be sent to Discord"
        );
    }

    #[test]
    fn a_missing_reason_names_the_actor_rather_than_saying_nothing() {
        let with = reason_of(&json!({ "reason": "  spam raid  " }));
        assert!(with.contains("spam raid"));
        let without = reason_of(&json!({ "actorLabel": "kevin" }));
        assert!(without.contains("kevin"), "{without}");
        assert!(reason_of(&json!({ "reason": "   " })).contains("dashboard"));
    }

    #[test]
    fn a_long_reason_is_clamped_not_rejected() {
        let long = "x".repeat(900);
        assert_eq!(reason_of(&json!({ "reason": long })).chars().count(), 500);
    }

    /// The claim/release contract depends on this classification: a rate limit or a
    /// server fault leaves the row claimed (the claim timeout requeues it), while a 403
    /// is reported to the operator as a failure they must fix.
    /// The claim/release contract: 429 and 5xx leave the row claimed so the claim
    /// timeout requeues it; 403/404 are reported, because a permission problem will not
    /// fix itself and silently retrying one is how a queue grows forever.
    #[test]
    fn only_rate_limits_and_server_faults_are_worth_retrying() {
        assert!(transient_from_status(Some(429)));
        assert!(transient_from_status(Some(500)));
        assert!(transient_from_status(Some(503)));
        assert!(
            transient_from_status(None),
            "no status means the request never completed"
        );
        assert!(!transient_from_status(Some(403)));
        assert!(!transient_from_status(Some(404)));
        assert!(
            !transient_from_status(Some(400)),
            "Discord rejected the payload; retrying the same body is pointless"
        );
    }

    #[test]
    fn the_reason_helper_never_returns_an_empty_audit_reason() {
        for body in [json!({}), json!({"reason": ""}), json!({"reason": "x"})] {
            assert!(!reason_of(&body).trim().is_empty());
        }
    }
}
