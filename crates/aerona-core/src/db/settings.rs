//! Typed accessors for per-guild settings documents, plus the `meta` store and
//! the config-change history the dashboard's diff view reads.
//!
//! Reads never fail on a missing row: a guild with no stored document gets the
//! struct's `Default`, which is what makes "install the bot and it behaves
//! sensibly" true without a seeding step.

use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::identity::Snowflake;
use crate::models::{
    AiConfig, AntiSpamConfig, AutoConfig, Feature, GuildState, GuildSummary, IdentityConfig,
    LinkGuardConfig, LogConfig, ModConfig, RaidConfig, StickyConfig, TicketConfig,
};
use crate::security;
use crate::time;

/// Kinds of gateway report the dashboard may read. Adding one is a bot-side write plus a
/// renderer; nothing else has to know it exists.
pub const REPORT_KINDS: [&str; 3] = ["commands", "picker", "identity"];

fn report_key(guild: Snowflake, kind: &str) -> String {
    format!("report:{guild}:{kind}")
}

use super::{now, Db};

impl Db {
    /// Load the stored document for `feature`, falling back to `T::default()`.
    pub fn settings<T>(&self, guild: Snowflake, feature: Feature) -> Result<T>
    where
        T: DeserializeOwned + Default + Send,
    {
        let key = feature.storage_key();
        self.read(move |conn| {
            let row: Option<String> = conn
                .query_row(
                    "SELECT payload FROM guild_config WHERE guild_id = ?1 AND feature = ?2",
                    params![guild.as_i64(), key],
                    |r| r.get(0),
                )
                .optional()
                .map_err(Error::db)?;
            match row {
                None => Ok(T::default()),
                Some(raw) => match serde_json::from_str::<T>(&raw) {
                    // A corrupt document must not brick the guild: log loudly and
                    // serve defaults; the next save rewrites it.
                    Ok(v) => Ok(v),
                    Err(e) => {
                        tracing::warn!(
                            guild = %guild,
                            feature = key,
                            "undecodable config, using defaults: {e}"
                        );
                        Ok(T::default())
                    }
                },
            }
        })
    }

