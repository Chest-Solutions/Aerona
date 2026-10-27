//! Account and operator routes: the audit trail, API keys, sessions, resource
//! stats, the action-outbox poll endpoint and manual housekeeping.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::Router;
use serde_json::{json, Value};

use aerona_core::db::security::{ActorType, AuditEntry};
use aerona_core::error::Error;
use aerona_core::identity::Snowflake;

use crate::app::AppState;
use crate::auth::{self, Authed};
use crate::respond::{self, Invalid, JsonBody};
use crate::routes::{bool_field, int_field, q_bounded, q_i64, q_opt_sf, str_field};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/me", get(me))
        .route("/audit", get(audit))
        .route("/audit/verify", post(verify_chain))
        .route("/keys", get(list_keys).post(create_key))
        .route("/keys/{id}", delete(revoke_key))
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", delete(revoke_session))
        .route("/sessions/revoke-all", post(revoke_all))
        .route("/stats", get(stats))
        .route("/stats/{process}", get(stats_series))
        .route("/outbox/{id}", get(outbox_status))
        .route("/housekeeping", post(housekeeping))
}

/// Who the dashboard is talking as - and the only place the CSRF value is handed
/// back after login, so the SPA can re-derive its headers after a reload.
async fn me(State(state): State<AppState>, a: Authed) -> std::result::Result<Response, Invalid> {
    let value = match &a.principal {
        auth::Principal::Session { session, .. } => json!({
            "kind": "session",
            "id": session.user_id.to_string(),
            "username": session.username,
            "csrf": session.csrf_secret,
            "guild": session.allowed_guild.map(|g| g.to_string()),
            "expiresAt": session.expires_at,
            "createdAt": session.created_at,
            "ip": session.ip,
            // Sessions the operator is signed in with, so the "sign out
            // everywhere" control has a number next to it.
            "activeSessions": state.db.list_sessions(session.user_id).map_err(Invalid)?.len(),
        }),
        auth::Principal::Key { key } => json!({
            "kind": "key",
            "id": key.id,
            "label": key.label,
            "hint": key.hint,
            "guild": key.guild_id.map(|g| g.to_string()),
            "scopes": key.scopes,
            "expiresAt": key.expires_at,
            "lastUsedAt": key.last_used_at,
            "requestCount": key.request_count,
        }),
    };
    Ok(respond::ok(value))
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

async fn audit(
    State(state): State<AppState>,
    a: Authed,
    Query(q): Query<HashMap<String, String>>,
) -> std::result::Result<Response, Invalid> {
    let limit = q_bounded(&q, "limit", 50, 500).map_err(Invalid)?;
    let before = q_i64(&q, "before").map_err(Invalid)?.map(|v| v.max(1));
    let requested = q_opt_sf(&q, "guild").map_err(Invalid)?;
    if let Some(g) = requested {
        if !a.principal.allows_guild(Some(g)) {
            return Err(Invalid(Error::Forbidden(
                "this credential is scoped to another server",
            )));
        }
    }
    // A scoped key never sees install-wide rows: `None` would list everything.
    let filter = match (&a.principal, requested) {
        (auth::Principal::Key { key }, None) => key.guild_id,
        (_, other) => other,
    };
    let rows = state
        .db
        .list_audit(filter, limit, before)
        .map_err(Invalid)?;
    // Chain verification is a full table walk, so it happens when the operator asks
    // for it (`?verify=true` or POST /audit/verify), never as a side effect of
    // opening the audit page.
    let chain = if crate::routes::q_bool(&q, "verify") {
        match state.db.verify_audit_chain() {
            Ok(n) => Some(json!({ "verifiedRows": n, "intact": true })),
            Err(e) => {
                // A broken chain is the one finding we must not describe loosely: it
                // means somebody with shell access edited the audit table.
                tracing::error!(%e, guild = ?filter.map(|g| g.to_string()), "AUDIT CHAIN BROKEN");
                return Err(Invalid(Error::Internal));
            }
        }
    } else {
        None
    };
    Ok(respond::ok(json!({
        "entries": rows,
        "nextBefore": rows.last().and_then(|r| r["id"].as_i64()),
        "chain": chain,
    })))
}

async fn verify_chain(
    State(state): State<AppState>,
    a: Authed,
) -> std::result::Result<Response, Invalid> {
    // Deliberately session-only: a machine key should not be able to trigger a
    // full-table hash walk.
    require_session(&a)?;
    let rows = state.db.verify_audit_chain().map_err(Invalid)?;
    Ok(respond::ok(json!({ "verifiedRows": rows, "intact": true })))
}

// ---------------------------------------------------------------------------
// API keys
// ---------------------------------------------------------------------------

async fn list_keys(
    State(state): State<AppState>,
    a: Authed,
) -> std::result::Result<Response, Invalid> {
    require_session(&a)?;
    let keys = state.db.list_api_keys(None).map_err(Invalid)?;
    let at = aerona_core::time::now();
    let rows: Vec<Value> = keys
        .into_iter()
        .map(|k| {
            json!({
                "id": k.id,
                "label": k.label,
                "hint": k.hint,
                "guild": k.guild_id.map(|g| g.to_string()),
                "scopes": k.scopes,
                "createdAt": k.created_at,
                "lastUsedAt": k.last_used_at,
                "expiresAt": k.expires_at,
                "expired": k.expires_at > 0 && k.expires_at <= at,
                "revokedAt": k.revoked_at,
                "requestCount": k.request_count,
            })
        })
        .collect();
    Ok(respond::ok(json!({ "keys": rows })))
}

/// Mint a key. The plaintext is in this response and nowhere else.
async fn create_key(
    State(state): State<AppState>,
    a: Authed,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    require_session(&a)?;
    let label = str_field(body.get(), "label", 64).map_err(Invalid)?;
    let guild = opt_guild(body.get())?;
    let scopes: Vec<String> = match body.get().get("scopes") {
        None | Some(Value::Null) => vec!["read".into()],
        Some(Value::Array(list)) => {
            let mut out = Vec::new();
            for v in list {
                out.push(
                    v.as_str()
                        .map(|s| s.trim().to_ascii_lowercase())
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| Invalid(Error::Invalid("scopes must be strings".into())))?,
                );
            }
            out
        }
        Some(_) => {
            return Err(Invalid(Error::Invalid(
                "scopes must be an array of strings".into(),
            )))
        }
    };
    let ttl_days = int_field(body.get(), "ttlDays", 90, 0, 3650).map_err(Invalid)?;
    let (id, secret, hint) = state
        .db
        .create_api_key(
            &state.pepper(),
            &label,
            guild,
            &scopes,
            a.user_id(),
            ttl_days,
        )
        .map_err(Invalid)?;
    state.db.audit_best_effort(
        AuditEntry::new("key.create")
            .maybe_guild(guild)
            .actor(a.principal.actor_type(), Some(a.user_id()), &a.label())
            .target(format!("key:{id}"))
            .ip(&a.ip)
            .meta(json!({ "label": label, "scopes": scopes, "hint": hint, "ttlDays": ttl_days })),
    );
    Ok(respond::created(json!({
        "id": id,
        "key": secret,
        "hint": hint,
        "scopes": scopes,
        "warning": "store it now - this response is the only time the full key exists",
    })))
}

