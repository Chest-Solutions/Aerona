//! The Minecraft cape notifier's records: what this server has been told about, what it looks like,
//! and when it last asked.
//!
//! ## Why a table and not a settings document
//!
//! The *config* is a document like every other feature's. The releases are rows, because they are a
//! history an operator queries: how many capes did we announce, was this one announced, when. A JSON
//! array of 60 objects rewritten whole on every poll would also lose the one property that makes the
//! notifier trustworthy - `UNIQUE (guild_id, slug)`, which is what makes "did we already ping about
//! this" a fact rather than a hope.
//!
//! ## The seed
//!
//! `cape_state.seeded` exists for the first run. A server that turns the feature on today has 60-odd
//! capes it has never been told about, and announcing all of them is not news, it is an unreadable
//! channel and a mute. So the first pass records the whole category with `announced_at` already set -
//! learned, not announced - and from then on only a title the source has never listed pings.

use rusqlite::{params, Connection, OptionalExtension};

use crate::cape;
use crate::error::{Error, Result};
use crate::identity::Snowflake;
use crate::models::{CapeConfig, CapeRelease, Feature};

use super::{now, Db};

/// What a poll decided is new. Built by the caller, inserted here, and read back by the outbox when
/// the card is made - so nothing about a cape lives in memory between those two, and a restart in
/// between loses nothing.
#[derive(Debug, Clone)]
pub struct NewCape {
    pub slug: String,
    pub title: String,
    pub name: String,
    pub article_url: String,
    /// The wiki file the texture came from, and where it was fetched from. Both are kept so the panel
    /// can explain a render it cannot reproduce.
    pub texture_file: String,
    pub texture_url: String,
    /// `drape`, `wiki` or `none` - whose picture `frames` holds. See `CapeRelease::render_kind`.
    pub render_kind: String,
    pub last_rev: i64,
    /// Already announced: the seed pass uses this to record without pinging.
    pub silent: bool,
    pub frames: Vec<Vec<u8>>,
}

impl NewCape {
    /// The row a source page produces before any texture is known about it.
    pub fn from_page(page: &cape::CapePage, silent: bool) -> Self {
        Self {
            slug: cape::slug(page),
            title: page.title.clone(),
            name: cape::display_name(&page.title),
            article_url: page.url.clone(),
            texture_file: String::new(),
            texture_url: String::new(),
            render_kind: "none".into(),
            last_rev: page.last_rev,
            silent,
            frames: Vec::new(),
        }
    }
}

/// The columns a `CapeRelease` is read from, in the order `release_from_row` expects.
const RELEASE_COLS: &str = "id, slug, title, name, article_url, texture_file, render_kind, \
                            last_rev, found_at, announced_at, attempts, \
                            (SELECT COUNT(*) FROM cape_frames f WHERE f.release_id = r.id)";

impl Db {
    /// The notifier's settings, with the interval already clamped where it is read rather than
    /// refused where it is written: a document saved by hand should degrade to the slowest sensible
    /// poll, not break the save.
    pub fn cape_config(&self, g: Snowflake) -> Result<CapeConfig> {
        let mut cfg: CapeConfig = self.settings(g, Feature::Capes)?;
        cfg.check_every_minutes = cfg.check_every_minutes.clamp(5, 1440);
        cfg.embed_color &= 0xff_ff_ff;
        Ok(cfg)
    }

    pub fn put_cape_config(
        &self,
        g: Snowflake,
        v: &CapeConfig,
        by: Option<Snowflake>,
    ) -> Result<()> {
        self.put_settings(g, Feature::Capes, v, by)
    }

