//! Tickets: panels, tickets, the transcript, and the counters the dashboard shows.
//!
//! A ticket's channel id is `UNIQUE` on purpose: two live tickets in one channel
//! would make every later lookup ambiguous, and a unique index is a cheaper
//! guarantee than a code path that has to cope with it.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::identity::{self, Snowflake};
use crate::models::{TicketPanel, TicketRecord, TicketStatus};
// (models re-export is intentional: repositories return model types, not rows)

use super::{now, Db};

/// Input for creating a ticket. Everything the caller must supply; the reference
/// and counters are generated here so they can never disagree with the DB.
#[derive(Debug, Clone, Deserialize)]
pub struct NewTicket {
    pub guild_id: Snowflake,
    pub channel_id: Snowflake,
    pub user_channel: Option<Snowflake>,
    pub owner_id: Snowflake,
    pub subject: String,
    pub panel_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct TicketCreation {
    pub id: i64,
    pub reference: String,
    pub name: String,
}

impl Db {
    // -- panels --------------------------------------------------------------

    /// One button in one channel. `color`, `subject` and `intro` are what make it a ticket
    /// *type* rather than a second way to open the same ticket: a ticket opened from this row
    /// takes them in place of the guild's own card settings.
    pub fn add_panel(&self, p: &TicketPanel) -> Result<i64> {
        // Discord's own ceiling for one message: five rows of five buttons. A type nobody can
        // click is worse than a refused write that says why, and it is the difference between a
        // board that stops drawing and a board the operator never sees.
        if self.panel_count(p.guild_id, p.channel_id)? >= Self::PANELS_PER_CHANNEL {
            return Err(Error::Invalid(format!(
                "a board holds {} buttons at most; remove one first",
                Self::PANELS_PER_CHANNEL
            )));
        }
        let at = now();
        let (label, emoji) = (
            crate::security::scrub(&p.label, 80),
            p.emoji.clone().filter(|e| !e.is_empty()),
        );
        let (subject, intro) = (
            crate::security::scrub(&p.subject, 200),
            crate::security::scrub_block(&p.intro, 1500),
        );
        let (gid, cid, mid) = (
            p.guild_id.as_i64(),
            p.channel_id.as_i64(),
            p.message_id.as_i64(),
        );
        let role = p.support_role_id.map(|s| s.as_i64());
        let color = p.color.map(|c| i64::from(c & 0xff_ff_ff));
        let seq = p.sort;
        self.tx(move |conn| {
            conn.execute(
                "INSERT INTO ticket_panels(guild_id,channel_id,message_id,label,emoji,support_role_id,seq,created_at,color,subject,intro)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![gid, cid, mid, label, emoji, role, seq, at, color, subject, intro],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// The rows a `SELECT` of panels reads, in the order `panel_from_row` expects.
    const PANEL_COLS: &'static str = "id, guild_id, channel_id, message_id, label, emoji,
             support_role_id, seq, color, subject, intro";

    pub fn panels(&self, guild: Snowflake) -> Result<Vec<TicketPanel>> {
        let g = guild.as_i64();
        self.read(move |conn| {
            let mut st = conn
                .prepare(&format!(
                    "SELECT {} FROM ticket_panels WHERE guild_id=?1 ORDER BY seq, id",
                    Self::PANEL_COLS
                ))
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![g], panel_from_row)
                .map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::db)
        })
    }

    pub fn panel_for_message(
        &self,
        guild: Snowflake,
        message: Snowflake,
    ) -> Result<Option<TicketPanel>> {
        let (g, m) = (guild.as_i64(), message.as_i64());
        self.read(move |conn| {
            let mut st = conn
                .prepare(&format!(
                    "SELECT {} FROM ticket_panels WHERE guild_id=?1 AND message_id=?2",
                    Self::PANEL_COLS
                ))
                .map_err(Error::db)?;
            let row = st
                .query_map(params![g, m], panel_from_row)
                .map_err(Error::db)?
                .next()
                .transpose()
                .map_err(Error::db)?;
            Ok(row)
        })
    }

    /// One panel by id, for a click whose custom id names its type.
    pub fn panel_by_id(&self, guild: Snowflake, panel_id: i64) -> Result<Option<TicketPanel>> {
        let (g, id) = (guild.as_i64(), panel_id);
        self.read(move |conn| {
            let mut st = conn
                .prepare(&format!(
                    "SELECT {} FROM ticket_panels WHERE guild_id=?1 AND id=?2",
                    Self::PANEL_COLS
                ))
                .map_err(Error::db)?;
            st.query_row(params![g, id], panel_from_row)
                .optional()
                .map_err(Error::db)
        })
    }

    /// Every type in one channel, in board order. This is what `panel.redraw` posts: the rows and
    /// the buttons in Discord are the same list, in the same order, or the board lies.
    /// Five action rows of five buttons, which is what one Discord message can carry.
    pub const PANELS_PER_CHANNEL: usize = 25;

    /// How many types this channel already offers. Counts every row, bound or not, because the
    /// unbound ones are about to become buttons too.
    pub fn panel_count(&self, guild: Snowflake, channel: Snowflake) -> Result<usize> {
        let n: i64 = self.read(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM ticket_panels WHERE guild_id = ?1 AND channel_id = ?2",
                params![guild.as_i64(), channel.as_i64()],
                |r| r.get(0),
            )
            .map_err(Error::db)
        })?;
        Ok(n.max(0) as usize)
    }

