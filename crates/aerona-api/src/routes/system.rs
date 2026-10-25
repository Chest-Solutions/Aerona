//! Install-level routes: status, health, the pairing bootstrap, Discord OAuth,
//! the guild list and the settings-document API.

use std::collections::HashMap;

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};

use aerona_core::error::Error;
use aerona_core::models::Feature;

use crate::app::{manifest, AppState};
use crate::auth::{self, Authed, Guild};
use crate::oauth;
use crate::respond::{self, Invalid, JsonBody, RequestId};
use crate::routes::{merge, normalize, q_bounded, settings_for};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/healthz", get(health))
        .route("/manifest", get(manifest_route))
        .route("/auth/discord", get(discord_start))
        .route("/auth/discord/callback", get(discord_callback_legacy))
        .route("/logout", post(logout))
        .route("/guilds", get(list_guilds))
        .route("/guilds/{guild}", get(guild_root))
        .route(
            "/guilds/{guild}/settings/{feature}",
            get(read_settings).patch(update_settings),
        )
        .route(
            "/guilds/{guild}/settings/{feature}/history",
            get(settings_history),
        )
}

/// Routes that live outside `/api/v1`, merged onto the outer router.
///
/// The Discord callback is mounted at `/auth/callback` — the path a portal entry reads as a
/// login endpoint rather than an API endpoint, and the shape the working examples use — while
/// `/api/v1/auth/discord/callback` keeps answering as an alias. An install configured before
/// the move therefore does not have to be re-registered in the portal to keep signing people
/// in, and a query string (the `code` and `state` Discord just sent) is carried across
/// verbatim, because dropping it would turn a redirect into a failed login.
/// The paths this crate answers *outside* `/api/v1`, in one list, because two things have to
/// agree on it: the router mounts them, and `app.rs`'s static layer has to know they exist.
/// That layer decides "there is no such page" by looking for `dist/<path>/index.html`, and a
/// route is not a file — so before this list existed, a sign-in bounce out of `/auth/callback`
/// had its 303 overwritten with a 404 by the layer that never knew a handler had answered. The
/// browser only follows 3xx, so the operator was left staring at the 404 document while the
/// `Location` header went unread, and the sign-in page was unreachable for real installs.
pub const ROOT_PATHS: &[&str] = &[aerona_core::config::Config::CALLBACK_PATH];

/// Prefixes of paths the *client* router owns, and which therefore have no file of their
/// own: a guild page URL carries the server id inside it, so `dist/guild/<id>/tickets/index
/// .html` will never exist while the route is entirely real. Without this list a refresh on
/// a deep link into a server answers the 404 document — which is how a shareable link to
/// somebody's moderation queue stops working, and why the OAuth return target is worth
/// having at all.
pub const APP_PREFIXES: &[&str] = &["/guild/"];

/// Whether `path` belongs to the client router rather than to the file tree.
pub fn owned_by_client_router(path: &str) -> bool {
    APP_PREFIXES.iter().any(|p| path.starts_with(p))
}

/// Built from `ROOT_PATHS`, so the two can never drift apart: adding a root route without
/// telling the static layer about it is the exact mistake this list exists to prevent.
pub fn public_routes() -> Router<AppState> {
    ROOT_PATHS.iter().fold(Router::new(), |r, path| {
        r.route(path, get(discord_callback))
    })
}