    /// Replace the document for `feature`, recording the change in
    /// `config_history` with a hash chain, and bumping the guild's config epoch
    /// so the gateway worker knows to reload.
    pub fn put_settings<T>(
        &self,
        guild: Snowflake,
        feature: Feature,
        next: &T,
        changed_by: Option<Snowflake>,
    ) -> Result<()>
    where
        T: serde::Serialize + Send,
    {
        let payload = serde_json::to_string(next)
            .map_err(|e| Error::Db(format!("could not store config: {e}")))?;
        let key = feature.storage_key().to_string();
        let at = now();
        let by = changed_by.map(|s| s.as_i64());
        self.tx(move |conn| {
            // A guild row must exist before any child row: on first run the
            // dashboard can save settings before the gateway has cached the
            // guild, and `PRAGMA foreign_keys = ON` would reject the write.
            // `INSERT OR IGNORE` keeps the real name if the bot already knows it.
            ensure_guild(conn, guild, at)?;

            let prev: Option<String> = conn
                .query_row(
                    "SELECT payload FROM guild_config WHERE guild_id=?1 AND feature=?2",
                    params![guild.as_i64(), key],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .map_err(Error::db)?;

            // Only write when something actually changed: this keeps
            // `config_history` and the reload signal meaningful.
            let changed = match &prev {
                None => true,
                Some(old) => !json_equal(old, &payload),
            };
            if changed {
                conn.execute(
                    "INSERT INTO guild_config(guild_id, feature, payload, updated_at)
                     VALUES(?1, ?2, ?3, ?4)
                     ON CONFLICT(guild_id, feature) DO UPDATE
                       SET payload = excluded.payload, updated_at = excluded.updated_at",
                    params![guild.as_i64(), key, payload, at],
                )?;
                let last: Option<String> = conn
                    .query_row(
                        "SELECT hash FROM config_history WHERE guild_id=?1 ORDER BY id DESC LIMIT 1",
                        params![guild.as_i64()],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(Error::db)?;
                let hash = chain_hash(last.as_deref(), guild, &key, &payload, at);
                conn.execute(
                    "INSERT INTO config_history(guild_id, feature, changed_by, at, before, after, hash)
                     VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![guild.as_i64(), key, by, at, prev, payload, hash],
                )?;
                conn.execute(
                    "UPDATE guilds SET config_epoch = config_epoch + 1, updated_at = ?2 WHERE id = ?1",
                    params![guild.as_i64(), at],
                )?;
            }
            Ok::<(), Error>(())
        })?;
        Ok(())
    }

    /// Current stored document, verbatim — used by the dashboard to show a diff
    /// and by `DELETE` handlers to undo a guild's setup.
    pub fn raw_settings(&self, guild: Snowflake, feature: Feature) -> Result<Option<Value>> {
        let key = feature.storage_key();
        self.read(move |conn| {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT payload FROM guild_config WHERE guild_id=?1 AND feature=?2",
                    params![guild.as_i64(), key],
                    |r| r.get(0),
                )
                .optional()
                .map_err(Error::db)?;
            Ok(raw.and_then(|v| serde_json::from_str(&v).ok()))
        })
    }

    pub fn delete_settings(&self, guild: Snowflake, feature: Feature) -> Result<()> {
        let key = feature.storage_key().to_string();
        let at = now();
        self.tx(move |conn| {
            conn.execute(
                "DELETE FROM guild_config WHERE guild_id=?1 AND feature=?2",
                params![guild.as_i64(), key],
            )?;
            conn.execute(
                "UPDATE guilds SET config_epoch = config_epoch + 1, updated_at=?2 WHERE id=?1",
                params![guild.as_i64(), at],
            )?;
            Ok(())
        })
    }

    /// `(guild, config_epoch)` pairs — the gateway worker compares against its
    /// cached value and reloads only what changed. O(guilds), one indexed query.
    /// Bot-side sync of one guild: name/icon/counts plus `guild_state`. The API
    /// reads exactly these rows to decide what the dashboard may show, so a
    /// re-invited bot repairs its own visibility by calling this again.
    pub fn sync_guild(
        &self,
        guild: Snowflake,
        name: &str,
        icon: Option<&str>,
        member_count: i64,
    ) -> Result<()> {
        let name = crate::security::clean(name, 120);
        // A blank name would render as an empty row in the dashboard's server list.
        let name = if name.is_empty() {
            "?".to_string()
        } else {
            name
        };
        let icon = icon.map(|v| crate::security::clean(v, 128));
        let at = now();
        let g = guild.as_i64();
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO guilds(id,name,icon,member_count,updated_at) VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(id) DO UPDATE SET name=excluded.name, icon=excluded.icon,
                   member_count=excluded.member_count, updated_at=excluded.updated_at",
                params![g, name, icon, member_count.max(0), at],
            )?;
            conn.execute(
                "INSERT INTO guild_state(guild_id,added_at,synced_at,bot_present) VALUES(?1,?2,?2,1)
                 ON CONFLICT(guild_id) DO UPDATE SET synced_at=excluded.synced_at, bot_present=1",
                params![g, at],
            )?;
            Ok(())
        })
    }

    /// Mark a guild the bot no longer serves. Presence is kept in its own table so
    /// a kick never destroys configuration, and `bot_present = 0` is what makes the
    /// dashboard show "re-invite Aerona" instead of stale numbers.
    pub fn mark_guild_absent(&self, guild: Snowflake) -> Result<()> {
        let g = guild.as_i64();
        self.tx(move |conn| {
            conn.execute(
                "UPDATE guild_state SET bot_present=0 WHERE guild_id=?1",
                params![g],
            )?;
            Ok(())
        })
    }

    pub fn forget_guild(&self, guild: Snowflake) -> Result<()> {
        let g = guild.as_i64();
        self.tx(move |conn| {
            // Cascades through every guild child (warns, tickets, stickies, …).
            conn.execute("DELETE FROM guilds WHERE id=?1", params![g])?;
            Ok(())
        })
    }

    pub fn guild_state(&self, guild: Snowflake) -> Result<Option<GuildState>> {
        let g = guild.as_i64();
        self.read(move |conn| {
            conn
                .query_row(
                    "SELECT guild_id, added_at, synced_at, bot_present FROM guild_state WHERE guild_id=?1",
                    params![g],
                    |r| {
                        Ok(GuildState {
                            guild_id: Snowflake(r.get::<_, i64>(0)? as u64),
                            added_at: r.get(1)?,
                            synced_at: r.get(2)?,
                            bot_present: r.get::<_, i64>(3)? != 0,
                        })
                    },
                )
                .optional()
                .map_err(Error::db)
        })
    }

    /// Everything the dashboard may list. `manageable` is resolved by the caller
    /// (it depends on *who* is asking), so this stays a pure read.
    pub fn guilds(&self) -> Result<Vec<GuildSummary>> {
        self.read(|conn| {
            let mut st = conn
                .prepare(
                    "SELECT g.id, g.name, g.icon, g.member_count, COALESCE(s.bot_present, 0),
                            COALESCE(s.synced_at, 0)
                     FROM guilds g LEFT JOIN guild_state s ON s.guild_id = g.id
                     ORDER BY g.name COLLATE NOCASE",
                )
                .map_err(Error::db)?;
            let rows = st
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)? != 0,
                        r.get::<_, i64>(5)?,
                    ))
                })
                .map_err(Error::db)?;
            let mut out = Vec::new();
            for row in rows {
                let (id, name, icon, members, present, synced) = row.map_err(Error::db)?;
                out.push(GuildSummary {
                    id: Snowflake(id as u64),
                    name,
                    icon,
                    member_count: members,
                    manageable: false,
                    bot_present: present && synced > 0,
                });
            }
            Ok(out)
        })
    }

    pub fn config_epochs(&self) -> Result<Vec<(Snowflake, i64)>> {
        self.read(|conn| {
            let mut st = conn
                .prepare("SELECT id, config_epoch FROM guilds")
                .map_err(Error::db)?;
            let out = st
                .query_map([], |r| {
                    Ok((Snowflake(r.get::<_, i64>(0)? as u64), r.get(1)?))
                })
                .map_err(Error::db)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::db)?;
            Ok(out)
        })
    }

    /// History newest-first, for the settings page's audit drawer.
    pub fn settings_history(&self, guild: Snowflake, limit: u32) -> Result<Vec<serde_json::Value>> {
        let limit = limit.clamp(1, 200) as i64;
        self.read(move |conn| {
            let mut st = conn
                .prepare(
                    "SELECT id, feature, changed_by, at, before, after, hash
                     FROM config_history WHERE guild_id=?1 ORDER BY id DESC LIMIT ?2",
                )
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![guild.as_i64(), limit], |r| {
                    Ok(serde_json::json!({
                        "id": r.get::<_, i64>(0)?,
                        "feature": r.get::<_, String>(1)?,
                        "changedBy": r.get::<_, Option<i64>>(2)?.map(|v| v.to_string()),
                        "at": r.get::<_, i64>(3)?,
                        "before": r
                            .get::<_, Option<String>>(4)?
                            .and_then(|v| serde_json::from_str::<Value>(&v).ok()),
                        "after": r
                            .get::<_, Option<String>>(5)?
                            .and_then(|v| serde_json::from_str::<Value>(&v).ok()),
                        "hash": r.get::<_, String>(6)?,
                    }))
                })
                .map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::db)
        })
    }

    // -- typed conveniences so call sites never stringly-type a feature -----

    pub fn guild_settings(&self, g: Snowflake) -> Result<crate::models::GuildSettings> {
        self.settings(g, Feature::General)
    }
    pub fn put_guild_settings(
        &self,
        g: Snowflake,
        v: &crate::models::GuildSettings,
        by: Option<Snowflake>,
    ) -> Result<()> {
        self.put_settings(g, Feature::General, v, by)
    }
    pub fn mod_config(&self, g: Snowflake) -> Result<ModConfig> {
        self.settings(g, Feature::Moderation)
    }
    pub fn put_mod_config(&self, g: Snowflake, v: &ModConfig, by: Option<Snowflake>) -> Result<()> {
        self.put_settings(g, Feature::Moderation, v, by)
    }
    pub fn ticket_config(&self, g: Snowflake) -> Result<TicketConfig> {
        self.settings(g, Feature::Tickets)
    }
    pub fn put_ticket_config(
        &self,
        g: Snowflake,
        v: &TicketConfig,
        by: Option<Snowflake>,
    ) -> Result<()> {
        self.put_settings(g, Feature::Tickets, v, by)
    }
    pub fn sticky_config(&self, g: Snowflake) -> Result<StickyConfig> {
        self.settings(g, Feature::Sticky)
    }
    pub fn put_sticky_config(
        &self,
        g: Snowflake,
        v: &StickyConfig,
        by: Option<Snowflake>,
    ) -> Result<()> {
        self.put_settings(g, Feature::Sticky, v, by)
    }
    pub fn raid_config(&self, g: Snowflake) -> Result<RaidConfig> {
        self.settings(g, Feature::AntiRaid)
    }
    pub fn put_raid_config(
        &self,
        g: Snowflake,
        v: &RaidConfig,
        by: Option<Snowflake>,
    ) -> Result<()> {
        self.put_settings(g, Feature::AntiRaid, v, by)
    }
    pub fn ai_config(&self, g: Snowflake) -> Result<AiConfig> {
        self.settings(g, Feature::AiFilter)
    }
    pub fn put_ai_config(&self, g: Snowflake, v: &AiConfig, by: Option<Snowflake>) -> Result<()> {
        self.put_settings(g, Feature::AiFilter, v, by)
    }
    pub fn auto_config(&self, g: Snowflake) -> Result<AutoConfig> {
        self.settings(g, Feature::Autorole)
    }
    pub fn put_auto_config(
        &self,
        g: Snowflake,
        v: &AutoConfig,
        by: Option<Snowflake>,
    ) -> Result<()> {
        self.put_settings(g, Feature::Autorole, v, by)
    }
    pub fn link_guard_config(&self, g: Snowflake) -> Result<LinkGuardConfig> {
        self.settings(g, Feature::LinkGuard)
    }
    pub fn put_link_guard_config(
        &self,
        g: Snowflake,
        v: &LinkGuardConfig,
        by: Option<Snowflake>,
    ) -> Result<()> {
        self.put_settings(g, Feature::LinkGuard, v, by)
    }
    pub fn anti_spam_config(&self, g: Snowflake) -> Result<AntiSpamConfig> {
        self.settings(g, Feature::AntiSpam)
    }
    pub fn put_anti_spam_config(
        &self,
        g: Snowflake,
        v: &AntiSpamConfig,
        by: Option<Snowflake>,
    ) -> Result<()> {
        self.put_settings(g, Feature::AntiSpam, v, by)
    }
    pub fn identity_config(&self, g: Snowflake) -> Result<IdentityConfig> {
        self.settings(g, Feature::Identity)
    }
    pub fn log_config(&self, g: Snowflake) -> Result<LogConfig> {
        self.settings(g, Feature::Logging)
    }
    pub fn put_log_config(&self, g: Snowflake, v: &LogConfig, by: Option<Snowflake>) -> Result<()> {
        self.put_settings(g, Feature::Logging, v, by)
    }

    // -- gateway reports -----------------------------------------------------

    /// One JSON blob the gateway worker writes for the dashboard to render, per guild and
    /// kind (`commands`, `picker`, `identity`). It is a *report*, never a source of truth:
    /// Discord owns commands, channels and profiles, and the row only says what the worker
    /// last saw. That is also why `reportedAt` travels inside the payload — the dashboard
    /// has to be able to say "as of 4 minutes ago" instead of implying it read Discord.
    pub fn guild_report(&self, guild: Snowflake, kind: &str) -> Result<Option<Value>> {
        let key = report_key(guild, kind);
        match self.meta_get(&key)? {
            None => Ok(None),
            Some(raw) => Ok(serde_json::from_str::<Value>(&raw).ok()),
        }
    }

    /// Every kind at once, which is what the dashboard's one poll wants, so a page does not
    /// issue three lookups for one render.
    pub fn guild_reports(&self, guild: Snowflake) -> Result<Value> {
        let mut out = serde_json::Map::new();
        for kind in REPORT_KINDS {
            if let Some(v) = self.guild_report(guild, kind)? {
                out.insert((*kind).to_string(), v);
            }
        }
        Ok(Value::Object(out))
    }

    /// Drop one report. Used for a staged avatar, which is single-use by design: keeping
    /// it would re-upload the same bytes on every later apply.
    pub fn delete_guild_report(&self, guild: Snowflake, kind: &str) -> Result<()> {
        self.meta_delete(&report_key(guild, kind))
    }

    pub fn put_guild_report(&self, guild: Snowflake, kind: &str, value: &Value) -> Result<()> {
        let payload = serde_json::to_string(value)
            .map_err(|e| Error::Db(format!("could not encode a guild report: {e}")))?;
        self.meta_set(&report_key(guild, kind), &payload)
    }

    /// When a guild is forgotten, its reports go with it: a stale command list for a server
    /// the bot no longer serves is worse than no list, because the dashboard would render
    /// it as if it were current.
    pub fn clear_guild_reports(&self, guild: Snowflake) -> Result<()> {
        for kind in REPORT_KINDS {
            self.meta_delete(&report_key(guild, kind))?;
        }
        Ok(())
    }

    // -- install-scoped key/value metadata ----------------------------------

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        let k = key.to_string();
        self.read(move |conn| {
            conn.query_row("SELECT value FROM meta WHERE key=?1", params![k], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .map_err(Error::db)
        })
    }

    pub fn meta_delete(&self, key: &str) -> Result<()> {
        let k = key.to_string();
        self.read(move |conn| {
            conn.execute("DELETE FROM meta WHERE key=?1", params![k])?;
            Ok(())
        })
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        let (k, v) = (key.to_string(), value.to_string());
        self.read(move |conn| {
            conn.execute(
                "INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![k, v],
            )?;
            Ok(())
        })
    }

    /// Stable per-install identifier, used in log lines and audit rows so one
    /// operator can tell two deployments apart. Never derived from a secret.
    pub fn install_id(&self) -> Result<String> {
        if let Some(existing) = self.meta_get("install_id")? {
            return Ok(existing);
        }
        let fresh = security::random_urlsafe(8);
        self.meta_set("install_id", &fresh)?;
        Ok(fresh)
    }

    /// Seconds until the next housekeeping pass should run; 0 means now.
    pub fn housekeeping_due(&self, every_secs: i64) -> Result<bool> {
        let last: i64 = self
            .meta_get("last_housekeeping")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        Ok(time::now() - last >= every_secs)
    }

    pub fn mark_housekeeping(&self) -> Result<()> {
        self.meta_set("last_housekeeping", &time::now().to_string())
    }
}

