//! Sticky messages and the ephemeral message cache that backs `!edit`/`!delete` logs.
//!
//! Sticky reposting is *count*-driven, not time-driven: a channel that receives
//! one message a day should not get a repost every minute, and a hyperactive
//! channel should still see the reminder often. So the bot increments a counter
//! per message and asks which stickies are due.

use rusqlite::params;
use serde_json::json;

use crate::error::{Error, Result};
use crate::identity::Snowflake;
use crate::models::{StickyDraft, StickyRecord};

use super::{now, Db};

/// The column list every sticky read shares, so a new column cannot be selected by one query and
/// forgotten by another. `sticky_from_row` reads these in this order.
const STICKY_COLS: &str = "id, guild_id, channel_id, message_id, sticky_message_id, content,
                     every_messages, since_count, created_by, created_at, last_run_at,
                     embed_title, embed_body, embed_color";

impl Db {
    /// Add, or re-add, one sticky.
    ///
    /// An anchored sticky (`message_id != 0`) replaces the previous row for the same message, so
    /// pointing at the same post twice updates it instead of stacking. An unanchored one is a new
    /// note every time: it has no message to be the same as, and a dashboard that says "add"
    /// twice means two stickies.
    pub fn add_sticky(&self, draft: &StickyDraft) -> Result<i64> {
        let d = draft.clone();
        // `scrub_block`, not `clean`: the lines a note is written with are part of the note.
        let content = crate::security::scrub_block(&d.content, 1800);
        let embed_title = d
            .embed_title
            .as_deref()
            .map(|t| crate::security::clean(t, 240))
            .filter(|t| !t.is_empty());
        let embed_body = d
            .embed_body
            .as_deref()
            .map(|t| crate::security::scrub_block(t, 1800))
            .filter(|t| !t.is_empty());
        if content.is_empty() && embed_title.is_none() && embed_body.is_none() {
            return Err(Error::Invalid(
                "a sticky needs text, an embed, or both".into(),
            ));
        }
        let (g, c, m, a) = (
            d.guild_id.as_i64(),
            d.channel_id.as_i64(),
            d.message_id.as_i64(),
            d.author.as_i64(),
        );
        let every = (d.every_messages.clamp(1, 1000)) as i64;
        let color = d.embed_color.map(|v| (v & 0xff_ff_ff) as i64);
        let at = now();
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO sticky_messages(guild_id,channel_id,message_id,content,every_messages,since_count,created_by,created_at,enabled,embed_title,embed_body,embed_color)
                 VALUES(?1,?2,?3,?4,?5,0,?6,?7,1,?8,?9,?10)
                 ON CONFLICT(guild_id, message_id) WHERE message_id != 0 DO UPDATE
                   SET content=excluded.content, every_messages=excluded.every_messages,
                       embed_title=excluded.embed_title, embed_body=excluded.embed_body,
                       embed_color=excluded.embed_color, enabled=1",
                params![g, c, m, content, every, a, at, embed_title, embed_body, color],
            )?;
            if m == 0 {
                return Ok(conn.last_insert_rowid());
            }
            conn.query_row(
                "SELECT id FROM sticky_messages WHERE guild_id=?1 AND message_id=?2",
                params![g, m],
                |r| r.get::<_, i64>(0),
            )
            .map_err(Error::db)
        })
    }

    pub fn stickies_for_channel(
        &self,
        guild: Snowflake,
        channel: Snowflake,
    ) -> Result<Vec<StickyRecord>> {
        let (g, c) = (guild.as_i64(), channel.as_i64());
        self.read(move |conn| {
            let mut st = conn
                .prepare(&format!("SELECT {STICKY_COLS} FROM sticky_messages WHERE guild_id=?1 AND channel_id=?2 AND enabled=1 ORDER BY id"))
                .map_err(Error::db)?;
            let rows = st.query_map(params![g, c], sticky_from_row).map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::db)
        })
    }

    pub fn stickies_for_guild(&self, guild: Snowflake) -> Result<Vec<StickyRecord>> {
        let g = guild.as_i64();
        self.read(move |conn| {
            let mut st = conn
                .prepare(&format!("SELECT {STICKY_COLS} FROM sticky_messages WHERE guild_id=?1 AND enabled=1 ORDER BY channel_id, id"))
                .map_err(Error::db)?;
            let rows = st.query_map(params![g], sticky_from_row).map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::db)
        })
    }

    /// One indexed UPDATE for the whole channel - the hot path runs on every
    /// message, so it must not do a read-modify-write per sticky.
    pub fn bump_stickies(&self, guild: Snowflake, channel: Snowflake) -> Result<()> {
        let (g, c) = (guild.as_i64(), channel.as_i64());
        self.tx(move |conn| {
            conn.execute(
                "UPDATE sticky_messages SET since_count = since_count + 1
                 WHERE guild_id=?1 AND channel_id=?2 AND enabled=1",
                params![g, c],
            )?;
            Ok(())
        })
    }

    /// Stickies in this channel whose interval has elapsed.
    pub fn due_stickies(&self, guild: Snowflake, channel: Snowflake) -> Result<Vec<StickyRecord>> {
        let (g, c) = (guild.as_i64(), channel.as_i64());
        self.read(move |conn| {
            let mut st = conn
                .prepare(&format!(
                    "SELECT {STICKY_COLS}
                     FROM sticky_messages
                     WHERE guild_id=?1 AND channel_id=?2 AND enabled=1 AND since_count >= every_messages
                     ORDER BY id LIMIT 8"
                ))
                .map_err(Error::db)?;
            let rows = st.query_map(params![g, c], sticky_from_row).map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::db)
        })
    }

    /// Record a repost/edit and reset the counter atomically for that row.
    pub fn mark_sticky_run(&self, sticky_id: i64, sticky_message: Option<Snowflake>) -> Result<()> {
        let id = sticky_id;
        let sm = sticky_message.map(|s| s.as_i64());
        let at = now();
        self.tx(move |conn| {
            conn.execute(
                "UPDATE sticky_messages
                 SET since_count=0, last_run_at=?2,
                     sticky_message_id = COALESCE(?3, sticky_message_id)
                 WHERE id=?1",
                params![id, at, sm],
            )?;
            Ok(())
        })
    }

    pub fn delete_sticky(&self, guild: Snowflake, sticky_id: i64) -> Result<bool> {
        let (g, id) = (guild.as_i64(), sticky_id);
        self.tx(move |conn| {
            Ok(conn.execute(
                "DELETE FROM sticky_messages WHERE id=?1 AND guild_id=?2",
                params![id, g],
            )? > 0)
        })
    }

    pub fn delete_stickies_for_channel(&self, guild: Snowflake, channel: Snowflake) -> Result<u64> {
        let (g, c) = (guild.as_i64(), channel.as_i64());
        self.tx(move |conn| {
            Ok(conn.execute(
                "DELETE FROM sticky_messages WHERE guild_id=?1 AND channel_id=?2",
                params![g, c],
            )? as u64)
        })
    }

    pub fn sticky_count(&self, guild: Snowflake) -> Result<u64> {
        let g = guild.as_i64();
        self.read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT COUNT(*) FROM sticky_messages WHERE guild_id=?1 AND enabled=1",
                    params![g],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0) as u64)
        })
    }

    // -- message cache used by the logger ----------------------------------

    /// Remember a message so an edit/delete event can quote what changed.
    /// Insert-then-trim keeps the table bounded without a DELETE on every message.
    pub fn remember_message(
        &self,
        guild: Snowflake,
        channel: Snowflake,
        message: Snowflake,
        author: Snowflake,
        content: &str,
    ) -> Result<()> {
        let content = crate::security::clean(content, 1500);
        let (g, c, m, a) = (
            guild.as_i64(),
            channel.as_i64(),
            message.as_i64(),
            author.as_i64(),
        );
        let at = now();
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO log_messages(guild_id,channel_id,message_id,author_id,content,kind,at)
                 VALUES(?1,?2,?3,?4,?5,'message',?6)",
                params![g, c, m, a, content, at],
            )?;
            Ok(())
        })
    }

    pub fn recalled_message(
        &self,
        guild: Snowflake,
        message: Snowflake,
    ) -> Result<Option<(String, Snowflake, i64)>> {
        let (g, m) = (guild.as_i64(), message.as_i64());
        self.read(move |conn| {
            let row = conn
                .query_row(
                    "SELECT content, author_id, at FROM log_messages WHERE guild_id=?1 AND message_id=?2 ORDER BY id DESC LIMIT 1",
                    params![g, m],
                    |r| Ok((r.get::<_, String>(0)?, Snowflake(r.get::<_, i64>(1)? as u64), r.get::<_, i64>(2)?)),
                )
                .ok();
            Ok(row)
        })
    }

    /// The newest messages the bot still holds text for, oldest of them last. This is what a
    /// picker on the panel shows, so that "pin the message that says read the rules" is a click on
    /// a sentence rather than a hunt for a 19-digit number in developer mode.
    ///
    /// It reads the same cache the logger quotes edits from, so its size is the guild's own
    /// "remember messages for" setting: nobody configures a second window for the panel.
    pub fn recent_messages(&self, guild: Snowflake, limit: u32) -> Result<Vec<serde_json::Value>> {
        let g = guild.as_i64();
        let cap = i64::from(limit.clamp(1, 500));
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT message_id, channel_id, author_id, content, at FROM log_messages
                 WHERE guild_id=?1 AND kind='message'
                 ORDER BY id DESC LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![g, cap], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?.to_string(),
                        "channelId": r.get::<_, i64>(1)?.to_string(),
                        "authorId": r.get::<_, i64>(2)?.to_string(),
                        "content": r.get::<_, String>(3)?,
                        "at": r.get::<_, i64>(4)?,
                    }))
                })?
                .filter_map(std::result::Result::ok)
                .collect();
            Ok(rows)
        })
    }

    pub fn forget_message(&self, guild: Snowflake, message: Snowflake) -> Result<()> {
        let (g, m) = (guild.as_i64(), message.as_i64());
        self.tx(move |conn| {
            conn.execute(
                "DELETE FROM log_messages WHERE guild_id=?1 AND message_id=?2",
                params![g, m],
            )?;
            Ok(())
        })
    }

    /// Keep at most `per_guild` cached messages per guild. Runs in the hourly
    /// housekeeping pass, never on the message path.
    ///
    /// `ROW_NUMBER()` needs SQLite ≥ 3.25; we bundle a much newer amalgamation,
    /// so this is safe and vastly cheaper than a per-guild delete loop.
    pub fn prune_log_messages(&self, per_guild: i64) -> Result<u64> {
        // Floor of 1 rather than a "sensible" 100: the caller's config is the
        // policy here, and silently refusing to prune a small cache is worse.
        let keep = per_guild.clamp(1, 200_000);
        self.tx(move |conn| {
            let n = conn.execute(
                "WITH ranked AS (
                     SELECT id, ROW_NUMBER() OVER (PARTITION BY guild_id ORDER BY id DESC) AS rn
                     FROM log_messages
                   )
                   DELETE FROM log_messages WHERE id IN (SELECT id FROM ranked WHERE rn > ?1)",
                params![keep],
            )?;
            Ok(n as u64)
        })
    }
}

