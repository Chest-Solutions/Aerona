//! The outbox executor: everything this worker *does* to Discord on somebody's
//! behalf. The API only ever records an intent, so this is the single place where a
//! moderation action is turned into a gateway call - which is also what makes a
//! Discord outage survivable instead of fatal.

use serde_json::{json, Value};
use serenity::all::{ChannelId, CreateEmbed, CreateMessage, GuildId, MessageId, UserId};

use aerona_core::db::security::OutboxItem;
use aerona_core::identity::Snowflake;
use aerona_core::time::now;

use crate::clamp_chars;
use crate::notify::{self, Action, Kind};
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
pub(crate) fn is_transient(err: &serenity::Error) -> bool {
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
            let avatar = state
                .db
                .guild_report(item.guild_id, "avatar-pending")
                .map_err(logged_db)?;
            let banner = state
                .db
                .guild_report(item.guild_id, "banner-pending")
                .map_err(logged_db)?;
            let plan = identity_plan(&cfg, avatar.as_ref(), banner.as_ref());
            if plan.is_empty() {
                // Nothing configured is not a failure. It is a save with no effect, and
                // the report is what tells the dashboard so in words.
                let _ = state.db.put_guild_report(
                    item.guild_id,
                    "identity",
                    &json!({ "reportedAt": now(), "ok": true, "skipped": "nothing to apply" }),
                );
                return Ok(());
            }

            // One request per field. Discord keeps these facts in two places, and answers a key the
            // token may not write by refusing the whole body - so a batched apply meant that a
            // banner the account is not allowed to set took the avatar and the bio down with it, and
            // left one error message to explain three fields. Field by field, what landed and what
            // was refused are both things the report can say, and the outbox row fails on the truth
            // rather than on a guess.
            let mut landed: Vec<&'static str> = Vec::new();
            let mut refused = serde_json::Map::new();
            for (half, field, body) in &plan {
                let route = if *half == "member" {
                    serenity::http::Route::GuildMemberMe { guild_id: guild }
                } else {
                    serenity::http::Route::UserMe
                };
                let req = serenity::http::Request::new(route, serenity::http::LightMethod::Patch)
                    .body(Some(body.to_string().into_bytes()));
                match ctx.http.fire::<Value>(req).await {
                    Ok(_) => {
                        landed.push(*field);
                        // Staged bytes are spent once, and only once Discord has taken them: a
                        // refused upload stays where it is so the next apply retries it.
                        if matches!(*field, "avatar" | "banner") {
                            let key = format!("{field}-pending");
                            let _ = state.db.delete_guild_report(item.guild_id, &key);
                        }
                    }
                    Err(e) => {
                        let text = clamp_chars(&e.to_string().replace('\n', " "), 280);
                        refused.insert((*field).to_string(), json!(text));
                    }
                }
            }
            let error = refusal_note(plan.len(), &refused);
            let mut report = json!({
                "reportedAt": now(),
                "ok": error.is_none(),
                // Which half of the API took something, so the page can say "this server" and "the
                // account" instead of implying the two are one switch.
                "member": landed.contains(&"nick"),
                "profile": landed.iter().any(|f| *f != "nick"),
                "fields": landed,
            });
            if !refused.is_empty() {
                report["refused"] = Value::Object(refused);
            }
            if let Some(text) = &error {
                report["error"] = json!(text);
                report["queued"] = json!(item.id);
            }
            let _ = state
                .db
                .put_guild_report(item.guild_id, "identity", &report);
            if let Some(text) = error {
                // Arms here are unit-shaped; the function decides the `Ok`. A refusal is the one
                // case that must not fall through, because the operator has to be able to tell
                // "applied" from "Discord said no", and a failed row is what makes the poll say so.
                return Err(text);
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
            tell(
                state,
                ctx,
                item,
                Kind::Ban,
                user,
                &reason,
                (days > 0).then(|| format!("last {days} day(s) of messages deleted")),
            )
            .await;
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
            tell(state, ctx, item, Kind::SoftBan, user, &reason, None).await;
        }
        "moderate.kick" => {
            let user = user_of(args)?;
            let reason = reason_of(args);
            guild.kick(&ctx.http, user).await.map_err(logged)?;
            tell(state, ctx, item, Kind::Kick, user, &reason, None).await;
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
            let user = user_of(args)?;
            tell(state, ctx, item, Kind::Unban, user, &reason_of(args), None).await;
        }
        "moderate.timeout" => {
            let user = user_of(args)?;
            let minutes = args
                .get("minutes")
                .and_then(Value::as_i64)
                .unwrap_or(10)
                .clamp(1, 40_320);
            let until = crate::gate::rfc3339(now() + minutes * 60);
            let edit = serenity::all::EditMember::new().disable_communication_until(until.clone());
            guild
                .edit_member(&ctx.http, user, edit)
                .await
                .map_err(logged)?;
            tell(
                state,
                ctx,
                item,
                Kind::Timeout,
                user,
                &reason_of(args),
                Some(format!("{minutes} minute(s), until {until}")),
            )
            .await;
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
            // The guild already decides what a ticket card looks like, and a reply is a ticket
            // card: reading the colour from the settings is what makes "I set white" mean something
            // here instead of arriving blurple forever.
            let colour = state
                .db
                .ticket_config(item.guild_id)
                .unwrap_or_default()
                // `TicketConfig` carries a plain u32, so nothing here is "unset": the default the
                // model gives a fresh install is Discord's own blurple.
                .embed_color
                & 0xff_ff_ff;
            let card = CreateEmbed::new()
                .color(colour)
                .title("Support replied")
                .description(clamp_chars(&content, 1800))
                .field("From", item.actor_label.clone(), true)
                .field("Ticket", format!("#{}", ticket.reference), true);
            channel
                .send_message(&ctx.http, CreateMessage::new().embed(card))
                .await
                .map_err(logged)?;
        }
        "raid.lockdown" => {
            let plan = crate::raid::Plan {
                active: args.get("active").and_then(Value::as_bool).unwrap_or(true),
                minutes: args
                    .get("minutes")
                    .and_then(Value::as_i64)
                    .unwrap_or(15)
                    .clamp(1, 1440),
                slowmode_secs: args
                    .get("slowmodeSecs")
                    .and_then(Value::as_i64)
                    .unwrap_or(21_600)
                    .clamp(0, 21_600) as u16,
                raise_verification: args
                    .get("raiseVerification")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                notice: args
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            };
            crate::raid::apply(state, ctx, item.guild_id, plan).await?;
        }
        // The panel's channel and role pickers read this report. Refreshing it on request is the
        // difference between "the bot has not reported this server's channels yet" for five
        // minutes and a list that appears when somebody opens the box.
        "picker.refresh" => {
            let n = crate::tasks::report_picker(state, ctx, item.guild_id, true).await;
            tracing::debug!(guild = %item.guild_id, entries = n, "picker report refreshed");
        }
        // A sticky written in the dashboard should be in the channel, not waiting for
        // `everyMessages` messages of chatter to arrive first. One row of the table is enough to
        // post it: the same `apply_sticky` the count-driven pass uses, so a "post now" and a
        // "post because the channel moved" cannot end up looking different.
        "sticky.post" => {
            let id = args
                .get("sticky")
                .and_then(Value::as_i64)
                .ok_or_else(|| "sticky is missing".to_string())?;
            let cfg = state.db.sticky_config(item.guild_id).map_err(logged_db)?;
            let rows = state
                .db
                .stickies_for_guild(item.guild_id)
                .map_err(logged_db)?;
            let sticky = rows
                .into_iter()
                .find(|r| r.id == id)
                .ok_or("sticky is gone")?;
            let ch = ChannelId::new(sticky.channel_id.as_u64());
            events::apply_sticky(state, ctx, &cfg, item.guild_id, ch, &sticky).await?;
        }
        // Ticket types exist as rows; a row with no button in a channel is a configuration nobody
        // can reach, so creating or deleting one redraws the board it belongs to.
        "panel.post" | "panel.redraw" => {
            let channel = args
                .get("channel")
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| "channel is missing".to_string())?;
            events::draw_panels(state, ctx, item.guild_id, Snowflake(channel)).await?;
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

/// The two cards an action owes: a DM to the member and a card in the mod log. One helper, called
/// after the Discord call that acted, so no arm can forget to tell anybody, and a failure to tell
/// never unwinds the action: the ban has happened, and a member with DMs closed is not a reason to
/// reverse it.
async fn tell(
    state: &BotState,
    ctx: &serenity::all::Context,
    item: &OutboxItem,
    kind: Kind,
    user: UserId,
    reason: &str,
    detail: Option<String>,
) {
    let name = item
        .args
        .get("userLabel")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let a = Action {
        kind,
        target: Snowflake(user.get()),
        target_name: name,
        actor: &item.actor_label,
        reason,
        detail: detail.as_deref(),
        // A purge knows the channel it happened in, and so does a ticket action; for the rest the
        // mod-log channel is the only place the card belongs.
        fallback_channel: None,
    };
    let out = notify::deliver(state, ctx, item.guild_id, &a).await;
    if kind.dms() && !out.dm {
        tracing::debug!(user = %user, "the member could not be DMed about this action");
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

/// The requests one Apply is allowed to make for this guild, in order: which half of Discord's API
/// the field lives in (`member` is this server's copy of the bot, `profile` is the account), the
/// field's name, and the one-key body that writes it.
///
/// A field is `None` or `Some`, and the difference matters: `None` is "nobody has said anything about
/// this", which sends nothing, and `Some("")` is "take it off", which sends a null. Omitting a key
/// and sending a null are different instructions, and conflating them is how clearing a field on the
/// panel leaves it on Discord for a week.
///
/// The pictures come from the staged reports, never from the stored digests: an `avatar_hash` on
/// disk is a note that something was uploaded once, and a hash sent in an image's place is a request
/// Discord fails for reasons no operator can read.
fn identity_plan(
    cfg: &aerona_core::models::IdentityConfig,
    avatar: Option<&Value>,
    banner: Option<&Value>,
) -> Vec<(&'static str, &'static str, Value)> {
    let mut out: Vec<(&'static str, &'static str, Value)> = Vec::new();
    // The guild's own fact goes first. `bio` used to ride along in this body, which is why a saved
    // biography used to appear nowhere: this endpoint has no such key, and Discord ignores what it
    // does not know.
    if let Some(nick) = cfg.nick.as_deref() {
        out.push(("member", "nick", json!({ "nick": written(nick) })));
    }
    for (field, staged) in [("avatar", avatar), ("banner", banner)] {
        let value = match staged {
            Some(v) if v.get("clear").and_then(Value::as_bool).unwrap_or(false) => {
                Some(Value::Null)
            }
            Some(v) => v
                .get("data")
                .and_then(Value::as_str)
                .map(|data| json!(data)),
            None => None,
        };
        if let Some(value) = value {
            out.push(("profile", field, json!({ (field): value })));
        }
    }
    if let Some(bio) = cfg.bio.as_deref() {
        out.push(("profile", "bio", json!({ "bio": written(bio) })));
    }
    for (field, raw) in [
        ("avatar_decoration_id", cfg.avatar_decoration_id.as_deref()),
        ("banner_decoration_id", cfg.banner_decoration_id.as_deref()),
    ] {
        // The id goes as text: the model stores a string rather than a snowflake, because what
        // Discord wants back is the same digits the account owns it by.
        if let Some(id) = raw {
            out.push(("profile", field, json!({ (field): written(id.trim()) })));
        }
    }
    out
}

/// A stored value as Discord is told about it: the text for what to change to, a null for what to
/// take off. The empty string is the clear, because that is what the model keeps for it and what
/// `normalize` promises the panel.
fn written(value: &str) -> Value {
    if value.is_empty() {
        Value::Null
    } else {
        json!(value)
    }
}

/// What the refusals add up to: the one sentence the panel shows and the outbox row carries. `None`
/// means Discord took everything the page asked for. The per-field reasons stay in the report as a
/// map, because a summary that hides which field was refused is the same silence as a batch.
fn refusal_note(asked: usize, refused: &serde_json::Map<String, Value>) -> Option<String> {
    if refused.is_empty() {
        return None;
    }
    let names: Vec<&str> = refused.keys().map(String::as_str).collect();
    let first = refused
        .values()
        .next()
        .and_then(Value::as_str)
        .unwrap_or("no reason given");
    Some(format!(
        "Discord refused {} of {}: {}; it said: {first}",
        names.len(),
        asked,
        names.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_empty_field_clears_it_and_a_missing_one_does_not() {
        use aerona_core::models::IdentityConfig as I;
        assert!(
            identity_plan(&I::default(), None, None).is_empty(),
            "nothing configured sends nothing"
        );
        let cfg = I {
            nick: Some(String::new()),
            bio: Some("keeps to itself".into()),
            ..Default::default()
        };
        let plan = identity_plan(&cfg, None, None);
        let names: Vec<&str> = plan.iter().map(|(_, f, _)| *f).collect();
        assert_eq!(names, ["nick", "bio"], "{plan:?}");
        assert_eq!(plan[0].0, "member", "a nickname is this server's fact");
        assert_eq!(plan[1].0, "profile", "a biography is the account's");
        assert_eq!(
            plan[0].2,
            json!({ "nick": null }),
            "an empty string is a clear, not an omission"
        );
        assert_eq!(plan[1].2, json!({ "bio": "keeps to itself" }), "{plan:?}");
    }

    #[test]
    fn a_staged_picture_is_sent_once_and_a_hash_alone_is_not_a_picture() {
        use aerona_core::models::IdentityConfig as I;
        let cfg = I {
            avatar_hash: Some("sha256:1234567890abcdef".into()),
            ..Default::default()
        };
        assert!(
            identity_plan(&cfg, None, None).is_empty(),
            "a stored hash with no staged bytes must not invent an upload"
        );
        let plan = identity_plan(
            &cfg,
            Some(&json!({ "data": "data:image/png;base64,AAA" })),
            Some(&json!({ "data": "data:image/png;base64,BBB" })),
        );
        let bodies: Vec<&Value> = plan.iter().map(|(_, _, b)| b).collect();
        assert_eq!(
            bodies,
            vec![
                &json!({ "avatar": "data:image/png;base64,AAA" }),
                &json!({ "banner": "data:image/png;base64,BBB" }),
            ],
            "each picture from its own staged slot, in a request of its own: {plan:?}"
        );
        let plan = identity_plan(&cfg, Some(&json!({ "clear": true })), None);
        assert_eq!(
            plan.as_slice(),
            &[("profile", "avatar", json!({ "avatar": null }))][..],
            "removing the avatar is an explicit null, and the banner is left alone: {plan:?}"
        );
    }

    #[test]
    fn a_decoration_is_an_id_or_a_clear_never_a_guess() {
        use aerona_core::models::IdentityConfig as I;
        let cfg = I {
            avatar_decoration_id: Some("1207328054057771069".into()),
            banner_decoration_id: Some("  ".into()),
            ..Default::default()
        };
        let plan = identity_plan(&cfg, None, None);
        let got: Vec<(&str, Value)> = plan.iter().map(|(_, f, b)| (*f, b.clone())).collect();
        assert_eq!(
            got,
            vec![
                (
                    "avatar_decoration_id",
                    json!({ "avatar_decoration_id": "1207328054057771069" })
                ),
                (
                    "banner_decoration_id",
                    json!({ "banner_decoration_id": null })
                ),
            ],
            "a set id is sent, and whitespace-only counts as the operator clearing it: {plan:?}"
        );
        assert!(
            identity_plan(&I::default(), None, None).is_empty(),
            "an account nobody told about a decoration keeps what it has"
        );
    }

    /// Why the apply is one request per field rather than one request per endpoint: a body Discord
    /// refuses has to cost the operator one field, not the whole save.
    #[test]
    fn a_field_travels_alone_and_a_refusal_names_only_itself() {
        use aerona_core::models::IdentityConfig as I;
        let plan = identity_plan(
            &I {
                nick: Some("Nine".into()),
                bio: Some(String::new()),
                avatar_decoration_id: Some("12".into()),
                ..Default::default()
            },
            Some(&json!({ "data": "data:image/png;base64,AAA" })),
            None,
        );
        assert_eq!(plan.len(), 4, "{plan:?}");
        for (half, field, body) in &plan {
            let keys = body.as_object().expect("an object body");
            assert_eq!(
                keys.len(),
                1,
                "{half}.{field} travels alone, so a refusal cannot take the others with it: {body}"
            );
            assert!(keys.contains_key(*field), "the key is the field: {body}");
        }

        let mut refused = serde_json::Map::new();
        refused.insert(
            "banner".into(),
            json!("This endpoint requires an OAuth2 token"),
        );
        let note = refusal_note(plan.len(), &refused).expect("a note when something was refused");
        assert!(note.contains("banner"), "{note}");
        assert!(
            !note.contains("nick") && !note.contains("bio"),
            "a refusal must not accuse the fields that landed: {note}"
        );
        assert!(note.contains("1 of 4"), "and say how much was lost: {note}");
        assert!(
            refusal_note(1, &serde_json::Map::new()).is_none(),
            "nothing refused is not an error"
        );
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
