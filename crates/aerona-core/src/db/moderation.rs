//! Moderation records: warns, strikes, kicks, bans, notes and the counts the
//! dashboard's overview tiles read.

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::identity::Snowflake;
use crate::models::WarnRecord;

use super::{now, Db};

/// What happened when a warn was recorded — the bot needs this to decide whether
/// to run the escalation action, and the API needs it for the command reply.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WarnOutcome {
    pub warn_id: i64,
    pub active_count: i64,
    pub strikes: i64,
    /// True when this warn crossed the configured threshold.
    pub escalated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModerationStats {
    pub warns_active: i64,
    pub warns_total: i64,
    pub strikes_active: i64,
    pub bans_total: i64,
    pub kicks_total: i64,
    pub warns_last_24h: i64,
}

impl Db {
    /// Record a warn and, in the same transaction, the strike that may follow it.
    /// `strike_threshold` of `0` disables escalation.
    #[allow(clippy::too_many_arguments)]
    pub fn add_warn(
        &self,
        guild: Snowflake,
        user: Snowflake,
        moderator: Snowflake,
        reason: &str,
        expires_days: i64,
        strike_threshold: i64,
        context_message: Option<Snowflake>,
    ) -> Result<WarnOutcome> {
        let reason = crate::security::scrub(reason, 800);
        if reason.is_empty() {
            return Err(Error::Invalid("a warn needs a reason".into()));
        }
        let at = now();
        let expires_at = if expires_days > 0 {
            at + expires_days * 86_400
        } else {
            0
        };
        let ctx = context_message.map(|s| s.as_i64());
        // One transaction: the warn, its count, and any threshold strike must be
        // atomic, otherwise a crash between them leaves a user escalated without
        // a recorded warn (or the reverse).
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO warns(guild_id,user_id,moderator_id,reason,at,expires_at,active,context_message)
                 VALUES(?1,?2,?3,?4,?5,?6,1,?7)",
                params![guild.as_i64(), user.as_i64(), moderator.as_i64(), reason, at, expires_at, ctx],
            )?;
            let warn_id = conn.last_insert_rowid();
            let active_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM warns
                 WHERE guild_id=?1 AND user_id=?2 AND active=1
                   AND (expires_at = 0 OR expires_at > ?3)",
                params![guild.as_i64(), user.as_i64(), at],
                |r| r.get::<_, i64>(0),
            )?;
            let escalated = strike_threshold > 0 && active_count >= strike_threshold;
            if escalated {
                conn.execute(
                    "INSERT INTO strikes(guild_id,user_id,source,category,reason,at,active)
                     VALUES(?1,?2,'warn','warn-threshold',?3,?4,1)",
                    params![guild.as_i64(), user.as_i64(), format!("{active_count} active warnings"), at],
                )?;
            }
            let strikes: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM strikes
                     WHERE guild_id=?1 AND user_id=?2 AND active=1
                       AND (expires_at = 0 OR expires_at > ?3)",
                    params![guild.as_i64(), user.as_i64(), at],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0);
            Ok(WarnOutcome { warn_id, active_count, strikes, escalated })
        })
    }

    pub fn warns_for(
        &self,
        guild: Snowflake,
        user: Snowflake,
        limit: u32,
    ) -> Result<Vec<WarnRecord>> {
        let limit = limit.clamp(1, 200) as i64;
        self.read(move |conn| {
            let mut st = conn
                .prepare(
                    "SELECT id, guild_id, user_id, moderator_id, reason, at, expires_at, active, pardoned_at, context_message
                     FROM warns WHERE guild_id=?1 AND user_id=?2 ORDER BY at DESC LIMIT ?3",
                )
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![guild.as_i64(), user.as_i64(), limit], WarnRecord::from_row)
                .map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::db)
        })
    }

    pub fn recent_warns(&self, guild: Snowflake, limit: u32) -> Result<Vec<WarnRecord>> {
        let limit = limit.clamp(1, 500) as i64;
        self.read(move |conn| {
            let mut st = conn
                .prepare(
                    "SELECT id, guild_id, user_id, moderator_id, reason, at, expires_at,
                            active, pardoned_at, context_message
                     FROM warns WHERE guild_id=?1 ORDER BY at DESC LIMIT ?2",
                )
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![guild.as_i64(), limit], WarnRecord::from_row)
                .map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::db)
        })
    }

    /// Pardon **one** warn by id (keeps the rest of the history intact).
    pub fn pardon_warn(
        &self,
        guild: Snowflake,
        warn_id: i64,
        moderator: Snowflake,
    ) -> Result<bool> {
        let at = now();
        let _ = moderator; // recorded in the audit log by the caller, not on the row
        self.tx(move |conn| {
            let n = conn.execute(
                "UPDATE warns SET active=0, pardoned_at=?3
                 WHERE id=?1 AND guild_id=?2 AND active=1",
                params![warn_id, guild.as_i64(), at],
            )?;
            Ok(n > 0)
        })
    }

    /// Clear the whole ledger for a user (`!clearwarn`), recording who did it.
    pub fn clear_warns(&self, guild: Snowflake, user: Snowflake) -> Result<u64> {
        self.tx(move |conn| {
            let n = conn.execute(
                "UPDATE warns SET active=0, pardoned_at=?3 WHERE guild_id=?1 AND user_id=?2 AND active=1",
                params![guild.as_i64(), user.as_i64(), now()],
            )?;
            conn.execute(
                "UPDATE strikes SET active=0 WHERE guild_id=?1 AND user_id=?2 AND active=1",
                params![guild.as_i64(), user.as_i64()],
            )?;
            Ok(n as u64)
        })
    }

    pub fn active_warn_count(&self, guild: Snowflake, user: Snowflake) -> Result<i64> {
        let at = now();
        self.read(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM warns
                 WHERE guild_id=?1 AND user_id=?2 AND active=1
                   AND (expires_at = 0 OR expires_at > ?3)",
                params![guild.as_i64(), user.as_i64(), at],
                |r| r.get(0),
            )
            .map_err(Error::db)
        })
    }

    /// Expire warnings (and strikes) whose deadline has passed. Returns rows touched.
    pub fn expire_warns(&self) -> Result<(u64, u64)> {
        let at = now();
        self.tx(move |conn| {
            let w = conn.execute(
                "UPDATE warns SET active=0 WHERE active=1 AND expires_at != 0 AND expires_at <= ?1",
                params![at],
            )? as u64;
            let s = conn.execute(
                "UPDATE strikes SET active=0 WHERE active=1 AND expires_at != 0 AND expires_at <= ?1",
                params![at],
            )? as u64;
            Ok((w, s))
        })
    }

    // -- strikes (independent ledger used by the AI filter) -----------------

    pub fn add_strike(
        &self,
        guild: Snowflake,
        user: Snowflake,
        source: &str,
        category: Option<&str>,
        reason: &str,
        ttl_days: i64,
    ) -> Result<i64> {
        let at = now();
        let expires = if ttl_days > 0 {
            at + ttl_days * 86_400
        } else {
            0
        };
        let reason = crate::security::scrub(reason, 300);
        let source = crate::security::scrub(source, 32);
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO strikes(guild_id,user_id,source,category,reason,at,expires_at,active)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,1)",
                params![guild.as_i64(), user.as_i64(), source, category, reason, at, expires],
            )?;
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM strikes
                     WHERE guild_id=?1 AND user_id=?2 AND active=1 AND (expires_at=0 OR expires_at>?3)",
                    params![guild.as_i64(), user.as_i64(), at],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            Ok(n)
        })
    }

    pub fn active_strikes(&self, guild: Snowflake, user: Snowflake) -> Result<u8> {
        let at = now();
        self.read(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM strikes
                 WHERE guild_id=?1 AND user_id=?2 AND active=1 AND (expires_at=0 OR expires_at>?3)",
                params![guild.as_i64(), user.as_i64(), at],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v.clamp(0, i64::from(u8::MAX)) as u8)
            .map_err(Error::db)
        })
    }

    /// Read the ledger without writing — used by the dashboard's filter tester.
    pub fn strikes_between(
        &self,
        guild: Snowflake,
        since: i64,
        limit: u32,
    ) -> Result<Vec<(Snowflake, i64)>> {
        let limit = limit.clamp(1, 200) as i64;
        self.read(|conn| {
            let mut st = conn
                .prepare(
                    "SELECT user_id, COUNT(*) FROM strikes
                     WHERE guild_id=?1 AND at>=?2 GROUP BY user_id ORDER BY 2 DESC, user_id LIMIT ?3",
                )
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![guild.as_i64(), since, limit], |r| {
                    Ok((Snowflake(r.get::<_, i64>(0)? as u64), r.get::<_, i64>(1)?))
                })
                .map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::db)
        })
    }

    // -- other actions -------------------------------------------------------

    pub fn record_kick(
        &self,
        guild: Snowflake,
        user: Snowflake,
        moderator: Snowflake,
        reason: &str,
    ) -> Result<()> {
        let reason = crate::security::scrub(reason, 500);
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO kicks(guild_id,user_id,moderator_id,reason,at) VALUES(?1,?2,?3,?4,?5)",
                params![
                    guild.as_i64(),
                    user.as_i64(),
                    moderator.as_i64(),
                    reason,
                    now()
                ],
            )?;
            Ok(())
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_ban(
        &self,
        guild: Snowflake,
        user: Snowflake,
        moderator: Snowflake,
        reason: &str,
        duration_secs: i64,
        soft: bool,
    ) -> Result<i64> {
        let reason = crate::security::scrub(reason, 500);
        let at = now();
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO ban_records(guild_id,user_id,moderator_id,reason,at,duration_secs,soft)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![guild.as_i64(), user.as_i64(), moderator.as_i64(), reason, at, duration_secs, i64::from(soft)],
            )?;
            let id = conn.last_insert_rowid();
            // A scheduled unban is an obligation, not a nice-to-have: `auto`
            // bans that were recorded but never applied are retried by the worker.
            Ok(id)
        })
    }

    pub fn mark_unbanned(&self, guild: Snowflake, ban_id: i64) -> Result<()> {
        let at = now();
        self.tx(move |conn| {
            conn.execute(
                "UPDATE ban_records SET unbanned_at=?2 WHERE id=?1 AND guild_id=?3",
                params![ban_id, at, guild.as_i64()],
            )?;
            Ok(())
        })
    }

    /// Temporary bans whose clock ran out, oldest first, capped so one slow tick
    /// cannot spray unban requests at the API.
    pub fn due_unbans(&self, limit: u32) -> Result<Vec<(i64, Snowflake, Snowflake)>> {
        let limit = limit.clamp(1, 100) as i64;
        let at = now();
        self.read(move |conn| {
            let mut st = conn
                .prepare(
                    "SELECT id, guild_id, user_id FROM ban_records
                     WHERE duration_secs > 0 AND unbanned_at IS NULL AND at + duration_secs <= ?1
                     ORDER BY at ASC LIMIT ?2",
                )
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![at, limit], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        Snowflake(r.get::<_, i64>(1)? as u64),
                        Snowflake(r.get::<_, i64>(2)? as u64),
                    ))
                })
                .map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::db)
        })
    }

    // -- notes ---------------------------------------------------------------

    pub fn add_note(
        &self,
        guild: Snowflake,
        user: Snowflake,
        author: Snowflake,
        body: &str,
    ) -> Result<i64> {
        let body = crate::security::scrub(body, 1000);
        if body.is_empty() {
            return Err(Error::Invalid("note body is empty".into()));
        }
        let at = now();
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO notes(guild_id,user_id,author_id,body,at) VALUES(?1,?2,?3,?4,?5)",
                params![guild.as_i64(), user.as_i64(), author.as_i64(), body, at],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn notes_for(
        &self,
        guild: Snowflake,
        user: Snowflake,
        limit: u32,
    ) -> Result<Vec<serde_json::Value>> {
        let limit = limit.clamp(1, 200) as i64;
        self.read(move |conn| {
            let mut st = conn
                .prepare("SELECT id, author_id, body, at FROM notes WHERE guild_id=?1 AND user_id=?2 ORDER BY at DESC LIMIT ?3")
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![guild.as_i64(), user.as_i64(), limit], |r| {
                    Ok(serde_json::json!({
                        "id": r.get::<_, i64>(0)?,
                        "authorId": r.get::<_, i64>(1)?.to_string(),
                        "body": r.get::<_, String>(2)?,
                        "at": r.get::<_, i64>(3)?,
                    }))
                })
                .map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::db)
        })
    }

    pub fn delete_note(&self, guild: Snowflake, note_id: i64) -> Result<bool> {
        self.tx(move |conn| {
            Ok(conn.execute(
                "DELETE FROM notes WHERE id=?1 AND guild_id=?2",
                params![note_id, guild.as_i64()],
            )? > 0)
        })
    }

    // -- aggregate tiles -----------------------------------------------------

    pub fn moderation_stats(&self, guild: Snowflake) -> Result<ModerationStats> {
        let day = now() - 86_400;
        let at = now();
        // One query per figure rather than a `CASE` tower: SQLite would reuse ?n
        // across the sub-expressions, and a wrong-but-plausible count is far worse
        // for an operator than a few cheap indexed scans. Each call binds exactly
        // the parameters its statement uses — rusqlite errors on surplus ones.
        self.read(move |conn| {
            let count = |sql: &str, args: &[&dyn rusqlite::ToSql]| -> Result<i64> {
                conn.query_row(sql, args, |r| r.get::<_, i64>(0))
                    .map_err(Error::db)
            };
            let g = guild.as_i64();
            Ok(ModerationStats {
                warns_active: count(
                    "SELECT COUNT(*) FROM warns
                     WHERE guild_id=?1 AND active=1 AND (expires_at=0 OR expires_at>?2)",
                    &[&g, &at],
                )?,
                warns_total: count("SELECT COUNT(*) FROM warns WHERE guild_id=?1", &[&g])?,
                strikes_active: count(
                    "SELECT COUNT(*) FROM strikes
                     WHERE guild_id=?1 AND active=1 AND (expires_at=0 OR expires_at>?2)",
                    &[&g, &at],
                )?,
                bans_total: count("SELECT COUNT(*) FROM ban_records WHERE guild_id=?1", &[&g])?,
                kicks_total: count("SELECT COUNT(*) FROM kicks WHERE guild_id=?1", &[&g])?,
                warns_last_24h: count(
                    "SELECT COUNT(*) FROM warns WHERE guild_id=?1 AND at>=?2",
                    &[&g, &day],
                )?,
            })
        })
    }

    /// Guild-wide leaderboard of the noisiest accounts, for triage.
    pub fn top_warned(&self, guild: Snowflake, limit: u32) -> Result<Vec<(Snowflake, i64)>> {
        let limit = limit.clamp(1, 50) as i64;
        self.read(move |conn| {
            let mut st = conn
                .prepare("SELECT user_id, COUNT(*) c FROM warns WHERE guild_id=?1 GROUP BY user_id ORDER BY c DESC, user_id LIMIT ?2")
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![guild.as_i64(), limit], |r| Ok((Snowflake(r.get::<_, i64>(0)? as u64), r.get(1)?)))
                .map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::db)
        })
    }
}

