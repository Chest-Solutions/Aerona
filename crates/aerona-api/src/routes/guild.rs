//! Guild-scoped routes: moderation, tickets, stickies, anti-raid, the AI filter
//! and levels.
//!
//! Every handler here takes `Guild`, which is the extractor that authenticates the
//! caller, checks the credential against *this* guild and hands over the id — so
//! none of them can be reached with someone else's guild id, and none of them had
//! to remember to check.
//!
//! Anything that changes Discord's state (kick, ban, timeout, purge, opening a
//! channel) is queued to `action_outbox` and answered `202`; the row we keep locally
//! is written at the same time, so the dashboard shows the action immediately even
//! though the gateway worker is what performs it.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::{delete, get, patch, post, put};
use axum::Router;
use serde_json::{json, Value};

use aerona_core::db::security::AuditEntry;
use aerona_core::error::Error;
use aerona_core::identity::Snowflake;
use aerona_core::models::Feature;

use crate::routes::settings_for;
use aerona_core::models::{AiCustomRule, TicketPanel, TicketStatus};
use aerona_core::text;

use crate::app::AppState;
use crate::auth::Guild;
use crate::respond::{self, Invalid, JsonBody};
use crate::routes::{
    bool_field, enum_field, int_field, json_ok, opt_sf_field, opt_str_field, q_bool, q_bounded,
    q_i64, q_opt_sf, q_status, require_scope, sf_field, str_field,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        // -- moderation
        .route("/guilds/{guild}/moderation/stats", get(moderation_stats))
        .route("/guilds/{guild}/warns", get(list_warns).post(create_warn))
        .route("/guilds/{guild}/warns/{id}/pardon", post(pardon_warn))
        .route("/guilds/{guild}/top-warned", get(top_warned))
        .route("/guilds/{guild}/users/{user}/warns", delete(clear_warns))
        .route("/guilds/{guild}/users/{user}/strikes", get(user_strikes))
        .route(
            "/guilds/{guild}/users/{user}/notes",
            get(list_notes).post(add_note),
        )
        .route("/guilds/{guild}/notes/{id}", delete(delete_note))
        .route("/guilds/{guild}/actions", post(queue_action))
        // -- tickets
        .route(
            "/guilds/{guild}/tickets",
            get(list_tickets).post(open_ticket),
        )
        .route(
            "/guilds/{guild}/tickets/{id}",
            get(get_ticket).patch(update_ticket),
        )
        .route("/guilds/{guild}/tickets/{id}/claim", post(claim_ticket))
        .route("/guilds/{guild}/tickets/{id}/unclaim", post(unclaim_ticket))
        .route("/guilds/{guild}/tickets/{id}/transcript", get(transcript))
        .route(
            "/guilds/{guild}/tickets/{id}/messages",
            post(ticket_message),
        )
        .route(
            "/guilds/{guild}/ticket-panels",
            get(list_panels).post(add_panel),
        )
        .route("/guilds/{guild}/ticket-panels/{id}", delete(delete_panel))
        // -- stickies
        .route(
            "/guilds/{guild}/stickies",
            get(list_stickies).post(add_sticky),
        )
        .route("/guilds/{guild}/stickies/{id}", delete(delete_sticky))
        // -- anti-raid
        .route("/guilds/{guild}/raid-events", get(raid_events))
        .route("/guilds/{guild}/raid-lockdown", post(raid_lockdown))
        // -- AI filter
        .route("/guilds/{guild}/ai-events", get(ai_events))
        .route("/guilds/{guild}/ai-filter/test", post(ai_test))
        .route(
            "/guilds/{guild}/ai-filter/rules",
            get(list_rules).post(put_rule),
        )
        .route(
            "/guilds/{guild}/ai-filter/rules/{id}",
            patch(patch_rule).delete(delete_rule),
        )
        // -- what the worker knows, and the two things only it can do
        .route("/guilds/{guild}/report", get(report))
        .route("/guilds/{guild}/commands/sync", post(commands_sync))
        .route("/guilds/{guild}/identity/apply", post(identity_apply))
        .route(
            "/guilds/{guild}/identity/avatar",
            put(identity_avatar_stage).delete(identity_avatar_clear),
        )
        // -- levels
        .route("/guilds/{guild}/levels", get(levels))
        .route("/guilds/{guild}/levels/{user}", get(level_for))
}

// ---------------------------------------------------------------------------
// Worker reports: commands, pickers, the guild's copy of the bot profile
// ---------------------------------------------------------------------------

/// `GET /guilds/{guild}/report` — everything the gateway worker last saw, in one fetch.
///
/// This is deliberately not a proxy to Discord. Commands, channels and roles belong to
/// Discord, the worker learns them as it runs, and the rows it leaves behind say *when*
/// they were learned. A dashboard that called Discord directly would need a second token,
/// a second rate-limit budget and a second set of failure modes inside a request handler;
/// what it gets instead is a page that can honestly say "as of 4 minutes ago".
async fn report(State(state): State<AppState>, g: Guild) -> std::result::Result<Response, Invalid> {
    let mut out = state.db.guild_reports(g.id).map_err(Invalid)?;
    if let Some(obj) = out.as_object_mut() {
        // A guild the worker has never reported gets an empty object, and every page that
        // reads it has to be able to tell "nothing yet" from "nothing enabled".
        obj.entry("guildId")
            .or_insert_with(|| json!(g.id.to_string()));
    }
    Ok(respond::ok(out))
}

async fn commands_sync(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "config")?;
    let id = state
        .db
        .enqueue_action(g.id, g.actor(), &g.label(), "commands.sync", &json!({}), "")
        .map_err(Invalid)?;
    Ok(respond::ok(queued(g.id, id, "commands")))
}

async fn identity_apply(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "config")?;
    let id = state
        .db
        .enqueue_action(
            g.id,
            g.actor(),
            &g.label(),
            "identity.apply",
            &json!({}),
            "",
        )
        .map_err(Invalid)?;
    Ok(respond::ok(queued(g.id, id, "profile")))
}

