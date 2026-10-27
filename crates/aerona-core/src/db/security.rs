//! Auth and infrastructure records: API keys, dashboard sessions, OAuth state,
//! the audit chain, and the API→bot outbox - plus the pairing-code functions, which no
//! product surface calls any more (the table and its rows stay for installs that have them;
//! see the note on `issue_pairing_code`).
//!
//! Two rules govern everything in this file:
//!
//! 1. **No secret is ever stored recoverably.** API keys, pairing codes and
//!    session tokens are written as HMAC digests (`security::token_mac_hex`). The
//!    plaintext exists only in the HTTP response that created it.
//! 2. **The audit log is append-only and chained.** `hash` covers the previous
//!    row's hash, so an operator with shell access can see that tampering
//!    happened even though they could technically rewrite the table.

use serde::{Deserialize, Serialize};

use rusqlite::params;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::identity::Snowflake;
use crate::security;
use crate::time::now;

use super::Db;

/// One row of `api_keys`, without the MAC.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyRecord {
    pub id: i64,
    pub label: String,
    pub hint: String,
    pub guild_id: Option<Snowflake>,
    pub scopes: Vec<String>,
    pub created_by: Snowflake,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub expires_at: i64,
    pub revoked_at: Option<i64>,
    pub request_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: i64,
    pub user_id: Snowflake,
    pub username: String,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub expires_at: i64,
    pub csrf_secret: String,
    pub ip: String,
    pub user_agent: String,
    /// The single guild this session may act on. `sessions.guild_grants` is the modern
    /// version of the same idea (a list rather than one id); this column stays because a
    /// session written before v4 may still carry it.
    /// `None` means "install-wide" (the OAuth path, after the bot has reported the
    /// operator's guilds) - which is why it is *not* derived from `guilds`: a
    /// bootstrap session must work before any guild row exists.
    pub allowed_guild: Option<Snowflake>,
}

/// One audit entry. Built with the chained setters so call sites read as the
/// action they describe (`AuditEntry::new("warn.create").guild(g).actor(u)`) and a
/// new audit dimension never means editing 40 call sites.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub guild: Option<Snowflake>,
    pub actor_type: ActorType,
    pub actor: Option<Snowflake>,
    pub actor_label: String,
    pub action: String,
    pub target: Option<String>,
    pub ip: String,
    pub meta: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorType {
    /// Authenticated dashboard user.
    User,
    /// API key (machine client).
    ApiKey,
    /// The bot itself, e.g. an automatic raid response.
    Bot,
    /// The install itself, before any identity exists: retention, schema, the purge pass.
    System,
}

impl ActorType {
    pub fn as_str(self) -> &'static str {
        match self {
            ActorType::User => "user",
            ActorType::ApiKey => "apikey",
            ActorType::Bot => "bot",
            ActorType::System => "system",
        }
    }
}

impl AuditEntry {
    pub fn new(action: impl Into<String>) -> Self {
        AuditEntry {
            guild: None,
            actor_type: ActorType::System,
            actor: None,
            actor_label: String::new(),
            action: action.into(),
            target: None,
            ip: String::new(),
            meta: Value::Null,
        }
    }

    /// Named `for_guild` rather than `guild`: a builder with a `guild` field cannot
    /// also expose `pub fn guild`, and hiding a public field behind a method of the
    /// same name makes `AuditEntry { guild, .. }` reads impossible from outside.
    pub fn for_guild(mut self, v: Snowflake) -> Self {
        self.guild = Some(v);
        self
    }

    pub fn maybe_guild(mut self, v: Option<Snowflake>) -> Self {
        self.guild = v;
        self
    }

    pub fn actor(mut self, kind: ActorType, id: Option<Snowflake>, label: &str) -> Self {
        self.actor_type = kind;
        self.actor = id;
        self.actor_label = security::clean(label, 64);
        self
    }

    pub fn user(self, id: Snowflake, label: &str) -> Self {
        self.actor(ActorType::User, Some(id), label)
    }

    pub fn bot(self, label: &str) -> Self {
        self.actor(ActorType::Bot, None, label)
    }

    pub fn key(self, id: Snowflake, label: &str) -> Self {
        self.actor(ActorType::ApiKey, Some(id), label)
    }

    pub fn target(mut self, v: impl Into<String>) -> Self {
        self.target = Some(v.into());
        self
    }

    pub fn maybe_target(mut self, v: Option<String>) -> Self {
        self.target = v;
        self
    }

    pub fn ip(mut self, v: &str) -> Self {
        self.ip = v.to_string();
        self
    }

    pub fn meta(mut self, v: Value) -> Self {
        self.meta = v;
        self
    }
}

/// A queued action for the bot to execute against Discord.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxItem {
    pub id: i64,
    pub guild_id: Snowflake,
    pub actor_id: Snowflake,
    pub actor_label: String,
    pub action: String,
    pub args: Value,
    pub reference: String,
}

pub const SCOPES: &[&str] = &["read", "moderate", "config", "tickets"];

/// A claim is abandoned after this long, so a crashed worker cannot wedge the queue.
const OUTBOX_CLAIM_TIMEOUT: i64 = 120;

/// How long a pairing code stays locked after exhausting its attempt budget.
const LOCKOUT_SECS: i64 = 900;

impl Db {
    // -- API keys ------------------------------------------------------------

    /// Mint a key as `(id, plaintext, hint)`. The plaintext is returned exactly
    /// once, in this response - it is never recoverable from the database again.
    pub fn create_api_key(
        &self,
        pepper: &[u8],
        label: &str,
        guild: Option<Snowflake>,
        scopes: &[String],
        created_by: Snowflake,
        ttl_days: i64,
    ) -> Result<(i64, String, String)> {
        let label = security::clean(label, 64);
        if label.is_empty() {
            return Err(Error::Invalid("api key label must not be empty".into()));
        }
        let clean_scopes = sanitize_scopes(scopes)?;
        let key = format!("{}{}", security::KEY_PREFIX, security::random_urlsafe(32));
        let mac = security::token_mac_hex(pepper, &key);
        let hint = key
            .chars()
            .take(security::KEY_PREFIX.len() + 8)
            .collect::<String>();
        let hint_s = hint.clone();
        let at = now();
        let expires = if ttl_days > 0 {
            at + ttl_days.min(3650) * 86_400
        } else {
            0
        };
        let by = created_by.as_i64();
        let gid = guild.map(|g| g.as_i64());
        let scopes_json = serde_json::to_string(&clean_scopes).unwrap_or_else(|_| "[]".into());
        let id = self.tx(move |conn| {
            conn.execute(
                "INSERT INTO api_keys(label,key_mac,hint,guild_id,scopes,created_by,created_at,expires_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![label, mac, hint, gid, scopes_json, by, at, expires],
            )?;
            Ok(conn.last_insert_rowid())
        })?;
        Ok((id, key, hint_s))
    }

    /// Authenticate a bearer key. Returns the row on success.
    ///
    /// The lookup is by MAC, so verification is a single indexed equality test -
    /// no per-key scan, and no timing signal about which keys exist.
    pub fn lookup_api_key(&self, pepper: &[u8], candidate: &str) -> Result<Option<ApiKeyRecord>> {
        if !candidate.starts_with(security::KEY_PREFIX) || candidate.len() > 200 {
            return Ok(None);
        }
        let mac = security::token_mac_hex(pepper, candidate);
        self.read(move |conn| read_api_key(conn, &mac))
    }

    /// Record a successful use. `request_count` drives the "last used" column and
    /// is the only way an operator learns a key is dead weight before rotating it.
    pub fn touch_api_key(&self, id: i64, count: bool) -> Result<()> {
        let at = now();
        self.tx(move |conn| {
            if count {
                conn.execute(
                    "UPDATE api_keys SET last_used_at=?2, request_count = request_count + 1 WHERE id=?1",
                    params![id, at],
                )?;
            } else {
                conn.execute("UPDATE api_keys SET last_used_at=?2 WHERE id=?1", params![id, at])?;
            }
            Ok(())
        })
    }

    pub fn list_api_keys(&self, guild: Option<Snowflake>) -> Result<Vec<ApiKeyRecord>> {
        self.read(move |conn| {
            let mut out = Vec::new();
            let mut st = conn
                .prepare(
                    "SELECT id, label, hint, guild_id, scopes, created_by, created_at, last_used_at, expires_at, revoked_at, request_count
                     FROM api_keys
                     WHERE (?1 IS NULL OR guild_id IS NULL OR guild_id = ?1)
                     ORDER BY revoked_at IS NOT NULL, created_at DESC",
                )
                .map_err(Error::db)?;
            let g = guild.map(|v| v.as_i64());
            let rows = st
                .query_map(params![g], |r| {
                    Ok(ApiKeyRecord {
                        id: r.get(0)?,
                        label: r.get(1)?,
                        hint: r.get(2)?,
                        guild_id: r.get::<_, Option<i64>>(3)?.map(|v| Snowflake(v as u64)),
                        scopes: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
                        created_by: Snowflake(r.get::<_, i64>(5)? as u64),
                        created_at: r.get(6)?,
                        last_used_at: r.get(7)?,
                        expires_at: r.get(8)?,
                        revoked_at: r.get(9)?,
                        request_count: r.get::<_, i64>(10).unwrap_or(0),
                    })
                })
                .map_err(Error::db)?;
            for row in rows {
                out.push(row.map_err(Error::db)?);
            }
            Ok(out)
        })
    }