impl WarnRecord {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Self::from_row_raw(
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
            r.get(7)?,
            r.get(8)?,
            r.get(9)?,
        )
        .map_err(|_| rusqlite::Error::InvalidQuery)
    }

    fn from_row_raw(
        id: i64,
        guild_id: i64,
        user_id: i64,
        moderator_id: i64,
        reason: String,
        created_at: i64,
        expires_at: i64,
        active: i64,
        pardoned_at: Option<i64>,
        context_message: Option<i64>,
    ) -> Result<Self> {
        Ok(WarnRecord {
            id,
            guild_id: Snowflake(guild_id as u64),
            user_id: Snowflake(user_id as u64),
            moderator_id: Snowflake(moderator_id as u64),
            reason,
            created_at,
            expires_at,
            active: active != 0,
            pardoned_at,
            context_message: context_message.map(|v| Snowflake(v as u64)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::testutil::temp_db;

    const G: u64 = 10;
    const U: u64 = 20;
    const M: u64 = 30;

    fn seed(db: &Db) {
        db.tx(|c| {
            c.execute(
                "INSERT INTO guilds(id,name,updated_at) VALUES(?1,'g',0)",
                params![G as i64],
            )?;
            Ok(())
        })
        .unwrap();
    }

    fn g() -> Snowflake {
        Snowflake(G)
    }
    fn u() -> Snowflake {
        Snowflake(U)
    }
    fn m() -> Snowflake {
        Snowflake(M)
    }

    #[test]
    fn warn_counts_and_escalation() {
        let (db, _d) = temp_db();
        seed(&db);
        let o1 = db.add_warn(g(), u(), m(), "first", 0, 3, None).unwrap();
        assert_eq!(o1.active_count, 1);
        assert!(!o1.escalated);
        db.add_warn(g(), u(), m(), "second", 0, 3, None).unwrap();
        let o3 = db.add_warn(g(), u(), m(), "third", 0, 3, None).unwrap();
        assert_eq!(o3.active_count, 3);
        assert!(o3.escalated, "third warn must cross the threshold");
        assert_eq!(o3.strikes, 1, "escalation writes exactly one strike");
        assert_eq!(db.active_warn_count(g(), u()).unwrap(), 3);
    }

    #[test]
    fn warn_rejects_blank_reason() {
        let (db, _d) = temp_db();
        seed(&db);
        assert!(db.add_warn(g(), u(), m(), "   ", 0, 0, None).is_err());
        // Control characters cannot forge a log line or a mod-log field.
        let o = db
            .add_warn(g(), u(), m(), "a\nb\u{0d}c", 0, 0, None)
            .unwrap();
        let w = &db.warns_for(g(), u(), 10).unwrap()[0];
        assert!(
            !w.reason.contains('\n') && !w.reason.contains('\r'),
            "{:?}",
            w.reason
        );
        assert!(o.warn_id > 0);
    }

    #[test]
    fn pardon_one_and_clear_all() {
        let (db, _d) = temp_db();
        seed(&db);
        for i in 0..3 {
            db.add_warn(g(), u(), m(), &format!("r{i}"), 0, 0, None)
                .unwrap();
        }
        let warns = db.warns_for(g(), u(), 10).unwrap();
        assert_eq!(warns.len(), 3);
        assert!(db.pardon_warn(g(), warns[0].id, m()).unwrap());
        assert_eq!(db.active_warn_count(g(), u()).unwrap(), 2);
        // Pardoning twice must report "nothing changed", not pretend success.
        assert!(!db.pardon_warn(g(), warns[0].id, m()).unwrap());
        assert_eq!(db.clear_warns(g(), u()).unwrap(), 2);
        assert_eq!(db.active_warn_count(g(), u()).unwrap(), 0);
        assert_eq!(
            db.warns_for(g(), u(), 10).unwrap().len(),
            3,
            "history is preserved"
        );
    }

    #[test]
    fn expiry_toggles_active_flag() {
        let (db, _d) = temp_db();
        seed(&db);
        // Already-expired warn (negative expiry window simulated via direct insert).
        db.tx(|c| {
            c.execute(
                "INSERT INTO warns(guild_id,user_id,moderator_id,reason,at,expires_at,active)
                 VALUES(?1,?2,?3,'old',1,5,1)",
                params![G as i64, U as i64, M as i64],
            )?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            db.active_warn_count(g(), u()).unwrap(),
            0,
            "expired warn is not active"
        );
        let (w, _s) = db.expire_warns().unwrap();
        assert_eq!(w, 1, "housekeeping flips the flag");
        assert!(!db.warns_for(g(), u(), 10).unwrap()[0].active);
    }

    #[test]
    fn strike_ledger_independent_of_warns() {
        let (db, _d) = temp_db();
        seed(&db);
        assert_eq!(
            db.add_strike(g(), u(), "ai_filter", Some("profanity"), "hit", 30)
                .unwrap(),
            1
        );
        assert_eq!(
            db.add_strike(g(), u(), "ai_filter", Some("spam"), "hit", 30)
                .unwrap(),
            2
        );
        assert_eq!(db.active_strikes(g(), u()).unwrap(), 2);
        let other = db
            .add_strike(g(), Snowflake(99), "manual", None, "x", 0)
            .unwrap();
        assert_eq!(other, 1, "ledger is per user");
        let tops = db.strikes_between(g(), 0, 50).unwrap();
        assert_eq!(tops.len(), 2);
        assert_eq!(tops[0].1, 2, "sorted by count desc");
    }

    #[test]
    fn strikes_never_overflow_a_u8() {
        let (db, _d) = temp_db();
        seed(&db);
        for _ in 0..300 {
            let _ = db.add_strike(g(), u(), "manual", None, "x", 0);
        }
        assert_eq!(
            db.active_strikes(g(), u()).unwrap(),
            u8::MAX,
            "clamped, not wrapped"
        );
    }

    #[test]
    fn scheduled_unbans_are_listed_once_ready() {
        let (db, _d) = temp_db();
        seed(&db);
        let future = db.record_ban(g(), u(), m(), "10m", 600, false).unwrap();
        assert!(db.due_unbans(10).unwrap().is_empty(), "not yet due");
        // Force it due.
        db.tx(|c| {
            c.execute("UPDATE ban_records SET at = 1 WHERE id=?1", params![future])?;
            Ok(())
        })
        .unwrap();
        let due = db.due_unbans(10).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].1, g());
        db.mark_unbanned(g(), future).unwrap();
        assert!(db.due_unbans(10).unwrap().is_empty(), "must not repeat");
    }

    #[test]
    fn notes_crud() {
        let (db, _d) = temp_db();
        seed(&db);
        let id = db
            .add_note(g(), u(), m(), "seen arguing in #general")
            .unwrap();
        let list = db.notes_for(g(), u(), 10).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["body"], "seen arguing in #general");
        assert!(db.delete_note(g(), id).unwrap());
        assert!(!db.delete_note(g(), id).unwrap());
        assert!(db.add_note(g(), u(), m(), "   ").is_err());
    }

    #[test]
    fn moderation_stats_counts() {
        let (db, _d) = temp_db();
        seed(&db);
        db.add_warn(g(), u(), m(), "a", 0, 0, None).unwrap();
        db.add_warn(g(), Snowflake(44), m(), "b", 0, 2, None)
            .unwrap();
        db.add_warn(g(), Snowflake(44), m(), "c", 0, 2, None)
            .unwrap();
        db.record_kick(g(), u(), m(), "spam").unwrap();
        db.record_ban(g(), u(), m(), "raid", 0, false).unwrap();
        let s = db.moderation_stats(g()).unwrap();
        assert_eq!(s.warns_active, 3);
        assert_eq!(s.warns_total, 3);
        assert_eq!(s.kicks_total, 1);
        assert_eq!(s.bans_total, 1);
        assert_eq!(s.warns_last_24h, 3);
        assert_eq!(s.strikes_active, 1, "threshold 2 gave #44 one strike");
        let top = db.top_warned(g(), 5).unwrap();
        assert_eq!(top[0].0, Snowflake(44));
    }

    #[test]
    fn recent_warns_and_pardon_semantics() {
        let (db, _d) = temp_db();
        seed(&db);
        let o = db.add_warn(g(), u(), m(), "x", 0, 0, None).unwrap();
        let rows = db.recent_warns(g(), 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reason, "x");
        assert_eq!(rows[0].id, o.warn_id);
        // `pardoned_at` is a timestamp, not a role/user id — the classic
        // bind-order mistake, so assert on the magnitude.
        db.pardon_warn(g(), o.warn_id, m()).unwrap();
        let pardoned = db.warns_for(g(), u(), 10).unwrap().remove(0);
        let when = pardoned.pardoned_at.expect("set");
        assert!(
            when > 1_700_000_000 && when < 4_000_000_000,
            "not a timestamp: {when}"
        );
    }
}
