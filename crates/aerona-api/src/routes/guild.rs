//! Guild-scoped routes: moderation, tickets, stickies, anti-raid, the AI filter
//! and levels.
//!
//! Every handler here takes `Guild`, which is the extractor that authenticates the
//! caller, checks the credential against *this* guild and hands over the id - so
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

use crate::routes::{normalize, settings_for};
use aerona_core::models::{AiCustomRule, TicketPanel, TicketStatus};
use aerona_core::text;

use crate::app::AppState;
use crate::auth::Guild;
use crate::respond::{self, Invalid, JsonBody};
use crate::routes::{
    bool_field, enum_field, int_field, json_ok, opt_block_field, opt_sf_field, opt_str_field,
    q_bool, q_bounded, q_i64, q_opt_sf, q_status, require_scope, sf_field, str_field,
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
        // -- minecraft cape notifier
        .route("/guilds/{guild}/capes", get(list_capes))
        .route("/guilds/{guild}/capes/check", post(check_capes))
        .route(
            "/guilds/{guild}/capes/{slug}/frame/{frame}",
            get(cape_frame),
        )
        // -- anti-raid
        .route("/guilds/{guild}/raid-events", get(raid_events))
        .route("/guilds/{guild}/log", get(log_events))
        .route("/guilds/{guild}/picker-refresh", post(picker_refresh))
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
        .route(
            "/guilds/{guild}/identity/banner",
            put(identity_banner_stage).delete(identity_banner_clear),
        )
        .route("/guilds/{guild}/identity/deco", put(identity_deco_put))
        // -- levels
        .route("/guilds/{guild}/levels", get(levels))
        .route("/guilds/{guild}/levels/{user}", get(level_for))
}

// ---------------------------------------------------------------------------
// Worker reports: commands, pickers, the guild's copy of the bot profile
// ---------------------------------------------------------------------------

/// `GET /guilds/{guild}/report` - everything the gateway worker last saw, in one fetch.
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

/// Which of the bot's two pictures a staging request is about.
///
/// The avatar and the banner are the same shape of problem: a base64 image too big to live in a
/// settings document, staged once, consumed by one apply, and remembered in the document only as
/// its digest. Two handlers written twice is how one of them gets fixed and the other does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageSlot {
    Avatar,
    Banner,
}

impl ImageSlot {
    /// The single-use report slot the bytes wait in.
    fn report(self) -> &'static str {
        match self {
            Self::Avatar => "avatar-pending",
            Self::Banner => "banner-pending",
        }
    }

    /// The key the document keeps, so the panel can say "an image is waiting" without carrying one.
    fn hash_key(self) -> &'static str {
        match self {
            Self::Avatar => "avatar_hash",
            Self::Banner => "banner_hash",
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::Avatar => "avatar",
            Self::Banner => "banner",
        }
    }
}

/// `PUT /guilds/{guild}/identity/avatar` and `.../identity/banner` - stage one image for the next
/// apply.
///
/// The bytes never enter `guild_config`: that document is written to `config_history` whole,
/// on every save, and a megabyte of base64 in each row would make the audit drawer unreadable
/// and the database grow on every preview. So the image goes to the single-use report slot
/// the worker consumes once, and the settings document keeps only its digest.
async fn identity_image_stage(
    state: AppState,
    g: Guild,
    slot: ImageSlot,
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
            slot.report(),
            &json!({ "data": data, "kind": kind, "bytes": approx_bytes, "hash": hash, "stagedAt": aerona_core::time::now() }),
        )
        .map_err(Invalid)?;
    // The digest lands in the settings document through the same validated path a nick
    // does, so the history drawer shows one change and the bytes stay out of it. A hand-merged
    // document that skipped `normalize` stored a key the model does not have, and every later
    // save of the profile answered "identity has no setting(s): avatarHash" - which is the whole
    // reason this write is shaped the way it is.
    let current = settings_for(&state.db, g.id, Feature::Identity).map_err(Invalid)?;
    let mut next = current.clone();
    if let Some(obj) = next.as_object_mut() {
        obj.insert(slot.hash_key().into(), json!(hash));
    }
    let next = normalize(Feature::Identity, next).map_err(Invalid)?;
    state
        .db
        .put_settings(g.id, Feature::Identity, &next, Some(g.actor()))
        .map_err(Invalid)?;
    Ok(respond::ok(json!({
        "staged": true,
        "kind": kind,
        "bytes": approx_bytes,
        "hash": hash,
        "next": format!("press Apply to Discord to send the {}", slot.word()),
    })))
}

