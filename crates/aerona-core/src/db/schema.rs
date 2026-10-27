//! Version 1 schema. Keep this file append-only in spirit: never edit a shipped
//! migration, always add the next one.

/// Every table, index and constraint Aerona needs, in one batch.
///
/// Design notes for reviewers:
///
/// * All ids are `INTEGER` (Discord snowflakes fit in i64) - 8 bytes, indexable,
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
/// code. Re-deriving it from the callback's own `Host` reproduces it in the ordinary case -
/// the browser went to the same address it came from - and silently breaks behind a proxy
/// that normalises the host, or when `.env` is edited mid-flight. Storing what we sent makes
/// the two halves agree by construction. It is never reflected into a response: the only
/// reader is the server, posting it to Discord over TLS.
pub const SCHEMA_V3: &str = r#"
ALTER TABLE oauth_state ADD COLUMN redirect_uri TEXT;
"#;

/// v4 - what a dashboard session is allowed to touch.
///
/// Discord decides who may administer a server, so a session minted by a Discord login is
/// stamped with the guilds that account managed *at the moment it signed in*. `NULL` means
/// "no grant list" and stays unrestricted, which is what every session written before this
/// column exists is; a new OAuth login always writes the list.
pub const SCHEMA_V4: &str = r#"
ALTER TABLE sessions ADD COLUMN guild_grants TEXT;
"#;

/// v5: the mod log, and the attachments a transcript has to be able to show.
///
/// `log_events` exists because everything the logger did before this was a message into a
/// channel: nothing to list, nothing to filter, nothing that survives somebody deleting the
/// post. It is written by the same code path that posts, so a row and a card describe the same
/// event or neither happens.
///
/// `ticket_messages.attachments` is a JSON array of `{url,name,kind,size,width,height}`, kept
/// beside the text rather than inside it: a transcript is opened in a browser, and an image that
/// is only a URL on a line of its own is an image nobody sees.
pub const SCHEMA_V5: &str = r#"
CREATE TABLE log_events (
  id         INTEGER PRIMARY KEY,
  guild_id   INTEGER NOT NULL,
  at         INTEGER NOT NULL,
  event      TEXT    NOT NULL,
  channel_id INTEGER,
  user_id    INTEGER,
  message_id INTEGER,
  summary    TEXT    NOT NULL,
  before     TEXT,
  after      TEXT
);
CREATE INDEX idx_log_events_guild ON log_events(guild_id, id DESC);
CREATE INDEX idx_log_events_at ON log_events(at);

ALTER TABLE ticket_messages ADD COLUMN attachments TEXT NOT NULL DEFAULT '[]';
"#;
/// v6: stickies that do not need a message to copy, and ticket panels that are ticket *types*.
///
/// `sticky_messages` loses its table-level `UNIQUE (guild_id, message_id)` and gains a partial
/// index instead, because a sticky authored in the dashboard has no message to be anchored to:
/// every unanchored row would have `message_id = 0` and collide with the last one. SQLite cannot
/// drop a constraint in place, so the table is rebuilt in the migration - ids are copied, so the
/// rows a live install already has keep their identity and their `sticky_message_id`.
///
/// The embed columns exist because a pinned line of text is not the same thing as a card: the
/// ask was a sticky that looks like an announcement, and a `content` string cannot do that.
///
/// `ticket_panels` gains a colour, a subject and an intro so that the buttons an operator already
/// puts in a channel can each open a different kind of ticket. That is what "multiple ticket
/// types" means here: not a second system, the same board with one row per type.
pub const SCHEMA_V6: &str = r#"
CREATE TABLE sticky_messages_v6 (
  id                INTEGER PRIMARY KEY,
  guild_id          INTEGER NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  channel_id        INTEGER NOT NULL,
  message_id        INTEGER NOT NULL DEFAULT 0,
  sticky_message_id INTEGER,
  content           TEXT    NOT NULL DEFAULT '',
  every_messages    INTEGER NOT NULL DEFAULT 12,
  since_count       INTEGER NOT NULL DEFAULT 0,
  created_by        INTEGER NOT NULL,
  created_at        INTEGER NOT NULL,
  last_run_at       INTEGER NOT NULL DEFAULT 0,
  enabled           INTEGER NOT NULL DEFAULT 1,
  embed_title       TEXT,
  embed_body        TEXT,
  embed_color       INTEGER
);
INSERT INTO sticky_messages_v6
  (id, guild_id, channel_id, message_id, sticky_message_id, content, every_messages,
   since_count, created_by, created_at, last_run_at, enabled)