    /// Revoke by id, refusing to revoke a key the caller cannot see.
    pub fn revoke_api_key(&self, id: i64, guild: Option<Snowflake>) -> Result<()> {
        let at = now();
        let g = guild.map(|v| v.as_i64());
        self.tx(move |conn| {
            let n = conn.execute(
                "UPDATE api_keys SET revoked_at=?2 WHERE id=?1 AND (?3 IS NULL OR guild_id IS NULL OR guild_id=?3) AND revoked_at IS NULL",
                params![id, at, g],
            )?;
            if n == 0 {
                return Err(Error::NotFound("api key"));
            }
            Ok(())
        })
    }

    // -- sessions ------------------------------------------------------------

    /// Create a dashboard session and return `(session, token)`. The token is what
    /// goes in the cookie; `csrf_secret` is what the dashboard echoes in a header,
    /// so a cross-site form submission cannot authenticate on its own.
    ///
    /// Both are plain `String`s here on purpose: `security::Secret` is deliberately
    /// not `Clone`, and a type that cannot be copied cannot be returned alongside a
    /// struct. The HTTP layer wraps the outbound copy and zeroes it; nothing in the
    /// database layer can hold one open anyway.
    pub fn create_session(
        &self,
        pepper: &[u8],
        user: Snowflake,
        username: &str,
        guild: Option<Snowflake>,
        ip: &str,
        user_agent: &str,
        hours: i64,
    ) -> Result<(Session, String)> {
        let name = security::clean(username, 64);
        let ip = security::normalize_ip(ip);
        let ua = security::clean(user_agent, 160);
        let token = security::random_urlsafe(32);
        let csrf = security::random_urlsafe(16);
        let mac = security::token_mac_hex(pepper, &token);
        let hours = hours.clamp(1, 24 * 14);
        let at = now();
        let expires = at + hours * 3600;
        let uid = user.as_i64();
        let csrf_s = csrf.clone();
        // A dashboard session without a readable guild is useless, so the first
        // session for a user bootstraps the guild row the same way a config write
        // does. Names are cosmetic; the real one arrives on the next gateway sync.
        if let Some(g) = guild.map(|v| v.as_i64()) {
            self.tx(move |conn| {
                conn.execute(
                    "INSERT OR IGNORE INTO guilds(id,name,updated_at) VALUES(?1,'?',?2)",
                    params![g, at],
                )?;
                Ok(())
            })?;
        }
        let session = self.tx(move |conn| {
            let allowed = guild.map(|v| v.as_i64());
            conn.execute(
                "INSERT INTO sessions(token_mac,user_id,username,created_at,last_seen_at,expires_at,csrf_secret,ip,user_agent,allowed_guild)
                 VALUES(?1,?2,?3,?4,?4,?5,?6,?7,?8,?9)",
                params![mac, uid, name, at, expires, csrf_s, ip, ua, allowed],
            )?;
            Ok(Session {
                id: conn.last_insert_rowid(),
                user_id: user,
                username: name,
                created_at: at,
                last_seen_at: at,
                expires_at: expires,
                csrf_secret: csrf,
                ip,
                user_agent: ua,
                allowed_guild: guild,
            })
        })?;
        Ok((session, token))
    }

    /// Resolve a session token. Returns `None` for unknown, expired, or revoked.
    pub fn lookup_session(&self, pepper: &[u8], token: &str) -> Result<Option<Session>> {
        if token.len() > 200 {
            return Ok(None);
        }
        let mac = security::token_mac_hex(pepper, token);
        let at = now();
        self.read(move |conn| {
            conn
                .query_row(
                    "SELECT id, user_id, username, created_at, last_seen_at, expires_at, csrf_secret, ip, user_agent, allowed_guild
                     FROM sessions
                     WHERE token_mac=?1 AND revoked_at IS NULL AND expires_at > ?2",
                    params![mac, at],
                    |r| {
                        Ok(Session {
                            id: r.get(0)?,
                            user_id: Snowflake(r.get::<_, i64>(1)? as u64),
                            username: r.get(2)?,
                            created_at: r.get(3)?,
                            last_seen_at: r.get(4)?,
                            expires_at: r.get(5)?,
                            csrf_secret: r.get(6)?,
                            ip: r.get(7)?,
                            user_agent: r.get(8)?,
                            allowed_guild: r.get::<_, Option<i64>>(9)?.map(|v| Snowflake(v as u64)),
                        })
                    },
                )
                .optional_ok()
        })
    }

    /// Refresh a session's activity clock. Sliding expiry: a session only stretches
    /// once less than half of its lifetime remains, so an actively-used dashboard
    /// never expires mid-task while a stolen idle one still dies on schedule.
    /// Returns whether the expiry was extended - callers do not need to know.
    pub fn touch_session(&self, id: i64, extend_hours: i64) -> Result<bool> {
        let at = now();
        let span = extend_hours.clamp(1, 24 * 14) * 3600;
        self.tx(move |conn| {
            let before: Option<i64> = conn
                .query_row("SELECT expires_at FROM sessions WHERE id=?1 AND revoked_at IS NULL", params![id], |r| r.get(0))
                .optional_ok()?;
            let Some(before) = before else {
                return Ok(false);
            };
            conn.execute(
                "UPDATE sessions
                 SET last_seen_at = ?2,
                     expires_at = CASE WHEN expires_at - ?2 < ?3 / 2 THEN ?2 + ?3 ELSE expires_at END
                 WHERE id = ?1 AND revoked_at IS NULL",
                params![id, at, span],
            )?;
            let after: i64 = conn
                .query_row("SELECT expires_at FROM sessions WHERE id=?1", params![id], |r| r.get(0))
                .unwrap_or(before);
            Ok(after != before)
        })
    }

    pub fn revoke_session(&self, id: i64) -> Result<()> {
        let at = now();
        self.tx(move |conn| {
            conn.execute(
                "UPDATE sessions SET revoked_at=?2 WHERE id=?1",
                params![id, at],
            )?;
            Ok(())
        })
    }

    /// Sign the user out everywhere - the right response to "someone had my laptop".
    pub fn revoke_all_sessions(&self, user: Snowflake, except: Option<i64>) -> Result<u64> {
        let uid = user.as_i64();
        let at = now();
        self.tx(move |conn| {
            let n = conn.execute(
                "UPDATE sessions SET revoked_at=?2 WHERE user_id=?1 AND revoked_at IS NULL AND (?3 IS NULL OR id <> ?3)",
                params![uid, at, except],
            )?;
            Ok(n as u64)
        })
    }

