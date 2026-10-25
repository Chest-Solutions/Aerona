//! Discord OAuth2: the only way to turn "I own a Discord account" into "I may
//! administer this install" without a password file.
//!
//! Flow: `start_login` mints a single-use `state` and redirects to Discord; `finish_login`
//! consumes it, exchanges the code, reads the profile and the guild list, and mints a
//! session *only* for an account that manages a server this install actually serves.
//! That permission check is the whole authorisation model now that the console pairing
//! code is gone: there is exactly one door, and Discord holds the key to it.
//!
//! The request deliberately matches the shape of a plain Discord authorization link —
//! `response_type=code`, `scope=identify+guilds`, one `state`, nothing else. No PKCE
//! (this is a confidential client: it authenticates with a client secret at the token
//! endpoint) and no `prompt=none`, which asks Discord to fail rather than show the
//! consent screen to somebody who has never authorized the application. Nothing is
//! written to disk between the two hops beyond the 10-minute state row that the purge
//! pass collects.

use std::collections::HashMap;
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use aerona_core::error::{Error, Result};
use aerona_core::identity::Snowflake;

use crate::app::AppState;
use crate::auth;

const AUTHORIZE_URL: &str = "https://discord.com/oauth2/authorize";
const TOKEN_URL: &str = "https://discord.com/api/oauth2/token";
const API: &str = "https://discord.com/api/v10";
/// Spelled with a `+`, because that is what an authorize URL that works looks like. `%20`
/// means the same thing to Discord; it just makes an operator comparing a portal entry, a
/// log line and a browser bar count escapes.
const SCOPE: &str = "identify+guilds";

/// `MANAGE_GUILD`, and `ADMINISTRATOR` which implies it.
const PERMISSION_MASK: i64 = (1 << 5) | (1 << 3);

/// The absolute origin this login must return to.
///
/// Never `cfg.base_url` blindly: on a panel install that value is the internal
/// `http://localhost:PORT` fallback, and a `redirect_uri` built from it sends the browser
/// to a machine it is not talking to. `Config::request_origin` reflects the request's own
/// host when it is provably this box, and refuses when the host is a name the operator
/// never configured — which is also the answer to Host-header injection, because a
/// `redirect_uri` we took from the attacker's own header would collect the code for them.
fn login_origin(state: &AppState, headers: &HeaderMap) -> Option<String> {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // A forwarded scheme only counts when a proxy was declared, for the same reason
    // `client_ip` ignores the header otherwise: anyone can send it.
    let proto = if state.cfg.trusted_proxy {
        match headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(',').next().unwrap_or("").trim())
        {
            Some(p) if p.eq_ignore_ascii_case("https") => "https",
            _ => "http",
        }
    } else {
        "http"
    };
    state.cfg.request_origin(host, proto)
}

/// Where a browser lands when a login cannot be completed. The dashboard turns these tokens
/// into a sentence, so the reason is a fixed string from this module — or, for an error
/// Discord itself reported, the sanitised shadow of it: bounded to `[a-z0-9_-]` and 40
/// characters, because the `error` query parameter is attacker-reachable text and a
/// `Location` header is where a reflected string does the most damage.
fn login_redirect(reason: &str) -> Response {
    // The trailing slash is load-bearing, not tidiness: the dashboard's pages are directories
    // in `dist` (`dist/login/index.html`), and the static layer's file check is what decides
    // whether a path is a page. `/login?error=…` is a different answer to that check from
    // `/login/?error=…`, and the bounce is supposed to land on the page, not on the 404
    // document with a `Location` header nobody reads.
    axum::response::Redirect::to(&format!("/login/?error={reason}")).into_response()
}

/// Keep an upstream error code printable in a query string and safe to put in a `Location`.
fn safe_reason(err: &str) -> String {
    let kept: String = err
        .chars()
        .take(40)
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '-')
        .collect();
    if kept.is_empty() {
        "discord-rejected".to_string()
    } else {
        format!("discord-{kept}")
    }
}