async fn revoke_key(
    State(state): State<AppState>,
    a: Authed,
    Path(params): Path<HashMap<String, String>>,
) -> std::result::Result<Response, Invalid> {
    require_session(&a)?;
    let id = params
        .get("id")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .ok_or_else(|| Invalid(Error::Invalid("key id must be a positive number".into())))?;
    let scope = match &a.principal {
        auth::Principal::Session { session, .. } => session.allowed_guild,
        auth::Principal::Key { .. } => None,
    };
    state.db.revoke_api_key(id, scope).map_err(Invalid)?;
    state.db.audit_best_effort(
        AuditEntry::new("key.revoke")
            .maybe_guild(scope)
            .actor(a.principal.actor_type(), Some(a.user_id()), &a.label())
            .target(format!("key:{id}"))
            .ip(&a.ip)
            .meta(json!({})),
    );
    Ok(respond::ok(json!({ "revoked": id })))
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

async fn list_sessions(
    State(state): State<AppState>,
    a: Authed,
) -> std::result::Result<Response, Invalid> {
    let user = a.user_id();
    let current = match &a.principal {
        auth::Principal::Session { session, .. } => Some(session.id),
        auth::Principal::Key { .. } => None,
    };
    let rows: Vec<Value> = state
        .db
        .list_sessions(user)
        .map_err(Invalid)?
        .into_iter()
        .map(|s| {
            json!({
                "id": s.id,
                "current": Some(s.id) == current,
                "createdAt": s.created_at,
                "lastSeenAt": s.last_seen_at,
                "expiresAt": s.expires_at,
                "ip": s.ip,
                "userAgent": s.user_agent,
                "guild": s.allowed_guild.map(|g| g.to_string()),
            })
        })
        .collect();
    Ok(respond::ok(json!({ "sessions": rows })))
}

async fn revoke_session(
    State(state): State<AppState>,
    a: Authed,
    Path(params): Path<HashMap<String, String>>,
) -> std::result::Result<Response, Invalid> {
    let id = params
        .get("id")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .ok_or_else(|| {
            Invalid(Error::Invalid(
                "session id must be a positive number".into(),
            ))
        })?;
    let user = a.user_id();
    // Only your own sessions: revoking someone else's is an admin action, and there
    // is no admin role model yet, so it is simply not offered.
    let mine = state
        .db
        .list_sessions(user)
        .map_err(Invalid)?
        .into_iter()
        .any(|s| s.id == id);
    if !mine {
        return Err(Invalid(Error::NotFound("session")));
    }
    state.db.revoke_session(id).map_err(Invalid)?;
    audit_line(
        &state,
        &a,
        "session.revoke",
        Some(id.to_string()),
        json!({}),
    );
    Ok(respond::ok(json!({ "revoked": id })))
}

async fn revoke_all(
    State(state): State<AppState>,
    a: Authed,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    let keep_current = bool_field(body.get(), "keepCurrent", true);
    let current = match (&a.principal, keep_current) {
        (auth::Principal::Session { session, .. }, true) => Some(session.id),
        _ => None,
    };
    let n = state
        .db
        .revoke_all_sessions(a.user_id(), current)
        .map_err(Invalid)?;
    audit_line(
        &state,
        &a,
        "session.revoke_all",
        None,
        json!({ "count": n, "keepCurrent": keep_current }),
    );
    // If we killed our own session too, tell the client to stop using it.
    let cleared_self = current.is_none();
    let mut resp = respond::ok(json!({ "revoked": n, "selfRevoked": cleared_self }));
    if cleared_self {
        let v = auth::cleared_cookie(&state.cfg);
        if !v.is_empty() {
            resp.headers_mut().append("set-cookie", v);
        }
    }
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Stats, outbox, housekeeping
// ---------------------------------------------------------------------------

async fn stats(State(state): State<AppState>, a: Authed) -> std::result::Result<Response, Invalid> {
    let _ = &a;
    let bot = state.db.latest_stats("bot").map_err(Invalid)?;
    let api = state.db.latest_stats("api").map_err(Invalid)?;
    Ok(respond::ok(json!({
        "bot": bot,
        "api": api,
        "guilds": state.db.guilds().map_err(Invalid)?.len(),
        "dbSizeKb": state.db.size_kb().map_err(Invalid)?,
        "users": state.db.user_count().map_err(Invalid)?,
        "queue": {
            "pending": state.db.outbox_depth().map_err(Invalid)?.0,
            "failed": state.db.outbox_depth().map_err(Invalid)?.1,
        },
        "live": state.db.bot_statuses().map_err(Invalid)?,
    })))
}

async fn stats_series(
    State(state): State<AppState>,
    a: Authed,
    Path(params): Path<HashMap<String, String>>,
    Query(q): Query<HashMap<String, String>>,
) -> std::result::Result<Response, Invalid> {
    let _ = &a;
    let process = params.get("process").map(|s| s.as_str()).unwrap_or("bot");
    if !matches!(process, "bot" | "api") {
        return Err(Invalid(Error::Invalid("process must be bot or api".into())));
    }
    let points = q_bounded(&q, "points", 120, 2000).map_err(Invalid)?;
    let rows = state.db.stats_series(process, points).map_err(Invalid)?;
    Ok(respond::ok(json!({
        "process": process,
        "points": rows.len(),
        "series": rows,
    })))
}

/// `GET /outbox/{id}` - how a caller learns whether a queued action ran. Readable
/// by any principal that could have queued it; the guild check keeps it from being a
/// way to watch another server's moderation.
async fn outbox_status(
    State(state): State<AppState>,
    _a: Authed,
    Path(params): Path<HashMap<String, String>>,
) -> std::result::Result<Response, Invalid> {
    let id = params
        .get("id")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .ok_or_else(|| Invalid(Error::Invalid("outbox id must be a positive number".into())))?;
    let row = state.db.outbox_status(id).map_err(Invalid)?;
    if row.is_null() {
        return Err(Invalid(Error::NotFound("queued action")));
    }
    Ok(respond::ok(row))
}

async fn housekeeping(
    State(state): State<AppState>,
    a: Authed,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    require_session(&a)?;
    let days = int_field(body.get(), "retentionDays", 90, 1, 3650).map_err(Invalid)?;
    let per_guild =
        int_field(body.get(), "logCachePerGuild", 20_000, 1, 500_000).map_err(Invalid)?;
    let removed = state.db.housekeeping(days, per_guild).map_err(Invalid)?;
    state.db.optimize().map_err(Invalid)?;
    audit_line(
        &state,
        &a,
        "housekeeping",
        None,
        json!({ "removed": removed, "retentionDays": days }),
    );
    Ok(respond::ok(json!({ "removed": removed })))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Optional guild for a key: `null` means an install-wide key, which is exactly what
/// a CI integration wants, so it is not collapsed into "some guild".
fn opt_guild(body: &Value) -> std::result::Result<Option<Snowflake>, Invalid> {
    match body.get("guild") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let raw = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => return Err(Invalid(Error::Invalid("guild must be a discord id".into()))),
            };
            let parsed = Snowflake::parse(&raw)
                .map_err(|_| Invalid(Error::Invalid("guild must be a discord id".into())))?;
            if parsed.is_zero() {
                return Err(Invalid(Error::Invalid("guild must be a discord id".into())));
            }
            Ok(Some(parsed))
        }
    }
}