    pub fn panels_for_channel(
        &self,
        guild: Snowflake,
        channel: Snowflake,
    ) -> Result<Vec<TicketPanel>> {
        let (g, c) = (guild.as_i64(), channel.as_i64());
        self.read(move |conn| {
            let mut st = conn
                .prepare(&format!(
                    "SELECT {} FROM ticket_panels WHERE guild_id=?1 AND channel_id=?2
                     ORDER BY seq, id",
                    Self::PANEL_COLS
                ))
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![g, c], panel_from_row)
                .map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::db)
        })
    }

    /// Record which message a type's button is on. `0` unbinds it, which is what a redraw does to
    /// every row before it binds the survivors to the message it just sent or edited.
    pub fn bind_panel(&self, guild: Snowflake, panel_id: i64, message: Snowflake) -> Result<()> {
        let (g, id, m) = (guild.as_i64(), panel_id, message.as_i64());
        self.tx(move |conn| {
            conn.execute(
                "UPDATE ticket_panels SET message_id=?3 WHERE id=?1 AND guild_id=?2",
                params![id, g, m],
            )?;
            Ok(())
        })
    }

    pub fn delete_panel(&self, guild: Snowflake, panel_id: i64) -> Result<bool> {
        let (g, id) = (guild.as_i64(), panel_id);
        self.tx(move |conn| {
            Ok(conn.execute(
                "DELETE FROM ticket_panels WHERE id=?1 AND guild_id=?2",
                params![id, g],
            )? > 0)
        })
    }

    // -- tickets -------------------------------------------------------------

    /// Create a ticket, generating a unique reference. Two columns are unique
    /// (`channel_id`, `(guild_id, reference)`) so a retry loop is bounded.
    pub fn create_ticket(
        &self,
        req: &NewTicket,
        name_format: &str,
        user_tag: &str,
    ) -> Result<TicketCreation> {
        let subject = crate::security::clean(&req.subject, 180);
        let (g, ch, owner) = (
            req.guild_id.as_i64(),
            req.channel_id.as_i64(),
            req.owner_id.as_i64(),
        );
        let user_ch = req.user_channel.map(|s| s.as_i64());
        let panel = req.panel_id;
        let at = now();
        let fmt = name_format.to_string();
        let tag = crate::security::scrub(user_tag, 32);

        // Deterministic pre-check: a ticket channel belongs to exactly one ticket,
        // so re-opening from a double click returns the live record. (Matching on
        // SQLite's error *text* would be brittle; we only use it as a backstop for
        // the race where two checks pass simultaneously.)
        if let Some(t) = self.ticket_by_channel(req.guild_id, req.channel_id)? {
            let reference = t.reference;
            let name = render_ticket_name(&fmt, &reference, &tag);
            return Ok(TicketCreation {
                id: t.id,
                reference,
                name,
            });
        }
        let mut last_err: Option<Error> = None;
        // The reference is unique per guild, so a collision is possible: retry a
        // few times with a fresh code instead of surfacing a 409 to a user who
        // did nothing wrong.
        for _ in 0..4 {
            let reference = identity::ticket_reference();
            let name = render_ticket_name(&fmt, &reference, &tag);
            let subject = subject.clone();
            let res = self.tx(move |conn| {
                conn.execute(
                    "INSERT INTO tickets(guild_id,channel_id,user_channel,owner_id,status,subject,reference,
                                          created_at,last_activity,panel_id)
                     VALUES(?1,?2,?3,?4,'open',?5,?6,?7,?7,?8)",
                    params![g, ch, user_ch, owner, subject, reference, at, panel],
                )?;
                Ok(TicketCreation {
                    id: conn.last_insert_rowid(),
                    reference: reference.clone(),
                    name: name.clone(),
                })
            });
            match res {
                Ok(v) => return Ok(v),
                // The channel already has a ticket: hand that one back rather than
                // failing a double-click on the "Open ticket" button.
                Err(Error::Conflict(msg)) if msg.contains("tickets.channel_id") => {
                    if let Some(t) = self.ticket_by_channel(req.guild_id, req.channel_id)? {
                        let reference = t.reference;
                        let name = render_ticket_name(&fmt, &reference, &tag);
                        return Ok(TicketCreation {
                            id: t.id,
                            reference,
                            name,
                        });
                    }
                    last_err = Some(Error::Conflict(msg));
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err
            .unwrap_or_else(|| Error::Conflict("could not allocate a ticket reference".into())))
    }

    pub fn ticket_by_channel(
        &self,
        guild: Snowflake,
        channel: Snowflake,
    ) -> Result<Option<TicketRecord>> {
        let (g, c) = (guild.as_i64(), channel.as_i64());
        self.read(move |conn| {
            // Guild-scoped as well: a channel id must never resolve across tenants.
            let sql = format!("{TICKET_SELECT} WHERE t.guild_id = ?1 AND t.channel_id = ?2");
            conn.query_row(&sql, params![g, c], TicketRecord::from_row)
                .optional()
                .map_err(Error::db)
        })
    }

    pub fn ticket_by_id(&self, id: i64) -> Result<Option<TicketRecord>> {
        self.read(move |conn| {
            let sql = format!("{TICKET_SELECT} WHERE t.id = ?1");
            conn.query_row(&sql, params![id], TicketRecord::from_row)
                .optional()
                .map_err(Error::db)
        })
    }

    pub fn list_tickets(
        &self,
        guild: Snowflake,
        status: Option<TicketStatus>,
        owner: Option<Snowflake>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<TicketRecord>> {
        let limit = limit.clamp(1, 200) as i64;
        let offset = offset.min(100_000) as i64;
        let g = guild.as_i64();
        let status = status.map(|s| s.as_str().to_string());
        let owner = owner.map(|o| o.as_i64());
        self.read(move |conn| {
            // Optional filters mean the SQL is assembled, but only from a closed
            // set of fragments, and every value stays bound. Plain `?` keeps the
            // binding order obvious - no index arithmetic to get wrong.
            let mut sql = String::from(TICKET_SELECT);
            let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(g)];
            sql.push_str(" WHERE t.guild_id=?");
            if status.is_some() {
                sql.push_str(" AND t.status=?");
                args.push(Box::new(status.clone().unwrap()));
            }
            if let Some(o) = owner {
                sql.push_str(" AND t.owner_id=?");
                args.push(Box::new(o));
            }
            sql.push_str(" ORDER BY CASE t.status WHEN 'open' THEN 0 WHEN 'pending' THEN 1 ELSE 2 END, t.last_activity DESC LIMIT ? OFFSET ?");
            args.push(Box::new(limit));
            args.push(Box::new(offset));
            let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
            let mut st = conn.prepare(&sql).map_err(Error::db)?;
            let rows = st.query_map(refs.as_slice(), TicketRecord::from_row).map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::db)
        })
    }

    pub fn open_ticket_count(&self, guild: Snowflake, owner: Option<Snowflake>) -> Result<i64> {
        let g = guild.as_i64();
        self.read(move |conn| match owner {
            Some(o) => conn.query_row(
                "SELECT COUNT(*) FROM tickets WHERE guild_id=?1 AND owner_id=?2 AND status IN ('open','pending')",
                params![g, o.as_i64()],
                |r| r.get::<_, i64>(0),
            )
            .map_err(Error::db),
            None => conn.query_row(
                "SELECT COUNT(*) FROM tickets WHERE guild_id=?1 AND status IN ('open','pending')",
                params![g],
                |r| r.get::<_, i64>(0),
            )
            .map_err(Error::db),
        })
    }

    pub fn claim_ticket(&self, ticket_id: i64, staff: Snowflake, force: bool) -> Result<bool> {
        let (id, s) = (ticket_id, staff.as_i64());
        let at = now();
        self.tx(move |conn| {
            let sql = if force {
                "UPDATE tickets SET claimed_by=?2, status='open', last_activity=?3 WHERE id=?1"
            } else {
                "UPDATE tickets SET claimed_by=?2, status='open', last_activity=?3 WHERE id=?1 AND claimed_by IS NULL"
            };
            Ok(conn.execute(sql, params![id, s, at])? > 0)
        })
    }

    pub fn unclaim_ticket(&self, ticket_id: i64) -> Result<bool> {
        let id = ticket_id;
        self.tx(move |conn| {
            Ok(conn.execute(
                "UPDATE tickets SET claimed_by=NULL WHERE id=?1",
                params![id],
            )? > 0)
        })
    }

    pub fn set_ticket_status(&self, ticket_id: i64, status: TicketStatus) -> Result<bool> {
        let (id, s) = (ticket_id, status.as_str().to_string());
        let at = now();
        self.tx(move |conn| {
            let n = conn.execute(
                "UPDATE tickets SET status=?2, last_activity=?3, closed_at=CASE WHEN ?2='closed' THEN ?3 ELSE closed_at END
                 WHERE id=?1",
                params![id, s, at],
            )?;
            Ok(n > 0)
        })
    }

    pub fn append_ticket_message(
        &self,
        ticket_id: i64,
        author: Snowflake,
        author_tag: &str,
        content: &str,
        internal: bool,
        attachments: &str,
    ) -> Result<i64> {
        // `clean`, not `scrub`: the ellipsis `scrub` appends would end up inside a
        // saved transcript, and character counting (not bytes) is what a limit means.
        let content = crate::security::clean(content, 4000);
        let tag = crate::security::clean(author_tag, 64);
        // Anything that is not a JSON array becomes an empty one: a transcript must render, and
        // a malformed attachment list is the recorder's problem, not the reader's.
        let attachments = match serde_json::from_str::<Vec<serde_json::Value>>(attachments) {
            Ok(list) if list.len() <= 10 => {
                serde_json::to_string(&list).unwrap_or_else(|_| "[]".into())
            }
            _ => "[]".into(),
        };
        let (id, a) = (ticket_id, author.as_i64());
        let at = now();
        let flag = i64::from(internal);
        self.tx(move |conn| {
            let guild: i64 = conn
                .query_row("SELECT guild_id FROM tickets WHERE id=?1", params![id], |r| r.get::<_, i64>(0))
                .optional()?
                .ok_or(Error::NotFound("ticket"))?;
            // Denormalise the author name at write time. The transcript is a legal
            // record of a conversation, so it must read correctly years later, when
            // the nickname has long since changed.
            if !tag.is_empty() {
                conn.execute(
                    "INSERT INTO users(id,username,discriminator,first_seen,last_seen)
                     VALUES(?1,?2,'0',?3,?3)
                     ON CONFLICT(id) DO UPDATE SET username=excluded.username, last_seen=excluded.last_seen",
                    params![a, tag, at],
                )?;
            }
            let seq: i64 = conn
                .query_row("SELECT COALESCE(MAX(seq),0) + 1 FROM ticket_messages WHERE ticket_id=?1", params![id], |r| r.get(0))
                .unwrap_or(1);
            conn.execute(
                "INSERT INTO ticket_messages(ticket_id,guild_id,author_id,author_tag,content,is_internal,seq,created_at,attachments)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![id, guild, a, tag, content, flag, seq, at, attachments],
            )?;
            let mid = conn.last_insert_rowid();
            conn.execute(
                "UPDATE tickets SET message_count = message_count + 1, last_activity=?2 WHERE id=?1",
                params![id, at],
            )?;
            Ok(mid)
        })
    }

    pub fn ticket_transcript(
        &self,
        ticket_id: i64,
        include_internal: bool,
    ) -> Result<Vec<serde_json::Value>> {
        let id = ticket_id;
        self.read(move |conn| {
            let sql = format!(
                "SELECT m.author_id, u.username, m.content, m.is_internal, m.created_at, m.seq, m.attachments
                 FROM ticket_messages m LEFT JOIN users u ON u.id = m.author_id
                 WHERE m.ticket_id = ?1 {} ORDER BY m.seq",
                if include_internal { "" } else { "AND m.is_internal = 0" }
            );
            let mut st = conn.prepare(&sql).map_err(Error::db)?;
            let rows = st
                .query_map(params![id], |r| {
                    Ok(serde_json::json!({
                        "authorId": r.get::<_, i64>(0)?.to_string(),
                        "author": r.get::<_, Option<String>>(1)?.unwrap_or_else(|| "unknown".into()),
                        "content": r.get::<_, String>(2)?,
                        "internal": r.get::<_, i64>(3)? != 0,
                        "at": r.get::<_, i64>(4)?,
                        "seq": r.get::<_, i64>(5)?,
                        // Stored as JSON text, handed on as JSON: a transcript renderer that had
                        // to parse this twice would be free to disagree with itself.
                        "attachments": r
                            .get::<_, Option<String>>(6)?
                            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                            .unwrap_or_else(|| serde_json::json!([])),
                    }))
                })
                .map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::db)
        })
    }

    /// Tickets idle past `days`, for auto-close. `days <= 0` disables the feature.
    pub fn stale_tickets(&self, guild: Snowflake, days: i64) -> Result<Vec<TicketRecord>> {
        if days <= 0 {
            return Ok(vec![]);
        }
        let g = guild.as_i64();
        let cutoff = now() - days * 86_400;
        self.read(move |conn| {
            let sql = format!("{TICKET_SELECT} WHERE t.guild_id=?1 AND t.status IN ('open','pending') AND t.last_activity < ?2 ORDER BY t.last_activity LIMIT 100");
            let mut st = conn.prepare(&sql).map_err(Error::db)?;
            let rows = st
                .query_map(params![g, cutoff], TicketRecord::from_row)
                .map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::db)
        })
    }

    /// Called when a ticket channel is deleted on Discord: close the record so
    /// the dashboard cannot offer actions against a channel that no longer exists.
    pub fn orphan_ticket(&self, guild: Snowflake, channel: Snowflake) -> Result<bool> {
        let (g, c) = (guild.as_i64(), channel.as_i64());
        let at = now();
        self.tx(move |conn| {
            Ok(conn.execute(
                "UPDATE tickets SET status='closed', closed_at=?3, last_activity=?3
                 WHERE guild_id=?1 AND channel_id=?2 AND status != 'closed'",
                params![g, c, at],
            )? > 0)
        })
    }

    pub fn ticket_summary(&self, guild: Snowflake) -> Result<serde_json::Value> {
        let g = guild.as_i64();
        let since = now() - 7 * 86_400;
        self.read(move |conn| {
            let open = count(conn, "SELECT COUNT(*) FROM tickets WHERE guild_id=?1 AND status='open'", &[&g])?;
            let pending = count(conn, "SELECT COUNT(*) FROM tickets WHERE guild_id=?1 AND status='pending'", &[&g])?;
            let closed_week = count(
                conn,
                "SELECT COUNT(*) FROM tickets WHERE guild_id=?1 AND status='closed' AND closed_at >= ?2",
                &[&g, &since],
            )?;
            Ok(serde_json::json!({
                "open": open,
                "pending": pending,
                "closedLast7d": closed_week,
            }))
        })
    }

    // -- a closed ticket's deadline, and the files its transcript keeps ------

    /// When this ticket's channel should be deleted, or `None` to un-schedule it. A reopen clears
    /// it: a channel that is talking again is not trash, and leaving the old deadline in place
    /// would have the janitor delete a live conversation an hour later.
    pub fn schedule_ticket_delete(&self, ticket_id: i64, at: Option<i64>) -> Result<()> {
        let id = ticket_id;
        self.tx(move |conn| {
            let n = conn.execute(
                "UPDATE tickets SET delete_at=?2, channel_deleted_at=NULL WHERE id=?1",
                params![id, at],
            )?;
            if n == 0 {
                return Err(Error::NotFound("ticket"));
            }
            Ok(())
        })
    }

    /// Tickets whose deadline has passed, oldest first. The `channel_deleted_at IS NULL` half is
    /// what makes this a queue rather than a repeated attempt: a row is offered until the janitor
    /// records the fact, and after that it never comes back.
    pub fn tickets_due_for_delete(&self, at: i64, limit: usize) -> Result<Vec<TicketRecord>> {
        let limit = limit.clamp(1, 64) as i64;
        self.read(move |conn| {
            let mut st = conn
                .prepare(&format!(
                    "{TICKET_SELECT} WHERE t.delete_at IS NOT NULL AND t.delete_at <= ?1
                       AND t.channel_deleted_at IS NULL
                     ORDER BY t.delete_at LIMIT ?2"
                ))
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![at, limit], TicketRecord::from_row)
                .map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::db)
        })
    }

    /// Record that the channel is gone. Idempotent, and it clears the deadline so a row cannot be
    /// offered twice even under a concurrent tick.
    pub fn mark_ticket_channel_deleted(&self, ticket_id: i64, at: i64) -> Result<()> {
        let (id, when) = (ticket_id, at);
        self.tx(move |conn| {
            conn.execute(
                "UPDATE tickets SET delete_at=NULL, channel_deleted_at=?2 WHERE id=?1",
                params![id, when],
            )?;
            Ok(())
        })
    }

    /// Keep one attachment for the transcript. The size limits are enforced here rather than only
    /// at the caller, because the caller is a best-effort path that would rather skip than fail,
    /// and because a hand-written row must not be able to blow up a document a browser has to open.
    #[allow(clippy::too_many_arguments)]
    pub fn put_ticket_file(
        &self,
        guild: Snowflake,
        ticket_id: i64,
        seq: i64,
        idx: i64,
        name: &str,
        content_type: &str,
        width: u32,
        height: u32,
        bytes: &[u8],
        source: &str,
    ) -> Result<()> {
        use crate::models::TicketConfig;
        if bytes.is_empty() {
            return Err(Error::Invalid("an empty file is not a kept file".into()));
        }
        if bytes.len() as u64 > TicketConfig::FILE_MAX {
            return Err(Error::Invalid(format!(
                "a kept file is at most {} bytes, this is {}",
                TicketConfig::FILE_MAX,
                bytes.len()
            )));
        }
        let (g, id, s, i) = (guild.as_i64(), ticket_id, seq, idx);
        let name = crate::security::clean(name, 180);
        let ctype = crate::security::clean(content_type, 120);
        let source = crate::security::clean(source, 500);
        let (bytes, at) = (bytes.to_vec(), now());
        self.tx(move |conn| {
            let held: i64 = conn
                .query_row(
                    "SELECT COALESCE(SUM(LENGTH(bytes)),0) FROM ticket_files WHERE ticket_id=?1",
                    params![id],
                    |r| r.get(0),
                )
                .map_err(Error::db)?;
            if held + bytes.len() as i64 > TicketConfig::FILES_MAX as i64 {
                return Err(Error::Invalid(format!(
                    "this ticket already holds {held} bytes of the {} it may keep",
                    TicketConfig::FILES_MAX
                )));
            }
            conn.execute(
                "INSERT INTO ticket_files(guild_id,ticket_id,seq,idx,name,content_type,width,height,bytes,source,copied_at)
                 VALUES(?10,?1,?2,?3,?4,?5,?6,?7,?8,?9,?11)
                 ON CONFLICT(ticket_id,seq,idx) DO UPDATE SET
                   name=excluded.name, content_type=excluded.content_type,
                   width=excluded.width, height=excluded.height,
                   bytes=excluded.bytes, source=excluded.source, copied_at=excluded.copied_at",
                params![id, s, i, name, ctype, width as i64, height as i64, bytes, source, g, at],
            )?;
            Ok(())
        })
    }

    /// Everything kept for one ticket, oldest row first: the map a transcript renderer looks a
    /// `(seq, idx)` pair up in.
    pub fn ticket_files(&self, ticket_id: i64) -> Result<Vec<crate::models::TicketFile>> {
        use crate::models::TicketFile;
        let id = ticket_id;
        self.read(move |conn| {
            let mut st = conn
                .prepare(
                    "SELECT seq, idx, name, content_type, width, height, bytes
                     FROM ticket_files WHERE ticket_id=?1 ORDER BY seq, idx",
                )
                .map_err(Error::db)?;
            let rows = st
                .query_map(params![id], |r| {
                    Ok(TicketFile {
                        seq: r.get(0)?,
                        idx: r.get(1)?,
                        name: r.get(2)?,
                        content_type: r.get(3)?,
                        width: r.get::<_, i64>(4)? as u32,
                        height: r.get::<_, i64>(5)? as u32,
                        bytes: r.get(6)?,
                    })
                })
                .map_err(Error::db)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::db)
        })
    }

    /// How much a ticket is holding, for the panel row that says so out loud instead of leaving a
    /// moderator to wonder why a download is large.
    pub fn ticket_file_stats(&self, ticket_id: i64) -> Result<(u32, i64)> {
        let id = ticket_id;
        self.read(move |conn| {
            conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(LENGTH(bytes)),0) FROM ticket_files WHERE ticket_id=?1",
                params![id],
                |r| Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i64>(1)?)),
            )
            .map_err(Error::db)
        })
    }

    /// Drop copies older than `older_than` for one guild, for a guild that set
    /// `keep_ticket_files_days`. The transcript itself is untouched: what goes is the *copy*, and
    /// the renderer falls back to the original link, which is the honest answer to being told not
    /// to keep it.
    pub fn prune_ticket_files(&self, guild: Snowflake, older_than: i64) -> Result<u64> {
        let (g, cut) = (guild.as_i64(), older_than);
        self.tx(move |conn| {
            let n = conn.execute(
                "DELETE FROM ticket_files WHERE guild_id=?1 AND copied_at < ?2",
                params![g, cut],
            )?;
            Ok(n as u64)
        })
    }
}

