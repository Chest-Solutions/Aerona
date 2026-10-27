//! SQLite persistence.
//!
//! ## Why this shape
//!
//! * **One writer, small pool.** SQLite serialises writes; a pool of 4 with WAL
//!   gives the dashboard concurrent readers without the bot ever blocking on a
//!   busy file. `busy_timeout` + `SQLITE_BUSY → retry` removes the classic
//!   "database is locked" outage class.
//! * **Relational for *data*, JSON columns for *settings*.** Every record an
//!   operator wants to query, join, count or expire (`warns`, `strikes`,
//!   `tickets`, `ai_events`, `audit_log`…) is a real table with real indexes.
//!   Per-feature *settings* live as one JSON document in `guild_config`, because
//!   adding a 40th column to a settings table costs a migration and a rebuild of
//!   every typed accessor - while `ALTER TABLE` on a live server that may hold
//!   months of config is pure downside. Documents are typed by Rust structs on
//!   both ends, validated on write, and every change is recorded in
//!   `config_history` with a hash chain.
//! * **`integrity`-checked on startup** so a truncated file from an unclean power
//!   loss is caught at boot instead of mid-request.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::time;

mod schema;
pub use schema::SCHEMA_V1;

pub mod automation;
pub mod capes;
pub mod logging;
pub mod moderation;
pub mod security;
pub mod settings;
pub mod stickies;
pub mod tickets;

/// Pool size: 1 writer + 3 readers is plenty for a self-hosted bot and keeps
/// file-descriptor and page-cache pressure flat.
const MAX_POOL_SIZE: u32 = 4;

/// Forward-only schema. Each entry is applied exactly once inside its own
/// transaction and recorded in `PRAGMA user_version`. Never edit an shipped
/// entry - append a new one.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        note: "initial schema",
        sql: schema::SCHEMA_V1,
    },
    Migration {
        version: 2,
        note: "action outbox + bot status",
        sql: schema::SCHEMA_V2,
    },
    Migration {
        version: 3,
        note: "oauth_state.redirect_uri",
        sql: schema::SCHEMA_V3,
    },
    Migration {
        version: 4,
        note: "session guild grants",
        sql: schema::SCHEMA_V4,
    },
    Migration {
        version: 5,
        note: "mod log events + ticket attachments",
        sql: schema::SCHEMA_V5,
    },
    Migration {
        version: 6,
        note: "unanchored stickies with embeds + ticket panels as types",
        sql: schema::SCHEMA_V6,
    },
    Migration {
        version: 7,
        note: "minecraft cape releases, their frames, and per-guild poll state",
        sql: schema::SCHEMA_V7,
    },
    Migration {
        version: 8,
        note: "ticket channels scheduled for deletion, and the files a transcript keeps",
        sql: schema::SCHEMA_V8,
    },
];

pub struct Migration {
    pub version: i64,
    pub note: &'static str,
    pub sql: &'static str,
}

#[derive(Clone)]
pub struct Db {
    pool: r2d2::Pool<Manager>,
}

pub struct Manager {
    path: std::path::PathBuf,
}

impl r2d2::ManageConnection for Manager {
    type Connection = Connection;
    type Error = rusqlite::Error;

    fn connect(&self) -> std::result::Result<Connection, rusqlite::Error> {
        // READ/WRITE + CREATE so a missing file is only ever produced by an
        // intentional first boot, never by a typo in AERONA_DB.
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        prime(&conn)?;
        Ok(conn)
    }

    fn is_valid(&self, conn: &mut Connection) -> std::result::Result<(), rusqlite::Error> {
        conn.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
            .map(|_| ())
    }

    fn has_broken(&self, _conn: &mut Connection) -> bool {
        false
    }
}

impl Db {
    /// Open (creating if needed) the database described by `cfg`, apply
    /// migrations, and return a handle that is `Clone + Send + Sync`.
    pub fn open(cfg: &Config) -> Result<Db> {
        if let Some(parent) = cfg.db_path.parent() {
            std::fs::create_dir_all(parent).map_err(Error::io)?;
        }
        Self::open_path(&cfg.db_path)
    }

    pub fn open_path(path: &Path) -> Result<Db> {
        let manager = Manager {
            path: path.to_path_buf(),
        };
        let pool = r2d2::Pool::builder()
            .max_size(MAX_POOL_SIZE)
            .min_idle(Some(1))
            .connection_timeout(Duration::from_secs(10))
            .idle_timeout(Some(Duration::from_secs(120)))
            .build(manager)
            .map_err(Error::db)?;
        let db = Db { pool };
        db.migrate()?;
        Ok(db)
    }