    /// Every cape this server knows about, newest first, for the panel's table. `limit` is clamped
    /// because the same query is used by a "recent" tile that must not be able to ask for the moon.
    pub fn cape_releases(&self, g: Snowflake, limit: u32) -> Result<Vec<CapeRelease>> {
        let g = g.as_i64();
        let limit = limit.clamp(1, 200) as i64;
        self.read(move |conn| {
            let mut st = conn.prepare(&format!(
                "SELECT {RELEASE_COLS} FROM cape_releases r WHERE guild_id=?1 \
                 ORDER BY found_at DESC, id DESC LIMIT ?2"
            ))?;
            let rows = st.query_map(params![g, limit], release_from_row)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::db)
        })
    }

    /// What the last polls saw, as `slug -> last_rev`, plus whether it was announced. One query per
    /// tick, and the diff is done in memory: the category is 60 rows, so this is cheaper and much
    /// harder to get wrong than an upsert with a conditional insert inside it.
    pub fn cape_known(&self, g: Snowflake) -> Result<Vec<(String, i64, i64, bool)>> {
        let g = g.as_i64();
        self.read(move |conn| {
            let mut st = conn.prepare(
                "SELECT slug, last_rev, id, announced_at FROM cape_releases WHERE guild_id=?1",
            )?;
            let rows = st.query_map(params![g], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<i64>>(3)?.is_some(),
                ))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::db)
        })
    }

    /// Insert one cape and its frames. `Ok(None)` means the guild already had that slug: the caller
    /// then updates it in place instead of pinging again.
    pub fn remember_cape(&self, g: Snowflake, new: &NewCape) -> Result<Option<i64>> {
        let (at, g) = (now(), g.as_i64());
        self.tx(move |conn| {
            let inserted = conn.execute(
                "INSERT INTO cape_releases
                   (guild_id, slug, title, name, article_url, texture_file, texture_url,
                    render_kind, last_rev, found_at, announced_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                 ON CONFLICT (guild_id, slug) DO NOTHING",
                params![
                    g,
                    new.slug,
                    new.title,
                    new.name,
                    new.article_url,
                    new.texture_file,
                    new.texture_url,
                    new.render_kind,
                    new.last_rev,
                    at,
                    new.silent.then_some(at),
                ],
            )?;
            let id: i64 = conn
                .query_row(
                    "SELECT id FROM cape_releases WHERE guild_id=?1 AND slug=?2",
                    params![g, new.slug],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0);
            if inserted == 0 || id == 0 {
                return Ok(None);
            }
            write_frames(conn, id, &new.frames)?;
            Ok(Some(id))
        })
    }

    /// A page that changed: refresh the article, the texture and the frames without touching
    /// `announced_at`, so an edit to an existing cape never pings twice for one release.
    pub fn refresh_cape(&self, id: i64, new: &NewCape) -> Result<()> {
        self.tx(move |conn| {
            conn.execute(
                "UPDATE cape_releases
                    SET title=?2, name=?3, article_url=?4, texture_file=?5, texture_url=?6,
                        last_rev=?7
                  WHERE id=?1",
                params![
                    id,
                    new.title,
                    new.name,
                    new.article_url,
                    new.texture_file,
                    new.texture_url,
                    new.last_rev
                ],
            )?;
            conn.execute("DELETE FROM cape_frames WHERE release_id=?1", params![id])?;
            write_frames(conn, id, &new.frames)?;
            Ok(())
        })
    }

    /// Capes this server has been told about but has not been *announced* yet. A tick records
    /// everything it found and posts at most a few, so a source that changed a lot arrives as a
    /// trickle instead of a wall; the rows in between are what this lists.
    /// What has been found and not yet said, oldest first, capped so a backlog arrives as a
    /// trickle. Rows the channel has refused `CAPE_MAX_ATTEMPTS` times are left out: the panel still
    /// lists them, with the count, and the worker stops knocking on a door that is shut.
    pub fn cape_pending(&self, g: Snowflake, limit: usize) -> Result<Vec<CapeRelease>> {
        let g = g.as_i64();
        let limit = limit.clamp(1, 10) as i64;
        let ceiling = i64::from(crate::models::CAPE_MAX_ATTEMPTS);
        self.read(move |conn| {
            let mut st = conn.prepare(&format!(
                "SELECT {RELEASE_COLS} FROM cape_releases r \
                 WHERE guild_id=?1 AND announced_at IS NULL AND attempts < ?3 \
                 ORDER BY found_at, id LIMIT ?2"
            ))?;
            let rows = st.query_map(params![g, limit, ceiling], release_from_row)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::db)
        })
    }

    /// One more failed post for one cape. Kept on the row rather than in memory because the whole
    /// point of the counter is to survive the restart that follows a broken channel.
    pub fn note_cape_attempt(&self, id: i64) -> Result<()> {
        self.tx(move |conn| {
            conn.execute(
                "UPDATE cape_releases SET attempts = attempts + 1 WHERE id=?1",
                params![id],
            )?;
            Ok(())
        })
    }

    pub fn mark_cape_announced(&self, id: i64) -> Result<()> {
        let at = now();
        self.tx(move |conn| {
            conn.execute(
                "UPDATE cape_releases SET announced_at=?2 WHERE id=?1",
                params![id, at],
            )?;
            Ok(())
        })
    }

    /// One frame of a cape's render, as PNG bytes. `frame` is the angle index into
    /// [`cape::YAWS`]; anything out of range answers `None` rather than wrapping, because a panel
    /// that asked for the fifth angle of a four-angle turntable is showing a bug, not a cape.
    pub fn cape_render(&self, g: Snowflake, slug: &str, frame: u8) -> Result<Option<Vec<u8>>> {
        let g = g.as_i64();
        let frame = i64::from(frame);
        self.read(move |conn| {
            let blob = conn
                .query_row(
                    "SELECT f.png FROM cape_releases r JOIN cape_frames f ON f.release_id = r.id
                      WHERE r.guild_id=?1 AND r.slug=?2 AND f.idx=?3",
                    params![g, slug, frame],
                    |r| r.get::<_, Vec<u8>>(0),
                )
                .optional()?;
            Ok(blob)
        })
    }

    /// When this server last asked the source, and whether it has been seeded yet.
    pub fn cape_state(&self, g: Snowflake) -> Result<(i64, bool)> {
        let g = g.as_i64();
        self.read(move |conn| {
            let row = conn
                .query_row(
                    "SELECT last_check, seeded FROM cape_state WHERE guild_id=?1",
                    params![g],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? != 0)),
                )
                .optional()?;
            Ok(row.unwrap_or((0, false)))
        })
    }

    /// Ask for the next pass to happen now.
    ///
    /// Zeroing the stamp *is* the request: the tick compares `last_check` against the interval, so
    /// there is no queue entry to write and nothing to cancel if the check is asked for twice. The
    /// API never reaches out to another machine itself - a panel button that fetched the wiki would
    /// be a fetch endpoint on the public port, and one that an operator could point at any host.
    ///
    /// A guild that has never been polled gets `seeded` set as well, which is the honest reading of
    /// a manual check on a feature that was switched on a minute ago: record what is there, do not
    /// ping for sixty capes.
    pub fn force_cape_check(&self, g: Snowflake) -> Result<()> {
        let g = g.as_i64();
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO cape_state (guild_id, last_check, seeded) VALUES (?1,0,1)
                 ON CONFLICT (guild_id) DO UPDATE SET last_check=0",
                params![g],
            )?;
            Ok(())
        })
    }

    pub fn note_cape_check(&self, g: Snowflake, seeded: bool) -> Result<()> {
        let (g, at) = (g.as_i64(), now());
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO cape_state (guild_id, last_check, seeded) VALUES (?1,?2,?3)
                 ON CONFLICT (guild_id) DO UPDATE SET last_check=?2, seeded=?3",
                params![g, at, i64::from(seeded)],
            )?;
            Ok(())
        })
    }

    /// Keep the newest `keep_per_guild` capes per server. The frames are the only bulky bytes in the
    /// database that a feature writes on its own, so they are the ones worth a ceiling; the source
    /// keeps its own history, so nothing here is a loss an operator cannot read elsewhere.
    pub fn prune_capes(&self, keep_per_guild: u32) -> Result<u64> {
        let keep = i64::from(keep_per_guild.max(1));
        self.tx(move |conn| {
            let n = conn.execute(
                "WITH ranked AS (
                     SELECT id, ROW_NUMBER() OVER (PARTITION BY guild_id ORDER BY id DESC) AS rn
                     FROM cape_releases
                 )
                 DELETE FROM cape_releases WHERE id IN (SELECT id FROM ranked WHERE rn > ?1)",
                params![keep],
            )? as u64;
            Ok(n)
        })
    }
}