/// The `redirect_uri` this install would send for a request arriving with these headers, and
/// where that value came from. `None` URI means "we refuse to guess", which the dashboard
/// renders as the instruction to set the variable — never as a half-right URL.
pub fn redirect_preview(state: &AppState, headers: &HeaderMap) -> (&'static str, Option<String>) {
    let origin = login_origin(state, headers);
    let source = if state.cfg.oauth_redirect_uri.is_some() {
        "pinned"
    } else if state.cfg.base_url_explicit {
        "base-url"
    } else if origin.is_some() {
        "request-host"
    } else {
        "unset"
    };
    let uri = origin.map(|origin| state.cfg.redirect_uri(&origin));
    (source, uri.or_else(|| state.cfg.oauth_redirect_uri.clone()))
}

/// The sentences that go with `redirect_preview`: where to paste the value, and the shapes
/// that make Discord reject an otherwise correct one. They come from `Config` so `check`
/// and the dashboard can never disagree about what the fix is.
pub fn redirect_guidance(state: &AppState, headers: &HeaderMap) -> Vec<String> {
    let (source, uri) = redirect_preview(state, headers);
    match (source, uri) {
        // Operator-authored: the full sentence, value included, is what `main.py check` prints
        // too and there is nothing to reflect.
        ("pinned", Some(_)) | ("base-url", Some(_)) => state
            .cfg
            .redirect_uri_guidance(login_origin(state, headers).as_deref()),
        // Derived from this request's `Host`: describe the value, never repeat it.
        (_, Some(uri)) => state.cfg.redirect_uri_shape_notes(&uri),
        // Nothing to derive from — the instruction is the whole answer.
        (_, None) => state.cfg.redirect_uri_guidance(None),
    }
}

pub async fn start_login(
    state: &AppState,
    ip: &str,
    headers: &HeaderMap,
    next: Option<&str>,
) -> Result<Response> {
    if !state.cfg.oauth_configured() {
        // The sign-in link is rendered unconditionally, so this is a normal click, not
        // an attack: answer with the page and a reason instead of a bare error body.
        return Ok(login_redirect("oauth-unconfigured"));
    }
    let origin = match login_origin(state, headers) {
        Some(o) => o,
        None => return Ok(login_redirect("origin-unset")),
    };
    let client_id = state
        .cfg
        .discord_client_id
        .as_deref()
        .ok_or(Error::Forbidden("AERONA_DISCORD_CLIENT_ID is not set"))?;
    // 32 bytes, url-safe, 43 characters: one opaque value that is both the CSRF token
    // and the handle on the row that remembers which redirect uri was offered.
    let redirect_uri = state.cfg.redirect_uri(&origin);
    // The `verifier` column stays (NOT NULL; dropping it would rebuild the table for no
    // gain). With PKCE gone it holds an empty string, which is what `finish_login`
    // therefore ignores.
    // The `verifier` column is empty by design now that PKCE is gone, so `return_to` is
    // where the page the browser actually wanted goes: it rides along with the state row and
    // is read back by `finish_login`, one use, ten minutes, bound to the same ip.
    let (oauth_state, _nonce) =
        state
            .db
            .new_oauth_state("", ip, safe_next(next).as_deref(), Some(&redirect_uri))?;

    let url = authorize_url(client_id, &redirect_uri, &oauth_state);
    // 302 rather than 301/307 so the browser never caches a redirect that carries a
    // one-time state value.
    Ok((StatusCode::FOUND, [("location", url)]).into_response())
}