/// `DELETE /guilds/{guild}/identity/avatar` and `.../identity/banner`.
async fn identity_image_clear(
    state: AppState,
    g: Guild,
    slot: ImageSlot,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "config")?;
    state
        .db
        .delete_guild_report(g.id, slot.report())
        .map_err(Invalid)?;
    // Recording the wish to *remove* Discord's copy is what an apply needs: dropping the
    // staged file alone would leave the previous image up in the server forever.
    state
        .db
        .put_guild_report(g.id, slot.report(), &json!({ "clear": true }))
        .map_err(Invalid)?;
    let current = settings_for(&state.db, g.id, Feature::Identity).map_err(Invalid)?;
    let mut next = current.clone();
    if let Some(obj) = next.as_object_mut() {
        obj.insert(slot.hash_key().into(), Value::Null);
    }
    let next = normalize(Feature::Identity, next).map_err(Invalid)?;
    state
        .db
        .put_settings(g.id, Feature::Identity, &next, Some(g.actor()))
        .map_err(Invalid)?;
    Ok(respond::ok(
        json!({ "cleared": true, "queuedFor": "the next apply" }),
    ))
}

async fn identity_avatar_stage(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    identity_image_stage(state, g, ImageSlot::Avatar, body).await
}

async fn identity_avatar_clear(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    identity_image_clear(state, g, ImageSlot::Avatar).await
}

async fn identity_banner_stage(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    identity_image_stage(state, g, ImageSlot::Banner, body).await
}

async fn identity_banner_clear(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    identity_image_clear(state, g, ImageSlot::Banner).await
}