fn count(conn: &Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Result<i64> {
    conn.query_row(sql, args, |r| r.get::<_, i64>(0))
        .map_err(Error::db)
}

const TICKET_SELECT: &str =
    "SELECT t.id, t.guild_id, t.channel_id, t.owner_id, t.claimed_by, t.status,
                t.subject, t.reference, t.created_at, t.closed_at, t.message_count,
                t.delete_at, t.channel_deleted_at
         FROM tickets t";

impl TicketRecord {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        // An unknown status string can only come from a hand-edited database;
        // reading it as `open` keeps the ticket actionable instead of hidden.
        let status = TicketStatus::parse(&r.get::<_, String>(5)?).unwrap_or(TicketStatus::Open);
        Ok(TicketRecord {
            id: r.get(0)?,
            guild_id: Snowflake(r.get::<_, i64>(1)? as u64),
            channel_id: Snowflake(r.get::<_, i64>(2)? as u64),
            owner_id: Snowflake(r.get::<_, i64>(3)? as u64),
            claimed_by: r.get::<_, Option<i64>>(4)?.map(|v| Snowflake(v as u64)),
            status,
            subject: r.get(6)?,
            reference: r.get(7)?,
            created_at: r.get(8)?,
            closed_at: r.get(9)?,
            message_count: r.get(10)?,
            delete_at: r.get::<_, Option<i64>>(11)?,
            channel_deleted_at: r.get::<_, Option<i64>>(12)?,
        })
    }
}