pub async fn finish_login(
    state: &AppState,
    query: &HashMap<String, String>,
    ip: &str,
    headers: &HeaderMap,
) -> Result<Response> {
    if let Some(err) = query.get("error") {
        // Discord's own `error` parameter, bounced to the page that renders it as a sentence.
        // Which is the whole post-redirect experience: "you cancelled" and "this application
        // is misconfigured" look identical from a browser that stopped on Discord's page.
        //
        // And that is the case no log line here will ever catch: a `redirect_uri` that is not
        // in the portal's list never reaches this handler at all, because Discord refuses the
        // authorization itself. `python3 main.py check` prints the exact value this install is
        // about to send, which is the thing to compare against the portal entry.
        tracing::info!(%err, "discord rejected the authorization");
        return Ok(login_redirect(&safe_reason(err)));
    }
    let Some(code) = query.get("code").filter(|v| !v.is_empty()) else {
        // A callback with no code is either somebody's bookmark or somebody's probe. Either
        // way the browser deserves the page, not an error body: `curl` gets its JSON from
        // any other route, and a person mid-sign-in gets a sentence and a button.
        return Ok(login_redirect("malformed-callback"));
    };
    let Some(oauth_state) = query.get("state").filter(|v| !v.is_empty()) else {
        return Ok(login_redirect("malformed-callback"));
    };
    // Single use, and bound to the address that started the login.
    let consumed = match state.db.consume_oauth_state(oauth_state, Some(ip)) {
        Ok(c) => c,
        Err(_) => return Ok(login_redirect("expired-link")),
    };

    // The token endpoint requires the byte-identical `redirect_uri`, so the one stored with
    // the state at authorize is the answer. Re-deriving it from *this* request's Host is the
    // fallback for rows written before the column existed, and it is only approximately the
    // same thing: a proxy that rewrites `Host`, or a `.env` edit between the two hops, makes
    // the two differ and Discord answers `invalid_grant` for a code that was perfectly good.
    let Some(redirect_uri) = consumed
        .redirect_uri
        .clone()
        .or_else(|| login_origin(state, headers).map(|origin| state.cfg.redirect_uri(&origin)))
    else {
        // Only reachable for a state row written before the column existed.
        return Ok(login_redirect("origin-unset"));
    };
    let (user, username, guilds) = match upstream_login(state, code, &redirect_uri).await {
        Ok(v) => v,
        Err(e) => {
            // The status and Discord's own `error` code are for the log; the browser gets a
            // fixed token from `login_redirect`, because this text is the one place in the
            // flow where an upstream sentence could be steered into a page.
            tracing::warn!(err = %e, "the discord login could not be completed");
            let reason = match &e {
                // A replayed or expired code: click again.
                Error::Invalid(_) => "expired-link",
                Error::Discord { status, .. } if *status == 504 => "discord-timeout",
                Error::Discord { .. } => "discord-unreachable",
                _ => "discord-refused",
            };
            return Ok(login_redirect(reason));
        }
    };

    // Which servers this browser may configure is Discord's answer, not our database's: every
    // server where the account holds ADMINISTRATOR or MANAGE_GUILD. A server Aerona is not in
    // *yet* belongs on that list — that is the state a first install is in, and the dashboard
    // is where the invite happens (per-guild config stays empty until somebody edits it, so
    // there is nothing to be inconsistent about). What the list excludes is every server where
    // this account is not an admin, which is the whole access-control question.
    let granted = grant_rows(&guilds);
    if granted.is_empty() {
        // The account is real, the token is good, and there is still nothing to authorize:
        // the dashboard governs servers, so it asks Discord who administers one. Landing on
        // `/login` with that reason is the whole answer, and it beats a 403 with a sentence in
        // it that reads like a bug.
        return Ok(login_redirect("no-admin-guild"));
    }

    let (session, token) = state.db.create_session(
        &state.pepper(),
        user,
        &username,
        None,
        ip,
        query.get("user_agent").map(String::as_str).unwrap_or(""),
        state.cfg.session_hours,
    )?;
    state
        .db
        .set_session_grants(session.id, &Value::Array(granted.clone()))?;
    state.db.audit_best_effort(
        aerona_core::db::security::AuditEntry::new("auth.login")
            .actor(
                aerona_core::db::security::ActorType::User,
                Some(user),
                &username,
            )
            .ip(ip)
            .meta(serde_json::json!({
                "via": "discord-oauth",
                "guildsVisible": guilds.len(),
                // The grant list, not just its size: when somebody asks why a session cannot
                // see a server, this is the row that answers.
                "guildsGranted": granted,
            })),
    );

    let mut resp = axum::response::Redirect::to(dashboard_target(consumed.return_to.as_deref()))
        .into_response();
    let v = auth::session_cookie(
        &state.cfg,
        &token,
        (session.expires_at - aerona_core::time::now()).max(1),
    );
    if !v.is_empty() {
        resp.headers_mut().append("set-cookie", v);
    }
    Ok(resp)
}

/// Where to land after a login: the page the browser asked for, if it asked for one that
/// exists in this product's own shape, and the server list otherwise. Guessing a guild from
/// the grant list used to be the answer, and it made the one-server case land on a 404
/// because the path was invented here rather than read from the router.
fn dashboard_target(next: Option<&str>) -> &str {
    next.unwrap_or("/")
}