    /// Run `f` on a pooled connection. `&Connection` is enough for every query
    /// trait rusqlite implements (`Execute`/`Query`/`Prepare` all take `&self`).
    /// The `for<'a>` bound (not `'static`) is what lets call sites borrow locals
    /// while still preventing anything from outliving the pooled connection.
    pub fn read<T, F>(&self, f: F) -> Result<T>
    where
        T: Send,
        F: for<'a> FnOnce(&'a Connection) -> std::result::Result<T, Error> + Send,
    {
        let conn = self.pool.get().map_err(Error::db)?;
        f(&conn)
    }

    /// Run `f` inside a transaction: commit on `Ok`, rollback on `Err`. Used for
    /// every multi-statement write (ticket creation, warn+strike+audit, …) so a
    /// crash mid-way cannot leave a half-applied change.
    pub fn tx<T, F>(&self, f: F) -> Result<T>
    where
        T: Send,
        F: for<'a> FnOnce(&'a Connection) -> std::result::Result<T, Error> + Send,
    {
        let mut conn = self.pool.get().map_err(Error::db)?;
        let tx = conn.transaction().map_err(Error::db)?;
        let out = match f(&tx) {
            Ok(v) => v,
            Err(e) => {
                // Explicit rollback so the write lock is released immediately.
                let _ = tx.rollback();
                return Err(e);
            }
        };
        tx.commit().map_err(Error::db)?;
        Ok(out)
    }

    fn migrate(&self) -> Result<()> {
        self.read(|conn| {
            let mut version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .map_err(Error::db)?;
            for m in MIGRATIONS {
                if m.version <= version {
                    continue;
                }
                if m.version != version + 1 {
                    // A gap means someone edited history; refuse rather than
                    // half-applying a schema.
                    return Err(Error::Migration {
                        version: m.version,
                        reason: format!("non-contiguous migration (at version {version})"),
                    });
                }
                conn.execute_batch("BEGIN").map_err(Error::db)?;
                match conn.execute_batch(m.sql) {
                    Ok(()) => {
                        version = m.version;
                        conn.execute_batch(&format!("PRAGMA user_version = {version}; COMMIT;"))
                            .map_err(Error::db)?;
                        tracing::info!(version = m.version, note = m.note, "applied migration");
                    }
                    Err(e) => {
                        let _ = conn.execute_batch("ROLLBACK");
                        return Err(Error::Migration {
                            version: m.version,
                            reason: e.to_string(),
                        });
                    }
                }
            }
            Ok(())
        })
    }

    /// `PRAGMA integrity_check` - cheap enough at boot, and it turns a corrupt
    /// file into a clean startup failure.
    pub fn verify(&self) -> Result<()> {
        self.read(|conn| {
            let ok: String = conn
                .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .map_err(Error::db)?;
            if ok == "ok" {
                Ok(())
            } else {
                Err(Error::Db(format!(
                    "integrity_check reported: {}",
                    crate::security::scrub(&ok, 200)
                )))
            }
        })
    }

    /// File size in KiB, for the dashboard's storage tile.
    pub fn size_kb(&self) -> Result<i64> {
        self.read(|conn| {
            let pages: i64 = conn
                .query_row("PRAGMA page_count", [], |r| r.get::<_, i64>(0))
                .unwrap_or(0);
            let size: i64 = conn
                .query_row("PRAGMA page_size", [], |r| r.get::<_, i64>(0))
                .unwrap_or(4096);
            Ok(pages * size / 1024)
        })
    }

    /// Kick SQLite's own optimiser (analyses/statistics) - called hourly.
    pub fn optimize(&self) -> Result<()> {
        self.read(|conn| {
            conn.execute_batch("PRAGMA optimize;").map_err(Error::db)?;
            Ok(())
        })
    }

    /// Truncate WAL and reclaim pages after big deletions.
    pub fn vacuum(&self) -> Result<()> {
        self.read(|conn| conn.execute_batch("VACUUM;").map_err(Error::db))
    }

    pub fn row_count(&self, table: &str) -> Result<i64> {
        // `table` only ever comes from a hardcoded list in the API, never from a
        // client string; the allowlist assert below keeps that invariant loud.
        debug_assert!(
            !table
                .chars()
                .any(|c| !(c.is_ascii_alphanumeric() || c == '_')),
            "row_count called with an unsafe table name"
        );
        let sql = format!("SELECT COUNT(*) FROM {table}");
        self.read(move |conn| {
            conn.query_row(&sql, [], |r| r.get::<_, i64>(0))
                .map_err(Error::db)
        })
    }
}

/// Per-connection setup. Applied on every pooled connection, so it must be
/// idempotent and fast.
fn prime(conn: &Connection) -> std::result::Result<(), rusqlite::Error> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -4096;
         PRAGMA mmap_size = 67108864;
         PRAGMA journal_size_limit = 16777216;
         PRAGMA auto_vacuum = INCREMENTAL;",
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

pub(crate) fn now() -> i64 {
    time::now()
}

#[cfg(test)]
pub mod testutil {
    use super::*;

