//! Version 1 schema. Keep this file append-only in spirit: never edit a shipped
//! migration, always add the next one.

/// Every table, index and constraint Aerona needs, in one batch.
///
/// Design notes for reviewers:
///
/// * All ids are `INTEGER` (Discord snowflakes fit in i64) — 8 bytes, indexable,
///   no string comparison.
/// * All timestamps are epoch seconds.
/// * Lists of ids live in JSON columns; *records* never do.
/// * Retention-bounded tables (`audit_log`, `log_messages`, `ai_verdict_cache`,
///   `stats`, `raid_events`) are pruned by the housekeeping job.
pub const SCHEMA_V1: &str = r#"
CREATE TABLE guilds (
  id          INTEGER PRIMARY KEY,
  name        TEXT    NOT NULL,
  icon        TEXT,
  member_count INTEGER NOT NULL DEFAULT 0,
  config_epoch INTEGER NOT NULL DEFAULT 0,
  updated_at  INTEGER NOT NULL
);

CREATE TABLE guild_state (
  guild_id    INTEGER PRIMARY KEY REFERENCES guilds(id) ON DELETE CASCADE,
  added_at    INTEGER NOT NULL,
  synced_at   INTEGER NOT NULL DEFAULT 0,
  bot_present INTEGER NOT NULL DEFAULT 1
);

-- Per-feature settings document. One row per (guild, feature); `feature` is one
-- of the names in `models::Feature::as_str`.
CREATE TABLE guild_config (
  guild_id   INTEGER NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  feature    TEXT    NOT NULL,
  payload    TEXT    NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (guild_id, feature)
);
CREATE INDEX idx_guild_config_feature ON guild_config(feature);

CREATE TABLE config_history (
  id        INTEGER PRIMARY KEY,
  guild_id  INTEGER,
  feature   TEXT    NOT NULL,
  changed_by INTEGER,
  at        INTEGER NOT NULL,
  before    TEXT,
  after     TEXT    NOT NULL,
  hash      TEXT    NOT NULL
);
CREATE INDEX idx_config_history_guild ON config_history(guild_id, at DESC);

CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- --------------------------------------------------------------------------
-- Moderation
-- --------------------------------------------------------------------------
CREATE TABLE warns (
  id            INTEGER PRIMARY KEY,
  guild_id      INTEGER NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  user_id       INTEGER NOT NULL,
  moderator_id  INTEGER NOT NULL,
  reason        TEXT    NOT NULL,
  at            INTEGER NOT NULL,
  expires_at    INTEGER NOT NULL DEFAULT 0,
  active        INTEGER NOT NULL DEFAULT 1,
  pardoned_at   INTEGER,
  context_message INTEGER
);
CREATE INDEX idx_warns_user  ON warns(guild_id, user_id, active, at DESC);
CREATE INDEX idx_warns_guild ON warns(guild_id, at DESC);
CREATE INDEX idx_warns_expiry ON warns(active, expires_at);

