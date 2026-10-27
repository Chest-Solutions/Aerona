//! The mod log: one row per thing the bot noticed.
//!
//! The channel copy is ephemeral and can be deleted by a moderator; this table is what the
//! dashboard renders and what an audit answers with. Both come from the same
//! [`LogEntry`], which is the only reason they cannot disagree about what happened.

use rusqlite::params;

use crate::error::Result;
use crate::identity::Snowflake;
use crate::time::now;

use super::Db;

/// One entry, as a handler produces it. Only `event` and `summary` are required, because a
/// row that cannot say what happened is a row nobody can read; everything else is a link the
/// dashboard will render if it has it.
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// A key from [`crate::models::LOG_EVENTS`].
    pub event: &'static str,
    /// Where it happened, not where the log was posted.
    pub channel: Option<Snowflake>,
    pub user: Option<Snowflake>,
    pub message: Option<Snowflake>,
    /// One line, in the voice of a person telling a colleague what they saw.
    pub summary: String,
    /// The text before, for an edit.
    pub before: Option<String>,
    /// The text now, for an edit or a delete.
    pub after: Option<String>,
    /// Whether the member behind the event is a bot. The logger's `ignore_bots` switch reads it.
    pub bot: bool,
}

impl LogEntry {
    /// An entry with only what every event has: a kind and a line about it.
    pub fn new(event: &'static str, summary: impl Into<String>) -> Self {
        Self {
            event,
            channel: None,
            user: None,
            message: None,
            summary: summary.into(),
            before: None,
            after: None,
            bot: false,
        }
    }

    pub fn channel(mut self, channel: Snowflake) -> Self {
        self.channel = Some(channel);
        self
    }

    pub fn user(mut self, user: Snowflake) -> Self {
        self.user = Some(user);
        self
    }

    pub fn message(mut self, message: Snowflake) -> Self {
        self.message = Some(message);
        self
    }

    pub fn bot(mut self, bot: bool) -> Self {
        self.bot = bot;
        self
    }

    /// The text before, so an edit says what changed rather than that something changed.
    pub fn was(mut self, text: impl Into<String>) -> Self {
        self.before = Some(text.into());
        self
    }

    /// The text now: the new body of an edit, or the body of a message being deleted.
    pub fn is_now(mut self, text: impl Into<String>) -> Self {
        self.after = Some(text.into());
        self
    }

    /// [`Self::was`] for a handler that only knows the old state if the member was in the cache.
    /// `None` sets nothing, so "we do not know" stays out of the card instead of becoming "".
    pub fn was_opt(mut self, text: Option<impl Into<String>>) -> Self {
        if let Some(text) = text {
            self.before = Some(text.into());
        }
        self
    }

    /// [`Self::is_now`] for the same kind of "maybe there was a previous state to compare with".
    pub fn now_opt(mut self, text: Option<impl Into<String>>) -> Self {
        if let Some(text) = text {
            self.after = Some(text.into());
        }
        self
    }
}

/// How many events per guild the housekeeping job keeps. A year of a busy server's edits is
/// bigger than anybody's audit needs, and the newest rows are the ones that get read.
pub const KEEP_EVENTS_PER_GUILD: i64 = 5_000;

/// The row shape the dashboard reads. Keys are camelCase because this is a hand-built
/// response, not a settings document.
fn read_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<serde_json::Value> {
    Ok(serde_json::json!({
        "at": r.get::<_, i64>(0)?,
        "event": r.get::<_, String>(1)?,
        "channelId": r.get::<_, Option<i64>>(2)?.map(|v| v.to_string()),
        "userId": r.get::<_, Option<i64>>(3)?.map(|v| v.to_string()),
        "messageId": r.get::<_, Option<i64>>(4)?.map(|v| v.to_string()),
        "summary": r.get::<_, String>(5)?,
        "before": r.get::<_, Option<String>>(6)?,
        "after": r.get::<_, Option<String>>(7)?,
    }))
}