    pub fn list_sessions(&self, user: Snowflake) -> Result<Vec<Session>> {
        let uid = user.as_i64();
        let at = now();
        self.read(move |conn| {
            let mut out = Vec::new();
            let mut st = conn
                .prepare(
                    "SELECT id, user_id, username, created_at, last_seen_at, expires_at, csrf_secret, ip, user_agent, allowed_guild
                     FROM sessions WHERE user_id=?1 AND revoked_at IS NULL AND expires_at > ?2 ORDER BY created_at DESC",
                )
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![uid, at], |r| {
                    Ok(Session {
                        id: r.get(0)?,
                        user_id: Snowflake(r.get::<_, i64>(1)? as u64),
                        username: r.get(2)?,
                        created_at: r.get(3)?,
                        last_seen_at: r.get(4)?,
                        expires_at: r.get(5)?,
                        csrf_secret: r.get(6)?,
                        ip: r.get(7)?,
                        user_agent: r.get(8)?,
                        allowed_guild: r.get::<_, Option<i64>>(9)?.map(|v| Snowflake(v as u64)),
                    })
                })
                .map_err(Error::db)?;
            for row in rows {
                out.push(row.map_err(Error::db)?);
            }
            Ok(out)
        })
    }

    /// Drop expired/revoked sessions. Returns rows deleted.
    pub fn purge_sessions(&self) -> Result<u64> {
        let at = now();
        self.tx(move |conn| {
            Ok(conn.execute(
                "DELETE FROM sessions WHERE expires_at < ?1 OR revoked_at IS NOT NULL",
                params![at],
            )? as u64)
        })
    }

    // -- OAuth state / PKCE --------------------------------------------------

    /// One-shot CSRF+PKCE state. 10 minutes is deliberately short: the state only
    /// has to survive the redirect round-trip to Discord.
    pub fn new_oauth_state(
        &self,
        verifier: &str,
        ip: &str,
        return_to: Option<&str>,
        redirect_uri: Option<&str>,
    ) -> Result<(String, String)> {
        // 32 bytes, so the value in the address bar is 43 url-safe characters - the length a
        // working authorize link carries. It is a database key and a CSRF token at once, and
        // `state` is the one thing in the URL a third party could otherwise try to guess.
        let state = security::random_urlsafe(32);
        let nonce = security::random_urlsafe(16);
        let at = now();
        let expires = at + 600;
        let ip = security::normalize_ip(ip);
        let ret = return_to.map(|v| security::clean(v, 240));
        let redirect = redirect_uri.map(|v| security::clean(v, 512));
        let redirect_row = redirect.clone();
        // Cloned *before* the closure: a `move` closure captures every name it
        // mentions, even ones it only calls `.clone()` on.
        let (state_row, nonce_row) = (state.clone(), nonce.clone());
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO oauth_state(state,nonce,verifier,created_at,expires_at,ip,return_to,redirect_uri) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(state) DO UPDATE SET nonce=excluded.nonce, verifier=excluded.verifier,
                   created_at=excluded.created_at, expires_at=excluded.expires_at, ip=excluded.ip,
                   return_to=excluded.return_to, redirect_uri=excluded.redirect_uri, consumed_at=NULL",
                params![state_row, nonce_row, verifier, at, expires, ip, ret, redirect_row],
            )?;
            Ok(())
        })?;
        Ok((state, nonce))
    }

    /// Consume a state, returning its PKCE verifier and the `redirect_uri` that was sent
    /// for it. A state can be spent once - re-delivery (refresh, retry, double callback) is
    /// a hard failure.
    pub fn consume_oauth_state(&self, state: &str, ip: Option<&str>) -> Result<OAuthState> {
        let state = security::clean(state, 128);
        let at = now();
        // Everything is decided inside one transaction from the *stored* row, so a
        // second delivery of the same code can never succeed: the `consumed_at`
        // stamp written below is the only record of the first use.
        /// The stored row, named rather than a five-slot tuple: the fifth field is the one
        /// people get wrong, and a tuple lets you swap it with `consumed_at` silently.
        struct Issued {
            verifier: String,
            ip: String,
            expires_at: i64,
            consumed_at: Option<i64>,
            redirect_uri: Option<String>,
            return_to: String,
        }
        self.tx(move |conn| {
            let row: Option<Issued> = conn
                .query_row(
                    "SELECT verifier, ip, expires_at, consumed_at, redirect_uri, COALESCE(return_to, '') FROM oauth_state WHERE state=?1",
                    params![state.clone()],
                    |r| {
                        Ok(Issued {
                            verifier: r.get(0)?,
                            ip: r.get(1)?,
                            expires_at: r.get(2)?,
                            consumed_at: r.get(3)?,
                            redirect_uri: r.get(4)?,
                            return_to: r.get::<_, String>(5)?,
                        })
                    },
                )
                .optional_ok()?;
            let issued = match row {
                None => return Err(Error::NotFound("oauth state")),
                Some(v) => v,
            };
            if issued.consumed_at.is_some() || issued.expires_at <= at {
                // Tombstone instead of delete: the DELETE would be rolled back by the
                // `Err` we are about to return, so "used" can only be recorded by
                // expiring the row - which also makes the next purge pass collect it.
                // A stolen-and-redeemed state therefore cannot be tried a third time.
                conn.execute(
                    "UPDATE oauth_state SET expires_at = 1 WHERE state=?1",
                    params![state],
                )?;
                return Err(Error::Invalid(
                    "oauth state was already used or has expired".into(),
                ));
            }
            if let Some(ip) = ip.map(security::normalize_ip) {
                if !ip.is_empty() && !security::ct_eq(&ip, &issued.ip) {
                    return Err(Error::Forbidden(
                        "oauth state was issued to a different client",
                    ));
                }
            }
            conn.execute(
                "UPDATE oauth_state SET consumed_at=?2 WHERE state=?1",
                params![state, at],
            )?;
            Ok(OAuthState {
                verifier: issued.verifier,
                redirect_uri: issued.redirect_uri,
                // Empty is "nobody asked to be sent anywhere", which the caller treats the
                // same as absent rather than as a path to redirect to.
                return_to: (!issued.return_to.is_empty()).then_some(issued.return_to),
            })
        })
    }

    pub fn purge_oauth_state(&self) -> Result<u64> {
        let at = now();
        self.tx(move |conn| {
            Ok(conn.execute("DELETE FROM oauth_state WHERE expires_at < ?1", params![at])? as u64)
        })
    }

    /// Stamp a session with the guilds its Discord account managed at sign-in. Stored as a
    /// JSON array of ids, because the alternative - a row per session per guild - would need
    /// a table, an index and a purge rule for a list that is read on every request and
    /// written once.
    /// The value has to be readable by both halves of the product: `session_allows_guild`
    /// (which only needs ids) and `GET /guilds` (which has to name a server the bot has never
    /// seen, because that is exactly when an invite link is useful). So it is a JSON array of
    /// `{id, name, icon}` rows, and id-only arrays from an earlier build still parse.
    pub fn set_session_grants(&self, session_id: i64, guilds: &serde_json::Value) -> Result<()> {
        let list = serde_json::to_string(guilds).map_err(Error::db)?;
        self.tx(move |conn| {
            Ok(conn.execute(
                "UPDATE sessions SET guild_grants=?2 WHERE id=?1",
                params![session_id, list],
            )? as u64)
        })?;
        Ok(())
    }

    /// Whether a session may act on `guild`. `NULL` (or unreadable JSON) is treated as
    /// unrestricted so a session written before v4 keeps working; an OAuth login always
    /// writes the list, so in practice only legacy rows take that path.
    pub fn session_allows_guild(&self, session_id: i64, guild: i64) -> Result<bool> {
        self.read(move |conn| {
            // Read as a `Value`, not as `Option<String>`: the column is NULL for every session
            // written before v4, and a typed `get` on a NULL column is an error, not a `None`.
            let raw = conn
                .query_row(
                    "SELECT guild_grants FROM sessions WHERE id=?1",
                    params![session_id],
                    |r| r.get::<_, rusqlite::types::Value>(0),
                )
                .optional_ok()?;
            let Some(rusqlite::types::Value::Text(raw)) = raw else {
                return Ok(true);
            };
            let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(&raw) else {
                return Ok(true);
            };
            Ok(rows.iter().any(|row| Self::grant_id(row) == Some(guild)))
        })
    }

    /// One row of `sessions.guild_grants`: a bare number, a numeric string, or an object
    /// carrying `id` alongside the display fields. Anything else is not a grant.
    fn grant_id(row: &serde_json::Value) -> Option<i64> {
        let text = |v: &serde_json::Value| match v {
            serde_json::Value::String(s) => s.parse::<i64>().ok(),
            serde_json::Value::Number(n) => n.to_string().parse::<i64>().ok(),
            _ => None,
        };
        match row {
            serde_json::Value::Object(m) => m.get("id").and_then(text),
            other => text(other),
        }
    }

    /// The grant document as stored, for whoever has to render it. `None` means the column is
    /// NULL, which every reader here treats as "no list, no filter".
    pub fn session_grants_json(&self, session_id: i64) -> Result<Option<String>> {
        self.read(move |conn| {
            let raw = conn
                .query_row(
                    "SELECT guild_grants FROM sessions WHERE id=?1",
                    params![session_id],
                    |r| r.get::<_, rusqlite::types::Value>(0),
                )
                .optional_ok()?;
            Ok(match raw {
                Some(rusqlite::types::Value::Text(v)) => Some(v),
                _ => None,
            })
        })
    }

    // -- pairing codes -------------------------------------------------------
    //
    // Not reachable from the product: the dashboard's only door is the Discord login in
    // `aerona-api/src/oauth.rs`, which stamps `sessions.guild_grants` instead. The table and
    // these functions stay for two reasons - installs out in the world have rows in it, and
    // removing a column from SQLite means rebuilding the table, which is a migration that can
    // fail halfway on somebody's moderation history. `purge_pairing_codes` still runs hourly so
    // those rows age out on their own.

    /// Issue a bootstrap code. Only the digest is stored, so reading the database
    /// is not enough to mint a session - the plaintext lives in `data/pair-code`,
    /// which is already protected by file permissions.
    pub fn issue_pairing_code(
        &self,
        pepper: &[u8],
        code: &str,
        ttl_secs: i64,
        ip: &str,
    ) -> Result<()> {
        let hash = security::token_mac_hex(pepper, code);
        let at = now();
        let ttl = ttl_secs.clamp(60, 3600);
        let ip = security::normalize_ip(ip);
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO pairing_codes(code_hash,created_at,expires_at,attempts,ip) VALUES(?1,?2,?3,0,?4)
                 -- consumed_at is deliberately NOT reset: a code that has bought a
                 -- session once must never buy another, even if the operator
                 -- re-issues the same value.
                 ON CONFLICT(code_hash) DO UPDATE SET created_at=excluded.created_at,
                   expires_at=excluded.expires_at, attempts=0, ip=excluded.ip",
                params![hash, at, at + ttl, ip],
            )?;
            Ok(())
        })
    }

    /// Redeem a code for a session. Every wrong guess increments `attempts`; past
    /// the limit the row is locked for 15 minutes and the code stops being testable.
    pub fn redeem_pairing_code(
        &self,
        pepper: &[u8],
        code: &str,
        user: Snowflake,
        username: &str,
        guild: Option<Snowflake>,
        ip: &str,
        user_agent: &str,
        max_attempts: i64,
        session_hours: i64,
    ) -> Result<(Session, String, String)> {
        let hash = security::token_mac_hex(pepper, code);
        let hash_for_claim = hash.clone();
        let at = now();
        let max_attempts = max_attempts.clamp(1, 20);
        // Attempt accounting and the consume flag happen in one transaction: the
        // row is only ever created by the operator of this install, so the code
        // doubles as the brute-force budget, and no caller can win twice.
        self.tx(move |conn| {
            let row: Option<(i64, i64, i64)> = conn
                .query_row(
                    "SELECT attempts, created_at, expires_at
                     FROM pairing_codes WHERE code_hash=?1 AND consumed_at IS NULL",
                    params![hash_for_claim],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional_ok()?;
            let (attempts, created_at, expires) = match row {
                None => {
                    // A burnt code still has a row, so "not found" here means
                    // "never issued" - indistinguishable to the caller on purpose.
                    return Err(Error::Unauthorized);
                }
                Some(v) => v,
            };
            if attempts >= max_attempts {
                // A lockout outlives the code's own expiry window: 15 minutes from
                // issuance is the whole budget, so a locked code stops being
                // testable rather than silently resetting mid-guess.
                if at - created_at < LOCKOUT_SECS {
                    let wait = (LOCKOUT_SECS - (at - created_at)).max(1);
                    return Err(Error::Invalid(format!(
                        "too many attempts; this code is locked for another {wait}s"
                    )));
                }
                conn.execute("UPDATE pairing_codes SET attempts=0 WHERE code_hash=?1", params![hash_for_claim])?;
            }
            if expires <= at {
                return Err(Error::Forbidden("this pairing code has expired"));
            }
            conn.execute("UPDATE pairing_codes SET attempts=attempts+1 WHERE code_hash=?1", params![hash_for_claim])?;
            let n = conn.execute(
                "UPDATE pairing_codes SET consumed_at=?2 WHERE code_hash=?1 AND consumed_at IS NULL",
                params![hash_for_claim, at],
            )?;
            if n == 0 {
                return Err(Error::Forbidden("this pairing code has already been used"));
            }
            Ok(())
        })?;
        // Issued only after the burn succeeds: a redemtion that failed validation
        // must not have created a session, and a code whose session failed to
        // create stays burnt (fail closed) rather than becoming reusable.
        let (session, token) =
            self.create_session(pepper, user, username, guild, ip, user_agent, session_hours)?;
        let csrf = session.csrf_secret.clone();
        Ok((session, token, csrf))
    }

    pub fn pairing_code_exists(&self, pepper: &[u8], code: &str) -> Result<bool> {
        let hash = security::token_mac_hex(pepper, code);
        self.read(move |conn| {
            Ok(conn
                .query_row("SELECT 1 FROM pairing_codes WHERE code_hash=?1 AND consumed_at IS NULL AND expires_at > ?2", params![hash, now()], |_| Ok(true))
                .unwrap_or(false))
        })
    }

    pub fn purge_pairing_codes(&self) -> Result<u64> {
        let at = now();
        self.tx(move |conn| Ok(conn.execute("DELETE FROM pairing_codes WHERE (expires_at < ?1 OR consumed_at IS NOT NULL) AND created_at < ?1", params![at])? as u64))
    }

    // -- audit log -----------------------------------------------------------

    /// Append one audit entry and return its chain hash.
    pub fn audit(&self, e: AuditEntry) -> Result<String> {
        let g = e.guild.map(|v| v.as_i64());
        let actor_type = e.actor_type.as_str().to_string();
        let actor = e.actor.map(|v| v.as_i64());
        let actor_label = security::clean(&e.actor_label, 64);
        let action = security::clean(&e.action, 64);
        let target = e.target.as_deref().map(|v| security::clean(v, 160));
        let ip = security::normalize_ip(&e.ip);
        let meta = security::clean(&compact(&e.meta), 4000);
        let at = now();
        self.tx(move |conn| {
            let prev: Option<String> = conn
                .query_row("SELECT hash FROM audit_log ORDER BY id DESC LIMIT 1", [], |r| r.get(0))
                .optional_ok()?;
            let hash = audit_hash(prev.as_deref(), g, &actor_type, actor, &action, target.as_deref(), at, &meta);
            conn.execute(
                "INSERT INTO audit_log(guild_id,at,actor_type,actor_id,actor_label,action,target,ip,meta,hash)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![g, at, actor_type, actor, actor_label, action, target, ip, meta, hash],
            )?;
            Ok(hash)
        })
    }

    /// Audit that never fails the caller: used on read paths (e.g. "someone listed
    /// API keys") where a broken audit write must not mask the result. The failure
    /// is logged loudly - silently dropping audit rows is how cover-ups start.
    pub fn audit_best_effort(&self, e: AuditEntry) {
        let action = e.action.clone();
        if let Err(err) = self.audit(e) {
            tracing::warn!(error = %err, action, "audit write failed");
        }
    }

    pub fn list_audit(
        &self,
        guild: Option<Snowflake>,
        limit: u32,
        before_id: Option<i64>,
    ) -> Result<Vec<Value>> {
        let g = guild.map(|v| v.as_i64());
        let limit = limit.clamp(1, 500) as i64;
        let before = before_id.unwrap_or(i64::MAX);
        self.read(move |conn| {
            let mut out = Vec::new();
            let mut st = conn
                .prepare(
                    "SELECT id, guild_id, at, actor_type, actor_id, actor_label, action, target, ip, meta, hash
                     FROM audit_log
                     WHERE (?1 IS NULL OR guild_id=?1) AND id < ?2
                     ORDER BY id DESC LIMIT ?3",
                )
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![g, before, limit], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "guildId": r.get::<_, Option<i64>>(1)?.map(|v| v.to_string()),
                        "at": r.get::<_, i64>(2)?,
                        "actorType": r.get::<_, String>(3)?,
                        "actorId": r.get::<_, Option<i64>>(4)?.map(|v| v.to_string()),
                        "actorLabel": r.get::<_, String>(5)?,
                        "action": r.get::<_, String>(6)?,
                        "target": r.get::<_, Option<String>>(7)?,
                        "ip": r.get::<_, String>(8)?,
                        "meta": serde_json::from_str::<Value>(&r.get::<_, String>(9)?).unwrap_or(Value::Null),
                        "hash": r.get::<_, String>(10)?,
                    }))
                })
                .map_err(Error::db)?;
            for row in rows {
                out.push(row.map_err(Error::db)?);
            }
            Ok(out)
        })
    }

    /// Recompute the whole chain over *every* hashed column - the same field set
    /// `audit` writes. A verifier that reads a subset would accept rows whose
    /// unchecked columns had been edited, which is precisely the tampering this is
    /// meant to catch. `Ok(count)` when intact.
    pub fn verify_audit_chain(&self) -> Result<u64> {
        self.read(|conn| {
            let mut st = conn
                .prepare(
                    "SELECT guild_id, actor_type, actor_id, action, target, at, meta, hash FROM audit_log ORDER BY id ASC",
                )
                .map_err(Error::db)?;
            let mut prev: Option<String> = None;
            let mut count = 0u64;
            let rows = st
                .query_map([], |r| {
                    Ok((
                        r.get::<_, Option<i64>>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?,
                    ))
                })
                .map_err(Error::db)?;
            for row in rows {
                let (g, actor_type, actor, action, target, at, meta, hash) = row.map_err(Error::db)?;
                let expected = audit_hash(prev.as_deref(), g, &actor_type, actor, &action, target.as_deref(), at, &meta);
                if !security::ct_eq(&expected, &hash) {
                    return Err(Error::Internal);
                }
                prev = Some(hash);
                count += 1;
            }
            Ok(count)
        })
    }

    // -- action outbox -------------------------------------------------------

    /// Queue an action for the bot. Returns the row id so the API can answer
    /// `202 Accepted` with something the caller can poll.
    pub fn enqueue_action(
        &self,
        guild: Snowflake,
        actor: Snowflake,
        actor_label: &str,
        action: &str,
        args: &Value,
        reference: &str,
    ) -> Result<i64> {
        let action = security::clean(action, 40);
        if action.is_empty() {
            return Err(Error::Invalid("outbox action name is required".into()));
        }
        let args = crate::security::clean(&compact(args), 8000);
        let label = security::clean(actor_label, 64);
        let reference = security::clean(reference, 40);
        let at = now();
        let (g, a) = (guild.as_i64(), actor.as_i64());
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO action_outbox(guild_id,actor_id,actor_label,action,args,created_at,reference)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![g, a, label, action, args, at, reference],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// Claim up to `limit` pending items for execution. Stale claims from a worker
    /// that died mid-flight are handed out again.
    pub fn claim_actions(&self, limit: u32) -> Result<Vec<OutboxItem>> {
        let take = limit.clamp(1, 32) as i64;
        let at = now();
        let stale = at - OUTBOX_CLAIM_TIMEOUT;
        self.tx(move |conn| {
            conn.execute(
                "UPDATE action_outbox SET status='pending', claimed_at=NULL
                 WHERE status='claimed' AND claimed_at < ?1",
                params![stale],
            )?;
            let mut ids = Vec::new();
            {
                let mut st = conn
                    .prepare("SELECT id FROM action_outbox WHERE status='pending' ORDER BY id LIMIT ?1")
                    .map_err(Error::db)?;
                let rows = st
                    .query_map(params![take], |r| r.get::<_, i64>(0))
                    .map_err(Error::db)?;
                for row in rows {
                    ids.push(row.map_err(Error::db)?);
                }
            }
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            let mut out = Vec::new();
            for id in ids {
                // `attempts` is bumped at claim time so a poison message that
                // crashes the worker still reaches `failed` instead of looping.
                let row = conn
                    .query_row(
                        "UPDATE action_outbox SET status='claimed', claimed_at=?2, attempts=attempts+1
                         WHERE id=?1 AND status='pending'
                         RETURNING id, guild_id, actor_id, actor_label, action, args, reference",
                        params![id, at],
                        |r| {
                            Ok(OutboxItem {
                                id: r.get(0)?,
                                guild_id: Snowflake(r.get::<_, i64>(1)? as u64),
                                actor_id: Snowflake(r.get::<_, i64>(2)? as u64),
                                actor_label: r.get(3)?,
                                action: r.get(4)?,
                                args: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(Value::Null),
                                reference: r.get(6)?,
                            })
                        },
                    )
                    .optional_ok()?;
                if let Some(item) = row {
                    out.push(item);
                }
            }
            Ok(out)
        })
    }

    pub fn finish_action(&self, id: i64, ok: bool, error: Option<&str>) -> Result<()> {
        let at = now();
        let err = error.map(|e| security::clean(e, 400)).unwrap_or_default();
        self.tx(move |conn| {
            if ok {
                conn.execute(
                    "UPDATE action_outbox SET status='done', done_at=?2, error='' WHERE id=?1",
                    params![id, at],
                )?;
            } else {
                // Three attempts is enough to distinguish "Discord hiccuped" from
                // "this message is gone"; beyond that a human should look.
                conn.execute(
                    "UPDATE action_outbox SET status=CASE WHEN attempts >= 3 THEN 'failed' ELSE 'pending' END,
                       claimed_at=NULL, error=?2 WHERE id=?1",
                    params![id, err],
                )?;
            }
            Ok(())
        })
    }

    pub fn outbox_status(&self, id: i64) -> Result<Value> {
        self.read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT status, error, reference, created_at, done_at, action, args
                     FROM action_outbox WHERE id=?1",
                    params![id],
                    |r| {
                        let args: String = r.get(6)?;
                        Ok(json!({
                            "id": id,
                            "status": r.get::<_, String>(0)?,
                            "error": r.get::<_, String>(1)?,
                            "reference": r.get::<_, String>(2)?,
                            "createdAt": r.get::<_, i64>(3)?,
                            "doneAt": r.get::<_, Option<i64>>(4)?,
                            // Echoed back so a caller holding only the queue id can
                            // see what it asked for, without a second query.
                            "action": r.get::<_, String>(5)?,
                            "args": serde_json::from_str(&args).unwrap_or(Value::Null),
                        }))
                    },
                )
                .optional_ok()?
                .unwrap_or(Value::Null))
        })
    }

    pub fn outbox_depth(&self) -> Result<(i64, i64)> {
        self.read(|conn| {
            let pending = conn
                .query_row(
                    "SELECT COUNT(*) FROM action_outbox WHERE status IN ('pending','claimed')",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0);
            let failed = conn
                .query_row(
                    "SELECT COUNT(*) FROM action_outbox WHERE status='failed'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0);
            Ok((pending, failed))
        })
    }

    /// Purge finished rows older than `days`. Pending/failed rows are kept -
    /// a failed action is evidence.
    pub fn purge_outbox(&self, days: i64) -> Result<u64> {
        let cutoff = now() - days.clamp(1, 365) * 86_400;
        self.tx(move |conn| {
            Ok(conn.execute("DELETE FROM action_outbox WHERE status='done' AND done_at IS NOT NULL AND done_at < ?1", params![cutoff])? as u64)
        })
    }

    // -- bot status ----------------------------------------------------------

    pub fn put_bot_status(&self, guild: Snowflake, status: &Value) -> Result<()> {
        let g = guild.as_i64();
        let at = now();
        let num = |k: &str| status.get(k).and_then(Value::as_i64).unwrap_or(0);
        let (lat, shards, ready, users, raid, tickets, queue, events) = (
            num("wsLatencyMs"),
            num("shardsTotal"),
            num("shardsReady"),
            num("usersCached"),
            num("activeRaid"),
            num("openTickets"),
            num("queueDepth"),
            num("gatewayEventsMin"),
        );
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO bot_status(guild_id,ws_latency_ms,shards_total,shards_ready,users_cached,active_raid,open_tickets,queue_depth,gateway_events_min,updated_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                 ON CONFLICT(guild_id) DO UPDATE SET ws_latency_ms=excluded.ws_latency_ms,
                   shards_total=excluded.shards_total, shards_ready=excluded.shards_ready,
                   users_cached=excluded.users_cached, active_raid=excluded.active_raid,
                   open_tickets=excluded.open_tickets, queue_depth=excluded.queue_depth,
                   gateway_events_min=excluded.gateway_events_min, updated_at=excluded.updated_at",
                params![g, lat, shards, ready, users, raid, tickets, queue, events, at],
            )?;
            Ok(())
        })
    }

    pub fn bot_statuses(&self) -> Result<Vec<Value>> {
        let at = now();
        self.read(move |conn| {
            let mut out = Vec::new();
            let mut st = conn
                .prepare(
                    "SELECT s.guild_id, g.name, s.ws_latency_ms, s.shards_total, s.shards_ready, s.users_cached,
                            s.active_raid, s.open_tickets, s.queue_depth, s.gateway_events_min, s.updated_at
                     FROM bot_status s LEFT JOIN guilds g ON g.id = s.guild_id ORDER BY s.guild_id",
                )
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![], |r| {
                    let updated: i64 = r.get(10)?;
                    Ok(json!({
                        "guildId": r.get::<_, i64>(0)?.to_string(),
                        "name": r.get::<_, Option<String>>(1)?.unwrap_or_else(|| "unknown".into()),
                        "wsLatencyMs": r.get::<_, i64>(2)?,
                        "shardsTotal": r.get::<_, i64>(3)?,
                        "shardsReady": r.get::<_, i64>(4)?,
                        "usersCached": r.get::<_, i64>(5)?,
                        "activeRaid": r.get::<_, i64>(6)? != 0,
                        "openTickets": r.get::<_, i64>(7)?,
                        "queueDepth": r.get::<_, i64>(8)?,
                        "gatewayEventsMin": r.get::<_, i64>(9)?,
                        "updatedAt": updated,
                        "stale": at - updated > 90,
                    }))
                })
                .map_err(Error::db)?;
            for row in rows {
                out.push(row.map_err(Error::db)?);
            }
            Ok(out)
        })
    }

    /// Remove status rows for guilds the bot no longer reports - called after the
    /// bot's own guild list is known, so a removed guild does not linger as
    /// "stale" forever.
    pub fn prune_bot_status(&self, present: &[Snowflake]) -> Result<u64> {
        let ids: Vec<i64> = present.iter().map(|s| s.as_i64()).collect();
        self.tx(move |conn| {
            let mut st = conn
                .prepare("SELECT guild_id FROM bot_status")
                .map_err(Error::db)?;
            let rows = st
                .query_map([], |r| r.get::<_, i64>(0))
                .map_err(Error::db)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::db)?;
            let mut n = 0;
            for id in rows {
                if !ids.contains(&id) {
                    n += conn.execute("DELETE FROM bot_status WHERE guild_id=?1", params![id])?;
                }
            }
            Ok(n as u64)
        })
    }
}