CREATE TABLE strikes (
  id          INTEGER PRIMARY KEY,
  guild_id    INTEGER NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  user_id     INTEGER NOT NULL,
  source      TEXT    NOT NULL DEFAULT 'manual',
  category    TEXT,
  reason      TEXT    NOT NULL DEFAULT '',
  at          INTEGER NOT NULL,
  expires_at  INTEGER NOT NULL DEFAULT 0,
  active      INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX idx_strikes_user ON strikes(guild_id, user_id, active, at DESC);
CREATE INDEX idx_strikes_expiry ON strikes(active, expires_at);

CREATE TABLE kicks (
  id        INTEGER PRIMARY KEY,
  guild_id  INTEGER NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  user_id   INTEGER NOT NULL,
  moderator_id INTEGER NOT NULL,
  reason    TEXT    NOT NULL DEFAULT '',
  at        INTEGER NOT NULL
);
CREATE INDEX idx_kicks_guild ON kicks(guild_id, at DESC);

CREATE TABLE ban_records (
  id        INTEGER PRIMARY KEY,
  guild_id  INTEGER NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  user_id   INTEGER NOT NULL,
  moderator_id INTEGER NOT NULL,
  reason    TEXT    NOT NULL DEFAULT '',
  at        INTEGER NOT NULL,
  duration_secs INTEGER NOT NULL DEFAULT 0,
  unbanned_at INTEGER,
  soft      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_ban_guild ON ban_records(guild_id, at DESC);
CREATE INDEX idx_ban_user  ON ban_records(guild_id, user_id, at DESC);

CREATE TABLE notes (
  id          INTEGER PRIMARY KEY,
  guild_id    INTEGER NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  user_id     INTEGER NOT NULL,
  author_id   INTEGER NOT NULL,
  body        TEXT    NOT NULL,
  at          INTEGER NOT NULL
);
CREATE INDEX idx_notes_user ON notes(guild_id, user_id, at DESC);

-- --------------------------------------------------------------------------
-- Tickets
-- --------------------------------------------------------------------------
CREATE TABLE ticket_panels (
  id              INTEGER PRIMARY KEY,
  guild_id        INTEGER NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  channel_id      INTEGER NOT NULL,
  message_id      INTEGER NOT NULL,
  label           TEXT    NOT NULL,
  emoji           TEXT,
  support_role_id INTEGER,
  seq             INTEGER NOT NULL DEFAULT 0,
  created_at      INTEGER NOT NULL,
  UNIQUE (guild_id, message_id)
);

CREATE TABLE tickets (
  id           INTEGER PRIMARY KEY,
  guild_id     INTEGER NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  channel_id   INTEGER NOT NULL UNIQUE,
  user_channel INTEGER,
  owner_id     INTEGER NOT NULL,
  claimed_by   INTEGER,
  status       TEXT    NOT NULL DEFAULT 'open',
  subject      TEXT    NOT NULL DEFAULT '',
  reference    TEXT    NOT NULL,
  created_at   INTEGER NOT NULL,
  closed_at    INTEGER,
  message_count INTEGER NOT NULL DEFAULT 0,
  last_activity INTEGER NOT NULL DEFAULT 0,
  panel_id     INTEGER,
  UNIQUE (guild_id, reference)
);
CREATE INDEX idx_tickets_owner ON tickets(guild_id, owner_id, status);
CREATE INDEX idx_tickets_status ON tickets(status, last_activity DESC);

CREATE TABLE ticket_messages (
  id         INTEGER PRIMARY KEY,
  ticket_id  INTEGER NOT NULL REFERENCES tickets(id) ON DELETE CASCADE,
  guild_id   INTEGER NOT NULL,
  author_id  INTEGER NOT NULL,
  author_tag TEXT,
  content    TEXT    NOT NULL,
  is_internal INTEGER NOT NULL DEFAULT 0,
  seq        INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX idx_ticket_messages ON ticket_messages(ticket_id, seq);

-- --------------------------------------------------------------------------
-- Sticky messages
-- --------------------------------------------------------------------------
CREATE TABLE sticky_messages (
  id                INTEGER PRIMARY KEY,
  guild_id          INTEGER NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  channel_id        INTEGER NOT NULL,
  message_id        INTEGER NOT NULL,
  sticky_message_id INTEGER,
  content           TEXT    NOT NULL,
  every_messages    INTEGER NOT NULL DEFAULT 12,
  since_count       INTEGER NOT NULL DEFAULT 0,
  created_by        INTEGER NOT NULL,
  created_at        INTEGER NOT NULL,
  last_run_at       INTEGER NOT NULL DEFAULT 0,
  enabled           INTEGER NOT NULL DEFAULT 1,
  UNIQUE (guild_id, message_id)
);
CREATE INDEX idx_sticky_channel ON sticky_messages(guild_id, channel_id, enabled);

-- --------------------------------------------------------------------------
-- Anti-raid
-- --------------------------------------------------------------------------
CREATE TABLE raid_events (
  id          INTEGER PRIMARY KEY,
  guild_id    INTEGER NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  started_at  INTEGER NOT NULL,
  ended_at    INTEGER,
  trigger     TEXT    NOT NULL,
  peak        INTEGER NOT NULL DEFAULT 0,
  window_secs INTEGER NOT NULL DEFAULT 0,
  flagged     INTEGER NOT NULL DEFAULT 0,
  kicked      INTEGER NOT NULL DEFAULT 0,
  banned      INTEGER NOT NULL DEFAULT 0,
  actions     TEXT    NOT NULL DEFAULT '[]'
);
CREATE INDEX idx_raid_active ON raid_events(guild_id, ended_at);
CREATE INDEX idx_raid_recent ON raid_events(started_at DESC);

-- --------------------------------------------------------------------------
-- AI text filter
-- --------------------------------------------------------------------------
CREATE TABLE ai_custom_rules (
  id          INTEGER PRIMARY KEY,
  guild_id    INTEGER NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  pattern     TEXT    NOT NULL,
  mode        TEXT    NOT NULL,
  is_regex    INTEGER NOT NULL DEFAULT 0,
  category    TEXT    NOT NULL DEFAULT 'custom',
  severity    INTEGER NOT NULL DEFAULT 2,
  action      TEXT    NOT NULL DEFAULT 'flag',
  strikes     INTEGER NOT NULL DEFAULT 0,
  description TEXT    NOT NULL DEFAULT '',
  enabled     INTEGER NOT NULL DEFAULT 1,
  hits        INTEGER NOT NULL DEFAULT 0,
  seq         INTEGER NOT NULL DEFAULT 0,
  UNIQUE (guild_id, pattern, mode)
);

CREATE TABLE ai_events (
  id          INTEGER PRIMARY KEY,
  guild_id    INTEGER NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  user_id     INTEGER NOT NULL,
  channel_id  INTEGER NOT NULL,
  message_id  INTEGER,
  category    TEXT    NOT NULL,
  severity    INTEGER NOT NULL DEFAULT 0,
  score       REAL    NOT NULL DEFAULT 0,
  action      TEXT    NOT NULL,
  at          INTEGER NOT NULL,
  detail      TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX idx_ai_guild ON ai_events(guild_id, at DESC);
CREATE INDEX idx_ai_user  ON ai_events(guild_id, user_id, at DESC);
CREATE INDEX idx_ai_expiry ON ai_events(at);

-- Verdict cache: identical text from the same author inside `ttl` reuses the
-- provider answer, which is the difference between 0 and real money on a flood.
CREATE TABLE ai_verdict_cache (
  guild_id  INTEGER NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  author_id INTEGER NOT NULL,
  hash      TEXT    NOT NULL,
  verdict   TEXT    NOT NULL,
  expires_at INTEGER NOT NULL,
  PRIMARY KEY (guild_id, author_id, hash)
);
CREATE INDEX idx_ai_cache_expiry ON ai_verdict_cache(expires_at);

-- --------------------------------------------------------------------------
-- Automation
-- --------------------------------------------------------------------------
CREATE TABLE users (
  id           INTEGER PRIMARY KEY,
  username     TEXT NOT NULL,
  discriminator TEXT NOT NULL DEFAULT '0',
  global_name  TEXT,
  avatar       TEXT,
  is_bot       INTEGER NOT NULL DEFAULT 0,
  first_seen   INTEGER NOT NULL,
  last_seen    INTEGER NOT NULL
);

CREATE TABLE levels (
  guild_id  INTEGER NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  user_id   INTEGER NOT NULL,
  xp        INTEGER NOT NULL DEFAULT 0,
  level     INTEGER NOT NULL DEFAULT 0,
  messages  INTEGER NOT NULL DEFAULT 0,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (guild_id, user_id)
);
CREATE INDEX idx_levels_rank ON levels(guild_id, xp DESC);

CREATE TABLE log_messages (
  id         INTEGER PRIMARY KEY,
  guild_id   INTEGER NOT NULL,
  channel_id INTEGER NOT NULL,
  message_id INTEGER NOT NULL,
  author_id  INTEGER NOT NULL,
  content    TEXT NOT NULL,
  kind       TEXT NOT NULL,
  at         INTEGER NOT NULL
);
CREATE INDEX idx_log_lookup ON log_messages(guild_id, channel_id, message_id);
CREATE INDEX idx_log_expiry ON log_messages(at);

-- --------------------------------------------------------------------------
-- API: keys, sessions, CSRF, OAuth, pairing, audit, stats
-- --------------------------------------------------------------------------
CREATE TABLE api_keys (
  id           INTEGER PRIMARY KEY,
  label        TEXT NOT NULL,
  key_mac      TEXT NOT NULL UNIQUE,
  hint         TEXT NOT NULL,
  guild_id     INTEGER,
  scopes       TEXT NOT NULL DEFAULT '["read"]',
  created_by   INTEGER NOT NULL,
  created_at   INTEGER NOT NULL,
  last_used_at INTEGER,
  expires_at   INTEGER NOT NULL DEFAULT 0,
  revoked_at   INTEGER,
  request_count INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_keys_guild ON api_keys(guild_id);

CREATE TABLE sessions (
  id            INTEGER PRIMARY KEY,
  token_mac     TEXT NOT NULL UNIQUE,
  user_id       INTEGER NOT NULL,
  username      TEXT NOT NULL DEFAULT '',
  created_at    INTEGER NOT NULL,
  last_seen_at  INTEGER NOT NULL,
  expires_at    INTEGER NOT NULL,
  csrf_secret   TEXT NOT NULL,
  ip            TEXT NOT NULL DEFAULT '',
  user_agent    TEXT NOT NULL DEFAULT '',
  revoked_at    INTEGER
);
CREATE INDEX idx_sessions_expiry ON sessions(expires_at);
CREATE INDEX idx_sessions_user ON sessions(user_id);

CREATE TABLE oauth_state (
  state        TEXT PRIMARY KEY,
  nonce        TEXT NOT NULL,
  verifier     TEXT NOT NULL,
  created_at   INTEGER NOT NULL,
  expires_at   INTEGER NOT NULL,
  ip           TEXT NOT NULL,
  consumed_at  INTEGER,
  return_to    TEXT
);
CREATE INDEX idx_oauth_expiry ON oauth_state(expires_at);

CREATE TABLE pairing_codes (
  code_hash  TEXT PRIMARY KEY,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  attempts   INTEGER NOT NULL DEFAULT 0,
  consumed_at INTEGER,
  ip         TEXT NOT NULL DEFAULT ''
);

CREATE TABLE audit_log (
  id          INTEGER PRIMARY KEY,
  guild_id    INTEGER,
  at          INTEGER NOT NULL,
  actor_type  TEXT NOT NULL,
  actor_id    INTEGER,
  actor_label TEXT NOT NULL DEFAULT '',
  action      TEXT NOT NULL,
  target      TEXT,
  ip          TEXT NOT NULL DEFAULT '',
  meta        TEXT NOT NULL DEFAULT '{}',
  hash        TEXT NOT NULL
);
CREATE INDEX idx_audit_guild ON audit_log(guild_id, at DESC);
CREATE INDEX idx_audit_actor ON audit_log(actor_type, actor_id, at DESC);
CREATE INDEX idx_audit_expiry ON audit_log(at);

CREATE TABLE stats (
  at      INTEGER NOT NULL,
  process TEXT NOT NULL,
  rss_kb  INTEGER NOT NULL DEFAULT 0,
  cpu_pct REAL NOT NULL DEFAULT 0,
  guilds  INTEGER NOT NULL DEFAULT 0,
  ws_latency_ms INTEGER NOT NULL DEFAULT 0,
  messages_per_min INTEGER NOT NULL DEFAULT 0,
  db_size_kb INTEGER NOT NULL DEFAULT 0,
  users_seen INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (at, process)
);
CREATE INDEX idx_stats_recent ON stats(at DESC);
"#;

/// Version 2: the two queues the first drafts of `db/mod.rs` assumed but never
/// created. `action_outbox` carries moderation actions queued by the API to the
/// bot (the only supported way to act on Discord from HTTP); `bot_status` is the
/// bot's live snapshot, so the dashboard never needs a second public port.
///
/// Everything here is `CREATE ... IF NOT EXISTS` and additive, which is what makes
/// upgrading a live self-hosted install safe: an interrupted migration re-runs
/// cleanly, and no existing column is touched.
pub const SCHEMA_V2: &str = r#"
CREATE TABLE IF NOT EXISTS action_outbox (
  id           INTEGER PRIMARY KEY,
  guild_id     INTEGER NOT NULL,
  actor_id     INTEGER NOT NULL,
  actor_label  TEXT NOT NULL DEFAULT '',
  action       TEXT NOT NULL,
  args         TEXT NOT NULL DEFAULT '{}',
  status       TEXT NOT NULL DEFAULT 'pending',
  attempts     INTEGER NOT NULL DEFAULT 0,
  created_at   INTEGER NOT NULL,
  claimed_at   INTEGER,
  done_at      INTEGER,
  error        TEXT NOT NULL DEFAULT '',
  reference    TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_outbox_claim ON action_outbox(status, id);
CREATE INDEX IF NOT EXISTS idx_outbox_guild ON action_outbox(guild_id, id DESC);

-- Dashboard sessions carry the guild they were bootstrapped for. Adding it here
-- (not in v1) is what keeps the migration forward-only: an existing install gains
-- the column, and a fresh one gets it in the same batch.
ALTER TABLE sessions ADD COLUMN allowed_guild INTEGER;
CREATE INDEX IF NOT EXISTS idx_sessions_guild ON sessions(user_id, allowed_guild);

CREATE TABLE IF NOT EXISTS bot_status (
  guild_id            INTEGER NOT NULL,
  ws_latency_ms       INTEGER NOT NULL DEFAULT 0,
  shards_total        INTEGER NOT NULL DEFAULT 1,
  shards_ready        INTEGER NOT NULL DEFAULT 0,
  users_cached        INTEGER NOT NULL DEFAULT 0,
  active_raid         INTEGER NOT NULL DEFAULT 0,
  open_tickets        INTEGER NOT NULL DEFAULT 0,
  queue_depth         INTEGER NOT NULL DEFAULT 0,
  gateway_events_min  INTEGER NOT NULL DEFAULT 0,
  updated_at          INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (guild_id)
);
"#;

/// The `redirect_uri` sent at authorize, kept with the state row.
///
/// Discord's token endpoint requires the byte-identical `redirect_uri` that produced the
/// code. Re-deriving it from the callback's own `Host` reproduces it in the ordinary case —
/// the browser went to the same address it came from — and silently breaks behind a proxy
/// that normalises the host, or when `.env` is edited mid-flight. Storing what we sent makes
/// the two halves agree by construction. It is never reflected into a response: the only
/// reader is the server, posting it to Discord over TLS.
pub const SCHEMA_V3: &str = r#"
ALTER TABLE oauth_state ADD COLUMN redirect_uri TEXT;
"#;

/// v4 — what a dashboard session is allowed to touch.
///
/// Discord decides who may administer a server, so a session minted by a Discord login is
/// stamped with the guilds that account managed *at the moment it signed in*. `NULL` means
/// "no grant list" and stays unrestricted, which is what every session written before this
/// column exists is; a new OAuth login always writes the list.
pub const SCHEMA_V4: &str = r#"
ALTER TABLE sessions ADD COLUMN guild_grants TEXT;
"#;