/// `PUT /guilds/{guild}/identity/deco` - the two decorations, as ids.
///
/// Discord publishes no catalogue of these for a bot to read. What the panel can offer is what the
/// worker can *see*: the decorations members of this server are wearing, gathered into
/// `picker.decorations` next to the channels and roles. A key absent from the body is left alone,
/// an empty string clears it, and anything that is not an id is refused here rather than at
/// Discord, where the answer would be an outbox row nobody is looking at.
async fn identity_deco_put(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    use aerona_core::error::Error;
    require_scope(g.principal(), "config")?;
    let mut patch = serde_json::Map::new();
    for key in ["avatar_decoration_id", "banner_decoration_id"] {
        let Some(raw) = body.get().get(key) else {
            continue;
        };
        let value = match raw {
            Value::Null => String::new(),
            Value::String(text) => text.trim().to_string(),
            Value::Number(n) => n.to_string(),
            _ => {
                return Err(Invalid(Error::Invalid(format!(
                    "{key} must be a decoration id or null"
                ))))
            }
        };
        // The shape is checked by `normalise_identity`, which is where every identity field is
        // checked: a second copy of the rule here is one more place for the two to disagree.
        // A null in a settings document means "never set", which the apply reads as silence about
        // the field; the clear is the empty string, exactly as the nickname and bio use it.
        patch.insert(key.into(), json!(value));
    }
    if patch.is_empty() {
        return Err(Invalid(Error::Invalid(
            "say which decoration to wear: avatar_decoration_id or banner_decoration_id".into(),
        )));
    }
    let current = settings_for(&state.db, g.id, Feature::Identity).map_err(Invalid)?;
    let mut next = current.clone();
    if let Some(obj) = next.as_object_mut() {
        for (k, v) in patch {
            obj.insert(k, v);
        }
    }
    let next = normalize(Feature::Identity, next).map_err(Invalid)?;
    state
        .db
        .put_settings(g.id, Feature::Identity, &next, Some(g.actor()))
        .map_err(Invalid)?;
    // The apply is what sends it, so pressing Save and getting nothing back is the failure mode
    // this route exists to avoid: queue the same follow-up the profile page's own button does.
    let queued = state
        .db
        .enqueue_action(
            g.id,
            g.actor(),
            &g.label(),
            "identity.apply",
            &json!({}),
            "",
        )
        .unwrap_or_default();
    audit(
        &state,
        &g,
        "identity.deco",
        None,
        json!({ "queued": queued }),
    );
    Ok(respond::ok(json!({
        "settings": next,
        "queued": queued,
        "next": "the bot sends it on its next beat",
    })))
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
/// gateway worker may create - so this queues the intent and answers `202`. The
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
    let (kept, bytes) = state.db.ticket_file_stats(id).unwrap_or((0, 0));
    Ok(json_ok(json!({
        "ticket": ticket,
        "transcript": state.db.ticket_transcript(id, !g.principal().has_scope("tickets")).map_err(Invalid)?,
        // What the transcript carries with it. Said out loud because a 3 MB download with no
        // explanation reads like a bug, and because "the pictures are in here" is the answer to
        // "will I still see them after the channel is deleted".
        "keptFiles": { "count": kept, "bytes": bytes },
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
    // The guild's own setting decides, and `?format=` overrides it for one download. A moderator
    // who wants the picture gets the file that shows it, without anybody having to change a
    // setting on the way to reading a transcript.
    let cfg = state.db.ticket_config(g.id).unwrap_or_default();
    let format = match q.get("format").map(|v| v.as_str()) {
        Some("text") | Some("txt") => aerona_core::models::TranscriptFormat::Text,
        Some("json") => aerona_core::models::TranscriptFormat::Json,
        Some("html") => aerona_core::models::TranscriptFormat::Html,
        _ => cfg.transcript_format,
    };
    // The name comes from the guild row rather than a Discord call: the API never reaches
    // Discord, and a transcript header that costs an outage to render is a header not worth
    // having.
    let server = state
        .db
        .guilds()
        .ok()
        .and_then(|list| {
            list.into_iter()
                .find(|row| row.id == g.id)
                .map(|row| row.name)
        })
        .unwrap_or_default();
    // The copies this ticket kept. Read here rather than baked into one document at close, so a
    // moderator who opens a transcript from last month sees the same pictures the close put in it,
    // and so a guild that later turns copying off still has what it already saved.
    let files = state.db.ticket_files(id).unwrap_or_default();
    let (name, body) =
        aerona_core::transcript::render_with_files(&ticket, &rows, format, &server, &files);
    let ctype = if name.ends_with(".json") {
        "application/json; charset=utf-8"
    } else if name.ends_with(".html") {
        "text/html; charset=utf-8"
    } else {
        "text/plain; charset=utf-8"
    };
    // `?download=1` is the file; without it the transcript is the page. That distinction is the
    // whole fix for "the transcript html does not work" as seen from the panel: an
    // `attachment` disposition on a document a browser could render is a link that appears to do
    // nothing but save a file, and a file whose pictures are expired CDN links looks broken
    // whichever way it arrives.
    let download = q_bool(&q, "download");
    Ok(transcript_file(&name, body, ctype, download))
}

/// A transcript, either as the thing to read or the thing to keep.
///
/// `inline` is safe here because the document is the panel's own, the reader is already a principal
/// of this guild, and the HTML is written by `transcript.rs` to carry no script. `private,
/// no-store` because it is a person's view of a conversation with internal notes in it: a shared
/// cache that kept it would be a second place the transcript lives, and the whole design of this
/// feature is that the database is the only one.
fn transcript_file(filename: &str, body: String, content_type: &str, download: bool) -> Response {
    // A quote or a control character in the name would break out of the header, so the name is
    // rebuilt from the characters that cannot, rather than escaped.
    let safe: String = filename
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    let disposition = if download { "attachment" } else { "inline" };
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header(
            axum::http::header::CONTENT_DISPOSITION,
            format!("{disposition}; filename=\"{safe}\""),
        )
        .header(axum::http::header::CACHE_CONTROL, "private, no-store")
        .header(axum::http::header::CONTENT_LENGTH, body.len().to_string())
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| axum::response::Response::new(axum::body::Body::empty()))
}

/// A transcript a moderator can attach to a case file. The header is always
/// present - including for a ticket with no messages, where an empty download would
/// be indistinguishable from a failure.
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
        .append_ticket_message(id, g.actor(), &g.label(), &content, internal, "[]")
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
        // A type may name its own channel; if it does not, the board lives where the Tickets page
        // says the board lives. That is what makes "pick a ticket channel, press Save" one action
        // rather than one row per type.
        channel_id: match opt_sf_field(body.get(), "channel").map_err(Invalid)? {
            Some(id) => id,
            None => state
                .db
                .ticket_config(g.id)
                .map_err(Invalid)?
                .board_channel_id
                .ok_or_else(|| {
                    Invalid(aerona_core::error::Error::Invalid(
                        "no ticket channel is set: name one on the Tickets page, or here".into(),
                    ))
                })?,
        },
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
        color: match int_field(body.get(), "color", -1, -1, 0xff_ff_ff).map_err(Invalid)? {
            -1 => None,
            v => Some(v as u32),
        },
        subject: opt_str_field(body.get(), "subject", 200)
            .map_err(Invalid)?
            .unwrap_or_default(),
        intro: opt_block_field(body.get(), "intro", 1500)
            .map_err(Invalid)?
            .unwrap_or_default(),
    };
    let id = state.db.add_panel(&panel).map_err(Invalid)?;
    // A row alone is a database row; the button is the product. The worker posts it (or, when the
    // channel already has an unbound board, adds this type to that message) and records which
    // message it landed on, which is what makes the click findable again.
    let queued = state
        .db
        .enqueue_action(
            g.id,
            g.actor(),
            &g.label(),
            "panel.post",
            &json!({ "panel": id, "channel": panel.channel_id.to_string() }),
            "",
        )
        .map_err(Invalid)?;
    audit(
        &state,
        &g,
        "ticket.panel.create",
        Some(id.to_string()),
        json!({ "channel": panel.channel_id.to_string(), "queued": queued }),
    );
    Ok(respond::created(
        json!({ "id": id, "queued": queued, "panels": state.db.panels(g.id).map_err(Invalid)? }),
    ))
}

async fn delete_panel(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "config")?;
    let id = g.param_i64("id").map_err(Invalid)?;
    let channel = state
        .db
        .panels(g.id)
        .map_err(Invalid)?
        .into_iter()
        .find(|p| p.id == id)
        .map(|p| p.channel_id);
    if state.db.delete_panel(g.id, id).map_err(Invalid)? {
        // The button lives in Discord, not here: removing the row without redrawing leaves a
        // button that opens a ticket type nobody configured.
        if let Some(channel) = channel {
            let _ = state.db.enqueue_action(
                g.id,
                g.actor(),
                &g.label(),
                "panel.redraw",
                &json!({ "channel": channel.to_string() }),
                "",
            );
        }
        audit(
            &state,
            &g,
            "ticket.panel.delete",
            Some(id.to_string()),
            json!({}),
        );
        return Ok(json_ok(json!({ "deleted": true })));
    }
    Err(Invalid(Error::NotFound("panel")))
}