fn sanitize_scopes(scopes: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for s in scopes {
        let s = s.trim().to_ascii_lowercase();
        if s.is_empty() {
            continue;
        }
        if !SCOPES.contains(&s.as_str()) {
            return Err(Error::Invalid(format!("unknown api key scope: {s}")));
        }
        if !out.contains(&s) {
            out.push(s);
        }
    }
    if out.is_empty() {
        out.push("read".to_string());
    }
    Ok(out)
}

fn compact(v: &Value) -> String {
    // `to_string` (not `to_string_pretty`) - the audit column is compared by hash,
    // and pretty-printing would make a cosmetic change look like tampering.
    serde_json::to_string(v).unwrap_or_else(|_| "{}".into())
}

fn audit_hash(
    prev: Option<&str>,
    guild: Option<i64>,
    actor_type: &str,
    actor: Option<i64>,
    action: &str,
    target: Option<&str>,
    at: i64,
    meta: &str,
) -> String {
    security::sha256_hex(format!(
        "{}|{}|{}|{}|{}|{}|{}|{}",
        prev.unwrap_or("genesis"),
        guild.unwrap_or(0),
        actor_type,
        actor.unwrap_or(0),
        action,
        target.unwrap_or(""),
        at,
        meta
    ))
}

fn read_api_key(conn: &rusqlite::Connection, mac: &str) -> Result<Option<ApiKeyRecord>> {
    let at = now();
    Ok(conn
        .query_row(
            "SELECT id, label, hint, guild_id, scopes, created_by, created_at, last_used_at, expires_at, revoked_at, request_count
             FROM api_keys WHERE key_mac=?1",
            params![mac],
            |r| {
                Ok((
                    ApiKeyRecord {
                        id: r.get(0)?,
                        label: r.get(1)?,
                        hint: r.get(2)?,
                        guild_id: r.get::<_, Option<i64>>(3)?.map(|v| Snowflake(v as u64)),
                        scopes: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
                        created_by: Snowflake(r.get::<_, i64>(5)? as u64),
                        created_at: r.get(6)?,
                        last_used_at: r.get(7)?,
                        expires_at: r.get(8)?,
                        revoked_at: r.get(9)?,
                        request_count: r.get::<_, i64>(10).unwrap_or(0),
                    },
                    r.get::<_, Option<i64>>(9)?,
                    r.get::<_, i64>(8)?,
                ))
            },
        )
        .optional_ok()?
        .and_then(|(rec, revoked, expires)| {
            // One place decides "is this key live", so a revoked or expired row can
            // never be authenticated by some future caller that forgets to check.
            if revoked.is_some() {
                return None;
            }
            if expires > 0 && expires <= at {
                return None;
            }
            Some(rec)
        }))
}