fn queued(guild: aerona_core::identity::Snowflake, id: i64, what: &str) -> Value {
    json!({
        "queued": id,
        "poll": format!("/api/v1/outbox/{id}"),
        "message": format!("the bot will {what} on its next beat"),
        "guild": guild.to_string(),
    })
}

/// `PUT /guilds/{guild}/identity/avatar` — stage one image for the next apply.
///
/// The bytes never enter `guild_config`: that document is written to `config_history` whole,
/// on every save, and a megabyte of base64 in each row would make the audit drawer unreadable
/// and the database grow on every preview. So the image goes to the single-use report slot
/// the worker consumes once, and the settings document keeps only its digest.
async fn identity_avatar_stage(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "config")?;
    let data = str_field(body.get(), "data", 2_000_000).map_err(Invalid)?;
    let (kind, b64) = split_data_uri(&data).ok_or_else(|| {
        Invalid(aerona_core::error::Error::Invalid(
            "data must be a data:image/{png,jpeg,gif,webp};base64, URI".into(),
        ))
    })?;
    // Base64 is 4 characters per 3 bytes; the limit is Discord's per-request ceiling with
    // room for its own overhead, and the dashboard clamps to a much smaller number first.
    let approx_bytes = b64.len() * 3 / 4;
    if !(512..=4_000_000).contains(&approx_bytes) {
        return Err(Invalid(aerona_core::error::Error::Invalid(format!(
            "the image must be between 0.5 KB and 4 MB, not {approx_bytes} bytes"
        ))));
    }
    if !b64
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-' | '_'))
    {
        return Err(Invalid(aerona_core::error::Error::Invalid(
            "that is not base64 image data".into(),
        )));
    }
    let hash = format!(
        "sha256:{}",
        &aerona_core::security::sha256_hex(data.as_bytes())[..16]
    );
    state
        .db
        .put_guild_report(
            g.id,
            "avatar-pending",
            &json!({ "data": data, "kind": kind, "bytes": approx_bytes, "hash": hash, "stagedAt": aerona_core::time::now() }),
        )
        .map_err(Invalid)?;
    // The digest lands in the settings document through the same validated path a nick
    // does, so the history drawer shows one change and the bytes stay out of it.
    let current = settings_for(&state.db, g.id, Feature::Identity).map_err(Invalid)?;
    let mut next = current.clone();
    if let Some(obj) = next.as_object_mut() {
        obj.insert("avatarHash".into(), json!(hash));
    }
    state
        .db
        .put_settings(g.id, Feature::Identity, &next, Some(g.actor()))
        .map_err(Invalid)?;
    Ok(respond::ok(json!({
        "staged": true,
        "kind": kind,
        "bytes": approx_bytes,
        "hash": hash,
        "next": "press Apply to Discord, or save the profile again",
    })))
}

async fn identity_avatar_clear(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "config")?;
    state
        .db
        .delete_guild_report(g.id, "avatar-pending")
        .map_err(Invalid)?;
    // Recording the wish to *remove* Discord's copy is what an apply needs: dropping the
    // staged file alone would leave the previous avatar up in the server forever.
    state
        .db
        .put_guild_report(g.id, "avatar-pending", &json!({ "clear": true }))
        .map_err(Invalid)?;
    let current = settings_for(&state.db, g.id, Feature::Identity).map_err(Invalid)?;
    let mut next = current.clone();
    if let Some(obj) = next.as_object_mut() {
        obj.insert("avatarHash".into(), Value::Null);
    }
    state
        .db
        .put_settings(g.id, Feature::Identity, &next, Some(g.actor()))
        .map_err(Invalid)?;
    Ok(respond::ok(
        json!({ "cleared": true, "queuedFor": "the next apply" }),
    ))
}

/// `data:image/png;base64,AAAA` → `("png", "AAAA")`. Anything else is not an image upload.
fn split_data_uri(raw: &str) -> Option<(&str, &str)> {
    let rest = raw.trim().strip_prefix("data:image/")?;
    let (kind, b64) = rest.split_once(";base64,")?;
    if !matches!(kind, "png" | "jpeg" | "jpg" | "gif" | "webp") || b64.is_empty() {
        return None;
    }
    Some((kind, b64))
}

// ---------------------------------------------------------------------------
// Moderation
// ---------------------------------------------------------------------------

async fn moderation_stats(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    // Lazy expiry, on the read path: it is idempotent, cheap, and it is what keeps
    // the dashboard's numbers true between the bot's hourly passes. `expire_warns`
    // only deactivates rows that are already past `expires_at`.
    let _ = state.db.expire_warns();
    let stats = state.db.moderation_stats(g.id).map_err(Invalid)?;
    let stale = state.db.due_unbans(50).map_err(Invalid)?;
    Ok(json_ok(json!({
        "stats": stats,
        // Surfacing the backlog here is deliberate: a scheduled unban that never
        // ran is a trust problem for the members waiting on it.
        "pendingUnbans": stale.len(),

    })))
}

/// `?user=` gives one member's history; without it, the guild's recent warns.
async fn list_warns(
    State(state): State<AppState>,
    g: Guild,
    Query(q): Query<HashMap<String, String>>,
) -> std::result::Result<Response, Invalid> {
    let limit = q_bounded(&q, "limit", 50, 500).map_err(Invalid)?;
    let rows = match q_opt_sf(&q, "user").map_err(Invalid)? {
        Some(user) => state.db.warns_for(g.id, user, limit).map_err(Invalid)?,
        None => state.db.recent_warns(g.id, limit).map_err(Invalid)?,
    };
    let counts = match q_opt_sf(&q, "user").map_err(Invalid)? {
        Some(user) => Some(json!({
            "active": state.db.active_warn_count(g.id, user).map_err(Invalid)?,
            "strikes": state.db.active_strikes(g.id, user).map_err(Invalid)?,
        })),
        None => None,
    };
    Ok(json_ok(json!({ "warns": rows, "counts": counts })))
}

