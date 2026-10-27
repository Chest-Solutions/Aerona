//! End-to-end HTTP tests: a real server on a real socket, a real SQLite file, and
//! `reqwest` as the client. These are the tests that prove the *wiring* - auth,
//! scopes, CSRF, rate limits, the error envelope, the outbox contract - which unit
//! tests on the individual helpers cannot.
//!
//! Each test gets its own install (temp dir + database + server), so rate-limit
//! buckets and lockouts never leak between cases.

use serde_json::{json, Value};

use aerona_core::db::Db;
use aerona_core::identity::Snowflake;

use crate::testutil::{Site, TestEnv};

const GUILD: u64 = 123456789012345678;
const GUILD_S: &str = "123456789012345678";

/// A guild the dashboard may serve: `require_guild` needs both the row and
/// `bot_present`, which is what `sync_guild` sets.
fn ready(db: &Db) {
    db.sync_guild(Snowflake(GUILD), "Test Guild", None, 25)
        .unwrap();
}

async fn ready_site(tag: &str) -> Site {
    let env = TestEnv::new(tag);
    ready(&env.state.db);
    Site::with_env(tag.to_string(), env).await
}

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn public_endpoints_answer_and_nothing_leaks() {
    let site = Site::start("public").await;
    // The API never lets a shared cache hold a credentialed or mutable response…

    let (status, body) = site.get("/api/v1/healthz").await;
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true);

    let (status, body, headers) = site.exchange("GET", "/api/v1/status", None, &[]).await;
    assert_eq!(
        status, 200,
        "/status is how the dashboard decides it can talk at all"
    );
    let rid = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        !rid.is_empty(),
        "every response carries an id the operator can quote"
    );
    assert!(
        headers
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .starts_with("no-store"),
        "status data must never be cached (this is the header that proves the api's own\n         decorate() runs: the nest prefix is stripped before the layer, so a path test\n         here would silently be false)"
    );
    let data = &body["data"];
    assert!(data["version"].is_string());
    assert!(data["uptimeSecs"].is_number());
    // The whole point of the unauthenticated route: nothing sensitive on it.

    for forbidden in ["token", "masterKey", "secret", "dbPath", "ip"] {
        assert!(data.get(forbidden).is_none(), "/status exposed {forbidden}");
    }
    assert!(
        body["meta"]["requestId"].is_null(),
        "the id is a header, not part of the json shape"
    );

    let (status, manifest) = site.get("/api/v1/manifest").await;
    assert_eq!(status, 200);
    assert!(
        manifest["data"]["routes"].as_array().unwrap().len() > 40,
        "the manifest lists the surface"
    );

    // An unknown API path is a JSON error, never the SPA's index.html - otherwise a
    // typo in the dashboard looks like a successful page load.
    let (status, body) = site.get("/api/v1/guilds/nope/nothing").await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found");
    assert!(body["data"].is_null());

    // And an unknown non-API path with no bundle built says how to build it.
    let (status, body) = site.get("/anywhere").await;
    assert_eq!(status, 404);
    assert!(
        body["raw"]
            .as_str()
            .unwrap_or_default()
            .contains("dashboard bundle"),
        "expected the build hint, got {body}"
    );
}

#[tokio::test]
async fn every_private_route_refuses_anonymity() {
    let site = ready_site("anon").await;
    for (method, path) in [
        ("GET", "/api/v1/guilds"),
        ("GET", &format!("/api/v1/guilds/{GUILD_S}")),
        ("GET", "/api/v1/audit"),
        ("GET", "/api/v1/sessions"),
        ("GET", "/api/v1/me"),
        ("POST", "/api/v1/logout"),
        ("POST", &format!("/api/v1/guilds/{GUILD_S}/warns")),
    ] {
        let (status, body) = site.raw(method, path, Some(json!({})), &[]).await;
        assert_eq!(
            status, 401,
            "{method} {path} must not answer anonymously: {body}"
        );
        assert_eq!(body["error"]["code"], "unauthorized");
    }
    // A garbage cookie is the same answer as no cookie: no hint that the format was
    // close.
    let (status, _) = site
        .raw(
            "GET",
            "/api/v1/guilds",
            None,
            &[("cookie", "aerona_session=nonsense".to_string())],
        )
        .await;
    assert_eq!(status, 401);
    // A bearer-style header the API does not speak is refused, not ignored.
    let (status, _) = site
        .raw(
            "GET",
            "/api/v1/guilds",
            None,
            &[("authorization", "Bearer abc".to_string())],
        )
        .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn a_cookie_that_authenticates_nothing_is_cleared_by_the_refusal() {
    let tag = "stale-cookie";
    let env = TestEnv::new(tag);
    ready(&env.state.db);
    // A session that *does* authenticate, for the last case below: the point is that only a 401
    // ends a cookie, so the test needs one that a request can present successfully.
    let (cookie, _csrf) = env.session_cookie(1041, "ops", None);
    let site = Site::with_env(tag.into(), env).await;
    // The browser cannot drop an `HttpOnly` cookie, and every request will keep carrying it, so
    // a refusal that arrived *with* one has to answer with its deletion. Without this an install
    // that lost its key or revoked a session 401s forever, and the only fix is for somebody to
    // find "clear site data" in a browser menu - which nobody suspects.
    let (status, _, headers) = site
        .exchange(
            "GET",
            "/api/v1/guilds",
            None,
            &[("cookie", "aerona_session=stale-session-token".to_string())],
        )
        .await;
    assert_eq!(status, 401);
    let cleared = headers
        .get("set-cookie")
        .expect("a refused cookie has to be cleared")
        .to_str()
        .unwrap()
        .to_string();
    for want in [
        "aerona_session=;",
        "Max-Age=0",
        "Path=/",
        "HttpOnly",
        "SameSite=Lax",
    ] {
        assert!(cleared.contains(want), "{want} missing from {cleared}");
    }

    // A 401 with no cookie to blame sends no `Set-Cookie`: an unauthenticated probe is not a
    // session to end, and a deletion header on every anonymous request hands any caching proxy
    // in front of the API a reason to stop caching.
    let (status, _, headers) = site.exchange("GET", "/api/v1/guilds", None, &[]).await;
    assert_eq!(status, 401);
    assert!(
        headers.get("set-cookie").is_none(),
        "nothing to clear, nothing sent"
    );

    // An empty value was never a session either, so it is not worth clearing.
    let (status, _, headers) = site
        .exchange(
            "GET",
            "/api/v1/guilds",
            None,
            &[("cookie", "aerona_session=".to_string())],
        )
        .await;
    assert_eq!(status, 401);
    assert!(
        headers.get("set-cookie").is_none(),
        "an empty cookie is not a session to end"
    );

    // And only a 401 does this. A refusal about *permission* or existence keeps the cookie:
    // clearing on a 403 or a 404 would sign an operator out of the whole panel for one server
    // they may not read, which is a worse bug than the one being fixed.
    let (status, _, headers) = site
        .exchange(
            "GET",
            "/api/v1/guilds/999999999999999999/audit",
            None,
            &[("cookie", cookie.clone())],
        )
        .await;
    assert_ne!(
        status, 401,
        "a valid session must not read as signed out: {status}"
    );
    let set = headers
        .get("set-cookie")
        .map(|v| v.to_str().unwrap().to_string());
    assert!(
        !set.as_deref().unwrap_or_default().contains("Max-Age=0"),
        "a non-401 answer must not end the session it just accepted: {set:?}"
    );
}

// ---------------------------------------------------------------------------
// CSRF, origin, sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_mutating_browser_request_needs_both_origin_and_csrf() {
    let env = TestEnv::new("csrf");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1001, "ops", None);
    let site = Site::with_env("csrf".into(), env).await;
    let path = format!("/api/v1/guilds/{GUILD_S}/settings/moderation");

    // Cookie, right origin, no CSRF header.
    let (status, body) = site
        .raw(
            "PATCH",
            &path,
            Some(json!({"strike_threshold": 5})),
            &[("cookie", cookie.clone()), ("origin", site.origin())],
        )
        .await;
    assert_eq!(
        status, 400,
        "missing csrf is a client bug, not a forbidden action: {body}"
    );
    assert_eq!(body["error"]["code"], "bad_request");

    // Cookie, CSRF, foreign origin.
    let (status, body) = site
        .raw(
            "PATCH",
            &path,
            Some(json!({"strike_threshold": 5})),
            &[
                ("cookie", cookie.clone()),
                ("origin", "https://evil.example".to_string()),
                ("x-aerona-csrf", csrf.clone()),
            ],
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "forbidden");
    // Reads from the same foreign origin are fine: only state changes are dangerous.
    let (status, _) = site
        .raw(
            "GET",
            &path,
            None,
            &[
                ("cookie", cookie.clone()),
                ("origin", "https://evil.example".to_string()),
            ],
        )
        .await;
    assert_eq!(status, 200, "a GET must not be blocked by origin");

    // Right origin, wrong token.
    let (status, _) = site
        .raw(
            "PATCH",
            &path,
            Some(json!({"strike_threshold": 5})),
            &[
                ("cookie", cookie.clone()),
                ("origin", site.origin()),
                ("x-aerona-csrf", "not-the-token".to_string()),
            ],
        )
        .await;
    assert_eq!(status, 400);

    // Both correct.
    let (status, body) = site
        .browser(
            "PATCH",
            &path,
            Some(json!({"strike_threshold": 5})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["changed"], true);
}

