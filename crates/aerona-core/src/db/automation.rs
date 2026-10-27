//! Automation and shared runtime records: users, levels, anti-raid events, the
//! AI verdict cache, and the stats series.

use rusqlite::{params, OptionalExtension};
use serde_json::json;

use crate::error::{Error, Result};
use crate::identity::Snowflake;
#[cfg(test)]
use crate::models::FilterAction;
use crate::models::{AiEvent, RaidEvent, Severity, StatsSnapshot};

use super::{now, Db};

/// `add_xp` returns this so the caller knows whether to announce a level-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XpOutcome {
    pub level: u32,
    pub xp: i64,
    pub leveled_up: bool,
}

impl Db {
    // -- users ---------------------------------------------------------------

    /// Upsert the small identity cache. Names change constantly on Discord, so we
    /// always overwrite them; `first_seen` is preserved.
    pub fn upsert_user(
        &self,
        id: Snowflake,
        username: &str,
        discriminator: &str,
        global_name: Option<&str>,
        avatar: Option<&str>,
        is_bot: bool,
    ) -> Result<()> {
        let u = crate::security::clean(username, 64);
        let d = crate::security::clean(discriminator, 8);
        let gn = global_name.map(|v| crate::security::clean(v, 64));
        let av = avatar.map(|v| crate::security::clean(v, 128));
        let id = id.as_i64();
        let bot = i64::from(is_bot);
        let at = now();
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO users(id,username,discriminator,global_name,avatar,is_bot,first_seen,last_seen)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?7)
                 ON CONFLICT(id) DO UPDATE SET
                   username=excluded.username,
                   discriminator=excluded.discriminator,
                   global_name=COALESCE(excluded.global_name, users.global_name),
                   avatar=excluded.avatar,
                   is_bot=excluded.is_bot,
                   last_seen=excluded.last_seen",
                params![id, u, d, gn, av, bot, at],
            )?;
            Ok(())
        })
    }

    pub fn user(&self, id: Snowflake) -> Result<Option<serde_json::Value>> {
        let id_i = id.as_i64();
        let id_s = id.to_string();
        self.read(move |conn| {
            conn
                .query_row(
                    "SELECT id, username, discriminator, global_name, avatar, is_bot, first_seen, last_seen
                     FROM users WHERE id=?1",
                    params![id_i],
                    |r| {
                        Ok(json!({
                            "id": id_s.clone(),
                            "username": r.get::<_, String>(1)?,
                            "discriminator": r.get::<_, String>(2)?,
                            "globalName": r.get::<_, Option<String>>(3)?,
                            "avatar": r.get::<_, Option<String>>(4)?,
                            "bot": r.get::<_, i64>(5)? != 0,
                            "firstSeen": r.get::<_, i64>(6)?,
                            "lastSeen": r.get::<_, i64>(7)?,
                        }))
                    },
                )
                .optional().map_err(Error::db)
        })
    }

    pub fn user_count(&self) -> Result<i64> {
        self.read(|conn| {
            Ok(conn
                .query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, i64>(0))
                .unwrap_or(0))
        })
    }

    // -- levels --------------------------------------------------------------

    /// Cumulative XP needed to *reach* `level`: level 1 costs 100, level 2 costs
    /// 200 more, and so on - `total(l) = 100 * l(l+1) / 2`. Both directions are
    /// derived from this single definition so a level-up can never disagree with
    /// the threshold shown in the dashboard.
    pub fn xp_for_level(level: u32) -> i64 {
        100 * i64::from(level) * (i64::from(level) + 1) / 2
    }

    /// Highest level whose cumulative threshold `xp` has met. Iterative rather
    /// than a closed-form square root: no float rounding at level boundaries,
    /// and at most `u16` iterations even for absurd XP.
    pub fn level_for_xp(xp: i64) -> u32 {
        let mut level = 0u32;
        while level < 10_000 && xp >= Self::xp_for_level(level + 1) {
            level += 1;
        }
        level
    }

    /// XP still needed to reach `level + 1` from `xp`.
    pub fn xp_to_next_level(xp: i64) -> i64 {
        let level = i64::from(Self::level_for_xp(xp));
        (Self::xp_for_level(level as u32 + 1) - xp).max(0)
    }

    pub fn add_xp(&self, guild: Snowflake, user: Snowflake, xp: i64) -> Result<XpOutcome> {
        let (g, u) = (guild.as_i64(), user.as_i64());
        let amount = xp.clamp(0, 1000);
        let at = now();
        self.tx(move |conn| {
            // RETURNING xp - reading `level` here would return the *stored* level,
            // which is stale until we have recomputed it.
            let (new_xp, prev_level): (i64, i64) = conn
                .query_row(
                    "INSERT INTO levels(guild_id,user_id,xp,level,messages,updated_at)
                     VALUES(?1,?2,?3,0,1,?4)
                     ON CONFLICT(guild_id,user_id) DO UPDATE SET
                       xp = levels.xp + excluded.xp,
                       messages = levels.messages + 1,
                       updated_at = excluded.updated_at
                     RETURNING xp, level",
                    params![g, u, amount, at],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                )
                .unwrap_or((0, 0));
            let new_level = i64::from(Self::level_for_xp(new_xp));
            let leveled_up = new_level > prev_level;
            if leveled_up {
                conn.execute(
                    "UPDATE levels SET level=?3 WHERE guild_id=?1 AND user_id=?2",
                    params![g, u, new_level],
                )?;
            }
            Ok(XpOutcome {
                level: new_level as u32,
                xp: new_xp,
                leveled_up,
            })
        })
    }

    pub fn level(&self, guild: Snowflake, user: Snowflake) -> Result<(u32, i64, i64)> {
        let (g, u) = (guild.as_i64(), user.as_i64());
        self.read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT level, xp, messages FROM levels WHERE guild_id=?1 AND user_id=?2",
                    params![g, u],
                    |r| Ok((r.get::<_, i64>(0)? as u32, r.get(1)?, r.get(2)?)),
                )
                .unwrap_or((0, 0, 0)))
        })
    }

    pub fn level_leaderboard(
        &self,
        guild: Snowflake,
        limit: u32,
    ) -> Result<Vec<serde_json::Value>> {
        let g = guild.as_i64();
        let limit = limit.clamp(1, 100) as i64;
        self.read(move |conn| {
            let mut st = conn
                .prepare(
                    "SELECT l.user_id, l.level, l.xp, l.messages, COALESCE(u.username, 'unknown')
                     FROM levels l LEFT JOIN users u ON u.id = l.user_id
                     WHERE l.guild_id=?1 ORDER BY l.xp DESC, l.user_id LIMIT ?2",
                )
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![g, limit], |r| {
                    Ok(json!({
                        "userId": r.get::<_, i64>(0)?.to_string(),
                        "level": r.get::<_, i64>(1)?,
                        "xp": r.get::<_, i64>(2)?,
                        "messages": r.get::<_, i64>(3)?,
                        "name": r.get::<_, String>(4)?,
                    }))
                })
                .map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::db)
        })
    }

    pub fn level_rank(&self, guild: Snowflake, user: Snowflake) -> Result<i64> {
        let (g, u) = (guild.as_i64(), user.as_i64());
        self.read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT COUNT(*) + 1 FROM levels WHERE guild_id=?1 AND xp > (SELECT xp FROM levels WHERE guild_id=?1 AND user_id=?2)",
                    params![g, u],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0))
        })
    }

    // -- anti-raid -----------------------------------------------------------

    /// Open a raid event, or return the live one. `ON CONFLICT … RETURNING` keeps
    /// "already in a raid" from becoming a race between two detections.
    pub fn raid_event_begin(
        &self,
        guild: Snowflake,
        trigger: &str,
        peak: i64,
        window_secs: i64,
    ) -> Result<(i64, bool)> {
        let g = guild.as_i64();
        let t = crate::security::clean(trigger, 40);
        let at = now();
        self.tx(move |conn| {
            let existing: Option<i64> = conn
                .query_row("SELECT id FROM raid_events WHERE guild_id=?1 AND ended_at IS NULL", params![g], |r| r.get(0))
                .optional().map_err(Error::db)?;
            if let Some(id) = existing {
                conn.execute("UPDATE raid_events SET peak = MAX(peak, ?2) WHERE id=?1", params![id, peak])?;
                return Ok((id, false));
            }
            conn.execute(
                "INSERT INTO raid_events(guild_id,started_at,trigger,peak,window_secs) VALUES(?1,?2,?3,?4,?5)",
                params![g, at, t, peak, window_secs],
            )?;
            Ok((conn.last_insert_rowid(), true))
        })
    }

    pub fn raid_event_update(
        &self,
        id: i64,
        peak: i64,
        flagged: i64,
        kicked: i64,
        banned: i64,
        actions: &serde_json::Value,
    ) -> Result<()> {
        let acts = crate::security::clean(&actions.to_string(), 4000);
        self.tx(move |conn| {
            conn.execute(
                "UPDATE raid_events SET peak=MAX(peak,?2), flagged=?3, kicked=?4, banned=?5, actions=?6 WHERE id=?1",
                params![id, peak, flagged, kicked, banned, acts],
            )?;
            Ok(())
        })
    }

    pub fn raid_event_end(&self, id: i64) -> Result<()> {
        let at = now();
        self.tx(move |conn| {
            conn.execute(
                "UPDATE raid_events SET ended_at=?2 WHERE id=?1",
                params![id, at],
            )?;
            Ok(())
        })
    }

    pub fn active_raid(&self, guild: Snowflake) -> Result<Option<RaidEvent>> {
        let g = guild.as_i64();
        self.read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT id, guild_id, started_at, ended_at, trigger, peak, window_secs, flagged, kicked, banned, actions
                     FROM raid_events WHERE guild_id=?1 AND ended_at IS NULL ORDER BY started_at DESC LIMIT 1",
                    params![g],
                    raid_from_row,
                )
                .ok())
        })
    }

    pub fn raid_history(&self, guild: Snowflake, limit: u32) -> Result<Vec<RaidEvent>> {
        let g = guild.as_i64();
        let limit = limit.clamp(1, 100) as i64;
        self.read(move |conn| {
            let mut st = conn
                .prepare(
                    "SELECT id, guild_id, started_at, ended_at, trigger, peak, window_secs, flagged, kicked, banned, actions
                     FROM raid_events WHERE guild_id=?1 ORDER BY started_at DESC LIMIT ?2",
                )
                .map_err(Error::db)?;
            let rows = st.query_map(params![g, limit], raid_from_row).map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::db)
        })
    }

    // -- AI custom rules (guild-local pre-filter) ----------------------------

    /// Add or update a local rule. `UNIQUE (guild, pattern, mode)` means the
    /// dashboard can save the same rule twice without duplicating it, and the
    /// pattern is validated *before* it reaches the database - an unusable regex
    /// stored as a rule would be evaluated on every message forever.
    pub fn put_custom_rule(
        &self,
        guild: Snowflake,
        rule: &crate::models::AiCustomRule,
    ) -> Result<i64> {
        use crate::models::AiCustomMode;
        use crate::text;
        // Length is checked on the *raw* input: `clean` truncates, and a silently
        // truncated pattern would store a rule that matches something other than
        // what the operator typed. A pattern is code, not prose - refuse it.
        if rule.pattern.chars().count() > 512 {
            return Err(Error::Invalid(
                "patterns are limited to 512 characters".into(),
            ));
        }
        let pattern = crate::security::clean(&rule.pattern, 512);
        if pattern.is_empty() {
            return Err(Error::Invalid("rule pattern must not be empty".into()));
        }
        let mode = if rule.is_regex {
            AiCustomMode::Regex
        } else {
            rule.mode
        };
        text::validate_pattern(&pattern, mode).map_err(Error::Invalid)?;
        let category = crate::security::clean(
            if rule.category.is_empty() {
                "custom"
            } else {
                &rule.category
            },
            32,
        );
        let description = crate::security::clean(&rule.description, 200);
        let action = rule.action.as_str().to_string();
        let sev = i64::from(rule.severity.rank());
        let strikes = i64::from(rule.strikes.min(20));
        let enabled = i64::from(rule.enabled);
        let is_regex = i64::from(mode == AiCustomMode::Regex);
        let mode_s = mode.as_str().to_string();
        let g = guild.as_i64();
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO ai_custom_rules(guild_id,pattern,mode,is_regex,category,severity,action,strikes,description,enabled,seq)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,(SELECT COALESCE(MAX(seq),0)+1 FROM ai_custom_rules WHERE guild_id=?1))
                 ON CONFLICT(guild_id,pattern,mode) DO UPDATE SET
                   is_regex=excluded.is_regex, category=excluded.category, severity=excluded.severity,
                   action=excluded.action, strikes=excluded.strikes, description=excluded.description,
                   enabled=excluded.enabled",
                params![g, pattern, mode_s, is_regex, category, sev, action, strikes, description, enabled],
            )?;
            let id = conn.query_row(
                "SELECT id FROM ai_custom_rules WHERE guild_id=?1 AND pattern=?2 AND mode=?3",
                params![g, pattern, mode_s],
                |r| r.get::<_, i64>(0),
            )?;
            Ok(id)
        })
    }

    pub fn custom_rules(&self, guild: Snowflake) -> Result<Vec<crate::models::AiCustomRule>> {
        use crate::models::{AiCustomMode, FilterAction, Severity};
        let g = guild.as_i64();
        self.read(move |conn| {
            let mut st = conn
                .prepare(
                    "SELECT id, pattern, mode, is_regex, category, severity, action, strikes, description, enabled, hits
                     FROM ai_custom_rules WHERE guild_id=?1 ORDER BY seq ASC, id ASC",
                )
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![g], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)? != 0,
                        r.get::<_, String>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, i64>(7)?,
                        r.get::<_, String>(8)?,
                        r.get::<_, i64>(9)? != 0,
                        r.get::<_, i64>(10)?,
                    ))
                })
                .map_err(Error::db)?;
            let mut out = Vec::new();
            for row in rows {
                let (id, pattern, mode, is_regex, category, severity, action, strikes, description, enabled, hits) =
                    row.map_err(Error::db)?;
                out.push(crate::models::AiCustomRule {
                    id,
                    pattern,
                    is_regex,
                    mode: AiCustomMode::parse(&mode).unwrap_or_default(),
                    category,
                    severity: Severity::from_u8(severity.clamp(0, 4) as u8),
                    action: FilterAction::parse(&action).unwrap_or(FilterAction::Flag),
                    strikes: strikes.clamp(0, 255) as u8,
                    description,
                    enabled,
                    hits,
                });
            }
            Ok(out)
        })
    }

    /// Toggle without a read-modify-write: the dashboard's switch must not be able
    /// to clobber an edit that landed a moment earlier.
    pub fn set_custom_rule_enabled(&self, guild: Snowflake, id: i64, enabled: bool) -> Result<()> {
        let g = guild.as_i64();
        let v = i64::from(enabled);
        self.tx(move |conn| {
            let n = conn.execute(
                "UPDATE ai_custom_rules SET enabled=?3 WHERE guild_id=?1 AND id=?2",
                params![g, id, v],
            )?;
            if n == 0 {
                return Err(Error::NotFound("rule"));
            }
            Ok(())
        })
    }

    pub fn delete_custom_rule(&self, guild: Snowflake, id: i64) -> Result<bool> {
        let g = guild.as_i64();
        self.tx(move |conn| {
            Ok(conn.execute(
                "DELETE FROM ai_custom_rules WHERE guild_id=?1 AND id=?2",
                params![g, id],
            )? > 0)
        })
    }

    pub fn count_custom_rules(&self, guild: Snowflake) -> Result<(i64, i64)> {
        let g = guild.as_i64();
        self.read(move |conn| {
            Ok((
                conn.query_row(
                    "SELECT COUNT(*) FROM ai_custom_rules WHERE guild_id=?1",
                    params![g],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0),
                conn.query_row(
                    "SELECT COUNT(*) FROM ai_custom_rules WHERE guild_id=?1 AND enabled=1",
                    params![g],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0),
            ))
        })
    }

    // -- AI verdict cache ----------------------------------------------------

    /// `hash` is SHA-256 of `author + normalized text`. A hit means we skip the
    /// provider entirely - both a latency win and the difference between 1 and
    /// 1000 billed units during a repost flood.
    pub fn cached_verdict(
        &self,
        guild: Snowflake,
        author: Snowflake,
        hash: &str,
    ) -> Result<Option<Vec<AiEvent>>> {
        let (g, a, h) = (guild.as_i64(), author.as_i64(), hash.to_string());
        let at = now();
        self.read(move |conn| {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT verdict FROM ai_verdict_cache
                     WHERE guild_id=?1 AND author_id=?2 AND hash=?3 AND expires_at > ?4",
                    params![g, a, h, at],
                    |r| r.get(0),
                )
                .optional()
                .map_err(Error::db)?;
            match raw {
                None => Ok(None),
                Some(v) => Ok(serde_json::from_str::<Vec<AiEvent>>(&v).ok()),
            }
        })
    }

    pub fn cache_verdict(
        &self,
        guild: Snowflake,
        author: Snowflake,
        hash: &str,
        verdict: &[AiEvent],
        ttl: i64,
    ) -> Result<()> {
        let (g, a, h) = (guild.as_i64(), author.as_i64(), hash.to_string());
        let payload = serde_json::to_string(verdict).unwrap_or_else(|_| "[]".into());
        let expires = now() + ttl.max(5);
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO ai_verdict_cache(guild_id,author_id,hash,verdict,expires_at)
                 VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(guild_id,author_id,hash) DO UPDATE SET verdict=excluded.verdict, expires_at=excluded.expires_at",
                params![g, a, h, payload, expires],
            )?;
            Ok(())
        })
    }

    pub fn log_ai_event(&self, e: &AiEvent) -> Result<()> {
        let detail = crate::security::clean(&e.detail, 500);
        let at = if e.created_at > 0 {
            e.created_at
        } else {
            now()
        };
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO ai_events(guild_id,user_id,channel_id,message_id,category,severity,score,action,at,detail)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    e.guild_id.as_i64(),
                    e.user_id.as_i64(),
                    e.channel_id.as_i64(),
                    e.message_id.map(|m| m.as_i64()),
                    e.category,
                    i64::from(e.severity.rank()),
                    e.score,
                    e.action.as_str(),
                    at,
                    detail
                ],
            )?;
            Ok(())
        })
    }

    pub fn recent_ai_events(&self, guild: Snowflake, limit: u32) -> Result<Vec<serde_json::Value>> {
        let g = guild.as_i64();
        let limit = limit.clamp(1, 200) as i64;
        self.read(move |conn| {
            let mut st = conn
                .prepare(
                    "SELECT e.id, e.user_id, e.channel_id, e.message_id, e.category, e.severity, e.score, e.action, e.at, e.detail,
                            COALESCE(u.username,'?')
                     FROM ai_events e LEFT JOIN users u ON u.id = e.user_id
                     WHERE e.guild_id=?1 ORDER BY e.at DESC LIMIT ?2",
                )
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![g, limit], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "userId": r.get::<_, i64>(1)?.to_string(),
                        "channelId": r.get::<_, i64>(2)?.to_string(),
                        "messageId": r.get::<_, Option<i64>>(3)?.map(|v| v.to_string()),
                        "category": r.get::<_, String>(4)?,
                        "severity": Severity::from_u8(r.get::<_, i64>(5)?.clamp(0, 4) as u8).as_str(),
                        "score": r.get::<_, f64>(6)?,
                        "action": r.get::<_, String>(7)?,
                        "at": r.get::<_, i64>(8)?,
                        "detail": r.get::<_, String>(9)?,
                        "user": r.get::<_, String>(10)?,
                    }))
                })
                .map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::db)
        })
    }

    pub fn ai_event_count_since(&self, guild: Snowflake, since: i64) -> Result<i64> {
        let (g, s) = (guild.as_i64(), since);
        self.read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT COUNT(*) FROM ai_events WHERE guild_id=?1 AND at>=?2",
                    params![g, s],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0))
        })
    }

    // -- stats series --------------------------------------------------------

    pub fn record_stats(&self, s: &StatsSnapshot) -> Result<()> {
        let at = s.at;
        let p = crate::security::clean(&s.process, 16);
        let (rss, cpu, guilds, lat, msg, db_kb, users) = (
            s.rss_kb,
            s.cpu_pct,
            s.guilds,
            s.ws_latency_ms,
            s.messages_per_min,
            s.db_size_kb,
            s.users_seen,
        );
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO stats(at,process,rss_kb,cpu_pct,guilds,ws_latency_ms,messages_per_min,db_size_kb,users_seen)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
                 ON CONFLICT(at,process) DO UPDATE SET
                   rss_kb=excluded.rss_kb, cpu_pct=excluded.cpu_pct, guilds=excluded.guilds,
                   ws_latency_ms=excluded.ws_latency_ms, messages_per_min=excluded.messages_per_min,
                   db_size_kb=excluded.db_size_kb, users_seen=excluded.users_seen",
                params![at, p, rss, cpu, guilds, lat, msg, db_kb, users],
            )?;
            Ok(())
        })
    }

    pub fn stats_series(&self, process: &str, points: u32) -> Result<Vec<StatsSnapshot>> {
        let p = crate::security::clean(process, 16);
        let limit = points.clamp(2, 2000) as i64;
        self.read(move |conn| {
            let mut st = conn
                .prepare(
                    "SELECT at, process, rss_kb, cpu_pct, guilds, ws_latency_ms, messages_per_min, db_size_kb, users_seen
                     FROM (SELECT * FROM stats WHERE process=?1 ORDER BY at DESC LIMIT ?2) ORDER BY at ASC",
                )
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![p, limit], |r| {
                    Ok(StatsSnapshot {
                        at: r.get(0)?,
                        process: r.get(1)?,
                        rss_kb: r.get(2)?,
                        cpu_pct: r.get(3)?,
                        guilds: r.get(4)?,
                        ws_latency_ms: r.get(5)?,
                        messages_per_min: r.get(6)?,
                        db_size_kb: r.get(7)?,
                        users_seen: r.get(8)?,
                    })
                })
                .map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::db)
        })
    }

    pub fn latest_stats(&self, process: &str) -> Result<Option<StatsSnapshot>> {
        let p = crate::security::clean(process, 16);
        self.read(move |conn| {
            conn
                .query_row(
                    "SELECT at, process, rss_kb, cpu_pct, guilds, ws_latency_ms, messages_per_min, db_size_kb, users_seen
                     FROM stats WHERE process=?1 ORDER BY at DESC LIMIT 1",
                    params![p],
                    |r| {
                        Ok(StatsSnapshot {
                            at: r.get(0)?,
                            process: r.get(1)?,
                            rss_kb: r.get(2)?,
                            cpu_pct: r.get(3)?,
                            guilds: r.get(4)?,
                            ws_latency_ms: r.get(5)?,
                            messages_per_min: r.get(6)?,
                            db_size_kb: r.get(7)?,
                            users_seen: r.get(8)?,
                        })
                    },
                )
                .optional().map_err(Error::db)
        })
    }

    // -- retention -----------------------------------------------------------

    /// One pass of retention: expired cache rows, aged-out event/log series.
    /// Returns rows removed per table for the status page.
    pub fn housekeeping(
        &self,
        retention_days: i64,
        log_cache_per_guild: i64,
    ) -> Result<serde_json::Value> {
        let days = retention_days.clamp(1, 365);
        let cutoff = now() - days * 86_400;
        let at = now();
        let mut removed = json!({});
        self.tx(|conn| {
            removed["aiEvents"] =
                json!(conn.execute("DELETE FROM ai_events WHERE at < ?1", params![cutoff])?);
            removed["audit"] =
                json!(conn.execute("DELETE FROM audit_log WHERE at < ?1", params![cutoff])?);
            removed["raidEvents"] = json!(conn.execute(
                "DELETE FROM raid_events WHERE started_at < ?1 AND ended_at IS NOT NULL",
                params![cutoff],
            )?);
            removed["stats"] =
                json!(conn.execute("DELETE FROM stats WHERE at < ?1", params![cutoff])?);
            removed["verdictCache"] = json!(conn.execute(
                "DELETE FROM ai_verdict_cache WHERE expires_at < ?1",
                params![at]
            )?);
            removed["oauthState"] =
                json!(conn.execute("DELETE FROM oauth_state WHERE expires_at < ?1", params![at])?);
            removed["logCache"] = json!(0);
            Ok(())
        })?;
        let n = self.prune_log_messages(log_cache_per_guild)?;
        removed["logCache"] = json!(n);
        // The mod log is bounded by row count per guild rather than age, because a quiet
        // server's audit trail is still an audit trail and a busy one outgrows any window.
        removed["logEvents"] = json!(self.prune_log_events()?);
        self.mark_housekeeping()?;
        Ok(removed)
    }

    /// Guilds the bot no longer serves, for the `purge` CLI. Config is kept
    /// intentionally (re-invite should not lose settings); this only reports.
    pub fn orphan_guilds(&self, present: &[Snowflake]) -> Result<Vec<Snowflake>> {
        let ids: Vec<i64> = present.iter().map(|s| s.as_i64()).collect();
        self.read(move |conn| {
            let mut out = Vec::new();
            let mut st = conn.prepare("SELECT id FROM guilds").map_err(Error::db)?;
            let rows = st
                .query_map([], |r| r.get::<_, i64>(0))
                .map_err(Error::db)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::db)?;
            for id in rows {
                if !ids.contains(&id) {
                    out.push(Snowflake(id as u64));
                }
            }
            Ok(out)
        })
    }
}