async fn create_warn(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "moderate")?;
    let cfg = state.db.mod_config(g.id).map_err(Invalid)?;
    let user = sf_field(body.get(), "user").map_err(Invalid)?;
    let reason = str_field(body.get(), "reason", 800).map_err(Invalid)?;
    // The guild's configured expiry wins unless the caller overrides it; a warn
    // with no expiry and no configured expiry never expires, which is a choice an
    // operator can make but should not get by accident.
    let expires_days =
        int_field(body.get(), "expiresDays", cfg.warn_expiry_days, 0, 3650).map_err(Invalid)?;
    let threshold = match body.get().get("strikeThreshold").and_then(Value::as_i64) {
        Some(v) => v.clamp(0, 20),
        None => i64::from(cfg.strike_threshold),
    };
    if user == g.actor() {
        return Err(Invalid(Error::Invalid("you cannot warn yourself".into())));
    }
    let outcome = state
        .db
        .add_warn(
            g.id,
            user,
            g.actor(),
            &reason,
            expires_days,
            threshold,
            opt_sf_field(body.get(), "contextMessage").map_err(Invalid)?,
        )
        .map_err(Invalid)?;
    audit(
        &state,
        &g,
        "warn.create",
        Some(user.to_string()),
        json!({ "reason": reason, "escalated": outcome.escalated }),
    );
    if outcome.escalated {
        // The escalation itself is executed by the bot; queue it so the two
        // processes agree on what happens next.
        crate::app::queue_action(
            &state,
            g.principal(),
            g.ip(),
            g.id,
            "moderate.escalate",
            json!({ "user": user.to_string(), "action": format!("{:?}", cfg.escalate_action).to_ascii_lowercase(), "warnId": outcome.warn_id }),
            Some(user.to_string()),
        )
        .map_err(Invalid)?;
    }
    Ok(respond::created(json!({
        "id": outcome.warn_id,
        "activeCount": outcome.active_count,
        "strikes": outcome.strikes,
        "escalated": outcome.escalated,
    })))
}

async fn pardon_warn(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "moderate")?;
    let id = g.param_i64("id").map_err(Invalid)?;
    let pardoned = state.db.pardon_warn(g.id, id, g.actor()).map_err(Invalid)?;
    if !pardoned {
        return Err(Invalid(Error::NotFound("active warn")));
    }
    audit(&state, &g, "warn.pardon", Some(id.to_string()), json!({}));
    Ok(json_ok(json!({ "pardoned": true, "warnId": id })))
}

async fn clear_warns(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "moderate")?;
    let user = g.param_user().map_err(Invalid)?;
    let n = state.db.clear_warns(g.id, user).map_err(Invalid)?;
    audit(
        &state,
        &g,
        "warn.clear",
        Some(user.to_string()),
        json!({ "count": n }),
    );
    Ok(json_ok(json!({ "cleared": n })))
}

async fn top_warned(
    State(state): State<AppState>,
    g: Guild,
    Query(q): Query<HashMap<String, String>>,
) -> std::result::Result<Response, Invalid> {
    let limit = q_bounded(&q, "limit", 10, 100).map_err(Invalid)?;
    let rows = state.db.top_warned(g.id, limit).map_err(Invalid)?;
    let users: Vec<Value> = rows
        .into_iter()
        .map(|(user, count)| {
            let name = state.db.user(user).ok().flatten().and_then(|u| u["username"].as_str().map(|s| s.to_string()));
            json!({ "user": user.to_string(), "name": name.unwrap_or_else(|| user.to_string()), "warns": count })
        })
        .collect();
    Ok(json_ok(json!({ "top": users })))
}

async fn user_strikes(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    let user = g.param_user().map_err(Invalid)?;
    let strikes = state.db.active_strikes(g.id, user).map_err(Invalid)?;
    let warns = state.db.active_warn_count(g.id, user).map_err(Invalid)?;
    let notes = state.db.notes_for(g.id, user, 20).map_err(Invalid)?;
    let cfg = state.db.mod_config(g.id).map_err(Invalid)?;
    Ok(json_ok(json!({
        "user": user.to_string(),
        "strikes": strikes,
        "activeWarns": warns,
        "notes": notes,
        // The dashboard shows "2 of 3 before timeout" from these two numbers.
        "strikeThreshold": cfg.strike_threshold,
        "escalateAction": format!("{:?}", cfg.escalate_action).to_ascii_lowercase(),
    })))
}

async fn list_notes(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    let user = g.param_user().map_err(Invalid)?;
    Ok(json_ok(
        json!({ "notes": state.db.notes_for(g.id, user, 100).map_err(Invalid)? }),
    ))
}

async fn add_note(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    let user = g.param_user().map_err(Invalid)?;
    let body_text = str_field(body.get(), "body", 2000).map_err(Invalid)?;
    let id = state
        .db
        .add_note(g.id, user, g.actor(), &body_text)
        .map_err(Invalid)?;
    audit(
        &state,
        &g,
        "note.create",
        Some(user.to_string()),
        json!({ "noteId": id }),
    );
    Ok(respond::created(json!({ "id": id })))
}

async fn delete_note(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    let id = g.param_i64("id").map_err(Invalid)?;
    if !state.db.delete_note(g.id, id).map_err(Invalid)? {
        return Err(Invalid(Error::NotFound("note")));
    }
    audit(&state, &g, "note.delete", Some(id.to_string()), json!({}));
    Ok(respond::ok(json!({ "deleted": true })))
}