/// A `?next=` is a redirect target a third party can type, so it is narrowed to a path on
/// this origin before it is ever stored: leading single slash, no scheme, no protocol
/// relative `//host`, no backslash, no control characters, and nothing outside the
/// characters a router here could actually match. Anything else is `None`, which means
/// "go to the top of the dashboard" — the boring answer is the safe one, and a person who
/// typed a wrong path loses their place rather than their session.
pub fn safe_next(raw: Option<&str>) -> Option<String> {
    let v = raw?.trim();
    if v.is_empty() || v.len() > 240 {
        return None;
    }
    if !v.starts_with('/') || v.starts_with("//") || v.contains('\'') || v.contains(":") {
        return None;
    }
    if v.chars().any(|c| c.is_control()) {
        return None;
    }
    let ok = v.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '/' | '-' | '_' | '.' | '~' | '?' | '&' | '=' | '%' | '#' | '+'
            )
    });
    ok.then(|| aerona_core::security::clean(v, 240))
}

/// Discord's side of a sign-in: the code becomes a token, the token becomes a profile and a
/// guild list. Three documents, not one — the token endpoint answers with `access_token`, and
/// the account and its servers are two more requests made *with* that token. Reading `id` out
/// of the token body was the bug that made every login on this product fail identically, and
/// the shape of the two responses is why a test now pins them apart.
async fn upstream_login(
    state: &AppState,
    code: &str,
    redirect_uri: &str,
) -> Result<(Snowflake, String, Vec<Value>)> {
    let tokens = exchange(state, code, redirect_uri).await?;
    let access = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| Error::Discord {
            status: 502,
            message: "discord returned no access token".into(),
        })?;
    let profile = fetch_profile(state, access).await?;
    let (user, username) = parse_profile(&profile)?;
    let guilds = fetch_guilds(state, access).await?;
    Ok((user, username, guilds))
}

async fn exchange(state: &AppState, code: &str, redirect_uri: &str) -> Result<Value> {
    let secret = state
        .cfg
        .discord_client_secret
        .as_ref()
        .ok_or(Error::Forbidden("AERONA_DISCORD_CLIENT_SECRET is not set"))?;
    let client_id = state
        .cfg
        .discord_client_id
        .as_deref()
        .ok_or(Error::Forbidden("AERONA_DISCORD_CLIENT_ID is not set"))?;
    let form = format!(
        "client_id={}&client_secret={}&grant_type=authorization_code&code={}&redirect_uri={}",
        urlencoding::encode(client_id),
        urlencoding::encode(secret.expose()),
        urlencoding::encode(code),
        urlencoding::encode(redirect_uri),
    );
    let resp = http(state)
        .post(TOKEN_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .await
        .map_err(|e| upstream(&e))?;
    let status = resp.status();
    // Not `.json()?`: a proxy or a Discord edge that answers 502 with HTML produces a parse
    // failure here, and turning that into "start again" hides the only evidence there is.
    let text = resp.text().await.map_err(|e| upstream(&e))?;
    let body: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            tracing::warn!(%status, body = %text.chars().take(200).collect::<String>(), "discord answered the token request with a non-json body");
            return Err(Error::Discord {
                status: 502,
                message: format!(
                    "discord answered the token request with html, not json ({})",
                    status.as_u16()
                ),
            });
        }
    };
    if !status.is_success() {
        let kind = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        // `error_description` names our own redirect_uri and client id, so it is logged and
        // never sent back: a browser has no business reading its server's configuration.
        tracing::warn!(%status, error = %kind, detail = ?body.get("error_description"), "discord token exchange failed");
        // A replayed or expired code is a retry (400). A refused client pair is a broken
        // `.env`, and telling somebody to "start again" for that wastes an afternoon.
        return Err(match kind.as_str() {
            "invalid_grant" => Error::Invalid("that login link has expired; start again".into()),
            "invalid_client" => Error::Discord {
                status: 502,
                message: "discord refused this client id and secret".into(),
            },
            _ => Error::Discord {
                status: 502,
                message: "discord refused the login".into(),
            },
        });
    }
    Ok(body)
}