#[tokio::test]
async fn logout_revokes_the_session_everywhere_and_clears_the_cookie() {
    let env = TestEnv::new("logout");
    let (cookie, csrf) = env.session_cookie(1002, "ops", None);
    let (cookie2, _) = env.session_cookie(1002, "ops", None);
    assert_ne!(cookie, cookie2);
    let site = Site::with_env("logout".into(), env).await;

    let (status, body) = site.authed_get("/api/v1/sessions", &cookie).await;
    assert_eq!(status, 200);
    assert_eq!(
        body["data"]["sessions"].as_array().unwrap().len(),
        2,
        "two live sessions for one user"
    );
    assert_eq!(body["data"]["sessions"][0]["current"], true);

    let (status, _) = site
        .browser(
            "POST",
            "/api/v1/sessions/revoke-all",
            Some(json!({"keepCurrent": true})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200);
    let (_status, body) = site.authed_get("/api/v1/sessions", &cookie).await;
    assert_eq!(body["data"]["sessions"].as_array().unwrap().len(), 1);
    let (status, _) = site.authed_get("/api/v1/sessions", &cookie2).await;
    assert_eq!(status, 401, "the other one did not");

    // Revoking everything, including self: the response must clear the cookie.
    let (status, body, headers) = site
        .exchange(
            "POST",
            "/api/v1/sessions/revoke-all",
            Some(json!({"keepCurrent": false})),
            &[
                ("cookie", cookie.clone()),
                ("origin", site.origin()),
                ("x-aerona-csrf", csrf.clone()),
            ],
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["selfRevoked"], true);
    let cleared = headers
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        cleared.contains("aerona_session=;") && cleared.contains("Max-Age=0"),
        "{cleared}"
    );
    let (status, _) = site.authed_get("/api/v1/sessions", &cookie).await;
    assert_eq!(status, 401);

    // `/logout` on a session that no longer exists is a plain 401: the SPA treats
    // that as "already signed out" and clears its own state. Pretending to log out
    // would leave the cookie in place.
    let (status, _) = site
        .browser("POST", "/api/v1/logout", None, Some(&cookie), Some(&csrf))
        .await;
    assert_eq!(status, 401);
    // A key cannot log out: it has no cookie.
    let (status, body) = site
        .raw(
            "POST",
            "/api/v1/logout",
            None,
            &[("authorization", "Aerona aer_live_x".to_string())],
        )
        .await;
    assert_eq!(status, 401, "{body}");
}

// ---------------------------------------------------------------------------
// API keys
// ---------------------------------------------------------------------------