fn raid_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RaidEvent> {
    let actions: String = r.get(10)?;
    Ok(RaidEvent {
        id: r.get(0)?,
        guild_id: Snowflake(r.get::<_, i64>(1)? as u64),
        started_at: r.get(2)?,
        ended_at: r.get(3)?,
        trigger: r.get(4)?,
        peak: r.get(5)?,
        window_secs: r.get(6)?,
        flagged: r.get(7)?,
        kicked: r.get(8)?,
        banned: r.get(9)?,
        actions: serde_json::from_str(&actions).unwrap_or_else(|_| json!([])),
    })
}

// Every lookup here uses `rusqlite::OptionalExtension::optional()`: it turns
// `QueryReturnedNoRows` into `Ok(None)` while propagating real errors, which is
// exactly the semantics we want, so there is no custom trait to invent.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::testutil::temp_db;

    const G: i64 = 700;
    fn seed(db: &Db) {
        db.tx(|c| {
            c.execute(
                "INSERT INTO guilds(id,name,updated_at) VALUES(?1,'g',0)",
                params![G],
            )?;
            Ok(())
        })
        .unwrap();
    }
    fn g() -> Snowflake {
        Snowflake(G as u64)
    }

    #[test]
    fn user_upsert_refreshes_name_but_keeps_first_seen() {
        let (db, _d) = temp_db();
        db.upsert_user(
            Snowflake(5),
            "old",
            "0001",
            Some("Old"),
            Some("a.png"),
            false,
        )
        .unwrap();
        let first = db.user(Snowflake(5)).unwrap().unwrap();
        assert_eq!(first["username"], "old");
        let seen = first["firstSeen"].as_i64().unwrap();
        db.upsert_user(Snowflake(5), "new", "0002", None, None, true)
            .unwrap();
        let after = db.user(Snowflake(5)).unwrap().unwrap();
        assert_eq!(after["username"], "new");
        assert_eq!(
            after["firstSeen"].as_i64(),
            Some(seen),
            "first_seen must not move"
        );
        assert_eq!(
            after["globalName"], first["globalName"],
            "None must not erase a known display name"
        );
        assert_eq!(after["bot"], true);
        assert!(db.user(Snowflake(999)).unwrap().is_none());
        assert_eq!(db.user_count().unwrap(), 1);
    }

    #[test]
    fn level_curve_is_monotonic_and_consistent() {
        let mut prev = 0i64;
        for l in 0..40u32 {
            let need = Db::xp_for_level(l + 1);
            assert!(need > prev, "xp_for_level must grow monotonically at {l}");
            // The exact threshold of level n+1 must read as level n+1, and one
            // point less must still be n.
            assert_eq!(Db::level_for_xp(need), l + 1, "boundary at level {}", l + 1);
            assert_eq!(Db::level_for_xp(need - 1), l, "just below level {}", l + 1);
            prev = need;
        }
        assert_eq!(Db::level_for_xp(0), 0);
        assert_eq!(
            Db::level_for_xp(i64::MAX),
            10_000,
            "capped, not infinite-looping"
        );
        assert_eq!(
            Db::xp_to_next_level(150),
            150,
            "level 1 reached at 100, level 2 at 300: 150 remains"
        );
        assert_eq!(
            Db::xp_to_next_level(200),
            100,
            "200 is mid-level-1 (100..300), so 100 remains"
        );
        assert_eq!(
            Db::xp_to_next_level(i64::MAX),
            0,
            "capped level never asks for the unreachable"
        );
    }

    #[test]
    fn add_xp_levels_up_once() {
        let (db, _d) = temp_db();
        seed(&db);
        let u = Snowflake(11);
        let o = db.add_xp(g(), u, 50).unwrap();
        assert_eq!(o.level, 0);
        assert!(!o.leveled_up);
        let o = db.add_xp(g(), u, 60).unwrap();
        assert_eq!(o.xp, 110);
        assert_eq!(o.level, 1, "110 crosses the level-1 threshold of 100");
        assert!(o.leveled_up);
        // Second call at the same level must not re-announce.
        let o = db.add_xp(g(), u, 5).unwrap();
        assert_eq!(o.level, 1);
        assert!(!o.leveled_up);
        let (lvl, xp, msgs) = db.level(g(), u).unwrap();
        assert_eq!((lvl, xp, msgs), (1, 115, 3));
        // Absurd and negative amounts are clamped, not stored.
        db.add_xp(g(), u, i64::MAX).unwrap();
        assert!(db.level(g(), u).unwrap().1 < i64::MAX / 2);
        db.add_xp(g(), u, -500).unwrap();
    }

    #[test]
    fn leaderboard_and_rank() {
        let (db, _d) = temp_db();
        seed(&db);
        for (i, xp) in [(1u64, 10i64), (2, 300), (3, 1200)] {
            db.add_xp(g(), Snowflake(i), xp).unwrap();
            db.upsert_user(Snowflake(i), &format!("u{i}"), "0", None, None, false)
                .unwrap();
        }
        let board = db.level_leaderboard(g(), 10).unwrap();
        assert_eq!(board.len(), 3);
        assert_eq!(board[0]["name"], "u3", "highest xp first");
        assert_eq!(db.level_rank(g(), Snowflake(3)).unwrap(), 1);
        assert_eq!(db.level_rank(g(), Snowflake(1)).unwrap(), 3);
        assert_eq!(
            db.level_leaderboard(Snowflake(999), 10).unwrap().len(),
            0,
            "per-guild"
        );
    }

    #[test]
    fn raid_events_are_singleton_per_guild() {
        let (db, _d) = temp_db();
        seed(&db);
        let (id, fresh) = db.raid_event_begin(g(), "join_burst", 9, 10).unwrap();
        assert!(fresh);
        let (id2, fresh2) = db.raid_event_begin(g(), "join_burst", 14, 10).unwrap();
        assert_eq!(id, id2, "a second detection must extend, not duplicate");
        assert!(!fresh2);
        db.raid_event_update(id, 14, 22, 5, 2, &json!(["lockdown", "slowmode"]))
            .unwrap();
        let active = db.active_raid(g()).unwrap().expect("active");
        assert_eq!(active.peak, 14, "peak takes the maximum");
        assert_eq!(active.flagged, 22);
        assert_eq!(active.actions.as_array().map(|v| v.len()), Some(2));
        db.raid_event_end(id).unwrap();
        assert!(db.active_raid(g()).unwrap().is_none());
        assert_eq!(db.raid_history(g(), 10).unwrap().len(), 1);
    }

    #[test]
    fn custom_rules_are_validated_and_upserted() {
        use crate::models::{AiCustomMode, AiCustomRule, FilterAction, Severity};
        let (db, _d) = temp_db();
        seed(&db);
        let rule = AiCustomRule {
            id: 0,
            pattern: "badword".into(),
            is_regex: false,
            mode: AiCustomMode::WholeWord,
            category: "slur".into(),
            severity: Severity::High,
            action: FilterAction::Delete,
            strikes: 1,
            description: "obvious slur".into(),
            enabled: true,
            hits: 0,
        };
        let id = db.put_custom_rule(g(), &rule).unwrap();
        let listed = db.custom_rules(g()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].pattern, "badword");
        assert_eq!(listed[0].action, FilterAction::Delete);
        assert_eq!(listed[0].severity, Severity::High);
        assert!(!listed[0].is_regex);
        // Saving the same (pattern, mode) again updates in place.
        db.put_custom_rule(
            g(),
            &AiCustomRule {
                description: "edited".into(),
                enabled: false,
                ..rule.clone()
            },
        )
        .unwrap();
        let listed = db.custom_rules(g()).unwrap();
        assert_eq!(listed.len(), 1, "upsert, not duplicate");
        assert_eq!(listed[0].description, "edited");
        assert_eq!(listed[0].id, id);
        assert!(!listed[0].enabled);
        // Same text with a different mode is a different rule.
        db.put_custom_rule(
            g(),
            &AiCustomRule {
                mode: AiCustomMode::Contains,
                ..rule.clone()
            },
        )
        .unwrap();
        // `rule` had enabled=true; the first was edited to disabled, the clone keeps it.
        assert_eq!(
            db.count_custom_rules(g()).unwrap(),
            (2, 1),
            "one enabled of two"
        );
        db.set_custom_rule_enabled(g(), id, true).unwrap();
        assert_eq!(db.count_custom_rules(g()).unwrap(), (2, 2));
        assert!(matches!(
            db.set_custom_rule_enabled(g(), 999, true),
            Err(Error::NotFound(_))
        ));
        assert!(db.delete_custom_rule(g(), id).unwrap());
        assert!(
            !db.delete_custom_rule(g(), id).unwrap(),
            "second delete is false"
        );
        // Empty and over-long are refused in every mode.
        for bad in ["", "   ", &"x".repeat(900)] {
            let r = db.put_custom_rule(
                g(),
                &AiCustomRule {
                    pattern: bad.into(),
                    ..Default::default()
                },
            );
            assert!(r.is_err(), "{bad:?} must be rejected");
        }
        // A *literal* pattern is never compiled, so regex metacharacters are just
        // characters there - `a{1,99999}` must be accepted as text to look for.
        assert!(
            db.put_custom_rule(
                g(),
                &AiCustomRule {
                    pattern: "a{1,99999}".into(),
                    mode: AiCustomMode::Contains,
                    ..Default::default()
                }
            )
            .is_ok(),
            "literal rules must not be regex-validated"
        );
        // Declaring a rule as a regex switches on the ReDoS and syntax guards.
        for bad in ["(unclosed", "(?<!x)foo", "(a+)+b", "a{1,999999}"] {
            let r = db.put_custom_rule(
                g(),
                &AiCustomRule {
                    pattern: bad.into(),
                    is_regex: true,
                    ..Default::default()
                },
            );
            assert!(r.is_err(), "regex {bad:?} must be rejected, got {r:?}");
        }
        assert!(
            db.put_custom_rule(
                g(),
                &AiCustomRule {
                    pattern: r"\d{1,3}\.\d{1,3}".into(),
                    is_regex: true,
                    ..Default::default()
                }
            )
            .is_ok(),
            "bounded repetition is legitimate"
        );
        // Rules are per guild.
        let other = Snowflake(901);
        db.tx(|c| {
            c.execute(
                "INSERT INTO guilds(id,name,updated_at) VALUES(901,'o',0)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        assert!(db.custom_rules(other).unwrap().is_empty());
        db.put_custom_rule(
            other,
            &AiCustomRule {
                pattern: "z".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(db.custom_rules(other).unwrap().len(), 1);
        // Surviving rows for guild G: `badword`/Contains, the literal
        // `a{1,99999}`/Contains and the bounded regex. The `badword`/WholeWord row
        // was deleted above. None of them leaked into the other guild.
        let mine: Vec<String> = db
            .custom_rules(g())
            .unwrap()
            .into_iter()
            .map(|r| r.pattern)
            .collect();
        assert_eq!(
            mine,
            vec!["badword", "a{1,99999}", r"\d{1,3}\.\d{1,3}"],
            "the first guild keeps its own"
        );
    }

    #[test]
    fn rule_order_is_stable_and_survives_edits() {
        use crate::models::{AiCustomMode, AiCustomRule};
        let (db, _d) = temp_db();
        seed(&db);
        for p in ["one", "two", "three"] {
            db.put_custom_rule(
                g(),
                &AiCustomRule {
                    pattern: p.into(),
                    mode: AiCustomMode::Contains,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let order: Vec<String> = db
            .custom_rules(g())
            .unwrap()
            .into_iter()
            .map(|r| r.pattern)
            .collect();
        assert_eq!(
            order,
            vec!["one", "two", "three"],
            "insertion order, not id order"
        );
        // Editing a rule keeps its position (it must not jump to the end).
        db.put_custom_rule(
            g(),
            &AiCustomRule {
                pattern: "one".into(),
                mode: AiCustomMode::Contains,
                description: "x".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let order: Vec<String> = db
            .custom_rules(g())
            .unwrap()
            .into_iter()
            .map(|r| r.pattern)
            .collect();
        assert_eq!(order, vec!["one", "two", "three"]);
    }

    #[test]
    fn verdict_cache_expiry_and_scoping() {
        let (db, _d) = temp_db();
        seed(&db);
        let ev = vec![AiEvent {
            id: 0,
            guild_id: g(),
            user_id: Snowflake(1),
            channel_id: Snowflake(2),
            message_id: None,
            category: "profanity".into(),
            severity: Severity::High,
            score: 0.85,
            action: FilterAction::Delete,
            created_at: 0,
            detail: "sexual".into(),
        }];
        assert!(db
            .cached_verdict(g(), Snowflake(1), "h1")
            .unwrap()
            .is_none());
        db.cache_verdict(g(), Snowflake(1), "h1", &ev, 60).unwrap();
        let hit = db
            .cached_verdict(g(), Snowflake(1), "h1")
            .unwrap()
            .expect("hit");
        assert_eq!(hit[0].category, "profanity");
        assert!(
            db.cached_verdict(g(), Snowflake(2), "h1")
                .unwrap()
                .is_none(),
            "per-author"
        );
        assert!(
            db.cached_verdict(g(), Snowflake(1), "h2")
                .unwrap()
                .is_none(),
            "per-hash"
        );
        // Expired entries never surface.
        db.tx(|c| {
            c.execute("UPDATE ai_verdict_cache SET expires_at = 1", [])?;
            Ok(())
        })
        .unwrap();
        assert!(db
            .cached_verdict(g(), Snowflake(1), "h1")
            .unwrap()
            .is_none());
        // A zero/negative TTL still floors at five seconds - never "forever stale".
        db.cache_verdict(g(), Snowflake(1), "h3", &ev, 0).unwrap();
        assert!(db
            .cached_verdict(g(), Snowflake(1), "h3")
            .unwrap()
            .is_some());
    }

    #[test]
    fn ai_events_are_truncated_and_readable() {
        let (db, _d) = temp_db();
        seed(&db);
        db.upsert_user(Snowflake(1), "spammer", "0", None, None, false)
            .unwrap();
        db.log_ai_event(&AiEvent {
            id: 0,
            guild_id: g(),
            user_id: Snowflake(1),
            channel_id: Snowflake(2),
            message_id: Some(Snowflake(3)),
            category: "spam".into(),
            severity: Severity::Critical,
            score: 0.99,
            action: FilterAction::DeleteStrike,
            created_at: 0,
            detail: "d".repeat(9000),
        })
        .unwrap();
        let rows = db.recent_ai_events(g(), 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["detail"].as_str().unwrap().chars().count(), 500);
        assert_eq!(rows[0]["user"], "spammer");
        assert_eq!(rows[0]["severity"], "critical");
        assert_eq!(rows[0]["action"], "delete_strike");
        assert!(db.ai_event_count_since(g(), 0).unwrap() >= 1);
    }

    #[test]
    fn stats_series_and_latest() {
        let (db, _d) = temp_db();
        assert!(db.latest_stats("bot").unwrap().is_none());
        for i in 0..10 {
            db.record_stats(&StatsSnapshot {
                at: 1_700_000_000 + i * 30,
                process: "bot".into(),
                rss_kb: 20_000 + i,
                cpu_pct: 0.5 + i as f64 / 10.0,
                guilds: 3,
                ws_latency_ms: 40,
                messages_per_min: 12,
                db_size_kb: 256,
                users_seen: 99,
            })
            .unwrap();
        }
        let s = db.stats_series("bot", 5).unwrap();
        assert_eq!(s.len(), 5, "limited");
        assert!(s.windows(2).all(|w| w[0].at < w[1].at), "ascending");
        let l = db.latest_stats("bot").unwrap().unwrap();
        assert_eq!(l.rss_kb, 20_009);
        // Same (at, process) overwrites instead of duplicating.
        db.record_stats(&StatsSnapshot {
            at: 1_700_000_270,
            process: "bot".into(),
            rss_kb: 1,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(db.row_count("stats").unwrap(), 10);
    }

    #[test]
    fn housekeeping_prunes_and_stamps() {
        let (db, _d) = temp_db();
        seed(&db);
        for i in 0..5 {
            db.log_ai_event(&AiEvent {
                id: 0,
                guild_id: g(),
                user_id: Snowflake(1),
                channel_id: Snowflake(2),
                message_id: None,
                category: "c".into(),
                severity: Severity::Low,
                score: 0.1,
                action: FilterAction::Flag,
                created_at: 1_000 + i,
                detail: String::new(),
            })
            .unwrap();
        }
        let out = db.housekeeping(30, 100).unwrap();
        assert_eq!(out["aiEvents"], 5, "all are ancient");
        assert!(db.row_count("ai_events").unwrap() == 0);
        assert!(!db.housekeeping_due(3600).unwrap(), "just ran");
    }

    #[test]
    fn orphan_guilds_are_listed() {
        let (db, _d) = temp_db();
        seed(&db);
        db.tx(|c| {
            c.execute(
                "INSERT INTO guilds(id,name,updated_at) VALUES(701,'gone',0)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let orphans = db.orphan_guilds(&[g()]).unwrap();
        assert_eq!(orphans, vec![Snowflake(701)]);
        assert!(db.orphan_guilds(&[g(), Snowflake(701)]).unwrap().is_empty());
    }
}