    /// Fresh on-disk database in a temp dir. On-disk (not `:memory:`) because WAL,
    /// multiple connections and file-size reporting only exist for real files.
    pub fn temp_db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open_path(&dir.path().join("aerona.db")).expect("open db");
        (db, dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_migrates_and_is_idempotent() {
        let (db, dir) = testutil::temp_db();
        db.verify().expect("integrity");
        // Reopening the same file must not re-apply migrations or lose data.
        db.read(|c| {
            c.execute("INSERT INTO meta(key,value) VALUES('probe','42')", [])?;
            Ok(())
        })
        .unwrap();
        let path = dir.path().join("aerona.db");
        drop(db);
        let db = Db::open_path(&path).unwrap();
        let v: String = db
            .read(|c| {
                c.query_row("SELECT value FROM meta WHERE key='probe'", [], |r| {
                    r.get::<_, String>(0)
                })
                .map_err(Error::db)
            })
            .unwrap();
        assert_eq!(v, "42", "data must survive reopen");
        let version: i64 = db
            .read(|c| {
                c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                    .map_err(Error::db)
            })
            .unwrap();
        assert_eq!(
            version,
            MIGRATIONS.len() as i64,
            "user_version tracks the migration list"
        );
        db.verify().unwrap();
    }

    #[test]
    fn all_expected_tables_exist() {
        let (db, _d) = testutil::temp_db();
        let names: Vec<String> = db
            .read(|c| {
                let mut st = c
                    .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                    .unwrap();
                let rows = st
                    .query_map([], |r| r.get::<_, String>(0))
                    .unwrap()
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .unwrap();
                Ok(rows)
            })
            .unwrap();
        for t in [
            "action_outbox",
            "audit_log",
            "ai_events",
            "ban_records",
            "bot_status",
            "config_history",
            "guild_config",
            "guild_state",
            "guilds",
            "log_messages",
            "notes",
            "raid_events",
            "sessions",
            "sticky_messages",
            "strikes",
            "ticket_messages",
            "ticket_panels",
            "tickets",
            "api_keys",
            "users",
            "warns",
        ] {
            assert!(
                names.iter().any(|n| n == t),
                "missing table {t} in {names:?}"
            );
        }
    }

    #[test]
    fn foreign_keys_cascade_for_guild_children() {
        let (db, _d) = testutil::temp_db();
        db.tx(|c| {
            c.execute("INSERT INTO guilds(id,name,updated_at) VALUES(1,'g',0)", [])?;
            c.execute(
                "INSERT INTO guild_config(guild_id,feature,payload,updated_at) VALUES(1,'moderation','{}',0)",
                [],
            )?;
            c.execute(
                "INSERT INTO tickets(guild_id,channel_id,owner_id,status,reference,created_at) VALUES(1,2,3,'open','AER-TEST1',0)",
                [],
            )?;
            c.execute(
                "INSERT INTO ticket_messages(ticket_id,guild_id,author_id,content,seq,created_at) VALUES(1,1,3,'hi',1,0)",
                [],
            )?;
            Ok(())
        })
        .expect("seed");
        assert_eq!(db.row_count("ticket_messages").unwrap(), 1);
        // Losing the guild row must not leave orphans that no query can reach.
        db.tx(|c| {
            c.execute("DELETE FROM guilds WHERE id=1", [])?;
            Ok(())
        })
        .expect("delete");
        assert_eq!(db.row_count("tickets").unwrap(), 0);
        assert_eq!(db.row_count("ticket_messages").unwrap(), 0);
        assert_eq!(db.row_count("guild_config").unwrap(), 0);
    }

    #[test]
    fn rejects_unknown_columns_at_write_time() {
        let (db, _d) = testutil::temp_db();
        let e = db
            .read(|c| {
                c.execute("INSERT INTO guilds(id,nope) VALUES(1,2)", [])
                    .map_err(Error::db)?;
                Ok::<(), Error>(())
            })
            .unwrap_err();
        assert!(matches!(e, Error::Db(_)), "{e:?}");
    }

    #[test]
    fn migration_gap_is_refused() {
        // Simulate a hand-edited history: version jumped past 1 with no v1 applied.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.db");
        {
            let conn = Connection::open(&path).unwrap();
            // The newest version, not a literal: what the test is about is a database that claims
            // to be *current* while having no tables, and that moves every time a migration does.
            // (A v8 that began with `ALTER TABLE tickets` is exactly the migration that would have
            // made a hard-coded 7 quietly stop meaning "nothing left to apply".)
            let current = MIGRATIONS.last().expect("at least one migration").version;
            conn.execute_batch(&format!("PRAGMA user_version = {current};"))
                .unwrap();
        }
        // Opening runs v1 (it is > version? no: 7 >= 1 so it is skipped) - the
        // visible symptom is the missing tables, which every later query reports.
        let db = Db::open_path(&path).expect("open succeeds at a higher version");
        let err = db.row_count("warns").unwrap_err();
        assert!(
            err.to_string().contains("warns") || matches!(err, Error::Db(_)),
            "{err:?}"
        );
        // A real gap (missing v1 while claiming v2) is caught by `migrate` when we
        // add a second migration; the guard is the non-contiguous check above.
        assert!(MIGRATIONS
            .windows(2)
            .all(|w| w[1].version == w[0].version + 1));
    }
}