SELECT id, guild_id, channel_id, message_id, sticky_message_id, content, every_messages,
       since_count, created_by, created_at, last_run_at, enabled
  FROM sticky_messages;
DROP TABLE sticky_messages;
ALTER TABLE sticky_messages_v6 RENAME TO sticky_messages;
CREATE INDEX IF NOT EXISTS idx_sticky_channel ON sticky_messages(guild_id, channel_id, enabled);
CREATE UNIQUE INDEX IF NOT EXISTS idx_sticky_anchor ON sticky_messages(guild_id, message_id)
  WHERE message_id != 0;

-- A board is one message with one button per type, so several rows point at the same
-- `message_id`. The table-level `UNIQUE (guild_id, message_id)` forbids that, so it becomes a
-- partial index: still one row per *anchored* message where a message means something, and no
-- constraint at all for the rows that have not been posted yet.
CREATE TABLE ticket_panels_v6 (
  id              INTEGER PRIMARY KEY,
  guild_id        INTEGER NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  channel_id      INTEGER NOT NULL,
  message_id      INTEGER NOT NULL DEFAULT 0,
  label           TEXT    NOT NULL,
  emoji           TEXT,
  support_role_id INTEGER,
  seq             INTEGER NOT NULL DEFAULT 0,
  created_at      INTEGER NOT NULL,
  color           INTEGER,
  subject         TEXT    NOT NULL DEFAULT '',
  intro           TEXT    NOT NULL DEFAULT ''
);
INSERT INTO ticket_panels_v6
  (id, guild_id, channel_id, message_id, label, emoji, support_role_id, seq, created_at)
SELECT id, guild_id, channel_id, message_id, label, emoji, support_role_id, seq, created_at
  FROM ticket_panels;
DROP TABLE ticket_panels;
ALTER TABLE ticket_panels_v6 RENAME TO ticket_panels;
CREATE INDEX IF NOT EXISTS idx_ticket_panels_channel ON ticket_panels(guild_id, channel_id, seq);
CREATE UNIQUE INDEX IF NOT EXISTS idx_ticket_panels_bound
  ON ticket_panels(guild_id, message_id) WHERE message_id != 0;
"#;

/// Minecraft capes: the notifier's own two tables.
///
/// `cape_releases` is one row per cape per server, because "have I told this server about this cape"
/// is a per-server question - each guild has its own channel, its own role, and its own idea of when
/// it started caring. The unique key is the source's page id (`slug`), not the title: a renamed page
/// is the same cape, and pinging twice for one release is how a notifier gets muted.
///
/// The frames are a second table rather than a column, so the list query never drags four PNGs per
/// row through the pool to draw a table of names.
pub const SCHEMA_V7: &str = r#"
CREATE TABLE cape_releases (
  id            INTEGER PRIMARY KEY,
  guild_id      INTEGER NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
  slug          TEXT    NOT NULL,
  title         TEXT    NOT NULL,
  name          TEXT    NOT NULL DEFAULT '',
  article_url   TEXT    NOT NULL DEFAULT '',
  texture_file  TEXT    NOT NULL DEFAULT '',
  texture_url   TEXT    NOT NULL DEFAULT '',
  -- 'drape' | 'wiki' | 'none': whose picture the frames are, and why there are four or one.
  render_kind   TEXT    NOT NULL DEFAULT 'none',
  last_rev      INTEGER NOT NULL DEFAULT 0,
  found_at      INTEGER NOT NULL,
  announced_at  INTEGER,
  -- Posts that were tried and refused. Announcing is the worker's own scheduled retry, not a
  -- queued action, so the row has to say how many times it knocked.
  attempts      INTEGER NOT NULL DEFAULT 0,
  UNIQUE (guild_id, slug)
);
CREATE INDEX idx_cape_lookup ON cape_releases(guild_id, found_at DESC);