/// Supports `{ref}`, `{user}`, `{tag}`, `{n}`. Unknown placeholders are preserved
/// verbatim so a typo is visible instead of silently dropped.
pub fn render_ticket_name(fmt: &str, reference: &str, user_tag: &str) -> String {
    let mut out = String::with_capacity(fmt.len() + 16);
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            out.push(c);
            continue;
        }
        let mut key = String::new();
        let mut closed = false;
        for ch in chars.by_ref() {
            if ch == '}' {
                closed = true;
                break;
            }
            key.push(ch);
            if key.len() > 16 {
                break;
            }
        }
        if !closed {
            out.push('{');
            out.push_str(&key);
            continue;
        }
        match key.as_str() {
            "ref" | "reference" => out.push_str(reference),
            "user" | "tag" => out.push_str(user_tag),
            "n" | "number" => out.push_str(reference.rsplit('-').next().unwrap_or("")),
            other => {
                out.push('{');
                out.push_str(other);
                out.push('}');
            }
        }
    }
    // Discord caps channel names at 100 chars and forbids a few glyphs.
    let cleaned: String = out
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '#' | '<' | '>' | '@' | '|' | '/' | '\\'))
        .collect();
    cleaned
        .to_lowercase()
        .replace(' ', "-")
        .chars()
        .take(96)
        .collect()
}