/// `GET /api/v1/status` — unauthenticated on purpose: the supervisor and any
/// operator with a browser must be able to see that the install is alive before a
/// session exists. It therefore reports only non-sensitive facts.
async fn status(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    id: RequestId,
) -> Response {
    let at = aerona_core::time::now();
    let redirect = crate::oauth::redirect_preview(&state, &headers);
    let guilds = state.db.guilds().unwrap_or_default();
    let db_kb = state.db.size_kb().unwrap_or_default();
    let bot = state.db.latest_stats("bot").ok().flatten();
    let (pending, failed) = state.db.outbox_depth().unwrap_or((0, 0));
    respond::ok(json!({
        "version": crate::VERSION,
        "apiVersion": aerona_core::API_VERSION,
        "uptimeSecs": (at - state.started_at).max(0),
        "guilds": guilds.len(),
        "db": { "sizeKb": db_kb },
        "queue": { "pending": pending, "failed": failed },
        "bot": bot.as_ref().map(|b| json!({
            "wsLatencyMs": b.ws_latency_ms,
            "rssKb": b.rss_kb,
            "messagesPerMin": b.messages_per_min,
            "reportingAt": b.at,
            // The bot writes this every ~20 s; three missed beats is an outage.
            "stale": at - b.at > 90,
        })),
        "features": {
            // Whether the AI text filter *can* run, not whether any guild enabled it.
            "aiFilter": state.cfg.sightengine.is_some(),
            "discordOauth": state.cfg.oauth_configured(),
            "contentIntent": state.cfg.content_intent,
        },
        // The value a login would send as `redirect_uri`, plus where it came from. Only the
        // two operator-authored sources are echoed back: when it is derived from the request's
        // own `Host`, reflecting it would hand an attacker a string to read out of JSON, and
        // the dashboard does not need it — the browser's own origin *is* that value, and
        // the login page builds the derived case locally.
        "oauth": {
            "configured": state.cfg.oauth_configured(),
            "redirectUriSource": redirect.0,
            "redirectUri": match redirect.0 {
                "pinned" | "base-url" => Value::from(redirect.1.clone()),
                _ => Value::Null,
            },
            "callbackPath": aerona_core::config::Config::CALLBACK_PATH,
            "guidance": crate::oauth::redirect_guidance(&state, &headers),
        },
        "requestId": id.0,
    }))
}

/// Liveness only: no database work, so it still answers when storage is wedged.
/// The liveness probe. Deliberately *not* the `{data,meta}` envelope: `main.py`
/// greps for `{"ok":true}`, a container healthcheck may curl it, and neither should
/// have to know the API's shape. No database touch, so it stays true while a query
/// is slow.
async fn health() -> Response {
    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )],
        r#"{"ok":true}"#,
    )
        .into_response()
}

async fn manifest_route(State(state): State<AppState>) -> Response {
    respond::ok(manifest(&state.cfg))
}