/// `query_row` yields `Err(QueryReturnedNoRows)`; "not found" is the normal case
/// here, so fold it to `None` while every other error still propagates.
trait OptionalOk {
    fn optional_ok(self) -> Result<Option<Self::T>>;
    type T;
}

impl<T> OptionalOk for rusqlite::Result<T> {
    type T = T;
    fn optional_ok(self) -> Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(Error::db(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::testutil::temp_db;

    fn pepper() -> [u8; 32] {
        security::derive_pepper("test-pepper")
    }
    const G: u64 = 810;
    const U: u64 = 42;

    fn seed(db: &Db) {
        db.tx(|c| {
            c.execute(
                "INSERT INTO guilds(id,name,updated_at) VALUES(810,'g',0)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_session_without_a_grant_list_is_unrestricted_and_one_with_a_list_is_not() {
        // The two halves of the rule that replaced the pairing code: a session written before
        // v4 (NULL) keeps working, and a session stamped at sign-in answers only for its list.
        let (db, _d) = temp_db();
        let p = pepper();
        let (legacy, _) = db
            .create_session(&p, Snowflake(U), "legacy", None, "", "", 12)
            .unwrap();
        assert!(db.session_allows_guild(legacy.id, G as i64).unwrap());
        assert!(
            db.session_allows_guild(legacy.id, 999).unwrap(),
            "NULL is unrestricted, which is the whole point of the v4 default"
        );

        let (scoped, _) = db
            .create_session(&p, Snowflake(U), "ops", None, "", "", 12)
            .unwrap();
        db.set_session_grants(
            scoped.id,
            &serde_json::json!([{ "id": G.to_string(), "name": "Ops", "icon": null }]),
        )
        .unwrap();
        assert!(db.session_allows_guild(scoped.id, G as i64).unwrap());
        assert!(
            !db.session_allows_guild(scoped.id, 999).unwrap(),
            "a granted list is the whole world for this session"
        );

        // An empty list is a real answer, not a missing one: it denies everything.
        let (none, _) = db
            .create_session(&p, Snowflake(U), "nobody", None, "", "", 12)
            .unwrap();
        db.set_session_grants(none.id, &serde_json::json!([]))
            .unwrap();
        assert!(!db.session_allows_guild(none.id, G as i64).unwrap());

        // A session id that is not in the table behaves like the unrestricted default; the
        // caller has already authenticated the token, so this row cannot be reached anyway.
        assert!(db.session_allows_guild(legacy.id + 500, G as i64).unwrap());
    }

    #[test]
    fn api_key_roundtrip_and_rotation() {
        let (db, _d) = temp_db();
        let p = pepper();
        let (id, key, hint) = db
            .create_api_key(
                &p,
                "CI",
                Some(Snowflake(G)),
                &["read".into(), "moderate".into()],
                Snowflake(U),
                30,
            )
            .unwrap();
        assert!(key.starts_with(security::KEY_PREFIX));
        assert_eq!(hint.chars().count(), security::KEY_PREFIX.len() + 8);
        let rec = db.lookup_api_key(&p, &key).unwrap().expect("valid key");
        assert_eq!(rec.id, id);
        assert_eq!(rec.scopes, vec!["read", "moderate"]);
        assert_eq!(rec.guild_id, Some(Snowflake(G)));
        // A different pepper (i.e. a rotated AERONA_SECRET) invalidates all keys.
        assert!(db
            .lookup_api_key(&security::derive_pepper("other"), &key)
            .unwrap()
            .is_none());
        // Truncated / altered keys must not match.
        let mut bad = key.clone();
        bad.pop();
        assert!(db.lookup_api_key(&p, &bad).unwrap().is_none());
        assert!(db.lookup_api_key(&p, "nope").unwrap().is_none());

        db.touch_api_key(id, true).unwrap();
        db.touch_api_key(id, true).unwrap();
        let listed = db.list_api_keys(Some(Snowflake(G))).unwrap();
        assert_eq!(listed[0].request_count, 2);
        assert!(listed[0].last_used_at.is_some());

        db.revoke_api_key(id, Some(Snowflake(G))).unwrap();
        assert!(db.lookup_api_key(&p, &key).unwrap().is_none());
        // Revoking twice is a 404, not a silent success.
        assert!(matches!(
            db.revoke_api_key(id, Some(Snowflake(G))),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn api_key_scopes_are_validated_and_deduped() {
        let (db, _d) = temp_db();
        let p = pepper();
        let (_, _, _) = db
            .create_api_key(
                &p,
                "l",
                None,
                &["read".into(), "read".into()],
                Snowflake(U),
                0,
            )
            .unwrap();
        let listed = db.list_api_keys(None).unwrap();
        assert_eq!(listed[0].scopes, vec!["read"]);
        assert_eq!(listed[0].guild_id, None, "a global key has no guild");
        assert!(
            !db.list_api_keys(Some(Snowflake(999))).unwrap().is_empty(),
            "global keys list everywhere"
        );
        assert!(matches!(
            db.create_api_key(&p, "l", None, &["sudo".into()], Snowflake(U), 0),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            db.create_api_key(&p, "   ", None, &[], Snowflake(U), 0),
            Err(Error::Invalid(_))
        ));
        // Empty scope list falls back to read, never to "everything".
        let (id, key, _) = db
            .create_api_key(&p, "dflt", None, &[], Snowflake(U), 0)
            .unwrap();
        assert_eq!(
            db.lookup_api_key(&p, &key).unwrap().unwrap().scopes,
            vec!["read"]
        );
        db.revoke_api_key(id, None).unwrap();
    }

    #[test]
    fn expired_api_key_is_refused() {
        let (db, _d) = temp_db();
        let p = pepper();
        let (_, key, _) = db
            .create_api_key(&p, "short", None, &["read".into()], Snowflake(U), 1)
            .unwrap();
        db.tx(|c| {
            c.execute("UPDATE api_keys SET expires_at = 100", [])?;
            Ok(())
        })
        .unwrap();
        assert!(db.lookup_api_key(&p, &key).unwrap().is_none());
        // expires_at = 0 means "never", which is what a ttl-less key must store.
        db.tx(|c| {
            c.execute("UPDATE api_keys SET expires_at = 0", [])?;
            Ok(())
        })
        .unwrap();
        assert!(db.lookup_api_key(&p, &key).unwrap().is_some());
    }

    #[test]
    fn session_lifecycle() {
        let (db, _d) = temp_db();
        seed(&db);
        let p = pepper();
        let (s, token) = db
            .create_session(
                &p,
                Snowflake(U),
                "ops",
                Some(Snowflake(G)),
                "203.0.113.7:9999",
                "Mozilla/5.0",
                12,
            )
            .unwrap();
        assert!(!s.csrf_secret.is_empty());
        let got = db
            .lookup_session(&p, &token)
            .unwrap()
            .expect("live session");
        assert_eq!(got.id, s.id);
        assert_eq!(got.ip, "203.0.113.7", "port stripped on write");
        // Unknown and wrong-pepper tokens fail identically.
        assert!(db.lookup_session(&p, "junk").unwrap().is_none());
        assert!(db
            .lookup_session(&security::derive_pepper("x"), &token)
            .unwrap()
            .is_none());
        // A touch on a session with plenty of life left refreshes the clock but
        // does not stretch the expiry (that is what makes this safe to call per request).
        let before = db.lookup_session(&p, &token).unwrap().unwrap();
        assert!(
            !db.touch_session(s.id, 12).unwrap(),
            "expiry should not move yet"
        );
        let after = db.lookup_session(&p, &token).unwrap().unwrap();
        assert_eq!(after.expires_at, before.expires_at);
        assert!(after.last_seen_at >= before.last_seen_at);
        // Once the window is nearly gone, the same touch slides it forward.
        db.tx(|c| {
            c.execute("UPDATE sessions SET expires_at = ?1", params![now() + 60])?;
            Ok(())
        })
        .unwrap();
        assert!(
            db.touch_session(s.id, 12).unwrap(),
            "a nearly-expired session must be extended"
        );
        assert!(db.lookup_session(&p, &token).unwrap().is_some());
        // A revoked session cannot be touched back to life.
        db.revoke_session(s.id).unwrap();
        assert!(!db.touch_session(s.id, 12).unwrap());
        // Revocation is immediate.
        db.revoke_session(s.id).unwrap();
        assert!(db.lookup_session(&p, &token).unwrap().is_none());
        assert_eq!(db.purge_sessions().unwrap(), 1);
    }

    #[test]
    fn revoke_everywhere_keeps_the_current_session() {
        let (db, _d) = temp_db();
        seed(&db);
        let p = pepper();
        let (a, _) = db
            .create_session(
                &p,
                Snowflake(U),
                "ops",
                Some(Snowflake(G)),
                "10.0.0.1",
                "ua",
                12,
            )
            .unwrap();
        let (b, tb) = db
            .create_session(
                &p,
                Snowflake(U),
                "ops",
                Some(Snowflake(G)),
                "10.0.0.2",
                "ua",
                12,
            )
            .unwrap();
        let (other, _) = db
            .create_session(
                &p,
                Snowflake(U + 1),
                "peer",
                Some(Snowflake(G)),
                "10.0.0.3",
                "ua",
                12,
            )
            .unwrap();
        assert_eq!(db.list_sessions(Snowflake(U)).unwrap().len(), 2);
        assert_eq!(db.revoke_all_sessions(Snowflake(U), Some(b.id)).unwrap(), 1);
        assert!(
            db.lookup_session(&p, &tb).unwrap().is_some(),
            "the caller's own session survives"
        );
        assert!(db
            .list_sessions(Snowflake(U))
            .unwrap()
            .iter()
            .all(|s| s.id != a.id));
        assert_eq!(
            db.list_sessions(Snowflake(U + 1)).unwrap().len(),
            1,
            "other users untouched"
        );
        assert_eq!(db.revoke_all_sessions(other.user_id, None).unwrap(), 1);
    }

    #[test]
    fn a_session_without_a_guild_is_install_wide() {
        let (db, _d) = temp_db();
        let p = pepper();
        let (s, _) = db
            .create_session(&p, Snowflake(U), "ops", None, "10.0.0.1", "ua", 12)
            .unwrap();
        assert_eq!(
            s.allowed_guild, None,
            "the oauth path must not be guild-bound"
        );
        let (bound, _) = db
            .create_session(
                &p,
                Snowflake(U),
                "ops",
                Some(Snowflake(1234)),
                "10.0.0.1",
                "ua",
                12,
            )
            .unwrap();
        assert_eq!(bound.allowed_guild, Some(Snowflake(1234)));
        // The bootstrap guild row is created for a bound session, never for a
        // guild-less one - there is nothing to name.
        assert_eq!(db.row_count("guilds").unwrap(), 1);
    }

    #[test]
    fn expired_session_is_not_resolvable() {
        let (db, _d) = temp_db();
        seed(&db);
        let p = pepper();
        let (_, token) = db
            .create_session(
                &p,
                Snowflake(U),
                "ops",
                Some(Snowflake(G)),
                "1.2.3.4",
                "ua",
                1,
            )
            .unwrap();
        db.tx(|c| {
            c.execute("UPDATE sessions SET expires_at = ?1", params![now() - 1])?;
            Ok(())
        })
        .unwrap();
        assert!(db.lookup_session(&p, &token).unwrap().is_none());
    }

    #[test]
    fn oauth_state_is_single_use_and_replays_the_redirect_uri() {
        let (db, _d) = temp_db();
        let (state, _nonce) = db
            .new_oauth_state(
                "verifier-xyz",
                "9.9.9.9",
                Some("/guild/810/settings"),
                Some("http://panel.example:25154/api/v1/auth/discord/callback"),
            )
            .unwrap();
        let got = db.consume_oauth_state(&state, Some("9.9.9.9")).unwrap();
        assert_eq!(got.verifier, "verifier-xyz");
        // The uri that was offered to Discord at /authorize is the one that must be resent to
        // /token; anything recomputed at callback time can disagree with it after a proxy hop.
        assert_eq!(
            got.redirect_uri.as_deref(),
            Some("http://panel.example:25154/api/v1/auth/discord/callback")
        );
        assert!(
            matches!(
                db.consume_oauth_state(&state, Some("9.9.9.9")),
                Err(Error::Invalid(_))
            ),
            "second use must fail"
        );
        assert!(matches!(
            db.consume_oauth_state("nonexistent", None),
            Err(Error::NotFound(_))
        ));
        let (state2, _) = db.new_oauth_state("v2", "8.8.8.8", None, None).unwrap();
        assert!(matches!(
            db.consume_oauth_state(&state2, Some("7.7.7.7")),
            Err(Error::Forbidden(_))
        ));
        // The state survives a wrong-IP attempt so the error is about the IP, not a phantom "not found".
        let got2 = db.consume_oauth_state(&state2, Some("8.8.8.8")).unwrap();
        assert_eq!(got2.verifier, "v2");
        assert_eq!(
            got2.redirect_uri, None,
            "an install with nothing pinned must not have a uri invented here"
        );
        let (state3, _) = db.new_oauth_state("v3", "1.1.1.1", None, None).unwrap();
        db.tx(|c| {
            c.execute(
                "UPDATE oauth_state SET expires_at = 1 WHERE state=?1",
                params![state3],
            )?;
            Ok(())
        })
        .unwrap();
        assert!(
            matches!(
                db.consume_oauth_state(&state3, None),
                Err(Error::Invalid(_))
            ),
            "expired is refused"
        );
        // Reuse and expiry both tombstone the row (`expires_at = 1`) rather than
        // deleting it, because the `Err` return rolls the transaction back. So the
        // third call fails the same way, and the purge pass is what collects it.
        assert!(
            matches!(
                db.consume_oauth_state(&state3, None),
                Err(Error::Invalid(_))
            ),
            "a tombstoned state stays unusable"
        );
        assert_eq!(
            db.row_count("oauth_state").unwrap(),
            3,
            "two spent, one tombstoned, none deleted"
        );
        // A spent state lingers until its own TTL runs out (nothing to gain from
        // deleting it early - the burn flag is the guard), and then one pass sweeps both.
        assert_eq!(
            db.purge_oauth_state().unwrap(),
            1,
            "only the tombstoned row is collectable yet"
        );
        assert_eq!(
            db.row_count("oauth_state").unwrap(),
            2,
            "both spent rows remain until their TTL"
        );
        db.tx(|c| {
            c.execute("UPDATE oauth_state SET expires_at = 1", [])?;
            Ok(())
        })
        .unwrap();
        assert_eq!(db.purge_oauth_state().unwrap(), 2);
        assert_eq!(db.row_count("oauth_state").unwrap(), 0);
    }

    #[test]
    fn pairing_code_is_burned_on_use_and_locks_after_guessing() {
        let (db, _d) = temp_db();
        seed(&db);
        let p = pepper();
        db.issue_pairing_code(&p, "123456", 600, "5.6.7.8").unwrap();
        assert!(db.pairing_code_exists(&p, "123456").unwrap());
        assert!(!db.pairing_code_exists(&p, "654321").unwrap());
        let (out, out_token, out_csrf) = db
            .redeem_pairing_code(
                &p,
                "123456",
                Snowflake(U),
                "owner",
                Some(Snowflake(G)),
                "5.6.7.8",
                "curl",
                5,
                12,
            )
            .unwrap();
        assert_eq!(out.username, "owner");
        assert_eq!(
            out.allowed_guild,
            Some(Snowflake(G)),
            "pairing binds the session to the operator's guild"
        );
        assert_eq!(
            out_csrf, out.csrf_secret,
            "the csrf the caller must echo is the stored one"
        );
        assert!(db.lookup_session(&p, &out_token).unwrap().is_some());
        assert!(!db.pairing_code_exists(&p, "123456").unwrap(), "spent");
        assert!(matches!(
            db.redeem_pairing_code(
                &p,
                "123456",
                Snowflake(U),
                "owner",
                Some(Snowflake(G)),
                "5.6.7.8",
                "curl",
                5,
                12
            ),
            Err(Error::Unauthorized)
        ));

        // Guessing: 5 attempts then lockout, and the lockout must not consume the code.
        db.issue_pairing_code(&p, "654321", 600, "").unwrap();
        for _ in 0..5 {
            let r = db.redeem_pairing_code(&p, "000000", Snowflake(U), "x", None, "", "", 5, 12);
            assert!(
                matches!(r, Err(Error::Unauthorized)),
                "wrong code is unauthorized, got {r:?}"
            );
        }
        assert!(
            db.pairing_code_exists(&p, "654321").unwrap(),
            "a wrong guess never burns the code"
        );
    }

    #[test]
    fn a_redeemed_code_cannot_be_redeemed_again_even_after_reissue() {
        let (db, _d) = temp_db();
        seed(&db);
        let p = pepper();
        let code = "111111";
        db.issue_pairing_code(&p, code, 600, "").unwrap();
        assert!(db
            .redeem_pairing_code(
                &p,
                code,
                Snowflake(U),
                "x",
                Some(Snowflake(G)),
                "",
                "",
                5,
                12
            )
            .is_ok());
        // Re-issuing the same code (operator pastes it twice, or the file is
        // regenerated with the same value) must not resurrect a burnt row.
        db.issue_pairing_code(&p, code, 600, "").unwrap();
        // Re-issuing the same code resets its TTL and attempt counter but never the
        // burn flag, and the caller learns only "unknown code" - whether a code was
        // already spent is not information worth leaking.
        assert!(matches!(
            db.redeem_pairing_code(
                &p,
                code,
                Snowflake(U),
                "x",
                Some(Snowflake(G)),
                "",
                "",
                5,
                12
            ),
            Err(Error::Unauthorized)
        ));
    }

    #[test]
    fn a_fresh_code_buys_exactly_one_session() {
        let (db, _d) = temp_db();
        seed(&db);
        let p = pepper();
        db.issue_pairing_code(&p, "333333", 600, "").unwrap();
        let (s1, t1, _) = db
            .redeem_pairing_code(
                &p,
                "333333",
                Snowflake(U),
                "x",
                Some(Snowflake(G)),
                "",
                "",
                5,
                12,
            )
            .unwrap();
        let second = db.redeem_pairing_code(
            &p,
            "333333",
            Snowflake(U),
            "x",
            Some(Snowflake(G)),
            "",
            "",
            5,
            12,
        );
        assert!(second.is_err());
        assert_eq!(
            db.list_sessions(Snowflake(U)).unwrap().len(),
            1,
            "the failed redemption created no session"
        );
        assert!(db.lookup_session(&p, &t1).unwrap().is_some());
        assert_eq!(s1.user_id, Snowflake(U));
    }

    #[test]
    fn expired_pairing_code_is_rejected() {
        let (db, _d) = temp_db();
        seed(&db);
        let p = pepper();
        db.issue_pairing_code(&p, "222222", 60, "").unwrap();
        db.tx(|c| {
            c.execute("UPDATE pairing_codes SET expires_at = 1", [])?;
            Ok(())
        })
        .unwrap();
        assert!(matches!(
            db.redeem_pairing_code(&p, "222222", Snowflake(U), "x", None, "", "", 5, 12),
            Err(Error::Forbidden(_))
        ));
    }

    #[test]
    fn audit_chain_detects_tampering() {
        let (db, _d) = temp_db();
        seed(&db);
        for i in 0..5 {
            db.audit(
                AuditEntry::new("warn.create")
                    .for_guild(Snowflake(G))
                    .user(Snowflake(U), "ops")
                    .target(format!("user:{i}"))
                    .ip("1.2.3.4")
                    .meta(json!({"reason": "spam", "n": i})),
            )
            .unwrap();
        }
        let rows = db.list_audit(Some(Snowflake(G)), 10, None).unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0]["action"], "warn.create", "newest first");
        assert_eq!(rows[0]["meta"]["n"], 4);
        assert_eq!(db.verify_audit_chain().unwrap(), 5);
        // Editing one row's payload breaks every hash from that point on.
        db.tx(|c| {
            c.execute("UPDATE audit_log SET meta = '{\"reason\":\"tampered\"}' WHERE id = (SELECT MIN(id) FROM audit_log)", [])?;
            Ok(())
        })
        .unwrap();
        assert!(
            db.verify_audit_chain().is_err(),
            "tampered audit log must be detected"
        );
        // Pagination walks backwards by id.
        let page1 = db.list_audit(Some(Snowflake(G)), 2, None).unwrap();
        let cursor = page1[1]["id"].as_i64().unwrap();
        let page2 = db.list_audit(Some(Snowflake(G)), 2, Some(cursor)).unwrap();
        assert_eq!(page2.len(), 2);
        assert!(page2[0]["id"].as_i64().unwrap() < cursor);
        // A guild cannot read another guild's audit rows.
        assert!(db
            .list_audit(Some(Snowflake(999)), 10, None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn outbox_claim_retry_and_purge() {
        let (db, _d) = temp_db();
        seed(&db);
        let id = db
            .enqueue_action(
                Snowflake(G),
                Snowflake(U),
                "ops",
                "message.delete",
                &json!({"channel": "1", "message": "2"}),
                "AER-1",
            )
            .unwrap();
        assert_eq!(db.outbox_status(id).unwrap()["status"], "pending");
        let claimed = db.claim_actions(5).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].action, "message.delete");
        assert_eq!(claimed[0].args["message"], "2");
        assert_eq!(claimed[0].reference, "AER-1");
        // A claimed item is invisible to a second claim while the claim is fresh,
        // and `finish_action(ok=false)` hands it back for a retry.
        assert!(
            db.claim_actions(5).unwrap().is_empty(),
            "double claim must be refused"
        );
        db.finish_action(id, false, Some(&"x".repeat(9000)))
            .unwrap();
        assert_eq!(
            db.outbox_status(id).unwrap()["status"],
            "pending",
            "attempt 1 retries"
        );
        assert_eq!(
            db.outbox_status(id).unwrap()["error"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            400,
            "error text is capped"
        );

        // Retry until attempts reach 3 - then it is parked in `failed` for a human
        // instead of spinning against a permanently failing Discord call.
        for expected in ["pending", "failed"] {
            let items = db.claim_actions(5).unwrap();
            assert_eq!(items.len(), 1, "the released row must be claimable again");
            db.finish_action(items[0].id, false, Some("nope")).unwrap();
            assert_eq!(db.outbox_status(id).unwrap()["status"], expected);
        }
        assert_eq!(db.outbox_depth().unwrap(), (0, 1), "failed is not pending");
        // A failed row is not handed out again even by a fresh claim pass.
        assert!(
            db.claim_actions(5).unwrap().is_empty(),
            "failed rows wait for a human"
        );

        // Success path, on a second item.
        let other = db
            .enqueue_action(Snowflake(G), Snowflake(U), "ops", "ban", &json!({}), "")
            .unwrap();
        assert_eq!(db.outbox_depth().unwrap(), (1, 1));
        assert_eq!(db.claim_actions(5).unwrap()[0].id, other);
        db.finish_action(other, true, None).unwrap();
        assert_eq!(db.outbox_status(other).unwrap()["status"], "done");
        assert_eq!(db.outbox_status(other).unwrap()["error"], "");
        // A crashed worker's claim is reclaimed after the timeout.
        let third = db
            .enqueue_action(Snowflake(G), Snowflake(U), "ops", "kick", &json!({}), "")
            .unwrap();
        assert_eq!(db.claim_actions(5).unwrap()[0].id, third);
        db.tx(|c| {
            c.execute(
                "UPDATE action_outbox SET claimed_at = ?1 WHERE id=?2",
                params![now() - OUTBOX_CLAIM_TIMEOUT - 5, third],
            )?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            db.claim_actions(5).unwrap()[0].id,
            third,
            "stale claim is handed out again"
        );
        db.finish_action(third, true, None).unwrap();
        assert_eq!(db.outbox_status(third).unwrap()["status"], "done");
        assert_eq!(db.purge_outbox(1).unwrap(), 0, "a fresh done row is kept");
        db.tx(|c| {
            c.execute(
                "UPDATE action_outbox SET done_at = 1 WHERE id=?1",
                params![third],
            )?;
            Ok(())
        })
        .unwrap();
        assert_eq!(db.purge_outbox(1).unwrap(), 1);
        assert_eq!(
            db.outbox_status(id).unwrap()["status"],
            "failed",
            "failed rows survive purge"
        );
    }

    #[test]
    fn outbox_rejects_an_empty_action_name() {
        let (db, _d) = temp_db();
        assert!(matches!(
            db.enqueue_action(Snowflake(G), Snowflake(U), "ops", "", &json!({}), ""),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn bot_status_is_upserted_and_marked_stale() {
        let (db, _d) = temp_db();
        seed(&db);
        assert!(db.bot_statuses().unwrap().is_empty());
        let s = json!({"wsLatencyMs": 41, "shardsTotal": 2, "shardsReady": 2, "usersCached": 1234, "openTickets": 3, "queueDepth": 0});
        db.put_bot_status(Snowflake(G), &s).unwrap();
        db.put_bot_status(Snowflake(G), &s).unwrap(); // upsert, not duplicate
        let rows = db.bot_statuses().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["wsLatencyMs"], 41);
        assert_eq!(
            rows[0]["name"], "g",
            "joined with guilds for the dashboard label"
        );
        assert_eq!(rows[0]["stale"], false);
        // Non-numeric and missing fields read as 0 rather than failing.
        db.put_bot_status(
            Snowflake(G),
            &json!({"wsLatencyMs": "nope", "usersCached": null}),
        )
        .unwrap();
        let rows = db.bot_statuses().unwrap();
        assert_eq!(rows[0]["wsLatencyMs"], 0);
        assert_eq!(rows[0]["usersCached"], 0);
        // Staleness flips after 90 seconds of silence, so the dashboard can say
        // "bot not reporting" instead of showing frozen numbers.
        db.tx(|c| {
            c.execute(
                "UPDATE bot_status SET updated_at = ?1",
                params![now() - 120],
            )?;
            Ok(())
        })
        .unwrap();
        assert_eq!(db.bot_statuses().unwrap()[0]["stale"], true);
        assert_eq!(db.prune_bot_status(&[Snowflake(12345)]).unwrap(), 1);
        assert!(db.bot_statuses().unwrap().is_empty());
    }

    #[test]
    fn install_id_is_stable_and_seeded() {
        let (db, _d) = temp_db();
        let a = db.install_id().unwrap();
        assert!(!a.is_empty());
        assert_eq!(db.install_id().unwrap(), a, "stable across calls");
    }
}

/// What a redeemed OAuth state hands back: the PKCE verifier, and the exact `redirect_uri`
/// that was sent for it, so the token exchange can repeat it byte for byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthState {
    pub verifier: String,
    pub redirect_uri: Option<String>,
    /// Where the browser wanted to go before it was bounced to Discord. `None` is the
    /// common case and means the dashboard's own root; a value here is already sanitised
    /// (`crate::oauth::safe_next`) - it is what a sign-in that started on
    /// `/guild/123/tickets/` lands back on.
    pub return_to: Option<String>,
}