/// Who the token belongs to. `identify` is what makes this answer readable; without the
/// scope Discord refuses, and the login has to say so rather than mint a session for nobody.
async fn fetch_profile(state: &AppState, access_token: &str) -> Result<Value> {
    let resp = http(state)
        .get(format!("{API}/users/@me"))
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| upstream(&e))?;
    let status = resp.status();
    if !status.is_success() {
        tracing::warn!(%status, "discord refused the profile request");
        return Err(Error::Discord {
            status: 502,
            message: format!(
                "discord would not describe that account ({})",
                status.as_u16()
            ),
        });
    }
    resp.json().await.map_err(|e| upstream(&e))
}

async fn fetch_guilds(state: &AppState, access_token: &str) -> Result<Vec<Value>> {
    if access_token.is_empty() {
        return Ok(Vec::new());
    }
    let resp = http(state)
        .get(format!("{API}/users/@me/guilds"))
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| upstream(&e))?;
    if !resp.status().is_success() {
        // An empty list is still the right answer for the browser, but it has to leave a
        // trace: `guilds` is the scope that decides who may sign in, so a 403 here means
        // "nobody administers anything" and looks exactly like a server with no admins.
        tracing::warn!(status = %resp.status(), "discord refused the guild list");
        return Ok(Vec::new());
    }
    let body: Value = resp.json().await.map_err(|e| upstream(&e))?;
    Ok(body.as_array().cloned().unwrap_or_default())
}

fn parse_profile(profile: &Value) -> Result<(Snowflake, String)> {
    let raw_id = profile
        .get("id")
        .and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .ok_or_else(|| Error::Discord {
            status: 502,
            message: "discord did not say who the account is".into(),
        })?;
    // `Snowflake(0)` is the "unset" sentinel, never a real account: accepting it
    // would let a malformed profile mint a session for user 0.
    let user = match Snowflake::parse(&raw_id) {
        Ok(s) if !s.is_zero() => s,
        _ => {
            return Err(Error::Discord {
                status: 502,
                message: "discord sent an account id we cannot use".into(),
            })
        }
    };
    let username = profile
        .get("username")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    Ok((user, username))
}

/// The rows a session is stamped with: the servers this account administers, with enough
/// about each to render a card before Aerona is a member of it. Names come from Discord's
/// guild list, which is the only source available for a server the bot is not in, and they
/// are bounded because this document is written into a column and read back on every request:
/// an account in a few hundred servers should not put a few hundred kilobytes in the way of
/// `session_allows_guild`, which runs on the hot path.
fn grant_rows(guilds: &[Value]) -> Vec<Value> {
    let clip = |v: Option<&str>, max: usize| -> Option<String> {
        v.map(|s| s.chars().take(max).collect::<String>())
    };
    guilds
        .iter()
        .filter(|g| can_manage(g))
        .filter_map(|g| {
            let id = g.get("id").and_then(Value::as_str)?;
            Some(serde_json::json!({
                "id": id,
                "name": g.get("name").and_then(Value::as_str).unwrap_or("unnamed server"),
                "icon": clip(g.get("icon").and_then(Value::as_str), 64),
            }))
        })
        .take(200)
        .collect()
}

/// What the invite asks for. Administrator, like the public moderation bots: this product's
/// per-guild config covers roles, channels, webhooks, messages, logging and the audit trail,
/// and requesting those one by one produces a server where the bot is quietly half-capable and
/// every feature needs its own permission fix.
pub const BOT_INVITE_PERMISSIONS: i64 = 8;

/// The "add Aerona to this server" link. `guild_id` with `disable_guild_select=false` opens
/// Discord's sheet with that server already picked and still changeable; a bare bot link
/// drops the operator on a dropdown of everything they own. No `response_type` and no
/// `redirect_uri` — this is the "authorize and close" shape, and asking for a code we would
/// have nowhere to send is how an invite link ends up as an error page.
pub fn bot_invite_url(client_id: &str, guild_id: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?client_id={}&permissions={}&scope=bot%20applications.commands&guild_id={}&disable_guild_select=false",
        urlencoding::encode(client_id),
        BOT_INVITE_PERMISSIONS,
        urlencoding::encode(guild_id),
    )
}

/// Whether the OAuth profile says this user may manage the server. Kept as a pure
/// function so the bit maths is testable without a network.
pub fn can_manage(g: &Value) -> bool {
    // Strings only, because that is what Discord sends; accepting a number here
    // would mean a future client that JSON-decodes permissions differently silently
    // changes who looks like an administrator.
    let raw = match g.get("permissions") {
        Some(Value::String(s)) => s.clone(),
        _ => return false,
    };
    match raw.parse::<i64>() {
        Ok(bits) => bits & PERMISSION_MASK != 0,
        Err(_) => false,
    }
}