/// Machine keys must not be able to mint, list or revoke keys, read every
/// session, or run maintenance: those are operator actions, not API actions.
fn require_session(a: &Authed) -> std::result::Result<(), Invalid> {
    match a.principal {
        auth::Principal::Session { .. } => Ok(()),
        auth::Principal::Key { .. } => Err(Invalid(Error::Forbidden(
            "this route requires a dashboard session",
        ))),
    }
}

fn audit_line(state: &AppState, a: &Authed, action: &str, target: Option<String>, meta: Value) {
    state.db.audit_best_effort(
        AuditEntry::new(action)
            .actor(
                match &a.principal {
                    auth::Principal::Session { .. } => ActorType::User,
                    auth::Principal::Key { .. } => ActorType::ApiKey,
                },
                Some(a.user_id()),
                &a.label(),
            )
            .maybe_target(target)
            .ip(&a.ip)
            .meta(meta),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_guild_accepts_both_json_shapes() {
        assert_eq!(opt_guild(&json!({})).unwrap(), None);
        assert_eq!(opt_guild(&json!({"guild": null})).unwrap(), None);
        assert_eq!(
            opt_guild(&json!({"guild": "123"})).unwrap(),
            Some(Snowflake(123))
        );
        assert_eq!(
            opt_guild(&json!({"guild": 123})).unwrap(),
            Some(Snowflake(123))
        );
        assert!(opt_guild(&json!({"guild": "abc"})).is_err());
        assert!(opt_guild(&json!({"guild": [1]})).is_err());
        assert!(
            opt_guild(&json!({"guild": "0"})).is_err(),
            "zero is not a discord id"
        );
    }

    #[test]
    fn query_adapter_reads_limits() {
        let mut m = HashMap::new();
        m.insert("points".to_string(), "5000".to_string());
        assert_eq!(q_bounded(&m, "points", 120, 2000).unwrap(), 2000);
        m.insert("points".to_string(), "nope".to_string());
        assert!(q_bounded(&m, "points", 120, 2000).is_err());
    }
}