/// A panel row, read in `PANEL_COLS`' order. Empty `subject` and `intro` mean "the guild's own
/// wording", which is why they are not `Option`: an empty string is what a guild that set nothing
/// stores, and the worker treats it the same way it treats a missing key.
fn panel_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<TicketPanel> {
    Ok(TicketPanel {
        id: r.get(0)?,
        guild_id: Snowflake(r.get::<_, i64>(1)? as u64),
        channel_id: Snowflake(r.get::<_, i64>(2)? as u64),
        message_id: Snowflake(r.get::<_, i64>(3)? as u64),
        label: r.get(4)?,
        emoji: r.get(5)?,
        support_role_id: r.get::<_, Option<i64>>(6)?.map(|v| Snowflake(v as u64)),
        sort: r.get(7)?,
        color: r.get::<_, Option<i64>>(8)?.map(|v| v as u32),
        subject: r.get(9)?,
        intro: r.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::testutil::temp_db;
    use crate::models::TicketConfig;

    const G: i64 = 500;

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
    fn ticket_name_rendering_is_safe_and_bounded() {
        assert_eq!(
            render_ticket_name("ticket-{ref}-{user}", "AER-ABC123", "rye"),
            "ticket-aer-abc123-rye"
        );
        assert_eq!(render_ticket_name("{n}-ticket", "AER-42", "x"), "42-ticket");
        // Both `|` and `/` are stripped before space→dash, so a doubled hyphen is
        // expected - it is cosmetic and Discord accepts it.
        assert_eq!(
            render_ticket_name("hi <@123> #chan | ok/\\", "R", "u"),
            "hi-123-chan--ok",
            "unsafe glyphs removed"
        );
        assert_eq!(
            render_ticket_name("{bogus}-{ref}", "R", "u"),
            "{bogus}-r",
            "unknown placeholder preserved"
        );
        assert_eq!(render_ticket_name("unclosed-{", "R", "u"), "unclosed-{");
        let long = render_ticket_name(&"a".repeat(300), "R", "u");
        assert!(long.len() <= 96, "{}", long.len());
        assert_eq!(render_ticket_name("", "R", "u"), "");
    }

    #[test]
    fn create_claim_transcript_close() {
        let (db, _d) = temp_db();
        seed(&db);
        let req = NewTicket {
            guild_id: g(),
            channel_id: Snowflake(1),
            user_channel: Some(Snowflake(2)),
            owner_id: Snowflake(77),
            subject: "billing question".into(),
            panel_id: None,
        };
        let c = db
            .create_ticket(&req, "ticket-{ref}-{user}", "rye")
            .unwrap();
        assert!(c.reference.starts_with("AER-"));
        assert!(!c.name.is_empty());

        assert!(db.claim_ticket(c.id, Snowflake(9), false).unwrap());
        // Double claim must fail, and `force` must succeed.
        assert!(!db.claim_ticket(c.id, Snowflake(10), false).unwrap());
        assert!(db.claim_ticket(c.id, Snowflake(10), true).unwrap());
        assert!(db.unclaim_ticket(c.id).unwrap());

        db.append_ticket_message(
            c.id,
            Snowflake(77),
            "rye",
            "hello there",
            false,
            r#"[{"url":"https://cdn.discordapp.com/attachments/1/2/shot.png","name":"shot.png","kind":"image"}]"#,
        )
            .unwrap();
        db.append_ticket_message(c.id, Snowflake(10), "mod", "internal note", true, "[]")
            .unwrap();
        let public = db.ticket_transcript(c.id, false).unwrap();
        assert_eq!(public.len(), 1, "internal notes stay out of the transcript");
        assert_eq!(public[0]["content"], "hello there");
        assert_eq!(public[0]["author"], "rye", "author must be resolvable");
        // The point of the column: a transcript that cannot show the screenshot is a transcript
        // that says "there was a picture here".
        assert_eq!(public[0]["attachments"][0]["name"], "shot.png");
        assert_eq!(
            public[0]["attachments"][0]["url"],
            "https://cdn.discordapp.com/attachments/1/2/shot.png"
        );
        let all = db.ticket_transcript(c.id, true).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1]["author"], "mod");
        assert_eq!(all[1]["internal"], true);
        assert_eq!(db.ticket_by_id(c.id).unwrap().unwrap().message_count, 2);

        assert!(db.set_ticket_status(c.id, TicketStatus::Closed).unwrap());
        let t = db.ticket_by_id(c.id).unwrap().unwrap();
        assert_eq!(t.status, TicketStatus::Closed);
        assert!(t.closed_at.is_some(), "closing must stamp closed_at");
    }

    #[test]
    fn duplicate_channel_reuses_existing_ticket() {
        let (db, _d) = temp_db();
        seed(&db);
        let req = NewTicket {
            guild_id: g(),
            channel_id: Snowflake(42),
            user_channel: None,
            owner_id: Snowflake(1),
            subject: String::new(),
            panel_id: None,
        };
        let a = db.create_ticket(&req, "ticket-{ref}", "u").unwrap();
        let b = db.create_ticket(&req, "ticket-{ref}", "u").unwrap();
        assert_eq!(a.id, b.id, "same channel must not create a second ticket");
        assert_eq!(db.open_ticket_count(g(), None).unwrap(), 1);
    }

    #[test]
    fn limits_and_filters() {
        let (db, _d) = temp_db();
        seed(&db);
        for i in 0..5 {
            let req = NewTicket {
                guild_id: g(),
                channel_id: Snowflake(100 + i),
                user_channel: None,
                owner_id: Snowflake(if i % 2 == 0 { 7 } else { 8 }),
                subject: format!("s{i}"),
                panel_id: None,
            };
            db.create_ticket(&req, "t-{ref}", "u").unwrap();
            if i == 4 {
                let id = db
                    .ticket_by_channel(g(), Snowflake(104))
                    .unwrap()
                    .unwrap()
                    .id;
                db.set_ticket_status(id, TicketStatus::Pending).unwrap();
            }
        }
        assert_eq!(
            db.list_tickets(g(), Some(TicketStatus::Open), None, 10, 0)
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            db.list_tickets(g(), Some(TicketStatus::Pending), None, 10, 0)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.list_tickets(g(), None, Some(Snowflake(7)), 10, 0)
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            db.list_tickets(g(), None, Some(Snowflake(8)), 10, 0)
                .unwrap()
                .len(),
            2
        );
        let page = db.list_tickets(g(), None, None, 2, 0).unwrap();
        assert_eq!(page.len(), 2, "limit honoured");
        let page2 = db.list_tickets(g(), None, None, 2, 2).unwrap();
        assert_eq!(page2.len(), 2, "offset honoured");
        assert_ne!(page[0].id, page2[0].id);
        // Open+pending count for the per-user limit check.
        assert_eq!(db.open_ticket_count(g(), Some(Snowflake(7))).unwrap(), 3);
        // Out-of-range inputs clamp rather than error.
        assert_eq!(
            db.list_tickets(g(), None, None, 0, 0).unwrap().len(),
            1,
            "limit 0 → 1"
        );
        assert_eq!(
            db.list_tickets(g(), None, None, 9_999, 0).unwrap().len(),
            5,
            "limit clamps to 200, only 5 exist"
        );
        assert!(
            db.list_tickets(g(), None, None, 10, 10_000_000)
                .unwrap()
                .is_empty(),
            "past-the-end offset is empty, not fatal"
        );
    }

    #[test]
    fn panels_crud_and_lookup_by_message() {
        let (db, _d) = temp_db();
        seed(&db);
        let p = TicketPanel {
            id: 0,
            guild_id: g(),
            channel_id: Snowflake(10),
            message_id: Snowflake(11),
            label: "Open a ticket".into(),
            emoji: Some("📩".into()),
            support_role_id: Some(Snowflake(12)),
            sort: 1,
            color: Some(0x9b_59_b6),
            subject: "Report a player".into(),
            intro: "Tell us who, and what happened.".into(),
        };
        let id = db.add_panel(&p).unwrap();
        assert_eq!(db.panels(g()).unwrap().len(), 1);
        let found = db
            .panel_for_message(g(), Snowflake(11))
            .unwrap()
            .expect("found");
        assert_eq!(found.id, id);
        assert_eq!(found.label, "Open a ticket");
        assert_eq!(found.support_role_id, Some(Snowflake(12)));
        // A panel is a ticket type, so the three fields that make it one have to survive a write.
        assert_eq!(found.color, Some(0x9b_59_b6));
        assert_eq!(found.subject, "Report a player");
        assert_eq!(found.intro, "Tell us who, and what happened.");
        assert!(db.panel_for_message(g(), Snowflake(999)).unwrap().is_none());
        assert!(
            db.panel_for_message(Snowflake(999), Snowflake(11))
                .unwrap()
                .is_none(),
            "guild-scoped"
        );
        assert!(db.delete_panel(g(), id).unwrap());
        assert!(!db.delete_panel(g(), id).unwrap());
    }

    /// A board is one Discord message, so it holds what one message can hold. The 26th type is
    /// refused rather than drawn, because a board that fails to send reads to the operator as
    /// "the button never appeared" - which is a worse bug than a message that says why.
    #[test]
    fn a_channel_holds_no_more_types_than_a_message_holds_buttons() {
        let (db, _d) = temp_db();
        seed(&db);
        let mk = |channel: u64, label: String, sort: i64| TicketPanel {
            id: 0,
            guild_id: g(),
            channel_id: Snowflake(channel),
            message_id: Snowflake(0),
            label,
            emoji: None,
            support_role_id: None,
            sort,
            color: None,
            subject: String::new(),
            intro: String::new(),
        };
        for i in 0..Db::PANELS_PER_CHANNEL {
            db.add_panel(&mk(10, format!("type {i}"), i as i64))
                .unwrap_or_else(|e| panic!("type {i} should fit: {e}"));
        }
        assert_eq!(db.panel_count(g(), Snowflake(10)).unwrap(), 25);
        let one_more = db.add_panel(&mk(10, "too many".into(), 99));
        assert!(one_more.is_err(), "a 26th button has nowhere to go");
        // Another channel is another board, with its own room.
        db.add_panel(&mk(11, "elsewhere".into(), 0)).unwrap();
        assert_eq!(db.panel_count(g(), Snowflake(11)).unwrap(), 1);
        // Deleting makes room again, so the cap is about the board, not about history.
        let first = db.panels_for_channel(g(), Snowflake(10)).unwrap()[0].id;
        db.delete_panel(g(), first).unwrap();
        db.add_panel(&mk(10, "room again".into(), 98)).unwrap();
        assert_eq!(db.panel_count(g(), Snowflake(10)).unwrap(), 25);
    }

    #[test]
    fn stale_and_orphaned_tickets() {
        let (db, _d) = temp_db();
        seed(&db);
        let req = NewTicket {
            guild_id: g(),
            channel_id: Snowflake(9),
            user_channel: None,
            owner_id: Snowflake(1),
            subject: String::new(),
            panel_id: None,
        };
        let c = db.create_ticket(&req, "t-{ref}", "u").unwrap();
        assert!(
            db.stale_tickets(g(), 0).unwrap().is_empty(),
            "disabled by 0"
        );
        assert!(db.stale_tickets(g(), 3).unwrap().is_empty(), "just created");
        db.tx(|conn| {
            conn.execute(
                "UPDATE tickets SET last_activity = 100 WHERE id=?1",
                params![c.id],
            )?;
            Ok(())
        })
        .unwrap();
        assert_eq!(db.stale_tickets(g(), 1).unwrap().len(), 1);

        // Deleting the Discord channel closes the record.
        assert!(db.orphan_ticket(g(), Snowflake(9)).unwrap());
        assert!(
            !db.orphan_ticket(g(), Snowflake(9)).unwrap(),
            "already closed"
        );
        assert_eq!(
            db.ticket_by_channel(g(), Snowflake(9))
                .unwrap()
                .unwrap()
                .status,
            TicketStatus::Closed
        );
    }

    #[test]
    fn transcript_messages_are_scrubbed_and_capped() {
        let (db, _d) = temp_db();
        seed(&db);
        let req = NewTicket {
            guild_id: g(),
            channel_id: Snowflake(5),
            user_channel: None,
            owner_id: Snowflake(1),
            subject: "a\nfake log line".into(),
            panel_id: None,
        };
        let c = db.create_ticket(&req, "t-{ref}", "u").unwrap();
        let t = db.ticket_by_id(c.id).unwrap().unwrap();
        assert!(!t.subject.contains('\n'), "subject: {}", t.subject);
        db.append_ticket_message(c.id, Snowflake(1), "u", &"x".repeat(9000), false, "[]")
            .unwrap();
        let tr = db.ticket_transcript(c.id, false).unwrap();
        assert_eq!(
            tr[0]["content"].as_str().unwrap().chars().count(),
            4000,
            "capped"
        );
    }

    #[test]
    fn missing_ticket_is_not_an_error_for_counts() {
        let (db, _d) = temp_db();
        seed(&db);
        assert_eq!(db.open_ticket_count(g(), None).unwrap(), 0);
        assert!(db.ticket_by_id(12_345).unwrap().is_none());
        assert!(
            db.append_ticket_message(12_345, Snowflake(1), "u", "x", false, "[]")
                .is_err(),
            "appending to a missing ticket must fail loudly"
        );
    }

    #[test]
    fn ticket_config_defaults_are_off() {
        // Sanity on the model this module depends on: nothing auto-creates without opting in.
        assert!(!TicketConfig::default().enabled);
    }

    /// The janitor's queue, tested as a queue: a row is offered once and then never again. The bug
    /// this guards is the retry loop - a delete that "succeeded" by the channel already being gone,
    /// or a tick that found the same deadline every sixty seconds and pinged the same moderator
    /// each time.
    #[test]
    fn a_close_deadline_is_a_queue_and_not_a_retry_loop() {
        let (db, _d) = temp_db();
        seed(&db);
        let open = |ch: u64| {
            db.create_ticket(
                &NewTicket {
                    guild_id: g(),
                    channel_id: Snowflake(ch),
                    user_channel: Some(Snowflake(ch + 100)),
                    owner_id: Snowflake(77),
                    subject: "s".into(),
                    panel_id: None,
                },
                "ticket-{ref}-{user}",
                "rye",
            )
            .unwrap()
        };
        let (a, b, c) = (open(1), open(2), open(3));
        db.schedule_ticket_delete(a.id, Some(100)).unwrap();
        db.schedule_ticket_delete(b.id, Some(50)).unwrap();
        db.schedule_ticket_delete(c.id, Some(9_000)).unwrap();

        let due = db.tickets_due_for_delete(100, 10).unwrap();
        assert_eq!(
            due.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![b.id, a.id],
            "the deadline that passed first is deleted first"
        );
        assert_eq!(due[0].delete_at, Some(50));
        assert_eq!(due[0].channel_deleted_at, None);

        // Recording the fact takes the row out of the queue permanently, and clears the deadline so
        // a second tick cannot find it again.
        db.mark_ticket_channel_deleted(b.id, 101).unwrap();
        let after = db.tickets_due_for_delete(10_000, 10).unwrap();
        assert!(
            after.iter().all(|t| t.id != b.id),
            "a ticket whose channel is recorded gone comes back forever: {:?}",
            after.iter().map(|t| t.id).collect::<Vec<_>>()
        );
        assert_eq!(
            db.ticket_by_id(b.id)
                .unwrap()
                .map(|t| (t.delete_at, t.channel_deleted_at))
                .unwrap(),
            (None, Some(101)),
            "the deadline is cleared and the fact is kept, so the panel can say when"
        );

        // A reopen is the same call with no time: the queue empties, nothing else moves.
        db.schedule_ticket_delete(a.id, None).unwrap();
        db.schedule_ticket_delete(c.id, None).unwrap();
        assert!(db.tickets_due_for_delete(1_000_000, 10).unwrap().is_empty());
        assert!(
            matches!(
                db.schedule_ticket_delete(424_242, Some(1)),
                Err(Error::NotFound(_))
            ),
            "a deadline for a ticket that does not exist is refused, not swallowed"
        );
        // The limit is a rate, not a correctness knob: five due rows, offered two at a time.
        for t in [a.id, b.id, c.id] {
            db.schedule_ticket_delete(t, Some(5)).ok();
        }
        assert_eq!(db.tickets_due_for_delete(6, 2).unwrap().len(), 2);
    }

    /// Kept attachments are the copy the transcript is built from, so the limits have to live in the
    /// store: the caller is a best-effort path that must be able to skip a file rather than fail a
    /// ticket close, and a document a browser opens cannot be allowed to be 200 MB.
    #[test]
    fn kept_files_are_recorded_capped_and_prunable() {
        let (db, _d) = temp_db();
        seed(&db);
        let t = db
            .create_ticket(
                &NewTicket {
                    guild_id: g(),
                    channel_id: Snowflake(1),
                    user_channel: None,
                    owner_id: Snowflake(77),
                    subject: "billing".into(),
                    panel_id: None,
                },
                "ticket-{ref}-{user}",
                "rye",
            )
            .unwrap();
        let png = |n: usize| {
            let mut v = vec![0u8; n];
            v[..8].copy_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
            v
        };
        db.put_ticket_file(
            g(),
            t.id,
            1,
            0,
            "shot.png",
            "image/png",
            800,
            600,
            &png(120),
            "https://cdn/1",
        )
        .unwrap();
        // The same (seq, idx) again is an update: a retry of the same close must not double the file.
        db.put_ticket_file(
            g(),
            t.id,
            1,
            0,
            "shot.png",
            "image/png",
            800,
            600,
            &png(90),
            "https://cdn/1",
        )
        .unwrap();
        let files = db.ticket_files(t.id).unwrap();
        assert_eq!(files.len(), 1, "a retry is the same row");
        assert_eq!(files[0].bytes.len(), 90);
        assert_eq!((files[0].width, files[0].height), (800, 600));
        assert_eq!(files[0].seq, 1);
        assert_eq!(db.ticket_file_stats(t.id).unwrap(), (1, 90));

        // A second file on a second message is a second row.
        db.put_ticket_file(
            g(),
            t.id,
            2,
            0,
            "log.txt",
            "text/plain",
            0,
            0,
            b"trace",
            "https://cdn/2",
        )
        .unwrap();
        assert_eq!(db.ticket_file_stats(t.id).unwrap(), (2, 95));

        // The caps are the model's, and they are enforced here rather than trusted to the caller.
        let one_too_many = vec![0u8; TicketConfig::FILE_MAX as usize + 1];
        assert!(matches!(
            db.put_ticket_file(
                g(),
                t.id,
                3,
                0,
                "big.png",
                "image/png",
                0,
                0,
                &one_too_many,
                "x"
            ),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            db.put_ticket_file(g(), t.id, 3, 0, "nothing.png", "image/png", 0, 0, b"", "x"),
            Err(Error::Invalid(_))
        ));
        let full = vec![7u8; TicketConfig::FILE_MAX as usize];
        db.put_ticket_file(g(), t.id, 4, 0, "a.png", "image/png", 0, 0, &full, "x")
            .unwrap();
        // One at the per-file ceiling fits, and so does a smaller second file: the total is what is
        // capped, not the count, which is why a ticket full of thumbnails is not a loophole.
        let small = vec![7u8; 1 << 20];
        db.put_ticket_file(g(), t.id, 4, 1, "b.png", "image/png", 0, 0, &small, "x")
            .unwrap();
        assert!(
            matches!(
                db.put_ticket_file(g(), t.id, 5, 0, "c.png", "image/png", 0, 0, &full, "x"),
                Err(Error::Invalid(_))
            ),
            "a third file at the per-file ceiling is over the per-ticket one"
        );

        // Pruning by age is a guild's retention choice, and it takes only the copies.
        let (db2, _d2) = temp_db();
        seed(&db2);
        let t2 = db2
            .create_ticket(
                &NewTicket {
                    guild_id: g(),
                    channel_id: Snowflake(9),
                    user_channel: None,
                    owner_id: Snowflake(77),
                    subject: "billing".into(),
                    panel_id: None,
                },
                "ticket-{ref}-{user}",
                "rye",
            )
            .unwrap();
        db2.put_ticket_file(
            g(),
            t2.id,
            1,
            0,
            "shot.png",
            "image/png",
            4,
            4,
            &png(50),
            "x",
        )
        .unwrap();
        assert_eq!(
            db2.prune_ticket_files(g(), i64::MIN).unwrap(),
            0,
            "nothing is old yet"
        );
        assert_eq!(
            db2.prune_ticket_files(g(), i64::MAX).unwrap(),
            1,
            "and then everything is"
        );
        assert!(db2.ticket_files(t2.id).unwrap().is_empty());
        db2.append_ticket_message(
            t2.id,
            Snowflake(77),
            "rye",
            "look at this",
            false,
            r#"[{"url":"https://cdn/1","name":"shot.png","kind":"image"}]"#,
        )
        .unwrap();
        let rows = db2.ticket_transcript(t2.id, false).unwrap();
        assert_eq!(rows.len(), 1);
        // `attachments` comes back parsed, not as text: the renderer that turns a transcript into a
        // document must not have to agree with a second parser about what is valid JSON.
        assert_eq!(
            rows[0]["attachments"][0]["url"].as_str(),
            Some("https://cdn/1"),
            "pruning the copy leaves the link, which is the whole point of making it a copy: {:?}",
            rows[0]
        );
    }
}