async fn discord_start(
    State(state): State<AppState>,
    peer: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> std::result::Result<Response, Invalid> {
    let ip = auth::client_ip(&headers, state.cfg.trusted_proxy, Some(peer.0));
    // `?next=` is the whole of the "come back where you were" story: a person who opens a
    // guild page without a session should land on that page, not on the top of the dashboard
    // with a sidebar to click through again. It is stored with the state row and validated
    // there, so a crafted value can only ever be a path on this origin.
    let next = crate::routes::q(&query, "next");
    oauth::start_login(&state, &ip, &headers, next)
        .await
        .map_err(Invalid)
}

/// The legacy path. It forwards the query unchanged rather than duplicating the handler, so
/// there is one place where a code is traded for a session.
async fn discord_callback_legacy(uri: axum::http::Uri) -> Response {
    let query = uri.query().unwrap_or_default();
    let target = if query.is_empty() {
        "/auth/callback".to_string()
    } else {
        format!("/auth/callback?{query}")
    };
    axum::response::Redirect::permanent(&target).into_response()
}

async fn discord_callback(
    State(state): State<AppState>,
    peer: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> std::result::Result<Response, Invalid> {
    let ip = auth::client_ip(&headers, state.cfg.trusted_proxy, Some(peer.0));
    oauth::finish_login(&state, &query, &ip, &headers)
        .await
        .map_err(Invalid)
}

async fn logout(
    State(state): State<AppState>,
    auth: Authed,
) -> std::result::Result<Response, Invalid> {
    match &auth.principal {
        auth::Principal::Session { session, .. } => {
            state.db.revoke_session(session.id).map_err(Invalid)?;
            state.db.audit_best_effort(
                aerona_core::db::security::AuditEntry::new("auth.logout")
                    .actor(
                        aerona_core::db::security::ActorType::User,
                        Some(session.user_id),
                        &session.username,
                    )
                    .ip(&auth.ip)
                    .meta(json!({ "sessionId": session.id })),
            );
        }
        // A key cannot be "logged out" — revoking it is an explicit admin action.
        auth::Principal::Key { key } => {
            return Err(Invalid(Error::Invalid(format!(
                "api key {} is revoked under /api/v1/keys, not logged out",
                key.hint
            ))));
        }
    }
    let mut resp = respond::ok(json!({ "ok": true }));
    let v = auth::cleared_cookie(&state.cfg);
    if !v.is_empty() {
        resp.headers_mut().append("set-cookie", v);
    }
    Ok(resp)
}

async fn list_guilds(
    State(state): State<AppState>,
    auth: Authed,
) -> std::result::Result<Response, Invalid> {
    let bound = match &auth.principal {
        auth::Principal::Session { session, .. } => session.allowed_guild,
        auth::Principal::Key { .. } => None,
    };
    let mut out = Vec::new();
    let mut seen: Vec<i64> = Vec::new();
    for g in state.db.guilds().map_err(Invalid)? {
        if !auth.principal.allows_guild(Some(g.id)) || bound.is_some_and(|b| b != g.id) {
            continue;
        }
        seen.push(g.id.as_i64());
        out.push(json!({
            "id": g.id.to_string(),
            "name": g.name,
            "icon": g.icon,
            "memberCount": g.member_count,
            "botPresent": g.bot_present,
            "openTickets": state.db.open_ticket_count(g.id, None).unwrap_or(0),
            "stickies": state.db.sticky_count(g.id).unwrap_or(0),
        }));
    }
    // The other half of the list: servers this session's operator administers that Aerona is
    // not in yet. They come from the grant list Discord answered with at sign-in, and without
    // them a fresh install tells the only person who can fix it that there is no server — the
    // invite step of a public bot's dashboard exists precisely for this state, so each row
    // carries its own link. A key principal has no grant list and sees only what the bot
    // already serves, which is also all a key is allowed to touch.
    if let auth::Principal::Session { session, .. } = &auth.principal {
        let raw = state.db.session_grants_json(session.id).map_err(Invalid)?;
        for row in raw
            .as_deref()
            .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
        {
            let id = match row.get("id") {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(serde_json::Value::Number(n)) => n.to_string(),
                _ => continue,
            };
            let Ok(numeric) = id.parse::<i64>() else {
                continue;
            };
            if seen.contains(&numeric) {
                continue;
            }
            seen.push(numeric);
            let invite = state
                .cfg
                .discord_client_id
                .as_deref()
                .map(|cid| crate::oauth::bot_invite_url(cid, &id));
            out.push(json!({
                "id": id,
                "name": row.get("name").and_then(serde_json::Value::as_str).unwrap_or("unnamed server"),
                "icon": row.get("icon").and_then(serde_json::Value::as_str),
                "memberCount": 0,
                "botPresent": false,
                "openTickets": 0,
                "stickies": 0,
                "inviteUrl": invite,
            }));
        }
    }
    Ok(respond::ok(json!({ "guilds": out })))
}

/// `GET /guilds/{guild}` — everything the dashboard's overview needs in one round
/// trip: every feature document, live state and the headline counts.
async fn guild_root(
    State(state): State<AppState>,
    guild: Guild,
) -> std::result::Result<Response, Invalid> {
    let mut settings = serde_json::Map::new();
    for f in Feature::ALL {
        if f == Feature::Keys {
            continue;
        }
        settings.insert(
            f.as_str().to_string(),
            settings_for(&state.db, guild.id, f).map_err(Invalid)?,
        );
    }
    let mod_stats = state.db.moderation_stats(guild.id).map_err(Invalid)?;
    let tickets = state.db.ticket_summary(guild.id).map_err(Invalid)?;
    let live = state
        .db
        .bot_statuses()
        .map_err(Invalid)?
        .into_iter()
        .find(|s| s["guildId"].as_str() == Some(guild.id.to_string().as_str()));
    let raid = state.db.active_raid(guild.id).map_err(Invalid)?;
    let rules = state.db.count_custom_rules(guild.id).map_err(Invalid)?;
    let state_row = state.db.guild_state(guild.id).map_err(Invalid)?;
    Ok(respond::ok(json!({
        "guild": {
            "id": guild.id.to_string(),
            "addedAt": state_row.as_ref().map(|s| s.added_at),
            "syncedAt": state_row.as_ref().map(|s| s.synced_at),
        },
        "settings": Value::Object(settings),
        "moderation": mod_stats,
        "tickets": tickets,
        "aiRules": { "total": rules.0, "enabled": rules.1 },
        "raid": { "active": raid.is_some(), "event": raid },
        "live": live,
    })))
}

async fn read_settings(
    State(state): State<AppState>,
    guild: Guild,
) -> std::result::Result<Response, Invalid> {
    let f = guild.param_feature().map_err(Invalid)?;
    Ok(respond::ok(
        settings_for(&state.db, guild.id, f).map_err(Invalid)?,
    ))
}

/// `PATCH /guilds/{guild}/settings/{feature}` — merge, validate, store, audit.
async fn update_settings(
    State(state): State<AppState>,
    guild: Guild,
    body: JsonBody,
) -> std::result::Result<Response, Invalid> {
    let f = guild.param_feature().map_err(Invalid)?;
    crate::routes::require_scope(guild.principal(), crate::routes::settings_scope(f))?;

    let current = settings_for(&state.db, guild.id, f).map_err(Invalid)?;
    let mut merged = current.clone();
    merge(&mut merged, body.get());
    let normalized = normalize(f, merged).map_err(Invalid)?;

    // The *normalized* document is stored, not the patch: `guild_config` then always
    // holds a complete config, so a reader (and the bot) never has to know which
    // fields were defaulted.
    state
        .db
        .put_settings(guild.id, f, &normalized, Some(guild.actor()))
        .map_err(Invalid)?;
    let changed = normalized != current;
    state.db.audit_best_effort(
        aerona_core::db::security::AuditEntry::new(format!("settings.{}", f.as_str()))
            .for_guild(guild.id)
            .actor(
                guild.principal().actor_type(),
                Some(guild.actor()),
                &guild.label(),
            )
            .target(f.as_str().to_string())
            .ip(guild.ip())
            .meta(json!({ "changed": changed, "patch": body.get().clone() })),
    );
    // Two documents have a Discord-side consequence, and the outbox is the only way to
    // reach Discord: saving the bot's guild profile queues the apply, and changing which
    // commands this server has queued a re-registration. Both are best-effort — a failed
    // enqueue must not turn a saved setting into an error, because the setting *is* saved.
    // At most one follow-up per document, so it is an id and not a list: a page that shows
    // "Saved · queued #4812" must be able to name the one row it queued.
    let follow_up = match f {
        Feature::Identity => Some("identity.apply"),
        Feature::Commands => Some("commands.sync"),
        _ => None,
    };
    let mut queued = None;
    if let Some(action) = follow_up {
        match state.db.enqueue_action(
            guild.id,
            guild.actor(),
            &guild.label(),
            action,
            &json!({}),
            "",
        ) {
            Ok(id) => queued = Some(id),
            // A failed enqueue must not turn a saved setting into an error: the setting *is*
            // saved, and the worker will pick the change up on its next reload anyway.
            Err(e) => {
                tracing::warn!(guild = %guild.id, %action, error = %e, "could not queue the follow-up")
            }
        }
    }

    Ok(respond::ok(json!({
        "changed": changed,
        "settings": normalized,
        "queued": queued,
    })))
}

async fn settings_history(
    State(state): State<AppState>,
    guild: Guild,
    Query(query): Query<HashMap<String, String>>,
) -> std::result::Result<Response, Invalid> {
    let f = guild.param_feature().map_err(Invalid)?;
    let limit = q_bounded(&query, "limit", 25, 200).map_err(Invalid)?;
    // `config_history` is per guild; filtering by feature keeps the drawer cheap.
    let rows: Vec<Value> = state
        .db
        .settings_history(guild.id, limit)
        .map_err(Invalid)?
        .into_iter()
        .filter(|r| r["feature"].as_str() == Some(f.as_str()))
        .collect();
    Ok(respond::ok(
        json!({ "history": rows, "feature": f.as_str() }),
    ))
}

#[cfg(test)]
mod tests {
    use aerona_core::identity::Snowflake;

    #[test]
    fn a_guild_url_is_a_route_even_though_no_file_matches_it() {
        // The rule the static layer needs: one prefix, and nothing else pretending to be it.
        assert!(owned_by_client_router("/guild/123456789012345678/tickets/"));
        assert!(owned_by_client_router("/guild/1/"));
        // A path that merely mentions the word is not the client's, and `/guilds/` (the API's
        // own collection) must never be served a document.
        for miss in ["/guilds/1/", "/guild", "/api/v1/guild/1", "/x/guild/1/"] {
            assert!(
                !owned_by_client_router(miss),
                "{miss} is not a client route"
            );
        }
    }

    use super::*;
    use crate::routes::default_doc;
    use crate::routes::q;

    fn key(scopes: &[&str]) -> auth::Principal {
        auth::Principal::Key {
            key: aerona_core::db::security::ApiKeyRecord {
                id: 1,
                label: "ci".into(),
                hint: "aer_live_ci".into(),
                guild_id: None,
                scopes: scopes.iter().map(|s| s.to_string()).collect(),
                created_by: Snowflake(1),
                created_at: 0,
                last_used_at: None,
                expires_at: 0,
                revoked_at: None,
                request_count: 0,
            },
        }
    }

    #[test]
    fn scopes_gate_writes() {
        assert!(crate::routes::require_scope(&key(&["read"]), "read").is_ok());
        assert!(crate::routes::require_scope(&key(&["read"]), "config").is_err());
        assert!(crate::routes::require_scope(&key(&["config"]), "config").is_ok());
        assert!(crate::routes::require_scope(
            &auth::Principal::Session {
                session: aerona_core::db::security::Session {
                    id: 1,
                    user_id: Snowflake(2),
                    username: "ops".into(),
                    created_at: 0,
                    last_seen_at: 0,
                    expires_at: i64::MAX,
                    csrf_secret: "c".into(),
                    ip: String::new(),
                    user_agent: String::new(),
                    allowed_guild: None,
                },
                token: "t".into(),
            },
            "config"
        )
        .is_ok());
    }

    #[test]
    fn every_feature_but_keys_has_a_default_document() {
        for f in Feature::ALL {
            let d = default_doc(f);
            if f == Feature::Keys {
                assert!(d.is_null(), "keys must have no settings document");
            } else {
                assert!(d.is_object(), "{f:?} must have an object default");
            }
        }
    }

    #[test]
    fn query_helpers_read_the_dashboard_params() {
        let mut query = HashMap::new();
        query.insert("limit".to_string(), "5000".to_string());
        assert_eq!(q_bounded(&query, "limit", 25, 200).unwrap(), 200);
        assert_eq!(q_bounded(&query, "missing", 7, 100).unwrap(), 7);
        assert_eq!(q(&query, "limit"), Some("5000"));
        assert_eq!(q(&query, "missing"), None);
    }
}