fn write_frames(conn: &Connection, id: i64, frames: &[Vec<u8>]) -> rusqlite::Result<()> {
    for (idx, png) in frames.iter().enumerate() {
        conn.execute(
            "INSERT INTO cape_frames (release_id, idx, png) VALUES (?1,?2,?3) ON CONFLICT (release_id, idx) DO UPDATE SET png=?3",
            params![id, idx as i64, png],
        )?;
    }
    Ok(())
}

/// The order is `RELEASE_COLS`, and the last entry is the frame count rather than a column: a row
/// whose picture failed to render must not look like a row with a picture on it.
fn release_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CapeRelease> {
    let frames: i64 = r.get(11)?;
    Ok(CapeRelease {
        id: r.get(0)?,
        slug: r.get(1)?,
        title: r.get(2)?,
        name: r.get(3)?,
        article_url: r.get(4)?,
        texture_file: r.get(5)?,
        render_kind: r.get(6)?,
        frames: frames.max(0) as u32,
        has_render: frames > 0,
        last_rev: r.get(7)?,
        found_at: r.get(8)?,
        announced_at: r.get(9)?,
        attempts: r.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cape::CapePage;
    use crate::db::testutil::temp_db;

    /// `cape_releases` and `cape_state` both point at `guilds`, and the pool turns
    /// `PRAGMA foreign_keys` on, so a test needs a server row before it can have a
    /// cape row. 7 and 8 because one test checks that two servers get two answers.
    fn db_with_guilds() -> (Db, tempfile::TempDir) {
        let (db, dir) = temp_db();
        for id in [7i64, 8] {
            db.tx(|c| {
                c.execute(
                    "INSERT INTO guilds(id,name,updated_at) VALUES(?1,'g',0)",
                    rusqlite::params![id],
                )?;
                Ok(())
            })
            .unwrap();
        }
        (db, dir)
    }

    fn page(id: i64, title: &str, rev: i64) -> CapePage {
        CapePage {
            title: title.into(),
            page_id: id,
            last_rev: rev,
            url: format!("https://minecraft.wiki/w/{title}"),
        }
    }

    fn new(id: i64, title: &str, rev: i64, silent: bool) -> NewCape {
        let mut n = NewCape::from_page(&page(id, title, rev), silent);
        n.texture_file = format!("File:{title} (texture) JE.png");
        n.render_kind = "drape".into();
        n.frames = vec![b"png-one".to_vec(), b"png-two".to_vec()];
        n
    }

    #[test]
    fn a_cape_is_recorded_once_per_server_and_its_frames_are_kept() {
        let (db, _dir) = db_with_guilds();
        let g = Snowflake(7);
        let other = Snowflake(8);
        let id = db
            .remember_cape(g, &new(1, "Aurora Cape", 10, false))
            .unwrap();
        assert!(id.is_some(), "a first write lands: {id:?}");
        let id = id.unwrap();

        // The same slug from the same server is not new twice, and the second answer says so.
        assert!(
            db.remember_cape(g, &new(1, "Aurora Cape", 11, false))
                .unwrap()
                .is_none(),
            "a rename or a re-poll cannot make the same cape new"
        );
        // Another server is another answer: each guild has its own channel and its own start date.
        assert!(db
            .remember_cape(other, &new(1, "Aurora Cape", 10, false))
            .unwrap()
            .is_some());

        let rows = db.cape_releases(g, 20).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].name, "Aurora Cape");
        assert_eq!(rows[0].slug, "mw1");
        assert!(rows[0].has_render, "two frames were written");
        assert_eq!(
            rows[0].render_kind, "drape",
            "and the row says whose picture it is"
        );
        assert_eq!(
            rows[0].announced_at, None,
            "recorded, not yet told to anybody"
        );
        assert_eq!(
            db.cape_render(g, "mw1", 1).unwrap().as_deref(),
            Some(&b"png-two"[..]),
            "the second angle is the second frame"
        );
        assert!(
            db.cape_render(g, "mw1", 9).unwrap().is_none(),
            "no wrapping a bad index"
        );
        assert_eq!(
            db.cape_render(other, "mw1", 0).unwrap().as_deref(),
            Some(&b"png-one"[..])
        );

        db.mark_cape_announced(id).unwrap();
        let rows = db.cape_releases(g, 20).unwrap();
        assert!(rows[0].announced_at.is_some(), "{rows:?}");
        assert!(
            db.cape_pending(g, 5).unwrap().is_empty(),
            "nothing is pending once it has been said"
        );
    }

    #[test]
    fn a_seeded_cape_is_learned_without_being_announced() {
        let (db, _dir) = db_with_guilds();
        let g = Snowflake(7);
        db.remember_cape(g, &new(2, "Birthday Cape", 5, true))
            .unwrap();
        let rows = db.cape_releases(g, 20).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].announced_at.is_some(),
            "the first pass must not fill a channel with every cape since 2011: {rows:?}"
        );
        assert!(db.cape_pending(g, 5).unwrap().is_empty());
    }

    #[test]
    fn an_edited_page_is_updated_without_a_second_ping() {
        let (db, _dir) = db_with_guilds();
        let g = Snowflake(7);
        let id = db
            .remember_cape(g, &new(3, "Copper Cape", 1, false))
            .unwrap()
            .unwrap();
        db.mark_cape_announced(id).unwrap();
        let mut updated = new(3, "Copper Cape", 7, false);
        updated.name = "Copper Cape (updated)".into();
        db.refresh_cape(id, &updated).unwrap();

        let rows = db.cape_releases(g, 20).unwrap();
        assert_eq!(rows[0].last_rev, 7, "the revision moved: {rows:?}");
        assert_eq!(rows[0].name, "Copper Cape (updated)");
        assert!(
            rows[0].announced_at.is_some(),
            "and it is still one announcement"
        );
        assert!(db.cape_pending(g, 5).unwrap().is_empty());
        let known = db.cape_known(g).unwrap();
        assert_eq!(known.len(), 1);
        assert_eq!(known[0].0, "mw3");
        assert_eq!(known[0].1, 7);
        assert!(
            known[0].3,
            "announced flag comes back so the tick can diff in memory"
        );
    }

    #[test]
    fn forcing_a_check_zeroes_the_stamp_and_seeds_an_unseen_server() {
        let (db, _dir) = db_with_guilds();
        let g = Snowflake(7);
        db.note_cape_check(g, true).unwrap();
        let (last, seeded) = db.cape_state(g).unwrap();
        assert!(last > 0 && seeded, "{last}, {seeded}");
        db.force_cape_check(g).unwrap();
        let (last, seeded) = db.cape_state(g).unwrap();
        assert_eq!(last, 0, "the next tick is the one that checks");
        assert!(seeded, "a guild that had a pass keeps its seed");

        // A guild that has never been polled is seeded by the forcing, so asking by hand cannot
        // turn into a wall of pings for everything the wiki already documents.
        let fresh = Snowflake(8);
        assert_eq!(
            db.cape_state(fresh).unwrap(),
            (0, false),
            "nothing recorded yet"
        );
        db.force_cape_check(fresh).unwrap();
        assert_eq!(db.cape_state(fresh).unwrap(), (0, true));
        // And the forced stamp is still a stamp: a second ask is not an error, just the same ask.
        db.force_cape_check(fresh).unwrap();
        assert_eq!(db.cape_state(fresh).unwrap(), (0, true));
    }

    #[test]
    fn a_cape_the_channel_refused_stops_being_tried() {
        let (db, _dir) = db_with_guilds();
        let g = Snowflake(7);
        let id = db
            .remember_cape(g, &new(5, "Turtle Cape", 3, false))
            .unwrap()
            .expect("a first write");
        assert_eq!(
            db.cape_pending(g, 5).unwrap().len(),
            1,
            "it starts out pending"
        );

        for _ in 0..crate::models::CAPE_MAX_ATTEMPTS {
            db.note_cape_attempt(id).unwrap();
        }
        assert!(
            db.cape_pending(g, 5).unwrap().is_empty(),
            "five refusals and the worker stops knocking"
        );
        let rows = db.cape_releases(g, 20).unwrap();
        assert_eq!(rows[0].attempts, crate::models::CAPE_MAX_ATTEMPTS);
        assert!(
            rows[0].announced_at.is_none(),
            "giving up is not the same as telling somebody: {rows:?}"
        );
        assert!(rows[0].has_render && rows[0].render_kind == "drape");
    }

    #[test]
    fn the_pending_list_is_a_trickle_not_a_flood() {
        let (db, _dir) = db_with_guilds();
        let g = Snowflake(7);
        for i in 0..12 {
            db.remember_cape(g, &new(100 + i, &format!("Cape {i}"), 1, false))
                .unwrap();
        }
        let first = db.cape_pending(g, 10).unwrap();
        assert_eq!(first.len(), 10, "a tick posts at most its cap");
        assert_eq!(
            first[0].name, "Cape 0",
            "and it starts with the oldest news: {first:?}"
        );
        // The limit is clamped, so a caller cannot ask for the whole table by accident.
        assert_eq!(db.cape_pending(g, 999).unwrap().len(), 10);
        assert_eq!(db.cape_pending(g, 0).unwrap().len(), 1);
    }

    #[test]
    fn poll_state_remembers_the_last_ask_and_the_seed() {
        let (db, _dir) = db_with_guilds();
        let g = Snowflake(7);
        assert_eq!(
            db.cape_state(g).unwrap(),
            (0, false),
            "never asked, never seeded"
        );
        db.note_cape_check(g, true).unwrap();
        let (at, seeded) = db.cape_state(g).unwrap();
        assert!(at > 0, "the clock is written: {at}");
        assert!(seeded);
        db.note_cape_check(g, false).unwrap();
        let (_, seeded) = db.cape_state(g).unwrap();
        assert!(
            !seeded,
            "the flag is written, not ORed, so a caller decides what it means"
        );
    }

    #[test]
    fn the_config_is_read_through_the_settings_document_and_clamped() {
        let (db, _dir) = db_with_guilds();
        let g = Snowflake(7);
        let mut cfg = CapeConfig::default();
        assert_eq!(
            cfg.check_every_secs(),
            1200,
            "the default is twenty minutes: one request per pass, and news that arrives inside a
             coffee break rather than inside a working morning"
        );
        cfg.enabled = true;
        cfg.check_every_minutes = 1;
        cfg.embed_color = 0xff_ff_ff_ff;
        db.put_cape_config(g, &cfg, None).unwrap();
        let back = db.cape_config(g).unwrap();
        assert_eq!(
            back.check_every_minutes, 5,
            "a one-minute poll is refused by the reader"
        );
        assert_eq!(back.check_every_secs(), 300);
        assert_eq!(
            back.embed_color, 0xff_ff_ff,
            "and a colour is masked, not truncated"
        );
        assert!(back.enabled, "the switch survives the document round trip");
        assert!(
            !back.announces(),
            "switched on but with no channel, there is nowhere to ping: {back:?}"
        );

        // A channel is all it takes to start posting, and the ping role stays optional: a server
        // that only wants the news in the channel gets the card without the mention.
        cfg.channel_id = Some(Snowflake(9));
        db.put_cape_config(g, &cfg, None).unwrap();
        let back = db.cape_config(g).unwrap();
        assert!(back.announces(), "{back:?}");
        assert_eq!(back.ping_role_id, None, "no role is not a mistake here");

        // Turning the feature off keeps the list - the page still shows what came out - but the
        // notifier stops treating an announcement as something it may send.
        cfg.enabled = false;
        db.put_cape_config(g, &cfg, None).unwrap();
        let back = db.cape_config(g).unwrap();
        assert!(
            !back.announces(),
            "off means off even with a channel set: {back:?}"
        );
        assert_eq!(
            back.channel_id,
            Some(Snowflake(9)),
            "the setting is remembered"
        );
        assert_eq!(
            db.cape_releases(g, 20).unwrap().len(),
            0,
            "nothing was written by this test"
        );
    }

    #[test]
    fn pruning_keeps_the_newest_and_drops_their_frames() {
        let (db, _dir) = db_with_guilds();
        let g = Snowflake(7);
        for i in 0..6 {
            db.remember_cape(g, &new(200 + i, &format!("Cape {i}"), 1, true))
                .unwrap();
        }
        assert_eq!(db.cape_releases(g, 50).unwrap().len(), 6);
        let gone = db.prune_capes(4).unwrap();
        assert_eq!(gone, 2, "the two oldest: {gone}");
        let rows = db.cape_releases(g, 50).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].name, "Cape 5");
        assert!(
            db.cape_render(g, "mw200", 0).unwrap().is_none(),
            "the frames went with the row"
        );
        assert!(db.cape_render(g, "mw205", 0).unwrap().is_some());
    }
}