// ---------------------------------------------------------------------------
// Sticky messages
// ---------------------------------------------------------------------------

async fn list_stickies(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    let cfg = state.db.sticky_config(g.id).map_err(Invalid)?;
    let rows = state.db.stickies_for_guild(g.id).map_err(Invalid)?;
    Ok(json_ok(
        json!({ "stickies": named_stickies(&state.db, g.id, &rows), "config": cfg }),
    ))
}

/// A sticky names a channel and says something; when it was copied from a message, that message's
/// id travels with it. None of those three mean anything to a person reading the Running table an
/// hour later, so the channel names the worker last saw travel with the row too.
fn named_stickies(
    db: &aerona_core::db::Db,
    guild: Snowflake,
    rows: &[aerona_core::models::StickyRecord],
) -> Vec<Value> {
    let names = reported_names(db, guild, "picker", "channels");
    rows.iter()
        .map(|r| {
            let mut v = serde_json::to_value(r).unwrap_or_else(|_| json!({}));
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "channelName".into(),
                    names
                        .get(&r.channel_id.to_string())
                        .map_or(Value::Null, |n| json!(n)),
                );
            }
            v
        })
        .collect()
}

/// `id -> name` for one list inside a worker report (`picker`'s `channels`, `categories`,
/// `roles`). A report that has never been written yields an empty map, which the panel renders
/// as an id rather than a lie.
fn reported_names(
    db: &aerona_core::db::Db,
    guild: Snowflake,
    kind: &str,
    list: &str,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Some(report) = db.guild_report(guild, kind).ok().flatten() else {
        return out;
    };
    for row in report
        .get(list)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(id), Some(name)) = (
            row.get("id").and_then(Value::as_str),
            row.get("name").and_then(Value::as_str),
        ) {
            out.insert(id.to_string(), name.to_string());
        }
    }
    out
}