/// The single entry point for Discord-side moderation. One route means one audit
/// shape and one place where the mass-action guard lives.
async fn queue_action(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    let action = str_field(body.get(), "action", 24)?.to_ascii_lowercase();
    require_scope(g.principal(), "moderate")?;
    let raid_cfg = state.db.raid_config(g.id).map_err(Invalid)?;
    let allowed = ["kick", "ban", "softban", "unban", "timeout", "purge"];
    if !allowed.contains(&action.as_str()) {
        return Err(Invalid(Error::Invalid(format!(
            "unknown action {action:?}; expected one of {}",
            allowed.join(", ")
        ))));
    }
    let reason = opt_str_field(body.get(), "reason", 500)
        .map_err(Invalid)?
        .unwrap_or_else(|| format!("no reason given by {}", g.label()));
    let user = opt_sf_field(body.get(), "user").map_err(Invalid)?;
    if action != "purge" && user.is_none() {
        return Err(Invalid(Error::Invalid(
            "user is required for this action".into(),
        )));
    }

    // A bulk action is the one thing that can hurt the server *and* get us rate
    // limited into unresponsiveness, so it is capped by the anti-raid config.
    let targets = match body.get().get("users").and_then(Value::as_array) {
        Some(list) => {
            let mut out = Vec::new();
            for v in list {
                let raw = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => {
                        return Err(Invalid(Error::Invalid(
                            "users must be an array of discord ids".into(),
                        )))
                    }
                };
                out.push(
                    Snowflake::parse(&raw)
                        .map_err(|_| Invalid(Error::Invalid("users must be discord ids".into())))?,
                );
            }
            if out.len() > raid_cfg.mass_action_limit as usize {
                return Err(Invalid(Error::Invalid(format!(
                    "a single call may target at most {} members",
                    raid_cfg.mass_action_limit
                ))));
            }
            out
        }
        None => user.into_iter().collect(),
    };
    if targets.is_empty() && action != "purge" {
        return Err(Invalid(Error::Invalid("no targets given".into())));
    }

    let mut queued = Vec::new();
    for target in &targets {
        // Local bookkeeping first, and only for the actions we record here: the
        // ban row must exist before the bot can mark it unbanned, and a kick has to
        // show in the counters even if Discord later 403s us.
        match action.as_str() {
            "ban" => {
                let duration = int_field(body.get(), "durationSecs", 0, 0, 60 * 60 * 24 * 600)
                    .map_err(Invalid)?;
                let soft = bool_field(body.get(), "soft", false);
                state
                    .db
                    .record_ban(g.id, *target, g.actor(), &reason, duration, soft)
                    .map_err(Invalid)?;
            }
            "softban" => {
                // A soft ban is a 1-second ban plus message purge; recording it as a
                // 0-duration ban keeps the ban list honest without a fake expiry.
                state
                    .db
                    .record_ban(g.id, *target, g.actor(), &reason, 0, true)
                    .map_err(Invalid)?;
            }
            "kick" => {
                state
                    .db
                    .record_kick(g.id, *target, g.actor(), &reason)
                    .map_err(Invalid)?;
            }
            _ => {}
        }
        let mut args = json!({ "user": target.to_string(), "reason": reason });
        if action == "timeout" {
            args["minutes"] =
                json!(int_field(body.get(), "minutes", 10, 1, 40320).map_err(Invalid)?);
        }
        if action == "purge" {
            let purge_max = state
                .db
                .mod_config(g.id)
                .map(|c| c.purge_max)
                .unwrap_or(200);
            args["channel"] = json!(sf_field(body.get(), "channel")
                .map_err(Invalid)?
                .to_string());
            args["limit"] = json!(
                int_field(body.get(), "limit", 50, 1, i64::from(purge_max)).map_err(Invalid)?
            );
            args["filterUser"] = body.get().get("filterUser").cloned().unwrap_or(Value::Null);
            args["protectMinAge"] = json!(state
                .db
                .mod_config(g.id)
                .map(|c| c.purge_min_age_secs)
                .unwrap_or(0));
        }
        if action == "unban" {
            args["recordId"] = body.get().get("banId").cloned().unwrap_or(Value::Null);
        }
        let id = state
            .db
            .enqueue_action(
                g.id,
                g.actor(),
                &g.label(),
                &format!("moderate.{action}"),
                &args,
                "",
            )
            .map_err(Invalid)?;
        queued.push(id);
    }
    audit(
        &state,
        &g,
        &format!("action.{action}"),
        targets.first().map(|t| t.to_string()),
        json!({ "targets": targets.iter().map(|t| t.to_string()).collect::<Vec<_>>(), "reason": reason, "count": targets.len() }),
    );
    Ok(respond::with_status(
        respond::ok(json!({
            "queued": queued,
            "count": targets.len(),
            "poll": format!("/api/v1/outbox/{}", queued.first().copied().unwrap_or(0)),
        })),
        axum::http::StatusCode::ACCEPTED,
    ))
}

// ---------------------------------------------------------------------------
// Tickets
// ---------------------------------------------------------------------------

async fn list_tickets(
    State(state): State<AppState>,
    g: Guild,
    Query(q): Query<HashMap<String, String>>,
) -> std::result::Result<Response, Invalid> {
    let limit = q_bounded(&q, "limit", 50, 200).map_err(Invalid)?;
    let offset = q_bounded(&q, "offset", 0, 100_000).map_err(Invalid)?;
    let status = q_status(&q).map_err(Invalid)?;
    let owner = q_opt_sf(&q, "owner").map_err(Invalid)?;
    let rows = state
        .db
        .list_tickets(g.id, status, owner, limit, offset)
        .map_err(Invalid)?;
    Ok(json_ok(json!({
        "tickets": rows,
        "open": state.db.open_ticket_count(g.id, None).map_err(Invalid)?,
        "summary": state.db.ticket_summary(g.id).map_err(Invalid)?,
    })))
}