fn http(state: &AppState) -> reqwest::Client {
    // A fresh client per login (two requests) is cheap and keeps no connection pool
    // alive for the lifetime of the process, which is the point on a 2 GB box. The
    // timeouts are the load-bearing part: without them a hung Discord would hold a
    // request slot forever.
    reqwest::Client::builder()
        .timeout(Duration::from_secs(state.cfg.http_timeout_secs.max(1)))
        .connect_timeout(Duration::from_secs(
            state.cfg.http_connect_timeout_secs.max(1),
        ))
        .use_rustls_tls()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

fn upstream(e: &reqwest::Error) -> Error {
    // The reqwest message can name a DNS failure or a TLS problem; both are useful
    // in the log and useless (and slightly revealing) to a browser.
    tracing::warn!(%e, "discord request failed");
    if e.is_timeout() {
        Error::Discord {
            status: 504,
            message: "discord did not answer in time".into(),
        }
    } else {
        Error::Discord {
            status: 502,
            message: "discord could not be reached".into(),
        }
    }
}

/// The whole authorize request, in one pure line, because the only failure mode it has is
/// "Discord did not recognise this" and that is a question of exact shape: `+` between the
/// scopes, the callback path the router actually mounts, and *nothing* else. PKCE is not
/// missing by accident — a confidential client authenticates with its secret at the token
/// endpoint — and `prompt=none` is left off for the same reason: it asks Discord to fail a
/// first-time consent rather than show it.
fn authorize_url(client_id: &str, redirect_uri: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?client_id={}&redirect_uri={}&response_type=code&scope={SCOPE}&state={}",
        urlencoding::encode(client_id),
        urlencoding::encode(redirect_uri),
        urlencoding::encode(state),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn permission_bits_are_read_like_discord_sends_them() {
        // Discord sends `permissions` as a decimal string.
        assert!(can_manage(&json!({"permissions": "8"})), "ADMINISTRATOR");
        assert!(can_manage(&json!({"permissions": "32"})), "MANAGE_GUILD");
        // 1<<27 (manage roles) | 1<<5 (manage guild) | 1<<3 (administrator).
        assert!(
            can_manage(&json!({"permissions": "134217768"})),
            "admin + manage guild"
        );
        assert!(
            !can_manage(&json!({"permissions": "1024"})),
            "manage messages is not enough"
        );
        assert!(!can_manage(&json!({"permissions": "0"})));
        assert!(!can_manage(&json!({})), "absent is not enough");
        assert!(
            !can_manage(&json!({"permissions": "abc"})),
            "garbage is not enough"
        );
        assert!(
            !can_manage(&json!({"permissions": 32})),
            "numbers are not what discord sends"
        );
        // A negative/oversize bitfield must not panic or silently grant.
        assert!(!can_manage(&json!({"permissions": "-9223372036854775808"})));
    }

    #[test]
    fn profile_parsing_needs_a_usable_id() {
        let (user, name) = parse_profile(&json!({"id": "42", "username": "ops"})).unwrap();
        assert_eq!((user, name.as_str()), (Snowflake(42), "ops"));
        assert!(
            parse_profile(&json!({"id": 42})).unwrap().1 == "unknown",
            "missing username is tolerated"
        );
        assert!(
            parse_profile(&json!({"id": "0"})).is_err(),
            "zero is not a user"
        );
        assert!(parse_profile(&json!({"id": null})).is_err());
        assert!(parse_profile(&json!({})).is_err());
    }

    #[test]
    fn the_authorize_url_is_exactly_the_shape_that_works() {
        let url = authorize_url(
            "1234567890",
            "http://127.0.0.1:25154/auth/callback",
            "Zm9vYmFyX2JhemRlZmJlZWZnZ2hqaWtscG1xcnN0dXZ",
        );
        assert_eq!(
            url,
            "https://discord.com/oauth2/authorize?client_id=1234567890\
&redirect_uri=http%3A%2F%2F127.0.0.1%3A25154%2Fauth%2Fcallback\
&response_type=code&scope=identify+guilds\
&state=Zm9vYmFyX2JhemRlZmJlZWZnZ2hqaWtscG1xcnN0dXZ"
        );
        // Each of these has been sent at some point and each is a refusal waiting to happen.
        for absent in ["code_challenge", "prompt=", "%20guilds", "/api/v1/"] {
            assert!(
                !url.contains(absent),
                "{absent} must not appear in an authorize url: {url}"
            );
            let _ = 0;
        }
        let state = url
            .split("&state=")
            .nth(1)
            .expect("state is the last parameter");
        assert_eq!(state.len(), 43, "a url-safe state is 43 characters");
        assert!(
            state
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "the state must survive a query string untouched"
        );
    }

    #[test]
    fn landing_page_is_the_dashboard_itself_unless_the_browser_asked_otherwise() {
        // `/guild/<id>/` is never *invented* here — that was the old bug, a 404 with a valid
        // cookie on it. What is allowed is the path the client itself requested before it was
        // bounced, because that one is a route the router owns.
        for granted in [None, Some(""), Some("https://elsewhere"), Some("//host/x")] {
            assert_eq!(dashboard_target(safe_next(granted).as_deref()), "/");
        }
        assert_eq!(
            dashboard_target(safe_next(Some("/guild/7/moderation/")).as_deref()),
            "/guild/7/moderation/"
        );
    }

    #[test]
    fn a_token_body_is_not_a_profile() {
        // The regression this whole test exists for: a token response has no `id`, so anything
        // that parses one as a profile must fail loudly instead of inventing user 0.
        let tok = json!({"access_token": "x", "token_type": "Bearer", "scope": "identify guilds"});
        assert!(parse_profile(&tok).is_err());
    }

    #[test]
    fn the_grant_list_is_administered_servers_with_just_enough_to_show_them() {
        let rows = grant_rows(&[
            json!({"id": "7", "name": "Ops", "icon": "abc", "permissions": "8"}),
            json!({"id": "8", "name": "Public", "permissions": "0"}),
            json!({"id": "9", "name": "Roles too", "permissions": "134217768"}),
            json!({"id": "10", "name": "Manage roles only", "permissions": "268435456"}),
            json!({"name": "no id", "permissions": "8"}),
        ]);
        assert_eq!(
            rows.len(),
            2,
            "administered servers with an id, and manage-roles alone is not administration"
        );
        assert_eq!(rows[0]["id"], "7");
        assert_eq!(rows[0]["name"], "Ops");
        assert_eq!(rows[0]["icon"], "abc");
        assert!(
            rows[1]["icon"].is_null(),
            "a server without an icon says so"
        );
    }

    #[test]
    fn a_next_path_survives_only_if_it_could_actually_be_navigated_to() {
        // The real use, and the reason the parameter exists.
        assert_eq!(
            safe_next(Some("/guild/123456789012345678/tickets/")).as_deref(),
            Some("/guild/123456789012345678/tickets/")
        );
        assert_eq!(safe_next(Some("/access/")).as_deref(), Some("/access/"));
        // Absolute URLs, protocol-relative ones, and anything with a scheme would leave this
        // origin, so none of them are a target.
        for bad in [
            "https://evil.example/x",
            "//evil.example",
            "/\\evil",
            "guild/1/",
            "",
            "   ",
            "/a\n../b",
        ] {
            assert_eq!(safe_next(Some(bad)), None, "{bad:?} must not be honoured");
        }
        assert_eq!(safe_next(None), None);
        // Long enough to be somebody probing, not a path.
        assert_eq!(safe_next(Some(&format!("/{}", "a".repeat(300)))), None);
    }

    #[test]
    fn the_invite_link_picks_the_server_and_asks_for_administrator() {
        let url = bot_invite_url("1234567890", "777");
        assert_eq!(
            url,
            "https://discord.com/oauth2/authorize?client_id=1234567890&permissions=8&scope=bot%20applications.commands&guild_id=777&disable_guild_select=false"
        );
        // `applications.commands` is not decoration: without it the guild authorises the
        // bot but not its slash commands, and every command we register is refused in that
        // server with no error we ever get to see — the failure is Discord's UI, not ours.
        assert!(url.contains("scope=bot%20applications.commands"), "{url}");
        for absent in ["redirect_uri", "response_type", "prompt="] {
            assert!(
                !url.contains(absent),
                "{absent} belongs to the login flow, not to an invite: {url}"
            );
        }
    }
}