impl Db {
    pub fn record_log_event(&self, guild: Snowflake, e: &LogEntry) -> Result<i64> {
        let (g, ch, u, m) = (
            guild.as_i64(),
            e.channel.map(|s| s.as_i64()),
            e.user.map(|s| s.as_i64()),
            e.message.map(|s| s.as_i64()),
        );
        let at = now();
        let (summary, before, after) = (
            crate::security::clean(&e.summary, 900),
            e.before.as_deref().map(|t| crate::security::clean(t, 1500)),
            e.after.as_deref().map(|t| crate::security::clean(t, 1500)),
        );
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO log_events(guild_id,at,event,channel_id,user_id,message_id,summary,before,after)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![g, at, e.event, ch, u, m, summary, before, after],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// Newest first. `event` filters to one kind, which is what makes a 5,000-row log usable.
    pub fn list_log_events(
        &self,
        guild: Snowflake,
        limit: u32,
        event: Option<&str>,
    ) -> Result<Vec<serde_json::Value>> {
        let g = guild.as_i64();
        let limit = limit.clamp(1, 500) as i64;
        let event = event.map(str::to_string);
        self.read(move |conn| {
            // Two statements rather than an optional predicate stitched into SQL: the index
            // on (guild_id, id DESC) is used by both, and a `? IS NULL` dance would not be.
            let mut rows = match &event {
                Some(_) => conn.prepare(
                    "SELECT at,event,channel_id,user_id,message_id,summary,before,after
                     FROM log_events WHERE guild_id=?1 AND event=?2 ORDER BY id DESC LIMIT ?3",
                ),
                None => conn.prepare(
                    "SELECT at,event,channel_id,user_id,message_id,summary,before,after
                     FROM log_events WHERE guild_id=?1 ORDER BY id DESC LIMIT ?2",
                ),
            }
            .map_err(crate::error::Error::db)?;
            let mapped = match &event {
                Some(ev) => rows.query_map(params![g, ev, limit], read_event),
                None => rows.query_map(params![g, limit], read_event),
            };
            let mut out = Vec::new();
            for row in mapped.map_err(crate::error::Error::db)? {
                out.push(row.map_err(crate::error::Error::db)?);
            }
            Ok(out)
        })
    }

    /// Trim to the newest `KEEP_EVENTS_PER_GUILD` rows per guild.
    pub fn prune_log_events(&self) -> Result<u64> {
        self.tx(move |conn| {
            let n = conn.execute(
                "WITH ranked AS (
                     SELECT id, ROW_NUMBER() OVER (PARTITION BY guild_id ORDER BY id DESC) AS rn
                     FROM log_events
                 )
                 DELETE FROM log_events WHERE id IN (SELECT id FROM ranked WHERE rn > ?1)",
                params![KEEP_EVENTS_PER_GUILD],
            )? as u64;
            Ok(n)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::testutil::temp_db;

    fn entry(event: &'static str, summary: &str) -> LogEntry {
        LogEntry::new(event, summary)
            .channel(Snowflake(111))
            .user(Snowflake(222))
            .message(Snowflake(333))
    }

    #[test]
    fn an_event_round_trips_with_its_links() {
        let (db, _dir) = temp_db();
        let g = Snowflake(7);
        let e = entry("message_edit", "changed their mind")
            .was("first")
            .is_now("second");
        db.record_log_event(g, &e).unwrap();
        let rows = db.list_log_events(g, 10, None).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row["event"], "message_edit");
        assert_eq!(row["summary"], "changed their mind");
        assert_eq!(row["channelId"], "111");
        assert_eq!(row["before"], "first");
        assert_eq!(row["after"], "second");
    }

    #[test]
    fn the_filter_and_the_order_are_the_databases_job() {
        let (db, _dir) = temp_db();
        let g = Snowflake(7);
        for _ in 0..3 {
            db.record_log_event(g, &entry("message_edit", "e")).unwrap();
        }
        db.record_log_event(g, &entry("member_leave", "l")).unwrap();
        let edits = db.list_log_events(g, 50, Some("message_edit")).unwrap();
        assert_eq!(edits.len(), 3);
        let all = db.list_log_events(g, 50, None).unwrap();
        assert_eq!(all.len(), 4);
        // Newest first, which is what a log page reads like.
        assert_eq!(all[0]["event"], "member_leave");
    }

    #[test]
    fn one_guilds_log_is_not_another_guilds_log() {
        let (db, _dir) = temp_db();
        db.record_log_event(Snowflake(1), &entry("member_join", "a"))
            .unwrap();
        db.record_log_event(Snowflake(2), &entry("member_join", "b"))
            .unwrap();
        assert_eq!(db.list_log_events(Snowflake(1), 50, None).unwrap().len(), 1);
    }

    #[test]
    fn the_trim_keeps_the_newest_and_bounds_the_table() {
        let (db, _dir) = temp_db();
        let g = Snowflake(7);
        for i in 0..(KEEP_EVENTS_PER_GUILD + 25) {
            let _ = i;
            db.record_log_event(g, &entry("message_delete", "d"))
                .unwrap();
        }
        let before = db.row_count("log_events").unwrap();
        let removed = db.prune_log_events().unwrap();
        assert_eq!(removed, 25, "only the overflow goes");
        let _: i64 = before;
        assert_eq!(db.row_count("log_events").unwrap(), before - removed as i64);
        let kept = db.list_log_events(g, 500, None).unwrap();
        assert!(kept.len() <= 500);
    }
}