/// Structural JSON equality (key order and whitespace must not count as a change).
/// Create the parent `guilds` row if it is missing, without clobbering a name the
/// bot already learned. Both the settings writer and the bot's sync path need it,
/// and doing it in two places is how a foreign-key race becomes a 500.
pub(crate) fn ensure_guild(conn: &Connection, guild: Snowflake, at: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO guilds(id,name,updated_at) VALUES(?1,'?',?2)",
        params![guild.as_i64(), at],
    )?;
    Ok(())
}

fn json_equal(a: &str, b: &str) -> bool {
    match (
        serde_json::from_str::<Value>(a),
        serde_json::from_str::<Value>(b),
    ) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

fn chain_hash(
    prev: Option<&str>,
    guild: Snowflake,
    feature: &str,
    payload: &str,
    at: i64,
) -> String {
    security::sha256_hex(format!(
        "{}|{}|{}|{}|{}",
        prev.unwrap_or("genesis"),
        guild.as_i64(),
        feature,
        at,
        payload
    ))
}

/// Recompute the chain to prove no history row was edited out of band.
pub fn verify_history_chain(conn: &Connection, guild: Snowflake) -> Result<u32> {
    let mut st = conn
        .prepare("SELECT at, feature, before, after, hash FROM config_history WHERE guild_id=?1 ORDER BY id ASC")
        .map_err(Error::db)?;
    let rows = st
        .query_map(params![guild.as_i64()], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })
        .map_err(Error::db)?;
    let mut prev: Option<String> = None;
    let mut n = 0u32;
    for row in rows {
        let (at, feature, _before, after, hash) = row.map_err(Error::db)?;
        let expected = chain_hash(prev.as_deref(), guild, &feature, &after, at);
        if !security::ct_eq(&expected, &hash) {
            return Err(Error::Db(format!(
                "config history broken at entry {n} for guild {guild}"
            )));
        }
        prev = Some(hash);
        n += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)] // partial-patch semantics are the point
    use super::*;
    use crate::db::testutil::temp_db;
    use crate::models::AiConfig;

    fn g() -> Snowflake {
        Snowflake(1001)
    }

    #[test]
    fn missing_document_yields_defaults() {
        let (db, _d) = temp_db();
        let cfg: ModConfig = db.settings(g(), Feature::Moderation).unwrap();
        assert_eq!(cfg.purge_max, ModConfig::default().purge_max);
        assert!(
            !db.settings::<AiConfig>(g(), Feature::AiFilter)
                .unwrap()
                .enabled
        );
        assert!(db.raw_settings(g(), Feature::Moderation).unwrap().is_none());
    }

    #[test]
    fn put_then_read_roundtrips_every_feature() {
        let (db, _d) = temp_db();
        db.tx(|c| {
            c.execute(
                "INSERT INTO guilds(id,name,updated_at) VALUES(?1,'g',0)",
                params![g().as_i64()],
            )?;
            Ok(())
        })
        .unwrap();

        let mut ai = AiConfig::default();
        // Deliberate field assignment rather than struct-literal syntax: this is the
        // *partial patch* semantics under test — everything else keeps its default.
        ai.enabled = true;
        ai.min_severity = crate::models::Severity::High;
        ai.categories = vec!["profanity".into(), "spam".into()];
        db.put_ai_config(g(), &ai, Some(Snowflake(7))).unwrap();
        let back: AiConfig = db.settings(g(), Feature::AiFilter).unwrap();
        assert!(back.enabled);
        assert_eq!(back.min_severity, crate::models::Severity::High);
        assert_eq!(back.categories, ai.categories);

        let mut raid = RaidConfig::default();
        raid.join_threshold = 25;
        db.put_raid_config(g(), &raid, None).unwrap();
        assert_eq!(db.raid_config(g()).unwrap().join_threshold, 25);
    }

    #[test]
    fn identical_write_is_a_noop_and_bumps_nothing() {
        let (db, _d) = temp_db();
        let cfg = ModConfig {
            purge_max: 55,
            ..Default::default()
        };
        db.put_mod_config(g(), &cfg, None).unwrap();
        let epoch1 = db
            .config_epochs()
            .unwrap()
            .into_iter()
            .find(|(i, _)| *i == g())
            .unwrap()
            .1;
        db.put_mod_config(g(), &cfg, None).unwrap();
        let epoch2 = db
            .config_epochs()
            .unwrap()
            .into_iter()
            .find(|(i, _)| *i == g())
            .unwrap()
            .1;
        assert_eq!(epoch1, epoch2, "no-op writes must not wake the gateway");
        assert_eq!(db.settings_history(g(), 10).unwrap().len(), 1);
    }

    #[test]
    fn key_order_does_not_count_as_a_change() {
        let (db, _d) = temp_db();
        db.put_settings(
            g(),
            Feature::Moderation,
            &serde_json::json!({"a":1,"b":2}),
            None,
        )
        .unwrap();
        db.put_settings(
            g(),
            Feature::Moderation,
            &serde_json::json!({"b":2,"a":1}),
            None,
        )
        .unwrap();
        assert_eq!(
            db.settings_history(g(), 10).unwrap().len(),
            1,
            "reordering keys is not a change"
        );
    }

    #[test]
    fn corrupt_document_falls_back_to_defaults() {
        let (db, _d) = temp_db();
        db.tx(|c| {
            c.execute("INSERT INTO guilds(id,name,updated_at) VALUES(?1,'g',0)", params![g().as_i64()])?;
            c.execute(
                "INSERT INTO guild_config(guild_id,feature,payload,updated_at) VALUES(?1,'ai_filter','{not json',0)",
                params![g().as_i64()],
            )?;
            Ok(())
        })
        .unwrap();
        let cfg: AiConfig = db.settings(g(), Feature::AiFilter).unwrap();
        assert_eq!(cfg, AiConfig::default(), "must degrade, not crash");
    }

    #[test]
    fn history_chain_verifies_and_detects_tampering() {
        let (db, _d) = temp_db();
        for i in 0..4 {
            let cfg = ModConfig {
                purge_max: 10 + i,
                ..Default::default()
            };
            db.put_mod_config(g(), &cfg, None).unwrap();
        }
        let n = db.read(|c| super::verify_history_chain(c, g())).unwrap();
        assert_eq!(n, 4);

        // Edit one payload out of band: the chain must catch it.
        db.tx(|c| {
            c.execute(
                "UPDATE guild_config SET payload='{\"purge_max\":999}' WHERE guild_id=?1",
                params![g().as_i64()],
            )?;
            c.execute(
                "UPDATE config_history SET after='{\"purge_max\":999}' WHERE guild_id=?1 AND id=(SELECT MAX(id) FROM config_history WHERE guild_id=?1)",
                params![g().as_i64()],
            )?;
            Ok(())
        })
        .unwrap();
        let e = db
            .read(|c| super::verify_history_chain(c, g()))
            .unwrap_err();
        assert!(e.to_string().contains("broken"), "{e}");
    }

    #[test]
    fn deleting_settings_restores_defaults_and_bumps_epoch() {
        let (db, _d) = temp_db();
        db.put_sticky_config(
            g(),
            &StickyConfig {
                cooldown_secs: 999,
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let before = db
            .config_epochs()
            .unwrap()
            .into_iter()
            .find(|(i, _)| *i == g())
            .unwrap()
            .1;
        db.delete_settings(g(), Feature::Sticky).unwrap();
        assert_eq!(db.sticky_config(g()).unwrap(), StickyConfig::default());
        let after = db
            .config_epochs()
            .unwrap()
            .into_iter()
            .find(|(i, _)| *i == g())
            .unwrap()
            .1;
        assert_eq!(after, before + 1);
    }

    #[test]
    fn guild_sync_creates_and_marks_absent() {
        let (db, _d) = temp_db();
        let g = Snowflake(900);
        assert!(db.guilds().unwrap().is_empty());
        assert!(db.guild_state(g).unwrap().is_none());
        db.sync_guild(g, "Aerona HQ", Some("icon.png"), 1234)
            .unwrap();
        let list = db.guilds().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Aerona HQ");
        assert_eq!(list[0].member_count, 1234);
        assert!(list[0].bot_present);
        assert!(!list[0].manageable, "resolved by the caller, never here");
        // A settings write before any sync must not lose the real name later.
        db.put_mod_config(g, &ModConfig::default(), None).unwrap();
        db.sync_guild(g, "Renamed", None, 0).unwrap();
        assert_eq!(db.guilds().unwrap()[0].name, "Renamed");
        assert_eq!(db.guilds().unwrap()[0].icon, None, "None means cleared");
        assert!(db.guild_state(g).unwrap().unwrap().bot_present);
        db.mark_guild_absent(g).unwrap();
        assert!(!db.guilds().unwrap()[0].bot_present);
        // Negative counts are a bot bug, not a schema problem.
        db.sync_guild(g, "x", None, -5).unwrap();
        assert_eq!(db.guilds().unwrap()[0].member_count, 0);
        // Forgetting the guild takes its settings with it (cascade), and the
        // dashboard then has nothing to show.
        db.forget_guild(g).unwrap();
        assert!(db.guilds().unwrap().is_empty());
        assert!(db.raw_settings(g, Feature::Moderation).unwrap().is_none());
    }

    #[test]
    fn settings_before_guild_sync_still_work() {
        // The FK-safe `ensure_guild` path: a fresh install saving config first.
        let (db, _d) = temp_db();
        let g = Snowflake(901);
        db.put_ticket_config(g, &TicketConfig::default(), None)
            .unwrap();
        let list = db.guilds().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(
            list[0].name, "?",
            "placeholder until the bot syncs the real name"
        );
        assert!(!list[0].bot_present, "never synced");
        db.sync_guild(g, "Real Name", None, 10).unwrap();
        assert!(db.guilds().unwrap()[0].bot_present);
        assert_eq!(db.guilds().unwrap()[0].name, "Real Name");
    }

    #[test]
    fn meta_and_install_id() {
        let (db, _d) = temp_db();
        assert!(db.meta_get("nope").unwrap().is_none());
        db.meta_set("k", "v").unwrap();
        db.meta_set("k", "v2").unwrap();
        assert_eq!(db.meta_get("k").unwrap().as_deref(), Some("v2"));
        let id = db.install_id().unwrap();
        assert!(!id.is_empty() && id.len() >= 8);
        assert_eq!(db.install_id().unwrap(), id, "install id must be stable");
    }

    #[test]
    fn housekeeping_gate() {
        let (db, _d) = temp_db();
        assert!(
            db.housekeeping_due(3600).unwrap(),
            "never-run means due now"
        );
        db.mark_housekeeping().unwrap();
        assert!(!db.housekeeping_due(3600).unwrap());
        assert!(
            db.housekeeping_due(0).unwrap(),
            "every_secs 0 must always run"
        );
    }
}