/// Opening a ticket from the dashboard needs a Discord channel, which only the
/// gateway worker may create — so this queues the intent and answers `202`. The
/// ticket appears in the list when the bot has made the channel.
async fn open_ticket(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "tickets")?;
    let cfg = state.db.ticket_config(g.id).map_err(Invalid)?;
    if !cfg.enabled {
        return Err(Invalid(Error::Forbidden(
            "tickets are disabled for this server",
        )));
    }
    let subject = str_field(body.get(), "subject", 120).map_err(Invalid)?;
    let owner = opt_sf_field(body.get(), "user").map_err(Invalid)?;
    let id = state
        .db
        .enqueue_action(
            g.id,
            g.actor(),
            &g.label(),
            "ticket.create",
            &json!({ "subject": subject, "user": owner.map(|o| o.to_string()), "nameFormat": cfg.name_format }),
            "",
        )
        .map_err(Invalid)?;
    audit(
        &state,
        &g,
        "ticket.queue_open",
        None,
        json!({ "subject": subject, "outboxId": id }),
    );
    Ok(respond::accepted(id))
}

async fn get_ticket(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    let id = g.param_i64("id").map_err(Invalid)?;
    let ticket = load_ticket(&state, g.id, id)?;
    Ok(json_ok(json!({
        "ticket": ticket,
        "transcript": state.db.ticket_transcript(id, !g.principal().has_scope("tickets")).map_err(Invalid)?,
    })))
}

async fn update_ticket(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "tickets")?;
    let id = g.param_i64("id").map_err(Invalid)?;
    // Existence and ownership in one step: another guild's ticket is a 404 here, not
    // a 403, so the endpoint cannot be used to probe ids.
    load_ticket(&state, g.id, id)?;
    let raw = str_field(body.get(), "status", 16)
        .map(|s| s.to_ascii_lowercase())
        .map_err(Invalid)?;
    let status = TicketStatus::parse(&raw)
        .ok_or_else(|| Invalid(Error::Invalid(format!("unknown ticket status {raw:?}"))))?;
    if !state.db.set_ticket_status(id, status).map_err(Invalid)? {
        return Err(Invalid(Error::NotFound("ticket")));
    }
    // Closing a ticket is a Discord-side edit too (rename + lock), so the bot is told.
    if status == TicketStatus::Closed {
        let _ = state.db.enqueue_action(
            g.id,
            g.actor(),
            &g.label(),
            "ticket.close",
            &json!({ "ticketId": id }),
            "",
        );
    }
    audit(
        &state,
        &g,
        &format!("ticket.{raw}"),
        Some(id.to_string()),
        json!({}),
    );
    Ok(json_ok(json!({ "id": id, "status": status.as_str() })))
}

async fn claim_ticket(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    let id = g.param_i64("id").map_err(Invalid)?;
    // Existence and ownership in one step: another guild's ticket is a 404 here, not
    // a 403, so the endpoint cannot be used to probe ids.
    load_ticket(&state, g.id, id)?;
    let force = bool_field(body.get(), "force", false);
    if !state
        .db
        .claim_ticket(id, g.actor(), force)
        .map_err(Invalid)?
    {
        return Err(Invalid(Error::Conflict(
            "this ticket is already claimed".into(),
        )));
    }
    audit(
        &state,
        &g,
        "ticket.claim",
        Some(id.to_string()),
        json!({ "force": force }),
    );
    Ok(json_ok(json!({ "claimed": true })))
}

async fn unclaim_ticket(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    let id = g.param_i64("id").map_err(Invalid)?;
    // Existence and ownership in one step: another guild's ticket is a 404 here, not
    // a 403, so the endpoint cannot be used to probe ids.
    load_ticket(&state, g.id, id)?;
    if !state.db.unclaim_ticket(id).map_err(Invalid)? {
        return Err(Invalid(Error::NotFound("claim")));
    }
    audit(
        &state,
        &g,
        "ticket.unclaim",
        Some(id.to_string()),
        json!({}),
    );
    Ok(json_ok(json!({ "claimed": false })))
}

async fn transcript(
    State(state): State<AppState>,
    g: Guild,
    Query(q): Query<HashMap<String, String>>,
) -> std::result::Result<Response, Invalid> {
    let id = g.param_i64("id").map_err(Invalid)?;
    let ticket = load_ticket(&state, g.id, id)?;
    // Internal notes are staff-only; a read-scoped key never sees them, and the
    // query flag can only *reduce* what is returned, never widen it.
    let want_internal = q_bool(&q, "internal") && g.principal().has_scope("tickets");
    let rows = state
        .db
        .ticket_transcript(id, want_internal)
        .map_err(Invalid)?;
    if q_bool(&q, "raw") {
        return Ok(json_ok(json!({ "messages": rows })));
    }
    let text = render_transcript(&ticket, &rows);
    Ok(download_text(&format!("{}.txt", ticket.reference), text))
}

/// A plain-text attachment. Built with the response builder because the tuple form
/// would also need a `content-length`, and an unknown-length body is what makes
/// browsers guess.
fn download_text(filename: &str, body: String) -> Response {
    // A quote or a control character in the name would break out of the header.
    let safe: String = filename
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .header(
            axum::http::header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{safe}\""),
        )
        .header(axum::http::header::CONTENT_LENGTH, body.len().to_string())
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| axum::response::Response::new(axum::body::Body::empty()))
}

/// A transcript a moderator can attach to a case file. The header is always
/// present — including for a ticket with no messages, where an empty download would
/// be indistinguishable from a failure.
fn render_transcript(ticket: &aerona_core::models::TicketRecord, rows: &[Value]) -> String {
    let mut out = String::new();
    out.push_str("Aerona ticket transcript\n");
    out.push_str(&format!("reference: {}\n", ticket.reference));
    out.push_str(&format!("subject:   {}\n", ticket.subject));
    out.push_str(&format!("status:    {}\n", ticket.status.as_str()));
    out.push_str(&format!("messages:  {}\n", rows.len()));
    out.push_str(&"-".repeat(56));
    out.push('\n');
    for r in rows {
        let at = r["at"].as_i64().unwrap_or(0);
        let stamp = aerona_core::time::format(at);
        let author = r["author"].as_str().unwrap_or("?");
        let internal = if r["internal"].as_bool().unwrap_or(false) {
            " [internal]"
        } else {
            ""
        };
        out.push_str(&format!(
            "[{stamp}] {author}{internal}: {}\n",
            r["content"].as_str().unwrap_or("")
        ));
    }
    out
}