fn sticky_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StickyRecord> {
    Ok(StickyRecord {
        id: r.get(0)?,
        guild_id: Snowflake(r.get::<_, i64>(1)? as u64),
        channel_id: Snowflake(r.get::<_, i64>(2)? as u64),
        message_id: Snowflake(r.get::<_, i64>(3)? as u64),
        sticky_message_id: r.get::<_, Option<i64>>(4)?.map(|v| Snowflake(v as u64)),
        content: r.get(5)?,
        every_messages: r.get::<_, i64>(6)?.clamp(1, i64::from(u32::MAX)) as u32,
        since_count: r.get::<_, i64>(7)?.max(0) as u32,
        created_by: Snowflake(r.get::<_, i64>(8)? as u64),
        created_at: r.get(9)?,
        last_run_at: r.get(10)?,
        embed_title: r.get(11)?,
        embed_body: r.get(12)?,
        embed_color: r.get::<_, Option<i64>>(13)?.map(|v| v as u32),
    })
}

/// Overview tile payload.
pub fn sticky_tile(db: &Db, guild: Snowflake) -> Result<serde_json::Value> {
    let g = guild.as_i64();
    db.read(move |conn| {
        let (count, channels): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COUNT(DISTINCT channel_id) FROM sticky_messages WHERE guild_id=?1 AND enabled=1",
                params![g],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((0, 0));
        Ok(json!({ "stickies": count, "channels": channels }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::testutil::temp_db;

    const G: i64 = 900;

    /// Every test that needs a sticky seeds an anchored one with no embed: that is what `!sticky`
    /// makes, and the dashboard's richer form is the same row with more fields set.
    fn draft(
        guild: Snowflake,
        channel: Snowflake,
        message: Snowflake,
        content: &str,
        every: u32,
        author: Snowflake,
    ) -> StickyDraft {
        StickyDraft {
            guild_id: guild,
            channel_id: channel,
            message_id: message,
            content: content.into(),
            every_messages: every,
            author,
            ..Default::default()
        }
    }
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
    fn due_only_after_the_interval_elapses() {
        let (db, _d) = temp_db();
        seed(&db);
        let id = db
            .add_sticky(&draft(
                g(),
                Snowflake(1),
                Snowflake(11),
                "read the rules",
                3,
                Snowflake(2),
            ))
            .unwrap();
        assert!(id > 0);
        assert!(
            db.due_stickies(g(), Snowflake(1)).unwrap().is_empty(),
            "0 of 3 messages"
        );
        for _ in 0..2 {
            db.bump_stickies(g(), Snowflake(1)).unwrap();
        }
        assert!(
            db.due_stickies(g(), Snowflake(1)).unwrap().is_empty(),
            "2 of 3"
        );
        db.bump_stickies(g(), Snowflake(1)).unwrap();
        let due = db.due_stickies(g(), Snowflake(1)).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].content, "read the rules");
        assert_eq!(due[0].every_messages, 3);
        assert_eq!(due[0].since_count, 3);

        db.mark_sticky_run(id, Some(Snowflake(77))).unwrap();
        assert!(
            db.due_stickies(g(), Snowflake(1)).unwrap().is_empty(),
            "counter reset"
        );
        let after = db
            .stickies_for_channel(g(), Snowflake(1))
            .unwrap()
            .remove(0);
        assert_eq!(after.sticky_message_id, Some(Snowflake(77)));
        assert!(after.last_run_at > 0);
    }

    #[test]
    fn scope_is_per_guild_and_per_channel() {
        let (db, _d) = temp_db();
        seed(&db);
        db.add_sticky(&draft(
            g(),
            Snowflake(1),
            Snowflake(11),
            "a",
            1,
            Snowflake(2),
        ))
        .unwrap();
        db.add_sticky(&draft(
            g(),
            Snowflake(2),
            Snowflake(12),
            "b",
            1,
            Snowflake(2),
        ))
        .unwrap();
        db.tx(|c| {
            c.execute(
                "INSERT INTO guilds(id,name,updated_at) VALUES(901,'other',0)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        db.add_sticky(&draft(
            Snowflake(901),
            Snowflake(1),
            Snowflake(13),
            "c",
            1,
            Snowflake(2),
        ))
        .unwrap();
        assert_eq!(db.stickies_for_channel(g(), Snowflake(1)).unwrap().len(), 1);
        assert_eq!(db.stickies_for_guild(g()).unwrap().len(), 2);
        assert_eq!(db.sticky_count(g()).unwrap(), 2);
        db.bump_stickies(g(), Snowflake(1)).unwrap();
        let due = db.due_stickies(g(), Snowflake(1)).unwrap();
        assert_eq!(due.len(), 1, "other channel untouched");
        assert_eq!(due[0].content, "a");
        assert!(db.due_stickies(g(), Snowflake(2)).unwrap().is_empty());
    }

    #[test]
    fn same_message_id_updates_instead_of_duplicating() {
        let (db, _d) = temp_db();
        seed(&db);
        let a = db
            .add_sticky(&draft(
                g(),
                Snowflake(1),
                Snowflake(11),
                "first",
                5,
                Snowflake(2),
            ))
            .unwrap();
        let b = db
            .add_sticky(&draft(
                g(),
                Snowflake(1),
                Snowflake(11),
                "second",
                9,
                Snowflake(2),
            ))
            .unwrap();
        assert_eq!(a, b, "re-adding the same message must update in place");
        let rows = db.stickies_for_guild(g()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content, "second");
        assert_eq!(rows[0].every_messages, 9);
    }

    #[test]
    fn invalid_content_and_clamping() {
        let (db, _d) = temp_db();
        seed(&db);
        assert!(db
            .add_sticky(&draft(
                g(),
                Snowflake(1),
                Snowflake(11),
                "   ",
                5,
                Snowflake(2),
            ))
            .is_err());
        let id = db
            .add_sticky(&draft(
                g(),
                Snowflake(1),
                Snowflake(11),
                "x",
                0,
                Snowflake(2),
            ))
            .unwrap();
        assert_eq!(
            db.stickies_for_guild(g()).unwrap()[0].every_messages,
            1,
            "interval 0 would spam"
        );
        db.add_sticky(&draft(
            g(),
            Snowflake(1),
            Snowflake(11),
            "y",
            100_000,
            Snowflake(2),
        ))
        .unwrap();
        assert_eq!(
            db.stickies_for_guild(g()).unwrap()[0].every_messages,
            1000,
            "absurd interval clamped"
        );
        assert!(db.delete_sticky(g(), id).unwrap());
        assert_eq!(db.sticky_count(g()).unwrap(), 0);
    }

    #[test]
    fn channel_deletion_clears_stickies() {
        let (db, _d) = temp_db();
        seed(&db);
        db.add_sticky(&draft(
            g(),
            Snowflake(1),
            Snowflake(11),
            "a",
            1,
            Snowflake(2),
        ))
        .unwrap();
        db.add_sticky(&draft(
            g(),
            Snowflake(1),
            Snowflake(12),
            "b",
            1,
            Snowflake(2),
        ))
        .unwrap();
        assert_eq!(
            db.delete_stickies_for_channel(g(), Snowflake(1)).unwrap(),
            2
        );
        assert_eq!(db.sticky_count(g()).unwrap(), 0);
    }

    #[test]
    fn message_cache_roundtrip() {
        let (db, _d) = temp_db();
        seed(&db);
        db.remember_message(
            g(),
            Snowflake(1),
            Snowflake(5),
            Snowflake(6),
            "original text",
        )
        .unwrap();
        let (content, author, at) = db
            .recalled_message(g(), Snowflake(5))
            .unwrap()
            .expect("cached");
        assert_eq!(content, "original text");
        assert_eq!(author, Snowflake(6));
        assert!(at > 0);
        assert!(
            db.recalled_message(Snowflake(999), Snowflake(5))
                .unwrap()
                .is_none(),
            "cross-guild isolation"
        );
        db.forget_message(g(), Snowflake(5)).unwrap();
        assert!(db.recalled_message(g(), Snowflake(5)).unwrap().is_none());
    }

    #[test]
    fn cache_is_prunable() {
        let (db, _d) = temp_db();
        seed(&db);
        for i in 0..20 {
            db.remember_message(
                g(),
                Snowflake(1),
                Snowflake(100 + i),
                Snowflake(6),
                &format!("m{i}"),
            )
            .unwrap();
        }
        let before = db.row_count("log_messages").unwrap();
        assert_eq!(before, 20);
        let removed = db.prune_log_messages(5).unwrap();
        assert!(removed >= 15, "removed {removed}");
        assert_eq!(
            db.row_count("log_messages").unwrap(),
            5,
            "keeps the newest N"
        );
        // Newest, not oldest: the last message must survive.
        assert!(db.recalled_message(g(), Snowflake(119)).unwrap().is_some());
        assert!(db.recalled_message(g(), Snowflake(100)).unwrap().is_none());
    }

    #[test]
    fn tile_reports_counts() {
        let (db, _d) = temp_db();
        seed(&db);
        assert_eq!(sticky_tile(&db, g()).unwrap()["stickies"], 0);
        db.add_sticky(&draft(
            g(),
            Snowflake(1),
            Snowflake(11),
            "a",
            1,
            Snowflake(2),
        ))
        .unwrap();
        db.add_sticky(&draft(
            g(),
            Snowflake(1),
            Snowflake(12),
            "b",
            1,
            Snowflake(2),
        ))
        .unwrap();
        db.add_sticky(&draft(
            g(),
            Snowflake(2),
            Snowflake(13),
            "c",
            1,
            Snowflake(2),
        ))
        .unwrap();
        let t = sticky_tile(&db, g()).unwrap();
        assert_eq!(t["stickies"], 3);
        assert_eq!(t["channels"], 2);
    }

    #[test]
    fn unanchored_stickies_stack_and_embeds_round_trip() {
        let (db, _d) = temp_db();
        seed(&db);
        let mk = |text: &str, title: &str| StickyDraft {
            guild_id: g(),
            channel_id: Snowflake(4),
            message_id: Snowflake(0),
            content: text.into(),
            embed_title: (!title.is_empty()).then(|| title.to_string()),
            embed_body: (!title.is_empty()).then(|| "line one\nline two".into()),
            embed_color: (!title.is_empty()).then_some(0xff_00_00),
            every_messages: 1,
            author: Snowflake(2),
        };
        let a = db.add_sticky(&mk("", "Rules")).unwrap();
        let b = db.add_sticky(&mk("read them", "")).unwrap();
        assert_ne!(a, b, "two stickies with no anchor are two stickies");
        let rows = db.stickies_for_channel(g(), Snowflake(4)).unwrap();
        assert_eq!(rows.len(), 2);
        let with_embed = rows.iter().find(|r| r.id == a).unwrap();
        assert!(
            !with_embed.anchored(),
            "message id 0 is the no-anchor marker"
        );
        assert!(with_embed.has_embed());
        assert_eq!(with_embed.embed_body.as_deref(), Some("line one\nline two"));
        assert_eq!(with_embed.color(), 0xff_00_00);
        let plain = rows.iter().find(|r| r.id == b).unwrap();
        assert!(!plain.has_embed(), "text alone is not an embed");
        assert_eq!(
            plain.color(),
            0x58_65_F2,
            "and it borrows blurple, not black"
        );

        // Both halves are optional; neither is an excuse for an empty row.
        assert!(db
            .add_sticky(&StickyDraft {
                embed_body: None,
                ..mk("   ", "   ")
            })
            .is_err());
        assert!(db
            .add_sticky(&StickyDraft {
                content: "  ".into(),
                embed_body: Some("  ".into()),
                ..mk("", "")
            })
            .is_err());
    }

    #[test]
    fn the_same_anchor_still_updates_and_an_unanchored_row_never_hijacks_it() {
        let (db, _d) = temp_db();
        seed(&db);
        let id = db
            .add_sticky(&draft(
                g(),
                Snowflake(7),
                Snowflake(70),
                "first",
                5,
                Snowflake(2),
            ))
            .unwrap();
        db.add_sticky(&draft(
            g(),
            Snowflake(7),
            Snowflake(70),
            "second",
            9,
            Snowflake(2),
        ))
        .unwrap();
        let rows = db.stickies_for_channel(g(), Snowflake(7)).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "an anchored message is re-added, not duplicated"
        );
        assert_eq!(rows[0].content, "second");
        assert_eq!(rows[0].id, id);
        // The same channel, no anchor: the row above must not be mistaken for this one.
        db.add_sticky(&StickyDraft {
            guild_id: g(),
            channel_id: Snowflake(7),
            message_id: Snowflake(0),
            content: "authored here".into(),
            every_messages: 3,
            author: Snowflake(2),
            ..Default::default()
        })
        .unwrap();
        let rows = db.stickies_for_channel(g(), Snowflake(7)).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .any(|r| r.content == "authored here" && !r.anchored()));
        assert!(rows.iter().any(|r| r.content == "second" && r.anchored()));
    }

    /// A note is written on lines, and the lines are the note. `clean` folds them, which is right
    /// for a label and wrong here; `scrub_block` keeps them and caps what one message can carry.
    #[test]
    fn a_note_keeps_its_lines_and_is_still_bounded() {
        let (db, _d) = temp_db();
        seed(&db);
        let text = (1..=25)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let id = db
            .add_sticky(&draft(
                g(),
                Snowflake(7),
                Snowflake(0),
                &text,
                20,
                Snowflake(2),
            ))
            .unwrap();
        let rows = db.stickies_for_guild(g()).unwrap();
        let row = rows
            .iter()
            .find(|r| r.id == id)
            .expect("the row just added");
        assert!(
            row.content.starts_with("line 1\nline 2\n"),
            "{:?}",
            row.content
        );
        assert!(
            row.content.lines().count() <= 20,
            "{} lines is more than a note",
            row.content.lines().count()
        );
        assert!(
            !row.content.contains("line 25"),
            "the tail is cut, not the shape"
        );

        // A control character is not formatting: it goes, and the next line survives.
        let id2 = db
            .add_sticky(&draft(
                g(),
                Snowflake(8),
                Snowflake(0),
                "one\u{7}two\nthree",
                20,
                Snowflake(2),
            ))
            .unwrap();
        let row2 = db
            .stickies_for_guild(g())
            .unwrap()
            .into_iter()
            .find(|r| r.id == id2)
            .unwrap();
        assert_eq!(row2.content, "one two\nthree", "{:?}", row2.content);
    }
}