async fn add_sticky(
    State(state): State<AppState>,
    g: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "config")?;
    let channel = sf_field(body.get(), "channel").map_err(Invalid)?;
    // The anchor is optional now: a sticky can be written here, in which case the bot's own post
    // is the sticky and there is no Discord message to point back at. `0` is that "no anchor",
    // and it is a number rather than an Option because the column has to compare equal to
    // nothing-but-zero for the partial unique index to let two of them exist.
    let message = opt_sf_field(body.get(), "message")
        .map_err(Invalid)?
        .unwrap_or(Snowflake(0));
    // A sticky's plain text keeps its lines: two sentences on two lines is one note, and a folded
    // second line is a formatting decision nobody made.
    let content = opt_block_field(body.get(), "content", 1800)
        .map_err(Invalid)?
        .unwrap_or_default();
    // The three keys are the row's own names, because a sticky is a table row more than it is a
    // settings document: the panel reads `embed_title` back from `GET /stickies` and should be
    // able to write the same spelling it read.
    let embed_title = opt_str_field(body.get(), "embed_title", 240).map_err(Invalid)?;
    let embed_body = opt_block_field(body.get(), "embed_body", 1800).map_err(Invalid)?;
    let embed_color =
        match int_field(body.get(), "embed_color", -1, -1, 0xff_ff_ff).map_err(Invalid)? {
            -1 => None,
            v => Some(v as u32),
        };
    let has_text = !content.trim().is_empty();
    let has_embed = embed_title.is_some() || embed_body.is_some();
    // With an anchor and no note, the cached text of that message is the note: retyping what
    // somebody just clicked is the same busywork the id box was.
    let content = match (has_text, message.as_u64()) {
        (true, _) => content,
        (false, m) if m != 0 && !has_embed => state
            .db
            .recalled_message(g.id, message)
            .map_err(Invalid)?
            .map(|(text, _, _)| text)
            .ok_or_else(|| {
                Invalid(Error::Invalid(
                    "write what to post, or pick a message the bot still has the text for".into(),
                ))
            })?,
        (false, _) => String::new(),
    };
    let every = int_field(body.get(), "everyMessages", 50, 1, 1000).map_err(Invalid)? as u32;
    let draft = aerona_core::models::StickyDraft {
        guild_id: g.id,
        channel_id: channel,
        message_id: message,
        content,
        embed_title,
        embed_body,
        embed_color,
        every_messages: every,
        author: g.actor(),
    };
    let id = state.db.add_sticky(&draft).map_err(Invalid)?;
    // Post it now, through the outbox like everything else that touches Discord. Without this a
    // sticky added from the dashboard waits for `everyMessages` messages of chatter before it
    // appears, which reads exactly like the button did nothing.
    let queued = state
        .db
        .enqueue_action(
            g.id,
            g.actor(),
            &g.label(),
            "sticky.post",
            &json!({ "sticky": id }),
            "",
        )
        .map_err(Invalid)?;
    audit(
        &state,
        &g,
        "sticky.create",
        Some(id.to_string()),
        json!({ "channel": channel.to_string(), "every": every, "queued": queued }),
    );
    let rows = state
        .db
        .stickies_for_channel(g.id, channel)
        .map_err(Invalid)?;
    Ok(respond::created(
        json!({ "id": id, "queued": queued, "stickies": named_stickies(&state.db, g.id, &rows) }),
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
// Minecraft cape notifier
// ---------------------------------------------------------------------------

/// `GET /guilds/{guild}/capes` - the settings, what the notifier has found, and when it will look
/// again.
///
/// The list comes out of the database and nothing else. The API never asks the wiki a question:
/// fetching belongs to the worker, which has the retry queue and the trickle, and a panel that
/// polled a third party from the request thread would be a way to make this server do somebody
/// else's denial of service. So the page can be open and correct while minecraft.wiki is down, and
/// the only thing it cannot say is "there is one I have not seen yet".
async fn list_capes(
    State(state): State<AppState>,
    g: Guild,
    Query(q): Query<HashMap<String, String>>,
) -> std::result::Result<Response, Invalid> {
    let limit = q_bounded(&q, "limit", 30, 200).map_err(Invalid)?;
    let cfg = state.db.cape_config(g.id).map_err(Invalid)?;
    let releases = state.db.cape_releases(g.id, limit).map_err(Invalid)?;
    let (last_check, seeded) = state.db.cape_state(g.id).map_err(Invalid)?;
    let channels = reported_names(&state.db, g.id, "picker", "channels");
    let roles = reported_names(&state.db, g.id, "picker", "roles");
    // An id the worker has never reported a name for is shown as an id, not as a blank: the
    // operator needs to see that a channel is set, and a picker refresh fixes the label.
    let label = |names: &std::collections::HashMap<String, String>, id: Option<Snowflake>| {
        id.map(|s| {
            json!({
                "id": s.to_string(),
                "name": names.get(&s.to_string()).map(|n| Value::String(n.clone())).unwrap_or(Value::Null),
            })
        })
        .unwrap_or(Value::Null)
    };
    let rows: Vec<Value> = releases
        .iter()
        .map(|r| {
            let mut v = serde_json::to_value(r).unwrap_or_else(|_| json!({}));
            if let Some(obj) = v.as_object_mut() {
                // The path the panel puts in an <img>. Kept here rather than spelled in the
                // dashboard so the frame count and the url cannot disagree about what exists.
                obj.insert(
                    "frameUrl".into(),
                    if r.frames > 0 {
                        json!(format!(
                            "/api/v1/guilds/{}/capes/{}/frame/{}",
                            g.id,
                            r.slug,
                            cape_hero_frame(r)
                        ))
                    } else {
                        Value::Null
                    },
                );
            }
            v
        })
        .collect();
    // The worker's own report, in its words: it is the one that knows whether the source answered,
    // how many pages it read and what it left for the next pass. A sentence written twice is two
    // chances to say different things about one event, so the panel repeats this one.
    let report = state
        .db
        .guild_report(g.id, "capes")
        .ok()
        .flatten()
        .unwrap_or(Value::Null);
    let status = aerona_core::cape::status_line(&report);
    let error = report
        .get("error")
        .and_then(Value::as_str)
        .map(|e| e.to_string());
    Ok(json_ok(json!({
        "config": cfg,
        "channel": label(&channels, cfg.channel_id),
        "pingRole": label(&roles, cfg.ping_role_id),
        "releases": rows,
        "status": status,
        "error": error,
        "lastCheck": last_check,
        "nextCheck": if cfg.enabled && last_check > 0 { Some(last_check + cfg.check_every_secs() as i64) } else { None },
        "seeded": seeded,
        "frames": aerona_core::models::CAPE_FRAMES,
    })))
}

/// Which of a release's stored frames is the one to show. A drape has four angles and the second is
/// the one where the fold reads; a wiki render is a single picture and frame 0 is all there is.
fn cape_hero_frame(release: &aerona_core::models::CapeRelease) -> u8 {
    if release.render_kind == "drape" {
        aerona_core::cape::HERO_FRAME as u8
    } else {
        0
    }
}

/// `POST /guilds/{guild}/capes/check` - ask the worker to poll now rather than on its interval.
///
/// All this does is zero the stamp the tick compares against, so a second press is the same ask and
/// a hundred are still one poll. Nothing is fetched here, on purpose: see `list_capes`.
async fn check_capes(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    require_scope(g.principal(), "config")?;
    state.db.force_cape_check(g.id).map_err(Invalid)?;
    audit(&state, &g, "cape.check", None, json!({}));
    Ok(json_ok(json!({
        "queued": true,
        "note": "the worker checks on its next pass, within a minute",
    })))
}

/// `GET /guilds/{guild}/capes/{slug}/frame/{frame}` - one stored render, as PNG.
///
/// The bytes are the ones the worker wrote, whether it draped the texture itself or kept the wiki's
/// own picture: no decoding, no resizing, no surprise where a resize is what made the image wrong.
/// A slug that exists with no frames is a `404` with a sentence rather than a broken image, because
/// a cape announced without artwork is a normal row and the panel still has to say why its box is
/// empty.
async fn cape_frame(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    let slug = g.param("slug").map_err(Invalid)?;
    let frame = g.param_i64("frame").map_err(Invalid)?;
    let Some(bytes) = state
        .db
        .cape_render(g.id, &slug, frame.clamp(0, 255) as u8)
        .map_err(Invalid)?
    else {
        return Err(Invalid(Error::Invalid(format!(
            "cape {slug} has no frame {frame} - it was announced without a render"
        ))));
    };
    Ok(inline_png(bytes))
}

/// A picture for an `<img>`, not a download: no disposition, so a browser draws it instead of
/// offering to save it.
///
/// No cache header either, deliberately. The security layer pins `no-store` on every credentialed
/// response, and these bytes are behind a session; teaching that layer an exception for a 224-pixel
/// png would trade a default that is easy to reason about for a saving nobody needs.
fn inline_png(body: Vec<u8>) -> Response {
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "image/png")
        .header(axum::http::header::CONTENT_LENGTH, body.len().to_string())
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| axum::response::Response::new(axum::body::Body::empty()))
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

/// What the logger saw, newest first. `?event=` narrows it to one kind, which is the difference
/// between a log and a search through 5,000 edits.
async fn log_events(
    State(state): State<AppState>,
    g: Guild,
    Query(q): Query<HashMap<String, String>>,
) -> std::result::Result<Response, Invalid> {
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(60)
        .clamp(1, 500);
    let event = q.get("event").map(|v| v.as_str());
    let rows = state
        .db
        .list_log_events(g.id, limit, event)
        .map_err(Invalid)?;
    let cfg = state.db.log_config(g.id).unwrap_or_default();
    Ok(json_ok(json!({
        "events": rows,
        "channelId": cfg.channel_id.map(|c| c.as_u64().to_string()),
        "tracks": cfg.events,
    })))
}

/// Ask the worker for a fresh channel and role list. The picker used to wait for the slow pass,
/// which is why a channel created in Discord took minutes to appear in a settings form and an
/// operator reached for the ID box instead.
async fn picker_refresh(
    State(state): State<AppState>,
    g: Guild,
) -> std::result::Result<Response, Invalid> {
    let id = state
        .db
        .enqueue_action(
            g.id,
            g.actor(),
            &g.label(),
            "picker.refresh",
            &json!({}),
            "",
        )
        .map_err(Invalid)?;
    Ok(json_ok(json!({ "queued": id })))
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
/// so this costs no quota and answers instantly - and it works with no network.
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
        // the dry run must not echo it back either - that would be a way to read the
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

    /// The transcript download is the one route that answers with a file rather than the
    /// envelope, so the builder is tested directly: a silent fallback to an empty body would look
    /// like a missing transcript to a moderator, and a wrong content type is a browser that opens
    /// an HTML transcript as source code.
    #[tokio::test]
    async fn a_text_download_carries_its_body() {
        let resp = transcript_file(
            "ticket-7.txt",
            "hello world".to_string(),
            "text/plain; charset=utf-8",
            true,
        );
        assert_eq!(resp.status(), 200);
        // The cache rule is part of the file contract: this document contains a moderator's
        // internal notes, and a proxy that kept a copy would be a second place it lives.
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default(),
            "private, no-store"
        );
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default(),
            "11"
        );
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default(),
            "attachment; filename=\"ticket-7.txt\""
        );
        let ctype = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), b"hello world");
        assert_eq!(ctype, "text/plain; charset=utf-8");
    }

    #[tokio::test]
    async fn an_html_transcript_is_offered_as_html_and_a_strange_name_is_neutralised() {
        let resp = transcript_file(
            r#"../evil";x.html"#,
            "<!doctype html>".to_string(),
            "text/html; charset=utf-8",
            true,
        );
        let name = resp
            .headers()
            .get(axum::http::header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            !name.contains('/'),
            "a filename cannot be allowed a path: {name}"
        );
        assert_eq!(
            name.matches('"').count(),
            2,
            "only the quotes this builder adds: {name}"
        );
        // The extension has to survive, or a browser is handed a nameless blob and the
        // whole point of choosing a format is lost. The header ends in a quote, so this is
        // about the name inside it.
        assert!(
            name.starts_with("attachment; filename=\"") && name.ends_with(".html\""),
            "{name}"
        );
    }

    /// The other half of the same handler: what the panel's Transcript button asks for. A document
    /// a browser will not render is the report this came in as ("transcript htmls don't work"), and
    /// the difference between the two buttons is this one header.
    #[tokio::test]
    async fn the_same_document_opens_inline_when_it_is_not_being_saved() {
        let open = transcript_file(
            "t.html",
            "<p>hi</p>".into(),
            "text/html; charset=utf-8",
            false,
        );
        let saved = transcript_file(
            "t.html",
            "<p>hi</p>".into(),
            "text/html; charset=utf-8",
            true,
        );
        let seen = |r: axum::response::Response| {
            r.headers()
                .get(axum::http::header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(seen(open), "inline; filename=\"t.html\"");
        assert_eq!(seen(saved), "attachment; filename=\"t.html\"");
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