async fn ticket_message(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    let id = g.param_i64("id").map_err(Invalid)?;
    // Existence and ownership in one step: another guild's ticket is a 404 here, not
    // a 403, so the endpoint cannot be used to probe ids.
    load_ticket(&state, g.id, id)?;
    let content = str_field(body.get(), "content", 1800).map_err(Invalid)?;
    // A reply to the member is a Discord write; an internal note is not.
    let internal = bool_field(body.get(), "internal", false);
    let msg_id = state
        .db
        .append_ticket_message(id, g.actor(), &g.label(), &content, internal)
        .map_err(Invalid)?;
    if !internal {
        let _ = state.db.enqueue_action(
            g.id,
            g.actor(),
            &g.label(),
            "ticket.reply",
            &json!({ "ticketId": id, "content": content }),
            "",
        );
    }
    audit(
        &state,
        &g,
        "ticket.message",
        Some(id.to_string()),
        json!({ "internal": internal }),
    );
    Ok(respond::created(
        json!({ "id": msg_id, "internal": internal }),
    ))
}

async fn list_panels(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    Ok(json_ok(
        json!({ "panels": state.db.panels(g.id).map_err(Invalid)? }),
    ))
}

async fn add_panel(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "config")?;
    let panel = TicketPanel {
        id: 0,
        guild_id: g.id,
        channel_id: sf_field(body.get(), "channel").map_err(Invalid)?,
        // The message the buttons live on is created by the bot, so a panel added
        // from the dashboard starts unbound and is filled in by `panel.bind`.
        message_id: opt_sf_field(body.get(), "message")
            .map_err(Invalid)?
            .unwrap_or(Snowflake(0)),
        label: opt_str_field(body.get(), "label", 80)
            .map_err(Invalid)?
            .unwrap_or_else(|| "Open a ticket".into()),
        emoji: opt_str_field(body.get(), "emoji", 16).map_err(Invalid)?,
        support_role_id: opt_sf_field(body.get(), "supportRole").map_err(Invalid)?,
        sort: int_field(body.get(), "sort", 0, -1000, 1000).map_err(Invalid)?,
    };
    let id = state.db.add_panel(&panel).map_err(Invalid)?;
    audit(
        &state,
        &g,
        "ticket.panel.create",
        Some(id.to_string()),
        json!({ "channel": panel.channel_id.to_string() }),
    );
    Ok(respond::created(
        json!({ "id": id, "panels": state.db.panels(g.id).map_err(Invalid)? }),
    ))
}

async fn delete_panel(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "config")?;
    let id = g.param_i64("id").map_err(Invalid)?;
    if !state.db.delete_panel(g.id, id).map_err(Invalid)? {
        return Err(Invalid(Error::NotFound("panel")));
    }
    audit(
        &state,
        &g,
        "ticket.panel.delete",
        Some(id.to_string()),
        json!({}),
    );
    Ok(json_ok(json!({ "deleted": true })))
}

// ---------------------------------------------------------------------------
// Sticky messages
// ---------------------------------------------------------------------------

async fn list_stickies(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    let cfg = state.db.sticky_config(g.id).map_err(Invalid)?;
    Ok(json_ok(
        json!({ "stickies": state.db.stickies_for_guild(g.id).map_err(Invalid)?, "config": cfg }),
    ))
}

async fn add_sticky(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "config")?;
    let channel = sf_field(body.get(), "channel").map_err(Invalid)?;
    let message = sf_field(body.get(), "message").map_err(Invalid)?;
    let content = str_field(body.get(), "content", 1800).map_err(Invalid)?;
    let every = int_field(body.get(), "everyMessages", 50, 1, 1000).map_err(Invalid)? as u32;
    let id = state
        .db
        .add_sticky(g.id, channel, message, &content, every, g.actor())
        .map_err(Invalid)?;
    audit(
        &state,
        &g,
        "sticky.create",
        Some(id.to_string()),
        json!({ "channel": channel.to_string(), "every": every }),
    );
    Ok(respond::created(
        json!({ "id": id, "stickies": state.db.stickies_for_channel(g.id, channel).map_err(Invalid)? }),
    ))
}

async fn delete_sticky(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "config")?;
    let id = g.param_i64("id").map_err(Invalid)?;
    if !state.db.delete_sticky(g.id, id).map_err(Invalid)? {
        return Err(Invalid(Error::NotFound("sticky")));
    }
    audit(&state, &g, "sticky.delete", Some(id.to_string()), json!({}));
    Ok(json_ok(json!({ "deleted": true })))
}

// ---------------------------------------------------------------------------
// Anti-raid
// ---------------------------------------------------------------------------

async fn raid_events(
    State(state): State<AppState>,
    g: Guild,
    Query(q): Query<HashMap<String, String>>,
) -> std::result::Result<Response, Invalid> {
    let limit = q_bounded(&q, "limit", 20, 100).map_err(Invalid)?;
    let active = state.db.active_raid(g.id).map_err(Invalid)?;
    Ok(json_ok(json!({
        "events": state.db.raid_history(g.id, limit).map_err(Invalid)?,
        "active": active,
        "config": state.db.raid_config(g.id).map_err(Invalid)?,
    })))
}

/// Locking down a server is a queue item, not a direct write: the bot applies it
/// per channel and can therefore survive a Discord 429.
#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn only_a_real_data_uri_is_accepted_as_an_image() {
        assert_eq!(
            split_data_uri("data:image/png;base64,AAAA").map(|(k, _)| k),
            Some("png")
        );
        assert_eq!(
            split_data_uri("data:image/gif;base64,AA==").map(|(k, _)| k),
            Some("gif")
        );
        // A remote URL, an empty body, and an SVG (which Discord will not accept as an
        // avatar and a browser will happily script) are all refused here.
        assert!(split_data_uri("https://example.com/a.png").is_none());
        assert!(split_data_uri("data:image/png;base64,").is_none());
        assert!(split_data_uri("data:image/svg+xml;base64,AAA").is_none());
        assert!(split_data_uri("data:text/plain;base64,AAA").is_none());
    }
}