CREATE TABLE cape_frames (
  release_id INTEGER NOT NULL REFERENCES cape_releases(id) ON DELETE CASCADE,
  idx        INTEGER NOT NULL,
  png        BLOB    NOT NULL,
  PRIMARY KEY (release_id, idx)
);

-- When this server last asked. Kept per guild rather than as one clock, because the interval is a
-- setting and a bot in forty servers must not hit the same API forty times in the same minute.
CREATE TABLE cape_state (
  guild_id   INTEGER PRIMARY KEY REFERENCES guilds(id) ON DELETE CASCADE,
  last_check INTEGER NOT NULL DEFAULT 0,
  -- Rows learned without an announcement: the first pass records the whole category so a feature
  -- switched on today does not ping about every cape since 2011.
  seeded     INTEGER NOT NULL DEFAULT 0
);
"#;

/// v8: a closed ticket that cleans itself up, and the pictures it was closed with.
///
/// Two things a moderator asked for, in one migration because they contradict each other unless
/// they ship together. "Delete the channel a while after it closes" keeps a category readable;
/// "the transcript must still show the screenshot" needs the bytes, because a Discord attachment
/// dies with its message, and a ticket channel that is deleted has taken every file in it with
/// it. A scheduler without a file store would therefore be a feature that destroys evidence on a
/// timer - so the store ships with it, and `keep_ticket_files` is on by default.
///
/// `tickets.delete_at` is when the channel is due to go, written at close from the guild's
/// setting. It is a column rather than `closed_at + config` so that changing the setting tomorrow
/// does not retroactively move a deadline somebody was told about, and so a restart mid-delay
/// resumes the same schedule. `channel_deleted_at` is the fact that it went, which is also what
/// stops the janitor trying again every tick, and what lets the panel say "this ticket's channel
/// no longer exists" instead of offering a link to nothing.
///
/// The files are keyed by `(ticket_id, seq, idx)` - the transcript row's own sequence number and
/// the position of the attachment inside it - because that is the pair the renderer walks in the
/// same order, and because the Discord message id is not what a transcript is sorted by.
pub const SCHEMA_V8: &str = r#"
ALTER TABLE tickets ADD COLUMN delete_at INTEGER;
ALTER TABLE tickets ADD COLUMN channel_deleted_at INTEGER;
CREATE INDEX idx_tickets_due ON tickets(delete_at) WHERE delete_at IS NOT NULL AND channel_deleted_at IS NULL;

CREATE TABLE ticket_files (
  id           INTEGER PRIMARY KEY,
  ticket_id    INTEGER NOT NULL REFERENCES tickets(id) ON DELETE CASCADE,
  -- Carried from the ticket rather than a foreign key: the retention setting is a guild's, and a
  -- prune that had to join through tickets to ask "whose files are these" is a prune that never
  -- runs. Cascade by ticket still applies, and a guild row that goes takes its tickets with it.
  guild_id     INTEGER NOT NULL,
  -- `ticket_messages.seq` this file was attached to, and its place in that row's list.
  seq          INTEGER NOT NULL,
  idx          INTEGER NOT NULL,
  name         TEXT    NOT NULL,
  content_type TEXT    NOT NULL DEFAULT '',
  width        INTEGER NOT NULL DEFAULT 0,
  height       INTEGER NOT NULL DEFAULT 0,
  bytes        BLOB    NOT NULL,
  -- The URL it was copied from, so a moderator can check the copy against the original while the
  -- original still exists, and so a failed copy is visibly a gap rather than a mystery.
  source       TEXT    NOT NULL DEFAULT '',
  copied_at    INTEGER NOT NULL,
  UNIQUE (ticket_id, seq, idx)
);
CREATE INDEX idx_ticket_files_ticket ON ticket_files(ticket_id);
CREATE INDEX idx_ticket_files_age ON ticket_files(guild_id, copied_at);
"#;