#[tokio::test]
async fn keys_are_minted_scoped_and_revoked() {
    let env = TestEnv::new("keys");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1003, "ops", None);
    let site = Site::with_env("keys".into(), env).await;

    // A read-only key.
    let (status, body, _) = site
        .exchange(
            "POST",
            "/api/v1/keys",
            Some(json!({"label": "ci read", "scopes": ["read"], "ttlDays": 30, "guild": GUILD_S})),
            &[
                ("cookie", cookie.clone()),
                ("origin", site.origin()),
                ("x-aerona-csrf", csrf.clone()),
            ],
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let key = body["data"]["key"].as_str().unwrap().to_string();
    assert!(key.starts_with("aer_live_"), "{key}");
    let hint = body["data"]["hint"].as_str().unwrap().to_string();
    assert!(
        key.starts_with(&hint) && hint.len() < key.len(),
        "the hint must identify the key without being it: {hint} / {key}"
    );
    assert!(body["data"]["warning"]
        .as_str()
        .unwrap()
        .contains("only time"));

    // The key reads what its guild allows…
    let (status, body) = site
        .key_call("GET", &format!("/api/v1/guilds/{GUILD_S}"), &key, None)
        .await;
    assert_eq!(status, 200, "{body}");
    // …but cannot write anything…
    let (status, body) = site
        .key_call(
            "PATCH",
            &format!("/api/v1/guilds/{GUILD_S}/settings/moderation"),
            &key,
            Some(json!({"strike_threshold": 9})),
        )
        .await;
    assert_eq!(status, 403, "a read key must not write: {body}");
    assert_eq!(body["error"]["code"], "forbidden");
    // …cannot leave its guild…
    let (status, _) = site
        .key_call("GET", "/api/v1/guilds/999/settings/moderation", &key, None)
        .await;
    assert_eq!(status, 403, "a guild-scoped key is fenced to its guild");
    // …and cannot see install-wide routes.
    let (status, _) = site.key_call("GET", "/api/v1/keys", &key, None).await;
    assert_eq!(status, 403, "keys are operator-only, let alone for a key");
    let (status, body) = site.key_call("GET", "/api/v1/stats", &key, None).await;
    assert_eq!(
        status, 403,
        "a guild-bound key never reaches install-wide routes: {body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("/guilds/{guild}"),
        "and it is told why: {body}"
    );

    // `selfcheck`/`/me` for a key says so.
    let (status, me) = site.key_call("GET", "/api/v1/me", &key, None).await;
    assert_eq!(status, 200);
    assert_eq!(me["data"]["kind"], "key");
    assert_eq!(me["data"]["label"], "ci read");
    assert_eq!(me["data"]["guild"], GUILD_S);

    // A config key can write, and the write is attributed to it in the audit log.
    let (status, body) = site
        .browser(
            "POST",
            "/api/v1/keys",
            Some(json!({"label": "ci config", "scopes": ["config", "read"]})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let config_key = body["data"]["key"].as_str().unwrap().to_string();
    // The create response carries the id precisely so a caller can revoke later
    // without a second round trip.
    let config_key_id = body["data"]["id"].as_i64().unwrap();
    let (status, body) = site
        .key_call(
            "PATCH",
            &format!("/api/v1/guilds/{GUILD_S}/settings/moderation"),
            &config_key,
            Some(json!({"strike_threshold": 9})),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["settings"]["strike_threshold"], 9);
    let (status, audit) = site.authed_get("/api/v1/audit", &cookie).await;
    assert_eq!(status, 200);
    let entry = audit["data"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["action"].as_str() == Some("settings.moderation"))
        .expect("the settings write is audited");
    assert_eq!(
        entry["ip"], "127.0.0.1",
        "the socket peer is recorded, not 0.0.0.0"
    );
    assert_eq!(
        entry["actorType"], "apikey",
        "the audit trail says which key did it"
    );
    assert!(entry["actorLabel"].as_str().unwrap().contains("ci config"));

    // Revoke by id, then the key stops working immediately.
    let (status, _) = site
        .browser(
            "DELETE",
            &format!("/api/v1/keys/{config_key_id}"),
            None,
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200);
    let (status, body) = site
        .key_call(
            "PATCH",
            &format!("/api/v1/guilds/{GUILD_S}/settings/moderation"),
            &config_key,
            Some(json!({"strike_threshold": 2})),
        )
        .await;
    assert_eq!(status, 401, "a revoked key is dead at once: {body}");
    // The read key is still alive (revocation is per row).
    let (status, _) = site
        .key_call("GET", &format!("/api/v1/guilds/{GUILD_S}"), &key, None)
        .await;
    assert_eq!(status, 200);
}

// ---------------------------------------------------------------------------
// Guild guard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_guild_is_only_as_readable_as_the_bots_presence() {
    let env = TestEnv::new("guard");
    // No CSRF value: this test only reads, which is exactly the case where the header
    // must not be required.
    let (cookie, _csrf) = env.session_cookie(1004, "ops", None);
    let site = Site::with_env("guard".into(), env).await;

    // Never seen by the bot at all: 404, and it does not say "not yours".
    let (status, body) = site
        .authed_get("/api/v1/guilds/555/settings/general", &cookie)
        .await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found");

    // Synced, then kicked: 409 with the re-invite message, because the dashboard has
    // to be able to tell the operator what to do.
    site.db()
        .sync_guild(Snowflake(GUILD), "Test Guild", None, 25)
        .unwrap();
    site.db().mark_guild_absent(Snowflake(GUILD)).unwrap();
    let (status, body) = site
        .authed_get(&format!("/api/v1/guilds/{GUILD_S}"), &cookie)
        .await;
    assert_eq!(status, 409, "{body}");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("invite"));

    // Present again: readable and writable.
    site.db()
        .sync_guild(Snowflake(GUILD), "Test Guild", None, 25)
        .unwrap();
    let (status, _) = site
        .authed_get(&format!("/api/v1/guilds/{GUILD_S}"), &cookie)
        .await;
    assert_eq!(status, 200);

    // A malformed id is refused before any query runs.
    for bad in ["0", "-1", "1e18", "abc", "12 34", &"9".repeat(21)] {
        let (status, _) = site
            .authed_get(&format!("/api/v1/guilds/{bad}/settings/general"), &cookie)
            .await;
        assert_eq!(status, 400, "{bad} must not reach the database");
    }

    // A session bound to one guild cannot browse the others.
    let (bound_cookie, _bound_csrf) = {
        let pepper = site.env.state.pepper();
        let (session, token) = site
            .env
            .state
            .db
            .create_session(
                &pepper,
                Snowflake(1005),
                "other",
                Some(Snowflake(GUILD)),
                "127.0.0.1",
                "test",
                12,
            )
            .unwrap();
        (format!("aerona_session={token}"), session.csrf_secret)
    };
    let (status, body) = site
        .authed_get("/api/v1/guilds/999/settings/general", &bound_cookie)
        .await;
    assert_eq!(status, 403, "a bound session is fenced: {body}");
    let (status, _) = site
        .authed_get("/api/v1/guilds/999/settings/general", &cookie)
        .await;
    assert_eq!(
        status, 404,
        "the unbound session gets the ordinary not-found"
    );
    let (status, body) = site.authed_get("/api/v1/guilds", &bound_cookie).await;
    assert_eq!(status, 200);
    assert_eq!(
        body["data"]["guilds"].as_array().unwrap().len(),
        1,
        "the list is filtered to the bound guild"
    );
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn settings_are_merged_validated_and_history_kept() {
    let env = TestEnv::new("settings");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1006, "ops", None);
    let site = Site::with_env("settings".into(), env).await;
    let mod_path = format!("/api/v1/guilds/{GUILD_S}/settings/moderation");

    // Defaults, in the shape the form needs.
    let (status, body) = site.authed_get(&mod_path, &cookie).await;
    assert_eq!(status, 200);
    assert_eq!(
        body["data"]["strike_threshold"], 0,
        "escalation is off until it is asked for"
    );
    assert_eq!(body["data"]["purge_max"], 200);

    // A patch touches one field and the response is the whole normalised document.
    let (status, body) = site
        .browser(
            "PATCH",
            &mod_path,
            Some(json!({"strike_threshold": 3})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["changed"], true);
    assert_eq!(body["data"]["settings"]["strike_threshold"], 3);
    assert_eq!(
        body["data"]["settings"]["purge_max"], 200,
        "an untouched field keeps its stored value"
    );

    // Patching to the same value is a no-op that still answers 200 - the dashboard
    // sends the whole form, so this must not look like a change (it would otherwise
    // grow the history table on every save).
    let (status, body) = site
        .browser(
            "PATCH",
            &mod_path,
            Some(json!({"strike_threshold": 3})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["data"]["changed"], false);

    // Out of range, wrong type, an explicit null and an unknown top-level key are
    // all 400s: a typo must never be silently ignored.
    for (label, patch) in [
        ("out of range for u8", json!({"strike_threshold": 9999})),
        ("wrong type", json!({"strike_threshold": "three"})),
        ("explicit null", json!({"strike_threshold": null})),
        ("typo in the key", json!({"strike_theshold": 4})),
        ("empty key", json!({"": 1})),
    ] {
        let (status, body) = site
            .browser("PATCH", &mod_path, Some(patch), Some(&cookie), Some(&csrf))
            .await;
        assert_eq!(status, 400, "{label} was accepted: {body}");
        assert_eq!(body["error"]["code"], "bad_request");
        // The client's own key is quoted back so the typo can be found; no *value* is
        // ever echoed, and the stored document is untouched either way.
        if label == "typo in the key" {
            assert!(
                body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("strike_theshold"),
                "name the offending key: {body}"
            );
        }
    }
    // The stored document is untouched by all of that.
    let (_status, body) = site.authed_get(&mod_path, &cookie).await;
    assert_eq!(
        body["data"]["strike_threshold"], 3,
        "a rejected patch must not half-apply"
    );

    // An empty body means "no changes", not "clear everything".
    let (status, body) = site
        .browser(
            "PATCH",
            &mod_path,
            Some(json!({})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["data"]["changed"], false);
    assert_eq!(body["data"]["settings"]["purge_max"], 200);

    // The history shows exactly the one real change, newest first, with the value.
    let (status, body) = site
        .authed_get(
            &format!("/api/v1/guilds/{GUILD_S}/settings/moderation/history"),
            &cookie,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let rows = body["data"]["history"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "only a real change is recorded: {rows:?}");
    assert_eq!(rows[0]["feature"], "moderation");
    assert_eq!(rows[0]["after"]["strike_threshold"], 3);
    assert_eq!(
        rows[0]["after"]["strike_threshold"], 3,
        "the row carries the value that was set"
    );
    assert!(
        rows[0]["before"].is_null(),
        "the first change has no previous document to roll back to"
    );
    assert_eq!(rows[0]["changedBy"], "1006", "the operator is named");

    // `keys` has no settings document, and an unknown feature is a 400.
    let (status, _) = site
        .browser(
            "PATCH",
            &format!("/api/v1/guilds/{GUILD_S}/settings/keys"),
            Some(json!({"a": 1})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 400);
    let (status, _) = site
        .authed_get(
            &format!("/api/v1/guilds/{GUILD_S}/settings/nonsense"),
            &cookie,
        )
        .await;
    assert_eq!(status, 400);

    // A second guild keeps its own copy: nothing is shared by accident.
    site.db()
        .sync_guild(Snowflake(900), "Other", None, 3)
        .unwrap();
    let (status, body) = site
        .authed_get("/api/v1/guilds/900/settings/moderation", &cookie)
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        body["data"]["strike_threshold"], 0,
        "the other guild was never touched"
    );
}

// ---------------------------------------------------------------------------
// Moderation + the outbox
// ---------------------------------------------------------------------------

#[tokio::test]
async fn warns_escalate_and_every_state_change_is_queued_and_audited() {
    let env = TestEnv::new("mod");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1007, "moderator", None);
    let site = Site::with_env("mod".into(), env).await;
    let target = "777000000000000001";

    // Threshold 3 so the third warn escalates.
    let (status, _) = site
        .browser(
            "PATCH",
            &format!("/api/v1/guilds/{GUILD_S}/settings/moderation"),
            Some(json!({"strike_threshold": 3})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200);

    for i in 1..=3 {
        let (status, body) = site
            .browser(
                "POST",
                &format!("/api/v1/guilds/{GUILD_S}/warns"),
                Some(json!({"user": target, "reason": format!("spam pass {i}")})),
                Some(&cookie),
                Some(&csrf),
            )
            .await;
        assert_eq!(status, 201, "warn {i}: {body}");
        assert_eq!(body["data"]["activeCount"], i);
        assert_eq!(
            body["data"]["escalated"],
            i == 3,
            "only the third warn escalates: {body}"
        );
    }

    let (status, body) = site
        .authed_get(
            &format!("/api/v1/guilds/{GUILD_S}/moderation/stats"),
            &cookie,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["data"]["stats"]["warns_active"], 3, "{body}");
    assert_eq!(body["data"]["stats"]["warns_total"], 3);
    assert_eq!(body["data"]["pendingUnbans"], 0);

    // A queued ban answers 202 with a poll URL, and the outbox row is what the bot
    // will pick up.
    let (status, body, _) = site
        .exchange(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/actions"),
            Some(json!({"action": "ban", "user": target, "reason": "raid participation"})),
            &[
                ("cookie", cookie.clone()),
                ("origin", site.origin()),
                ("x-aerona-csrf", csrf.clone()),
            ],
        )
        .await;
    assert_eq!(status, 202, "{body}");
    // The funnel answers with the list it queued (one bulk call can be many rows)
    // plus a poll URL for the first, which is what a UI needs to show progress.
    let outbox_id = body["data"]["queued"][0].as_i64().unwrap();
    assert_eq!(body["data"]["count"], 1);
    assert_eq!(body["data"]["poll"], format!("/api/v1/outbox/{outbox_id}"));

    let (status, body) = site
        .authed_get(&format!("/api/v1/outbox/{outbox_id}"), &cookie)
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["data"]["status"], "pending");
    assert_eq!(
        body["data"]["action"], "moderate.ban",
        "the poll names the queued work"
    );
    assert_eq!(body["data"]["args"]["user"], target);

    // Exactly the queued items are visible to the worker, and they leave the queue
    // when claimed.
    let claimed = site.db().claim_actions(10).unwrap();
    let names: Vec<&str> = claimed.iter().map(|c| c.action.as_str()).collect();
    assert!(names.contains(&"moderate.ban"), "{names:?}");
    assert!(
        names.contains(&"moderate.escalate"),
        "the escalation was queued too: {names:?}"
    );
    // `outbox_depth().0` counts `pending` *and* `claimed`: a claimed row is still
    // unfinished work, which is exactly what an operator needs to see.
    assert_eq!(
        site.db().outbox_depth().unwrap().0,
        2,
        "both rows are claimed but not finished"
    );
    for row in &claimed {
        site.db().finish_action(row.id, true, None).unwrap();
    }
    assert_eq!(
        site.db().outbox_depth().unwrap(),
        (0, 0),
        "the queue is empty again"
    );
    let (status, body) = site
        .authed_get(&format!("/api/v1/outbox/{outbox_id}"), &cookie)
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        body["data"]["status"], "done",
        "the poll endpoint reports completion"
    );

    // An unknown action is refused without touching the queue.
    let (status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/actions"),
            Some(json!({"action": "nuke", "user": target})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"]["message"].as_str().unwrap().contains("kick"));
    assert_eq!(site.db().outbox_depth().unwrap().0, 0);

    // A purge needs a channel and no user; a ban needs a user.
    let (status, _) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/actions"),
            Some(json!({"action": "ban"})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 400, "a ban with no target is refused");
    let (status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/actions"),
            Some(json!({"action": "purge", "channel": "555", "limit": 20})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 202, "{body}");

    // The audit trail covers all of it, with the moderator named.
    let (status, body) = site
        .raw(
            "GET",
            &format!("/api/v1/audit?guild={GUILD_S}"),
            None,
            &[("cookie", cookie.clone())],
        )
        .await;
    assert_eq!(status, 200, "an audit read needs no csrf header");
    let actions: Vec<&str> = body["data"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap_or_default())
        .collect();
    assert!(actions.iter().any(|a| a.contains("warn")), "{actions:?}");
    assert!(
        actions
            .iter()
            .any(|a| a.contains("queue") || a.contains("action")),
        "{actions:?}"
    );
    assert_eq!(body["data"]["entries"][0]["actorLabel"], "moderator");
}

#[tokio::test]
async fn a_warn_cannot_be_aimed_at_yourself_or_without_a_reason() {
    let env = TestEnv::new("warn-guard");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1008, "self", None);
    let site = Site::with_env("warn-guard".into(), env).await;
    // Session user 1008 is the actor, so warning 1008 is warning yourself.
    let (status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/warns"),
            Some(json!({"user": "1008", "reason": "testing"})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    let (status, _) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/warns"),
            Some(json!({"user": "1009"})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 400, "a reason is required");
    let (status, _) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/warns"),
            Some(json!({"user": "not-a-snowflake", "reason": "x"})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 400);
}

// ---------------------------------------------------------------------------
// Tickets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tickets_are_disabled_until_the_guild_turns_them_on() {
    let env = TestEnv::new("tickets");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1009, "ops", None);
    let site = Site::with_env("tickets".into(), env).await;
    let open = || json!({"subject": "billing question", "user": "1010"});

    let (status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/tickets"),
            Some(open()),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 403, "disabled tickets must not queue work: {body}");
    assert_eq!(body["error"]["code"], "forbidden");

    site.browser(
        "PATCH",
        &format!("/api/v1/guilds/{GUILD_S}/settings/tickets"),
        Some(json!({"enabled": true})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    let (status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/tickets"),
            Some(open()),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 202, "{body}");
    let id = body["data"]["id"].as_i64().unwrap();

    let (status, body) = site
        .authed_get(&format!("/api/v1/guilds/{GUILD_S}/tickets"), &cookie)
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        body["data"]["open"], 0,
        "the ticket exists once the bot has made the channel"
    );

    // The queued intent carries everything the worker needs.
    let row = site.db().outbox_status(id).unwrap();
    assert_eq!(row["action"], "ticket.create");
    assert_eq!(row["args"]["subject"], "billing question");

    // A ticket row created out-of-band (as the bot would) is then readable, and
    // another guild's ticket is a 404 rather than a 403.
    let ticket = site
        .db()
        .create_ticket(
            &aerona_core::db::tickets::NewTicket {
                guild_id: Snowflake(GUILD),
                channel_id: Snowflake(4242),
                user_channel: None,
                owner_id: Snowflake(1010),
                subject: "billing question".into(),
                panel_id: None,
            },
            "ticket-{n}",
            "1010",
        )
        .unwrap();
    let (status, body) = site
        .authed_get(
            &format!("/api/v1/guilds/{GUILD_S}/tickets/{}", ticket.id),
            &cookie,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["data"]["ticket"]["reference"].as_str().unwrap().len(),
        10,
        "AER-XXXXXX"
    );
    assert_eq!(
        body["data"]["ticket"]["subject"], "billing question",
        "the ticket rides under `ticket`"
    );
    assert!(
        body["data"]["transcript"].is_array(),
        "the transcript comes with it"
    );
    // Unknown guild: 404 from the guard. A *known* guild that does not own the ticket
    // is also 404 (`load_ticket`), never 403 - the existence of somebody else's
    // ticket id is not information the dashboard should leak.
    let (status, _) = site
        .authed_get(
            &format!("/api/v1/guilds/999/tickets/{}", ticket.id),
            &cookie,
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = site
        .authed_get(&format!("/api/v1/guilds/{GUILD_S}/tickets/999999"), &cookie)
        .await;
    assert_eq!(status, 404);

    // Claiming is idempotent-guarded, and a transcript renders as text or json.
    let (status, _body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/tickets/{}/claim", ticket.id),
            Some(json!({})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200);
    let (status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/tickets/{}/claim", ticket.id),
            Some(json!({})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 409, "a claimed ticket stays claimed: {body}");
    let (status, body) = site
        .authed_get(
            &format!("/api/v1/guilds/{GUILD_S}/tickets/{}/transcript", ticket.id),
            &cookie,
        )
        .await;
    assert_eq!(status, 200);
    assert!(
        body["raw"].as_str().unwrap().contains(&ticket.reference),
        "{body}"
    );
    let (status, body) = site
        .authed_get(
            &format!(
                "/api/v1/guilds/{GUILD_S}/tickets/{}/transcript?raw=1",
                ticket.id
            ),
            &cookie,
        )
        .await;
    assert_eq!(status, 200);
    assert!(body["data"]["messages"].is_array());
}

// ---------------------------------------------------------------------------
// AI filter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_filter_dry_run_is_local_only_and_rules_round_trip() {
    let env = TestEnv::new("ai");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1010, "ops", None);
    let site = Site::with_env("ai".into(), env).await;
    let rules = format!("/api/v1/guilds/{GUILD_S}/ai-filter/rules");

    // Nothing is configured, so nothing matches.
    let (status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/ai-filter/test"),
            Some(json!({"text": "you are a total idiot"})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["blocked"], false);
    assert_eq!(
        body["data"]["provider"]["called"], false,
        "the dry run must never spend a paid call"
    );
    assert_eq!(body["data"]["echo"], "you are a total idiot");

    // A literal rule that blocks.
    let (status, body) = site
        .browser("POST", &rules, Some(json!({"pattern": "idiot", "action": "delete_strike", "severity": "high", "strikes": 1, "description": "insult"})), Some(&cookie), Some(&csrf))
        .await;
    assert_eq!(status, 201, "{body}");
    let rule_id = body["data"]["id"].as_i64().unwrap();
    assert_eq!(body["data"]["rules"].as_array().unwrap().len(), 1);

    let (status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/ai-filter/test"),
            Some(json!({"text": "you are a total idiot"})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        body["data"]["blocked"], true,
        "the local rule must fire in the dry run"
    );
    assert_eq!(body["data"]["verdict"]["action"], "delete_strike");
    assert_eq!(body["data"]["rulesCompared"], 1);

    // A regex is only compiled when asked for: a literal containing `(` is legal.
    let (status, body) = site
        .browser(
            "POST",
            &rules,
            Some(json!({"pattern": "(unbalanced", "action": "warn"})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 201, "a literal is not a regex: {body}");
    let (status, body) = site
        .browser(
            "POST",
            &rules,
            Some(json!({"pattern": "(unbalanced", "isRegex": true, "action": "warn"})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 400, "an invalid regex is refused on write: {body}");

    // PATCH only flips `enabled`; anything else is refused so a partial edit cannot
    // quietly reset a rule.
    let (status, body) = site
        .browser(
            "PATCH",
            &format!("{rules}/{rule_id}"),
            Some(json!({"enabled": false})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = site
        .browser(
            "PATCH",
            &format!("{rules}/{rule_id}"),
            Some(json!({"pattern": "safe"})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(
        status, 400,
        "a pattern patch would need the whole rule: {body}"
    );
    let (_status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/ai-filter/test"),
            Some(json!({"text": "you are a total idiot"})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(
        body["data"]["blocked"], false,
        "a disabled rule does not fire"
    );
    // …but the simulator can show what would happen if it were on.
    let (status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/ai-filter/test"),
            Some(json!({"text": "you are a total idiot", "ignoreEnabled": true})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["blocked"], true);

    // Delete, then the id is gone.
    let (status, _) = site
        .raw(
            "DELETE",
            &format!("{rules}/{rule_id}"),
            None,
            &[
                ("cookie", cookie.clone()),
                ("origin", site.origin()),
                ("x-aerona-csrf", csrf.clone()),
            ],
        )
        .await;
    assert_eq!(status, 200);
    let (status, _) = site
        .raw(
            "DELETE",
            &format!("{rules}/{rule_id}"),
            None,
            &[
                ("cookie", cookie),
                ("origin", site.origin()),
                ("x-aerona-csrf", csrf),
            ],
        )
        .await;
    assert_eq!(status, 404);
}

// ---------------------------------------------------------------------------
// Audit integrity, stats, maintenance
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_audit_chain_detects_a_tampered_row() {
    let env = TestEnv::new("audit");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1011, "ops", None);
    let site = Site::with_env("audit".into(), env).await;
    site.browser(
        "PATCH",
        &format!("/api/v1/guilds/{GUILD_S}/settings/moderation"),
        Some(json!({"purge_max": 50})),
        Some(&cookie),
        Some(&csrf),
    )
    .await;

    let (status, body) = site
        .browser(
            "POST",
            "/api/v1/audit/verify",
            None,
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["intact"], true);
    assert!(body["data"]["verifiedRows"].as_i64().unwrap() >= 1);

    // Somebody with shell access deletes the incriminating row.
    site.db()
        .tx(|conn| {
            conn.execute(
                "UPDATE audit_log SET action = 'nothing.happened' WHERE 1=1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let (status, body) = site
        .browser(
            "POST",
            "/api/v1/audit/verify",
            None,
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(
        status, 500,
        "a broken chain must not read as a clean audit log: {body}"
    );
    assert_eq!(body["error"]["code"], "internal");
    // Listing still works (the rows are still there), but `?verify=1` now fails too.
    let (status, _body) = site.authed_get("/api/v1/audit?verify=1", &cookie).await;
    assert_eq!(status, 500);
    let (status, body) = site.authed_get("/api/v1/audit", &cookie).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["data"]["chain"],
        Value::Null,
        "no chain claim is made unless it was verified"
    );
    assert!(
        body["data"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "nothing.happened"),
        "the tampering is visible in the list too"
    );
}

#[tokio::test]
async fn maintenance_and_stats_are_session_only_but_polling_is_not() {
    let env = TestEnv::new("maint");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1012, "ops", None);
    let site = Site::with_env("maint".into(), env).await;

    // The poll endpoint is how a key learns whether its action ran.
    let (status, _) = site
        .browser("POST", &format!("/api/v1/guilds/{GUILD_S}/stickies"), Some(json!({"channel": "111", "message": "222", "content": "read the rules", "everyMessages": 25})), Some(&cookie), Some(&csrf))
        .await;
    assert_eq!(status, 201);
    let (status, body) = site.authed_get("/api/v1/stats", &cookie).await;
    assert_eq!(status, 200, "{body}");
    assert!(body["data"]["guilds"].as_i64().unwrap() >= 1);
    assert!(body["data"]["dbSizeKb"].as_i64().unwrap() > 0);
    let (status, body) = site
        .authed_get("/api/v1/stats/bot?points=10", &cookie)
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        body["data"]["series"].as_array().unwrap().len(),
        0,
        "the bot has not reported yet"
    );
    let (status, _) = site.authed_get("/api/v1/stats/nonsense", &cookie).await;
    assert_eq!(status, 400);

    let (status, body) = site
        .browser(
            "POST",
            "/api/v1/housekeeping",
            Some(json!({"retentionDays": 1})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body["data"]["removed"].is_object());

    // Sessions and keys are operator surfaces; a key cannot enumerate either.
    let (status, _) = site
        .raw(
            "GET",
            "/api/v1/sessions",
            None,
            &[("authorization", "Aerona aer_live_bogus".to_string())],
        )
        .await;
    assert_eq!(status, 401, "an unknown key is simply unauthorised");
}

#[tokio::test]
async fn a_body_over_the_limit_is_413_and_not_a_panic() {
    let env = TestEnv::new("big");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1013, "ops", None);
    let site = Site::with_env("big".into(), env).await;
    let huge = json!({"text": "x".repeat(crate::app::MAX_BODY_BYTES + 1024)});
    let (status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/ai-filter/test"),
            Some(huge),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 413, "{body}");
    assert_eq!(body["error"]["code"], "payload_too_large");
    // The server is still alive and answering afterwards.
    let (status, _) = site.get("/api/v1/healthz").await;
    assert_eq!(status, 200);
}

// ---------------------------------------------------------------------------
// The callback's failure paths, and what `/guilds` promises a fresh install
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unfinished_callback_bounces_to_the_page_not_to_an_error_body() {
    let site = Site::start("callback").await;

    // A callback with no code is a bookmark, a probe, or Discord answering `?error=` (handled
    // above, with its own reason). None of those deserve a wall of JSON in the browser.
    let (status, _, headers) = site.exchange("GET", "/auth/callback", None, &[]).await;
    // `Redirect::to` is a 303 on purpose: the callback can arrive as a POST, and the only
    // honest way to send a browser back to a page after one is "see other, and use GET".
    assert_eq!(
        status, 303,
        "a redirect, not a 400 the operator reads as a crash"
    );
    assert_eq!(
        headers.get("location").and_then(|v| v.to_str().ok()),
        Some("/login/?error=malformed-callback"),
        "the page decides what the words are"
    );

    // A state this install never minted: expired, replayed, or started from another address.
    let (status, _, headers) = site
        .exchange("GET", "/auth/callback?code=x&state=never-minted", None, &[])
        .await;
    assert_eq!(status, 303);
    assert_eq!(
        headers.get("location").and_then(|v| v.to_str().ok()),
        Some("/login/?error=expired-link"),
        "one usable try per state, and the answer to a used one is to click again"
    );
}

#[tokio::test]
async fn the_sign_in_bounce_survives_the_static_layer() {
    // The bug this pins is invisible without a `dist`: `static_headers` decides "there is no
    // such page" by looking for `dist/<path>/index.html`, and `/auth/callback` is a *route*,
    // so the 303 the handler produced came back with its status replaced by 404. The browser
    // only follows 3xx, so every real sign-in failure showed the 404 document and the
    // `Location` header - with the sentence in it - was never read. Unit tests passed because
    // they ran the router with `assets: None`.
    let env = TestEnv::new("bounce");
    let dist = env.write_dist();
    let site = Site::with_env_and(
        "bounce".to_string(),
        env,
        Some(crate::app::StaticAssets { dist_dir: dist }),
    )
    .await;

    let (status, _, headers) = site
        .exchange("GET", "/auth/callback?code=x&state=never-minted", None, &[])
        .await;
    assert_eq!(status, 303, "a redirect the browser will actually follow");
    let location = headers
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(location, "/login/?error=expired-link");

    // And the target of that bounce is a document the static layer serves, at 200. The trailing
    // slash is what makes the two agree.
    let (status, body) = site
        .raw(
            "GET",
            &location,
            None,
            &[("accept", "text/html; charset=utf-8".to_string())],
        )
        .await;
    assert_eq!(status, 200, "the login page is reachable from the bounce");
    assert!(
        body["raw"]
            .as_str()
            .unwrap_or_default()
            .contains("<html>login</html>"),
        "the document, not the 404 one: {body}"
    );

    // A path that is neither a file nor a router path is still a miss, and still answers with
    // the styled page at a real 404 rather than the shell at 200.
    let (status, _) = site
        .raw(
            "GET",
            "/no-such-page/",
            None,
            &[("accept", "text/html; charset=utf-8".to_string())],
        )
        .await;
    assert_eq!(status, 404, "the exemption is for routes, not for typos");
}

#[tokio::test]
async fn the_server_list_lists_the_administered_ones_and_offers_an_invite() {
    let env = TestEnv::new("guild-list");
    ready(&env.state.db);
    let (cookie, _csrf) = env.session_with_grants(
        1090,
        "ops",
        &json!([
            { "id": GUILD_S, "name": "Test Guild", "icon": null },
            { "id": "999", "name": "Somewhere else", "icon": "abc" },
        ]),
    );
    let site = Site::with_env("guild-list".to_string(), env).await;

    let (status, body) = site.authed_get("/api/v1/guilds", &cookie).await;
    assert_eq!(status, 200);
    // The envelope keeps the payload under `data`, like every other route here.
    let list = body["data"]["guilds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        list.len(),
        2,
        "the served guild and the one still waiting for an invite"
    );

    let here = list
        .iter()
        .find(|g| g["id"] == GUILD_S)
        .expect("the bot's guild");
    assert_eq!(here["botPresent"], true);
    assert_eq!(here["name"], "Test Guild", "the row from our database wins");

    let away = list
        .iter()
        .find(|g| g["id"] == "999")
        .expect("the other one");
    assert_eq!(away["botPresent"], false);
    assert_eq!(
        away["name"], "Somewhere else",
        "Discord's name is all we have for it"
    );
    // The test application has no client id, which is the honest state of a `TestEnv` install
    // and the case the dashboard has a sentence for. The link itself is pinned in
    // `oauth::tests::the_invite_link_picks_the_server_and_asks_for_administrator`.
    assert!(away["inviteUrl"].is_null(), "no application, no invite");

    // A guild nobody granted is not in the list at all, and cannot be read through.
    let (status, _) = site
        .authed_get("/api/v1/guilds/555/settings/general", &cookie)
        .await;
    assert_eq!(
        status, 403,
        "the grant list is the boundary, not a display preference"
    );
}

// ---------------------------------------------------------------------------
// Round 12: the writes the dashboard makes, in the words the dashboard reads
// ---------------------------------------------------------------------------

/// The bug this test exists for: `POST /identity/avatar` stored its digest under a key the model
/// does not have, and `normalize` answers an unknown key with a 400 - so uploading an avatar made
/// every *later* profile save fail, with a message about a spelling nobody had typed. A staged
/// avatar has to leave a document the next PATCH accepts.
#[tokio::test]
async fn a_staged_avatar_leaves_the_identity_document_savable() {
    let env = TestEnv::new("identity-avatar");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1009, "ops", None);
    let site = Site::with_env("identity-avatar".into(), env).await;
    let guild = aerona_core::identity::Snowflake(GUILD_S.parse::<u64>().unwrap());
    async fn patch(site: &Site, cookie: &str, csrf: &str, body: Value) -> (u16, Value) {
        site.browser(
            "PATCH",
            &format!("/api/v1/guilds/{GUILD_S}/settings/identity"),
            Some(body),
            Some(cookie),
            Some(csrf),
        )
        .await
    }

    // First a plain save, so the sequence starts where the report left it.
    let (status, body) = patch(&site, &cookie, &csrf, json!({ "nick": "Aerona" })).await;
    assert_eq!(status, 200, "{body}");

    let png = "iVBORw0KGgoAAAANSUhEUg".repeat(48);
    let (status, body) = site
        .browser(
            // A stage, not an apply: the route is `PUT`, because the bytes wait for the worker
            // rather than going to Discord on the request.
            "PUT",
            &format!("/api/v1/guilds/{GUILD_S}/identity/avatar"),
            Some(json!({ "data": format!("data:image/png;base64,{png}") })),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["staged"], true);
    let hash = body["data"]["hash"].as_str().unwrap().to_string();
    assert!(hash.starts_with("sha256:"), "{hash}");

    // The save that used to answer 400.
    let (status, body) = patch(&site, &cookie, &csrf, json!({ "bio": "here to help" })).await;
    assert_eq!(
        status, 200,
        "an identity save after staging an avatar must work: {body}"
    );

    let doc: Value = site
        .db()
        .settings(guild, aerona_core::models::Feature::Identity)
        .unwrap();
    assert_eq!(doc["nick"], "Aerona", "{doc}");
    assert_eq!(doc["avatar_hash"], hash.clone(), "{doc}");
    assert!(
        doc.get("avatarHash").is_none(),
        "the row stores the model's own name: {doc}"
    );
    assert!(
        doc.get("avatar")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .is_empty(),
        "the bytes never land in the document: {doc}"
    );

    // Clearing goes through the same door and leaves a null, not a leftover hash.
    let (status, body) = site
        .browser(
            "DELETE",
            &format!("/api/v1/guilds/{GUILD_S}/identity/avatar"),
            None,
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let doc: Value = site
        .db()
        .settings(guild, aerona_core::models::Feature::Identity)
        .unwrap();
    assert!(doc["avatar_hash"].is_null(), "{doc}");
    assert_eq!(doc["bio"], "here to help", "{doc}");
}

/// A link button Discord would refuse is dropped rather than stored: one bad button fails the whole
/// ticket open, so a typo must not be able to switch the feature off for everyone.
#[tokio::test]
async fn ticket_link_buttons_are_cleaned_deduped_and_cut_to_five() {
    let env = TestEnv::new("ticket-links");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1009, "ops", None);
    let site = Site::with_env("ticket-links".into(), env).await;
    let guild = aerona_core::identity::Snowflake(GUILD_S.parse::<u64>().unwrap());
    let long = "x".repeat(120);
    let links = json!([
        { "label": "Rules", "url": "https://example.com/rules" },
        { "label": "Rules", "url": "https://example.com/rules" },
        { "label": "Status", "url": "https://status.example.com/" },
        { "label": long.as_str(), "url": "https://example.com/long" },
        { "label": "FTP", "url": "ftp://example.com/nope" },
        { "label": "Open", "url": "https://example.com/open" },
        { "label": "Sixth", "url": "https://example.com/sixth" },
        { "label": "Seventh", "url": "https://example.com/seventh" },
    ]);
    let (status, body) = site
        .browser(
            "PATCH",
            &format!("/api/v1/guilds/{GUILD_S}/settings/tickets"),
            Some(
                json!({ "links": links, "close_label": "Close ticket", "claim_label": "Take it" }),
            ),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let doc: Value = site
        .db()
        .settings(guild, aerona_core::models::Feature::Tickets)
        .unwrap();
    let kept = doc["links"].as_array().expect("a list of links");
    assert_eq!(kept.len(), 5, "six usable lines, five buttons: {kept:?}");
    let urls: Vec<&str> = kept.iter().map(|l| l["url"].as_str().unwrap()).collect();
    assert_eq!(
        &urls[..3],
        [
            "https://example.com/rules",
            "https://status.example.com/",
            "https://example.com/long"
        ],
        "the order the operator wrote, with the duplicate gone: {urls:?}"
    );
    assert!(
        !urls.iter().any(|u| u.contains("nope")),
        "an unusable scheme: {urls:?}"
    );
    assert!(
        !urls.iter().any(|u| u.ends_with("seventh")),
        "Discord's row holds five: {urls:?}"
    );
    assert_eq!(
        urls.len(),
        urls.iter().collect::<std::collections::HashSet<_>>().len(),
        "{urls:?}"
    );
    assert_eq!(
        kept[2]["label"].as_str().unwrap().chars().count(),
        40,
        "{kept:?}"
    );
    // Wording that repeats the built-in one is stored as "no change", so the default stays
    // recognisable as the default.
    assert_eq!(doc["close_label"], "", "{doc}");
    assert_eq!(doc["claim_label"], "Take it", "{doc}");
}

/// A sticky is allowed to be written here, with an embed and no Discord message behind it, and it
/// is posted immediately instead of waiting for `everyMessages` lines of chatter.
#[tokio::test]
async fn a_sticky_can_be_written_here_and_is_posted_at_once() {
    let env = TestEnv::new("sticky-author");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1009, "ops", None);
    let site = Site::with_env("sticky-author".into(), env).await;

    let (status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/stickies"),
            Some(json!({ "channel": "1" })),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 400, "nothing to post is not a sticky: {body}");

    let (status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/stickies"),
            Some(json!({
                "channel": "1",
                "embed_title": "Read this first",
                "embed_body": "line one\nline two",
                "embed_color": 0xff_aa_00,
                "everyMessages": 30,
            })),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let id = body["data"]["id"].as_i64().expect("an id");
    assert!(
        body["data"]["queued"].as_i64().is_some(),
        "the post is queued: {body}"
    );
    let row = &body["data"]["stickies"].as_array().unwrap()[0];
    assert_eq!(row["message_id"], "0", "no anchor: {row}");
    assert_eq!(row["embed_title"], "Read this first", "{row}");
    assert_eq!(
        row["embed_body"], "line one\nline two",
        "a sticky keeps its lines: {row}"
    );
    assert_eq!(row["content"], "", "an embed alone is a sticky: {row}");
    assert_eq!(row["embed_color"], 0xff_aa_00, "{row}");
    assert_eq!(row["every_messages"], 30, "{row}");

    // Writing the same thing again updates the row rather than stacking a second sticky.
    let (status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/stickies"),
            Some(json!({ "channel": "1", "message": "500", "content": "same anchor, new note" })),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let (status, list) = site
        .browser(
            "GET",
            &format!("/api/v1/guilds/{GUILD_S}/stickies"),
            None,
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{list}");
    let rows = list["data"]["stickies"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        2,
        "an anchored one and an unanchored one: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r["id"] == json!(id)),
        "the first survives: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r["channel_id"] == json!("1")),
        "rows name their channel: {rows:?}"
    );
}

/// A ticket type is not a row in a table until the button exists in Discord, so the write that
/// creates it also queues the post, and deleting it queues a redraw of the board.
#[tokio::test]
async fn a_ticket_type_is_a_button_not_only_a_row() {
    let env = TestEnv::new("ticket-panels");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1009, "ops", None);
    let site = Site::with_env("ticket-panels".into(), env).await;

    let (status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/ticket-panels"),
            Some(json!({
                "channel": "7",
                "label": "Report a player",
                "emoji": "🚩",
                "color": 0xe7_3c_88,
                "subject": "player report",
                "intro": "Say who and when. {user} {ticket} work here.",
            })),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let id = body["data"]["id"].as_i64().expect("an id");
    let poll = body["data"]["queued"]
        .as_i64()
        .expect("a queued outbox row");
    let row = &body["data"]["panels"].as_array().unwrap()[0];
    assert_eq!(row["label"], "Report a player", "{row}");
    assert_eq!(row["color"], 0xe7_3c_88, "{row}");
    assert_eq!(
        row["message_id"], "0",
        "unbound until the bot posts it: {row}"
    );
    assert_eq!(
        row["intro"], "Say who and when. {user} {ticket} work here.",
        "{row}"
    );

    let (status, out) = site
        .authed_get(&format!("/api/v1/outbox/{poll}"), &cookie)
        .await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["data"]["action"], "panel.post", "{out}");

    let (status, body) = site
        .browser(
            "GET",
            &format!("/api/v1/guilds/{GUILD_S}/ticket-panels"),
            None,
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["data"]["panels"].as_array().unwrap().len(),
        1,
        "{body}"
    );

    let (status, body) = site
        .browser(
            "DELETE",
            &format!("/api/v1/guilds/{GUILD_S}/ticket-panels/{id}"),
            None,
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = site
        .browser(
            "GET",
            &format!("/api/v1/guilds/{GUILD_S}/ticket-panels"),
            None,
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(
        body["data"]["panels"].as_array().unwrap().len(),
        0,
        "{body}"
    );
    assert_eq!(status, 200);
}

/// The banner is the avatar's problem all over again: bytes that must not enter the settings
/// document, a digest that must, and a profile that has to stay savable afterwards. Half of RED-13
/// was "the next save 400s", so this test is that same save after a *banner* stage - and it is the
/// one test that would have caught a second handler written without the `normalize` call.
#[tokio::test]
async fn a_staged_banner_leaves_the_identity_document_savable() {
    let env = TestEnv::new("identity-banner");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1009, "ops", None);
    let site = Site::with_env("identity-banner".into(), env).await;
    let guild = aerona_core::identity::Snowflake(GUILD_S.parse::<u64>().unwrap());
    async fn patch(site: &Site, cookie: &str, csrf: &str, body: Value) -> (u16, Value) {
        site.browser(
            "PATCH",
            &format!("/api/v1/guilds/{GUILD_S}/settings/identity"),
            Some(body),
            Some(cookie),
            Some(csrf),
        )
        .await
    }

    let png = "iVBORw0KGgoAAAANSUhEUg".repeat(64);
    let (status, body) = site
        .browser(
            "PUT",
            &format!("/api/v1/guilds/{GUILD_S}/identity/banner"),
            Some(json!({ "data": format!("data:image/png;base64,{png}") })),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["staged"], true);
    let hash = body["data"]["hash"].as_str().unwrap().to_string();
    assert!(hash.starts_with("sha256:"), "{hash}");

    let (status, body) = patch(&site, &cookie, &csrf, json!({ "bio": "quiet and useful" })).await;
    assert_eq!(
        status, 200,
        "a banner stage must not lock the profile: {body}"
    );

    let doc: Value = site
        .db()
        .settings(guild, aerona_core::models::Feature::Identity)
        .unwrap();
    assert_eq!(doc["banner_hash"], hash, "{doc}");
    assert!(
        doc["avatar_hash"].is_null(),
        "the banner slot must not write into the avatar's: {doc}"
    );
    assert!(
        doc["bio"].as_str().unwrap_or_default().contains("quiet"),
        "{doc}"
    );

    // And clearing it is a write, not a deletion of the row: the digest goes, the bytes were
    // never there, and the apply is told to send a null.
    let (status, body) = site
        .browser(
            "DELETE",
            &format!("/api/v1/guilds/{GUILD_S}/identity/banner"),
            None,
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let doc: Value = site
        .db()
        .settings(guild, aerona_core::models::Feature::Identity)
        .unwrap();
    assert!(
        doc.get("banner_hash").is_none() || doc["banner_hash"].is_null(),
        "the cleared digest is gone from the document: {doc}"
    );
    let staged = site
        .db()
        .guild_report(guild, "banner-pending")
        .unwrap()
        .expect("the clear is queued as an instruction");
    assert_eq!(staged["clear"], true, "{staged}");
}

/// A decoration id is the one cosmetic Discord will not list for a bot, so the field is a bare id
/// - which makes the shape the only thing the API can check. It checks it in `normalize`, the place
/// every other identity field is checked, and the panel gets a 400 with the field named rather than
/// a save that Discord refuses ten seconds later in an outbox row nobody is reading.
#[tokio::test]
async fn a_decoration_is_validated_where_the_rest_of_the_profile_is() {
    let env = TestEnv::new("identity-deco");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1009, "ops", None);
    let site = Site::with_env("identity-deco".into(), env).await;
    let guild = aerona_core::identity::Snowflake(GUILD_S.parse::<u64>().unwrap());
    async fn put_deco(site: &Site, cookie: &str, csrf: &str, body: Value) -> (u16, Value) {
        site.browser(
            "PUT",
            &format!("/api/v1/guilds/{GUILD_S}/identity/deco"),
            Some(body),
            Some(cookie),
            Some(csrf),
        )
        .await
    }

    let (status, body) = put_deco(
        &site,
        &cookie,
        &csrf,
        json!({ "avatar_decoration_id": "not-an-id" }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("digits"),
        "the refusal names what is wrong: {body}"
    );

    let (status, body) = put_deco(
        &site,
        &cookie,
        &csrf,
        json!({ "avatar_decoration_id": " 1207328054057771069 ", "banner_decoration_id": "" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let doc: Value = site
        .db()
        .settings(guild, aerona_core::models::Feature::Identity)
        .unwrap();
    assert_eq!(
        doc["avatar_decoration_id"], "1207328054057771069",
        "spaces are trimmed on the way in: {doc}"
    );
    assert_eq!(
        doc["banner_decoration_id"], "",
        "an empty string is the clear, and it is kept as one: {doc}"
    );

    // The apply is what sends it. A save that queues nothing is a save that changes a row and
    // leaves the bot looking exactly as it did.
    let queued = body["data"]["queued"].as_i64().expect("an outbox row");
    assert!(queued > 0, "{body}");
    let (status, out) = site
        .authed_get(&format!("/api/v1/outbox/{queued}"), &cookie)
        .await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["data"]["action"], "identity.apply", "{out}");
}

/// `repost` used to be a sticky setting; the behaviour is now the only behaviour. A document saved
/// while the switch existed still carries the key, and an unknown key is a 400 - so without
/// retiring it on the way in, *every* later save of the stickies page would be refused because of a
/// word nobody is typing. That is RED-13's bug met from the other side.
#[tokio::test]
async fn a_sticky_document_from_before_the_switch_went_away_still_saves() {
    let env = TestEnv::new("sticky-retire");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1009, "ops", None);
    let guild = aerona_core::identity::Snowflake(GUILD_S.parse::<u64>().unwrap());
    env.state
        .db
        .put_settings(
            guild,
            aerona_core::models::Feature::Sticky,
            &json!({
                "enabled": true,
                "cooldown_secs": 60,
                "max_per_channel": 3,
                "repost": false,
            }),
            None,
        )
        .unwrap();
    let site = Site::with_env("sticky-retire".into(), env).await;

    let (status, body) = site
        .browser(
            "PATCH",
            &format!("/api/v1/guilds/{GUILD_S}/settings/sticky"),
            Some(json!({ "cooldown_secs": 75 })),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["settings"]["cooldown_secs"], 75);
    assert!(
        body["data"]["settings"].get("repost").is_none(),
        "the retired key leaves the document on the next write: {}",
        body["data"]["settings"]
    );

    // The tolerance is scoped to the keys a model says it used to have. Anything else is still
    // refused, which is the whole reason a typo cannot masquerade as a saved setting.
    let (status, body) = site
        .browser(
            "PATCH",
            &format!("/api/v1/guilds/{GUILD_S}/settings/sticky"),
            Some(json!({ "cooldown_secs_2": 5 })),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 400, "an invented key is still a refusal: {body}");
}

/// The Tickets page gets one channel for the board, and pressing Save is what puts it there - not
/// a setting the worker may honour some day. A type with no channel of its own goes to the same
/// place, so the page can hold one choice instead of one per row.
#[tokio::test]
async fn the_ticket_channel_is_where_the_board_goes() {
    let env = TestEnv::new("ticket-channel");
    ready(&env.state.db);
    let (cookie, csrf) = env.session_cookie(1009, "ops", None);
    let site = Site::with_env("ticket-channel".into(), env).await;

    let (status, body) = site
        .browser(
            "PATCH",
            &format!("/api/v1/guilds/{GUILD_S}/settings/tickets"),
            Some(json!({ "enabled": true, "board_channel_id": "7" })),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["settings"]["board_channel_id"], "7");
    let queued = body["data"]["queued"]
        .as_i64()
        .expect("a queued board post");
    let (status, out) = site
        .authed_get(&format!("/api/v1/outbox/{queued}"), &cookie)
        .await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["data"]["action"], "panel.redraw", "{out}");
    assert_eq!(out["data"]["args"]["channel"], "7", "{out}");

    let (status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/ticket-panels"),
            Some(json!({ "label": "Ask about a bill" })),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        body["data"]["panels"][0]["channel_id"], "7",
        "a type with no channel goes where the board lives: {}",
        body["data"]["panels"]
    );

    // And with no channel anywhere, the refusal says what to do about it rather than naming a
    // field the operator never looked at.
    let (status, body) = site
        .browser(
            "PATCH",
            &format!("/api/v1/guilds/{GUILD_S}/settings/tickets"),
            Some(json!({ "board_channel_id": null })),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/ticket-panels"),
            Some(json!({ "label": "Nowhere to go" })),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no ticket channel is set"),
        "the refusal says what to do: {body}"
    );
}

// ---------------------------------------------------------------------------
// Minecraft cape notifier
// ---------------------------------------------------------------------------

/// A cape row as the worker would write one, with frames made of ASCII so a test can compare the
/// body byte for byte: `send` reads a response as text, and real png bytes do not survive that.
fn cape_release(page_id: u64, title: &str, frames: usize) -> aerona_core::db::capes::NewCape {
    let page = aerona_core::cape::CapePage {
        title: title.into(),
        page_id: page_id as i64,
        last_rev: 900,
        url: format!("https://minecraft.wiki/w/{title}"),
    };
    let mut new = aerona_core::db::capes::NewCape::from_page(&page, false);
    new.render_kind = "drape".into();
    new.texture_file = format!("File:{title} (texture) JE.png");
    new.frames = (0..frames)
        .map(|i| format!("frame-{i}").into_bytes())
        .collect();
    new
}

/// The cape page is a read of the worker's own tables, and the only thing the panel may cause is
/// another look. Both halves are asserted here because each is a temptation: a `GET` that asked
/// minecraft.wiki would put a third-party request on the public port, and a `POST` that did the
/// same would be a denial-of-service tool with a settings page attached.
#[tokio::test]
async fn the_cape_page_shows_what_the_worker_found_and_asks_for_a_look() {
    let env = TestEnv::new("capes");
    ready(&env.state.db);
    let guild = aerona_core::identity::Snowflake(GUILD_S.parse::<u64>().unwrap());
    let (cookie, csrf) = env.session_cookie(1009, "ops", None);
    // Two frames on one cape and none on the other: a cape with no artwork is still a cape.
    env.state
        .db
        .remember_cape(guild, &cape_release(42, "Aurora Cape", 2))
        .unwrap();
    env.state
        .db
        .remember_cape(guild, &cape_release(7, "Spade Cape", 0))
        .unwrap();
    let site = Site::with_env("capes".into(), env).await;

    let (status, body) = site
        .authed_get(&format!("/api/v1/guilds/{GUILD_S}/capes"), &cookie)
        .await;
    assert_eq!(status, 200, "{body}");
    let data = &body["data"];
    assert_eq!(
        data["config"]["enabled"], false,
        "nothing has been saved yet, so the defaults answer: {data}"
    );
    assert_eq!(
        data["config"]["check_every_minutes"], 20,
        "the default is the one the notifier can actually afford: one request per pass for a guild \
         that has seen everything"
    );
    assert_eq!(data["frames"], 4, "the renderer makes four angles");
    assert!(data["channel"].is_null(), "no channel is set: {data}");
    assert!(
        data["nextCheck"].is_null(),
        "this server has never been polled, so there is no schedule to show: {data}"
    );
    assert_eq!(data["lastCheck"], 0);

    assert_eq!(
        data["status"], "the source has not answered a poll yet",
        "a server with no report is told that, rather than shown a pass with nothing in it: {data}"
    );
    assert!(data["error"].is_null(), "{data}");

    let rows = data["releases"].as_array().expect("a list");
    assert_eq!(rows.len(), 2, "{rows:?}");
    let aurora = rows
        .iter()
        .find(|r| r["slug"] == "mw42")
        .expect("the cape the worker recorded");
    assert_eq!(aurora["name"], "Aurora Cape", "{aurora}");
    assert_eq!(
        aurora["article_url"], "https://minecraft.wiki/w/Aurora Cape",
        "the link is the one the source handed back, spelled here: {aurora}"
    );
    assert_eq!(aurora["render_kind"], "drape");
    assert_eq!(aurora["frames"], 2);
    assert_eq!(aurora["has_render"], true);
    assert_eq!(
        aurora["frameUrl"],
        json!(format!(
            "/api/v1/guilds/{GUILD_S}/capes/mw42/frame/{}",
            aerona_core::cape::HERO_FRAME
        )),
        "the panel is handed the hero angle rather than told to guess one, and the angle is the \
         renderer's constant - which turned view is worth a message is the drawing code's business, \
         not the route's"
    );
    let spade = rows
        .iter()
        .find(|r| r["slug"] == "mw7")
        .expect("the other cape");
    assert_eq!(
        spade["frameUrl"],
        Value::Null,
        "a cape with no picture has no image url to be broken: {spade}"
    );
    assert_eq!(spade["has_render"], false);

    // The stored bytes, with the headers a panel needs to draw them inline.
    let (status, body, headers) = site
        .exchange(
            "GET",
            aurora["frameUrl"].as_str().expect("a url"),
            None,
            &[("cookie", cookie.clone())],
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["raw"],
        format!("frame-{}", aerona_core::cape::HERO_FRAME),
        "the frame the renderer says is the hero, not whatever index it happened to land on"
    );
    assert_eq!(
        headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default(),
        "image/png"
    );
    let cache = headers
        .get(axum::http::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        cache.contains("no-store"),
        "a render is served behind a session, so the shared-cache rule holds for it too: {cache}"
    );

    // A frame that is not there answers in words. This is the case where an operator wonders why a
    // row has no picture, and a 404 with no sentence would leave them guessing about a bug.
    let (status, body) = site
        .authed_get(
            &format!("/api/v1/guilds/{GUILD_S}/capes/mw42/frame/7"),
            &cookie,
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("without a render"),
        "the refusal says why: {body}"
    );

    // Saving the page clamps what the worker refuses to do and rejects what it does not model.
    let (status, body) = site
        .browser(
            "PATCH",
            &format!("/api/v1/guilds/{GUILD_S}/settings/capes"),
            Some(json!({
                "enabled": true,
                "channel_id": "777",
                "ping_role_id": "888",
                "check_every_minutes": 1,
                "show_render": true,
                "embed_color": 4294967295_u64,
            })),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let cfg = &body["data"]["settings"];
    assert_eq!(
        cfg["check_every_minutes"], 15,
        "a one-minute poll is cut to the interval that is worth having: {cfg}"
    );
    assert_eq!(cfg["embed_color"], 16_777_215, "a colour is 24 bits: {cfg}");
    assert_eq!(cfg["channel_id"], "777");
    assert_eq!(cfg["ping_role_id"], "888");

    let (status, body) = site
        .authed_get(&format!("/api/v1/guilds/{GUILD_S}/capes"), &cookie)
        .await;
    assert_eq!(status, 200, "{body}");
    let data = &body["data"];
    assert_eq!(data["config"]["enabled"], true);
    assert_eq!(data["channel"]["id"], "777");
    assert!(
        data["channel"]["name"].is_null(),
        "the worker has not reported this server's channels, so the id is what there is: {data}"
    );
    assert_eq!(data["pingRole"]["id"], "888");

    // A typo is a refusal rather than a silent nothing, which is how every settings page behaves
    // and why the notifier can trust the document it reads.
    let (status, body) = site
        .browser(
            "PATCH",
            &format!("/api/v1/guilds/{GUILD_S}/settings/capes"),
            Some(json!({ "ping_role": "8" })),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 400, "an invented key is still a refusal: {body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("ping_role"),
        "{body}"
    );

    // What the worker said about its last pass is the sentence the panel prints, word for word.
    site.db()
        .put_guild_report(
            guild,
            "capes",
            &json!({ "pages": 60, "recorded": 2, "waiting": 4, "stuck": 1, "error": null }),
        )
        .unwrap();
    let (status, body) = site
        .authed_get(&format!("/api/v1/guilds/{GUILD_S}/capes"), &cookie)
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["data"]["status"],
        "60 pages looked at, 2 recorded, 4 waiting for the next look, \
         1 not posted after 5 tries - check the channel",
        "one wording for one event, written by the process that saw it: {body}"
    );
    assert!(body["data"]["error"].is_null(), "{body}");

    // And asking for a look now is a stamp, not a fetch.
    let (status, body) = site
        .browser(
            "POST",
            &format!("/api/v1/guilds/{GUILD_S}/capes/check"),
            Some(json!({})),
            Some(&cookie),
            Some(&csrf),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["queued"], true);
    assert!(
        body["data"]["note"]
            .as_str()
            .unwrap_or_default()
            .contains("next pass"),
        "the button says when it happens: {body}"
    );
    let (last, seeded) = site.db().cape_state(guild).unwrap();
    assert_eq!(last, 0, "the stamp is what the tick compares against");
    assert!(seeded, "a first look by hand records without pinging");
    let (status, body) = site
        .authed_get(&format!("/api/v1/audit?guild={GUILD_S}&limit=20"), &cookie)
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["data"]["entries"]
            .as_array()
            .map(|rows| rows.iter().any(|a| a["action"] == "cape.check"))
            .unwrap_or(false),
        "asking for a poll is written down, because it is a thing the bot will go do: {body}"
    );
}