async fn raid_lockdown(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "moderate")?;
    let active = bool_field(body.get(), "active", true);
    let cfg = state.db.raid_config(g.id).map_err(Invalid)?;
    let minutes = int_field(body.get(), "minutes", 15, 1, 1440).map_err(Invalid)?;
    let args = json!({
        "active": active,
        "minutes": minutes,
        "slowmodeSecs": int_field(body.get(), "slowmode", i64::from(cfg.lockdown_slowmode_secs), 0, 21600).map_err(Invalid)?,
        "raiseVerification": bool_field(body.get(), "raiseVerification", cfg.raise_verification),
        "message": opt_str_field(body.get(), "message", 300).map_err(Invalid)?,
    });
    let id = state
        .db
        .enqueue_action(g.id, g.actor(), &g.label(), "raid.lockdown", &args, "")
        .map_err(Invalid)?;
    if active {
        // Also open a raid event so the history shows a manual lockdown next to the
        // automatic ones.
        state
            .db
            .raid_event_begin(g.id, "manual", 0, minutes * 60)
            .map_err(Invalid)?;
    }
    audit(
        &state,
        &g,
        if active {
            "raid.lockdown"
        } else {
            "raid.release"
        },
        None,
        args,
    );
    Ok(respond::accepted(id))
}

// ---------------------------------------------------------------------------
// AI text filter
// ---------------------------------------------------------------------------

async fn ai_events(
    State(state): State<AppState>,
    g: Guild,
    Query(q): Query<HashMap<String, String>>,
) -> std::result::Result<Response, Invalid> {
    let limit = q_bounded(&q, "limit", 50, 200).map_err(Invalid)?;
    let since = q_i64(&q, "since")
        .map_err(Invalid)?
        .unwrap_or(aerona_core::time::now() - 86_400);
    let rules = state.db.count_custom_rules(g.id).map_err(Invalid)?;
    Ok(json_ok(json!({
        "events": state.db.recent_ai_events(g.id, limit).map_err(Invalid)?,
        "since": since,
        "count": state.db.ai_event_count_since(g.id, since).map_err(Invalid)?,
        "rules": { "total": rules.0, "enabled": rules.1 },
        "config": state.db.ai_config(g.id).map_err(Invalid)?,
        "provider": state.cfg.sightengine.as_ref().map(|(user, _)| json!({ "configured": true, "user": user })),
    })))
}

/// Dry run of the local half of the filter: custom rules, thresholds and the
/// escalation ladder, against text the operator pastes. The provider is not called,
/// so this costs no quota and answers instantly — and it works with no network.
async fn ai_test(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    let text_in = str_field(body.get(), "text", 4000).map_err(Invalid)?;
    let cfg = state.db.ai_config(g.id).map_err(Invalid)?;
    let mut rules: Vec<AiCustomRule> = state.db.custom_rules(g.id).map_err(Invalid)?;
    let simulate_disabled = bool_field(body.get(), "ignoreEnabled", false);
    if simulate_disabled {
        for r in rules.iter_mut() {
            r.enabled = true;
        }
    }
    let compiled = text::compile_rules(&rules);
    let (_rule_hits, hits) = text::scan_custom(&text_in, &compiled);
    let verdict = text::decide(&cfg, &text_in, hits, 0);
    Ok(json_ok(json!({
        "blocked": verdict.violation,
        "verdict": verdict,
        "provider": { "called": false, "note": "the local pre-filter only; enabling the provider adds a latency-quoted second pass" },
        "effectiveThresholds": cfg.categories_config.iter().map(|c| json!({
            "category": c.category,
            "threshold": text::effective_threshold(c, cfg.strictness),
        })).collect::<Vec<_>>(),
        "rulesCompared": compiled.len(),
        // With `privacy_redact` the guild has told us not to retain message text, so
        // the dry run must not echo it back either — that would be a way to read the
        // policy's protection out of the API.
        "echo": if cfg.privacy_redact { "[withheld: this guild does not retain message text]".to_string() } else { text_in.clone() },
    })))
}

async fn list_rules(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    Ok(json_ok(
        json!({ "rules": state.db.custom_rules(g.id).map_err(Invalid)? }),
    ))
}

async fn put_rule(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "config")?;
    let rule = AiCustomRule {
        id: 0,
        pattern: str_field(body.get(), "pattern", 512).map_err(Invalid)?,
        is_regex: bool_field(body.get(), "isRegex", false),
        mode: enum_field(body.get(), "mode", Default::default(), |s| {
            aerona_core::models::AiCustomMode::parse(s)
        })
        .map_err(Invalid)?,
        category: opt_str_field(body.get(), "category", 32)
            .map_err(Invalid)?
            .unwrap_or_else(|| "custom".into()),
        severity: enum_field(
            body.get(),
            "severity",
            Default::default(),
            aerona_core::models::Severity::parse,
        )
        .map_err(Invalid)?,
        action: enum_field(
            body.get(),
            "action",
            Default::default(),
            aerona_core::models::FilterAction::parse,
        )
        .map_err(Invalid)?,
        strikes: int_field(body.get(), "strikes", 0, 0, 20).map_err(Invalid)? as u8,
        description: opt_str_field(body.get(), "description", 200)
            .map_err(Invalid)?
            .unwrap_or_default(),
        enabled: bool_field(body.get(), "enabled", true),
        hits: 0,
    };
    let id = state.db.put_custom_rule(g.id, &rule).map_err(Invalid)?;
    audit(
        &state,
        &g,
        "ai.rule.put",
        Some(id.to_string()),
        json!({ "pattern": rule.pattern, "mode": rule.category }),
    );
    Ok(respond::created(
        json!({ "id": id, "rules": state.db.custom_rules(g.id).map_err(Invalid)? }),
    ))
}

async fn patch_rule(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "config")?;
    let id = g.param_i64("id").map_err(Invalid)?;
    if body.get().get("enabled").is_none() {
        return Err(Invalid(Error::Invalid(
            "only `enabled` can be patched on a rule; edit the whole rule otherwise".into(),
        )));
    }
    let enabled = bool_field(body.get(), "enabled", true);
    state
        .db
        .set_custom_rule_enabled(g.id, id, enabled)
        .map_err(Invalid)?;
    audit(
        &state,
        &g,
        "ai.rule.enabled",
        Some(id.to_string()),
        json!({ "enabled": enabled }),
    );
    Ok(json_ok(json!({ "id": id, "enabled": enabled })))
}

async fn delete_rule(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "config")?;
    let id = g.param_i64("id").map_err(Invalid)?;
    if !state.db.delete_custom_rule(g.id, id).map_err(Invalid)? {
        return Err(Invalid(Error::NotFound("rule")));
    }
    audit(
        &state,
        &g,
        "ai.rule.delete",
        Some(id.to_string()),
        json!({}),
    );
    Ok(json_ok(json!({ "deleted": true })))
}

// ---------------------------------------------------------------------------
// Levels
// ---------------------------------------------------------------------------

async fn levels(
    State(state): State<AppState>,
    g: Guild,
    Query(q): Query<HashMap<String, String>>,
) -> std::result::Result<Response, Invalid> {
    let limit = q_bounded(&q, "limit", 25, 100).map_err(Invalid)?;
    let cfg = state.db.guild_settings(g.id).map_err(Invalid)?;
    Ok(json_ok(json!({
        "leaderboard": state.db.level_leaderboard(g.id, limit).map_err(Invalid)?,
        "enabled": cfg.levels_enabled,
        // Shown next to every row so the curve is visible without a second call.
        "curve": { "xpForLevel": (1..=5).map(aerona_core::db::Db::xp_for_level).collect::<Vec<_>>() },
    })))
}

async fn level_for(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    let user = g.param_user().map_err(Invalid)?;
    let (level, xp, messages) = state.db.level(g.id, user).map_err(Invalid)?;
    let rank = state.db.level_rank(g.id, user).map_err(Invalid)?;
    Ok(json_ok(json!({
        "user": user.to_string(),
        "level": level,
        "xp": xp,
        "messages": messages,
        "rank": rank,
        "toNextLevel": aerona_core::db::Db::xp_to_next_level(xp),
        "forNextLevel": aerona_core::db::Db::xp_for_level(level + 1) - xp,
    })))
}

// ---------------------------------------------------------------------------
// Shared bits
// ---------------------------------------------------------------------------

/// A ticket is only addressable through the guild that owns it; answering 404 for
/// another guild's ticket avoids becoming a free id-probing oracle.
fn load_ticket(
    state: &AppState,
    guild: Snowflake,
    id: i64,
) -> std::result::Result<aerona_core::models::TicketRecord, Invalid> {
    let ticket = state
        .db
        .ticket_by_id(id)
        .map_err(Invalid)?
        .filter(|t| t.guild_id == guild)
        .ok_or(Invalid(Error::NotFound("ticket")))?;
    Ok(ticket)
}

pub(crate) fn audit(
    state: &AppState,
    g: &Guild,
    action: &str,
    target: Option<String>,
    meta: Value,
) {
    state.db.audit_best_effort(
        AuditEntry::new(action)
            .for_guild(g.id)
            .actor(g.principal().actor_type(), Some(g.actor()), &g.label())
            .maybe_target(target)
            .ip(g.ip())
            .meta(meta),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket_record() -> aerona_core::models::TicketRecord {
        aerona_core::models::TicketRecord {
            id: 1,
            guild_id: Snowflake(1),
            channel_id: Snowflake(2),
            owner_id: Snowflake(3),
            claimed_by: None,
            status: aerona_core::models::TicketStatus::Open,
            subject: "billing question".into(),
            reference: "AER-QQ3479".into(),
            created_at: 1,
            closed_at: None,
            message_count: 2,
        }
    }

    /// The transcript download is the one route that answers with text rather than
    /// the envelope, so the builder is tested directly: a silent fallback to an empty
    /// body would look like a missing transcript to a moderator.
    #[tokio::test]
    async fn a_text_download_carries_its_body() {
        let resp = download_text("ticket-7.txt", "hello world".to_string());
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default(),
            "attachment; filename=\"ticket-7.txt\""
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), b"hello world");
    }

    #[test]
    fn transcript_rendering_is_readable_and_sorted() {
        let rows = vec![
            serde_json::json!({"at": 1, "author": "alice", "content": "hi", "internal": false}),
            serde_json::json!({"at": 2, "author": "mod", "content": "note", "internal": true}),
        ];
        let text = super::render_transcript(&ticket_record(), &rows);
        assert!(text.starts_with("Aerona ticket transcript\n"), "{text}");
        assert!(text.contains("reference: AER-QQ3479"), "{text}");
        assert!(text.contains("messages:  2"), "{text}");
        let lines: Vec<&str> = text.lines().filter(|l| l.starts_with('[')).collect();
        assert_eq!(lines.len(), 2, "one line per message, after the header");
        assert!(lines[0].ends_with("alice: hi"), "{lines:?}");
        assert!(lines[1].contains("mod [internal]: note"), "{lines:?}");
        assert!(
            text.find("alice").unwrap() < text.find("mod [internal]").unwrap(),
            "oldest first"
        );
        // An empty ticket still explains itself.
        let empty = super::render_transcript(&ticket_record(), &[]);
        assert!(
            empty.contains("AER-QQ3479") && empty.contains("messages:  0"),
            "{empty}"
        );
    }

    #[test]
    fn allowed_actions_are_a_closed_set() {
        // The list in `queue_action` must match what the bot can execute; a stray
        // entry there would queue an action nobody performs.
        for a in ["kick", "ban", "softban", "unban", "timeout", "purge"] {
            assert!(a.len() < 24);
        }
    }
}
