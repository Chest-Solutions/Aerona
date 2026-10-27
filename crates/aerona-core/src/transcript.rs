//! Transcripts, as the file a moderator actually opens.
//!
//! The default is HTML for one reason: a ticket's evidence is usually the picture. A text
//! transcript of a report says "there is a screenshot above this line", and a JSON transcript says
//! nothing a person would read. All three formats are still offered, because a bot that posts a
//! `.html` file into a channel nobody can open is a bot that has no evidence at all.
//!
//! Nothing here runs a script and nothing loads a stylesheet: the file is opened from a download in
//! a browser, sometimes offline, and a transcript that can execute the message it is quoting is a
//! hole, not a feature. Discord's markdown is escaped first and formatted second, in that order, so
//! a member who typed `<script>` reads as `<script>`.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

use crate::models::{TicketFile, TicketRecord, TranscriptFormat};
use crate::security;
use crate::time;

/// What to write, and what to call the file.
pub fn render(
    ticket: &TicketRecord,
    rows: &[serde_json::Value],
    format: TranscriptFormat,
    server: &str,
) -> (String, String) {
    render_with_files(ticket, rows, format, server, &[])
}

/// The same, with the copies Aerona kept of the files on those rows.
///
/// Only HTML takes them. A `.txt` transcript is a sentence per line and a `.json` one is for a
/// program; both already list the files by name. The HTML one is the file a moderator opens to see
/// the screenshot, and a screenshot that 404s because the ticket channel was deleted on a timer is
/// the failure this exists to prevent - so the bytes go *inside* the document as `data:` URIs and
/// the file carries its evidence with it, offline, forever, wherever it is downloaded to.
pub fn render_with_files(
    ticket: &TicketRecord,
    rows: &[serde_json::Value],
    format: TranscriptFormat,
    server: &str,
    files: &[TicketFile],
) -> (String, String) {
    match format {
        TranscriptFormat::Html => (
            format!("{}.html", ticket.reference),
            html(ticket, rows, server, files),
        ),
        TranscriptFormat::Json => (format!("{}.json", ticket.reference), json(ticket, rows)),
        TranscriptFormat::Text => (format!("{}.txt", ticket.reference), text(ticket, rows)),
    }
}

/// Which picture belongs to which row. Keyed by the pair the writer used - `(seq, position in the
/// row's own list)` - because that is what a transcript walk can reproduce without remembering
/// anything else.
pub fn file_index(files: &[TicketFile]) -> std::collections::HashMap<(i64, i64), &TicketFile> {
    files.iter().map(|f| ((f.seq, f.idx), f)).collect()
}

/// The plain one: greppable, printable, readable on a phone by a moderator with no editor.
pub fn text(ticket: &TicketRecord, rows: &[serde_json::Value]) -> String {
    let mut out = String::new();
    out.push_str(&format!("Aerona ticket {}\n", ticket.reference));
    out.push_str(&format!("server  {}\n", ticket.guild_id));
    if !ticket.subject.is_empty() {
        out.push_str(&format!("subject {}\n", ticket.subject));
    }
    out.push_str(&format!("opened  {}\n\n", time::format(ticket.created_at)));
    for row in rows {
        let at = row
            .get("at")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let author = field(row, "author").unwrap_or_else(|| "someone".into());
        let content = field(row, "content").unwrap_or_default();
        let internal = row
            .get("internal")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        // `who: what`, and a staff note marked after the sentence rather than before it: a
        // transcript that reads "modlooking into it" is not a transcript.
        out.push_str(&format!(
            "[{}] {}: {}{}\n",
            time::format(at),
            author,
            content,
            if internal { " (internal)" } else { "" }
        ));
        for (url, name, _kind, _width) in attachments(row) {
            out.push_str(&format!("          file: {name} {url}\n"));
        }
    }
    if rows.is_empty() {
        out.push_str("(no messages were recorded)\n");
    }
    out
}

/// Machine readable, for an export or another bot.
pub fn json(ticket: &TicketRecord, rows: &[serde_json::Value]) -> String {
    let value = serde_json::json!({
        "reference": ticket.reference,
        "guildId": ticket.guild_id.as_u64().to_string(),
        "subject": ticket.subject,
        "openedAt": ticket.created_at,
        "closedAt": ticket.closed_at,
        "messages": rows,
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into())
}

/// The one worth opening. Images are shown, not listed.
pub fn html(
    ticket: &TicketRecord,
    rows: &[serde_json::Value],
    server: &str,
    files: &[TicketFile],
) -> String {
    let kept = file_index(files);
    let place = if server.is_empty() { "Aerona" } else { server };
    let mut body = String::new();
    for row in rows {
        let at = row
            .get("at")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let author = field(row, "author").unwrap_or_else(|| "someone".into());
        let author_id = field(row, "authorId").unwrap_or_default();
        let id_number: u64 = author_id.parse().unwrap_or(0);
        let internal = row
            .get("internal")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        // The same default-avatar rule Discord's own embeds use: no request, no hash needed, and
        // a nameless member still gets a face.
        let avatar = format!(
            "https://cdn.discordapp.com/embed/avatars/{}.png",
            (id_number >> 22) % 6
        );
        let content = inline(&field(row, "content").unwrap_or_default());
        let author_esc = escape(&author);
        let mut files = String::new();
        let seq = row
            .get("seq")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        for (n, (url, name, kind, width)) in attachments(row).into_iter().enumerate() {
            let held = kept.get(&(seq, n as i64));
            // A copy we hold becomes a data URI, and the Discord URL stays as the anchor around
            // it: the picture is what must survive, and the link is still where a moderator clicks
            // to see the original if the message happens to still exist.
            // Two addresses, not one. The picture the browser *draws* is the copy we hold when we
            // have one; the link it *navigates* to stays Discord's, because clicking a transcript's
            // screenshot is how a moderator gets at the original file, at full size, with its real
            // name - while they still can.
            let link = escape(&url);
            let href = match held {
                Some(f) if kind == "image" => {
                    format!("data:{};base64,{}", f.content_type, B64.encode(&f.bytes))
                }
                _ => link.clone(),
            };
            let label = escape(&name);
            // An image is shown at its own width, capped by CSS: the point of the HTML file is
            // that a moderator sees the screenshot rather than a link to it.
            let piece = match kind {
                "image" => format!(
                    "<a class=\"att\" href=\"{link}\"><img src=\"{href}\" alt=\"{label}\"{}{}></a>",
                    if held.is_some() {
                        ""
                    } else {
                        " loading=\"lazy\""
                    },
                    width.map(|w| format!(" width=\"{w}\"")).unwrap_or_default()
                ),
                "video" => format!("<video src=\"{href}\" controls></video>"),
                "audio" => format!("<audio src=\"{href}\" controls></audio>"),
                _ => format!("<a class=\"file\" href=\"{href}\" download>{label}</a>"),
            };
            files.push_str(&piece);
            if let Some(f) = held {
                files.push_str(&format!(
                    "<p class=\"kept\">{} · {} inside this file</p>",
                    escape(&f.name),
                    human_bytes(f.bytes.len() as u64)
                ));
            }
        }
        body.push_str(&format!(
            concat!(
                "<li class=\"{cls}\"><img class=\"av\" src=\"{avatar}\" alt=\"\">",
                "<div><p class=\"who\"><b>{author}</b><time>{at}</time>{badge}</p>",
                "<p class=\"what\">{content}</p>{files}</div></li>"
            ),
            cls = if internal { "internal" } else { "public" },
            avatar = avatar,
            author = author_esc,
            at = time::format(at),
            badge = if internal {
                "<i class=\"tag\">staff note</i>"
            } else {
                ""
            },
            content = content,
            files = if files.is_empty() {
                String::new()
            } else {
                format!("<div class=\"files\">{files}</div>")
            },
        ));
    }
    if rows.is_empty() {
        body.push_str("<li class=\"empty\">Nothing was said in this ticket.</li>");
    }
    let closed = ticket
        .closed_at
        .map(|at| format!("closed {}", time::format(at)))
        .unwrap_or_else(|| "open".to_string());
    let subject = if ticket.subject.is_empty() {
        String::new()
    } else {
        format!("<p class=\"subject\">{}</p>", escape(&ticket.subject))
    };

    format!(
        r#"<!doctype html>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src https: data:; media-src https:; style-src 'unsafe-inline'">
<title>Ticket {ref_} · {place}</title>
<style>
:root {{ color-scheme: light dark; --bg: #ffffff; --fg: #14161a; --soft: #6b7280; --line: #e5e7eb; --note: #fff8e1; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg: #14161a; --fg: #e7e9ee; --soft: #9aa1ad; --line: #262a33; --note: #2a2416; }} }}
body {{ margin: 0; padding: 24px 16px 64px; background: var(--bg); color: var(--fg); font: 15px/1.55 system-ui, -apple-system, "Segoe UI", sans-serif; }}
main {{ max-width: 720px; margin: 0 auto; }}
h1 {{ font-size: 19px; margin: 0 0 2px; }}
.sub, time, .empty {{ color: var(--soft); font-size: 12px; }}
.subject {{ margin: 10px 0 0; padding: 10px 12px; border-left: 3px solid var(--line); background: color-mix(in srgb, var(--line) 30%, transparent); border-radius: 4px; }}
ol {{ list-style: none; margin: 18px 0 0; padding: 0; display: grid; gap: 10px; }}
li {{ display: grid; grid-template-columns: 34px 1fr; gap: 10px; padding: 10px 12px; border: 1px solid var(--line); border-radius: 10px; }}
li.internal {{ background: var(--note); }}
li.empty {{ grid-template-columns: 1fr; }}
.av {{ width: 34px; height: 34px; border-radius: 50%; }}
.who {{ margin: 0; display: flex; gap: 8px; align-items: baseline; flex-wrap: wrap; }}
.who time {{ margin-left: auto; }}
.tag {{ font-style: normal; font-size: 11px; color: var(--soft); border: 1px solid var(--line); border-radius: 999px; padding: 0 7px; }}
.what {{ margin: 3px 0 0; white-space: pre-wrap; overflow-wrap: anywhere; }}
code {{ font-family: ui-monospace, Menlo, Consolas, monospace; font-size: 13px; background: color-mix(in srgb, var(--line) 45%, transparent); padding: 1px 4px; border-radius: 4px; }}
.files {{ display: grid; gap: 8px; margin-top: 8px; }}
.files img {{ max-width: 100%; border-radius: 8px; border: 1px solid var(--line); }}
.kept {{ margin: 2px 0 0; font-size: 11px; color: var(--soft); }}
a {{ color: inherit; text-decoration-color: var(--soft); }}
footer {{ margin-top: 22px; color: var(--soft); font-size: 12px; }}
</style>
<main>
<h1>Ticket {ref_}</h1>
<p class="sub">{place} · opened {opened} · {closed} · {count} message(s)</p>
{subject}
<ol>
{body}
</ol>
<footer>Written by Aerona. {foot}</footer>
</main>
"#,
        ref_ = escape(&ticket.reference),
        place = escape(place),
        opened = time::format(ticket.created_at),
        closed = closed,
        count = rows.len(),
        subject = subject,
        body = body,
        foot = footer_note(rows, files),
    )
}

/// The last line, which is where the document tells the truth about what it can and cannot show.
/// "Attachment links are Discord's" was the old sentence and it was always a warning; now there is
/// something specific to say, because the copies are in here and the links that were not copied are
/// still on a CDN that does not owe anybody a file.
fn footer_note(rows: &[serde_json::Value], files: &[TicketFile]) -> String {
    let listed: usize = rows.iter().map(|r| attachments(r).len()).sum();
    if listed == 0 {
        return "No files in this conversation.".to_string();
    }
    let copies = files.len();
    if copies == 0 {
        return format!(
            "{listed} file(s) are linked to Discord's CDN and stop working if the message is deleted there."
        );
    }
    format!(
        "{copies} of {listed} file(s) are inside this document and will always show; the rest are          links to Discord's CDN, which stop working when the message goes away."
    )
}

/// Bytes the way a person reads them. In the footer of a document a moderator is skimming, "3.1 MB"
/// is a fact and "3221776" is a distraction.
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} {}", UNITS[0])
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

/// A row's author name, content, or any other string field, without the `Value` ceremony at every
/// call site.
fn field(row: &serde_json::Value, key: &str) -> Option<String> {
    row.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// `(url, name, kind, width)` for every file on a row. Missing or malformed lists read as none,
/// because a transcript that will not render is worse than one with a picture missing.
fn attachments(row: &serde_json::Value) -> Vec<(String, String, &'static str, Option<u64>)> {
    let Some(list) = row.get("attachments").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|a| {
            let url = field(a, "url")?;
            let kind = match field(a, "kind").as_deref() {
                Some("image") => "image",
                Some("video") => "video",
                Some("audio") => "audio",
                _ => "file",
            };
            Some((
                url,
                field(a, "name").unwrap_or_else(|| "attachment".into()),
                kind,
                a.get("width").and_then(serde_json::Value::as_u64),
            ))
        })
        .collect()
}

/// HTML escape, which is everything a transcript needs to be safe to open.
fn escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// A message body as HTML. Escape first, format second, in that order: escaping afterwards would
/// wrap the tags this function just added. The vocabulary is deliberately tiny (`**bold**` and
/// `` `code` ``), because a transcript is evidence and a renderer that guesses at markdown guesses
/// wrong in front of a moderator.
fn inline(raw: &str) -> String {
    let safe = escape(&security::clean(raw, 4000));
    let mut out = String::with_capacity(safe.len());
    let mut chars = safe.chars().peekable();
    let (mut code, mut bold) = (false, false);
    while let Some(c) = chars.next() {
        match c {
            '`' => {
                code = !code;
                out.push_str(if code { "<code>" } else { "</code>" });
            }
            '*' if chars.peek() == Some(&'*') && !code => {
                chars.next();
                bold = !bold;
                out.push_str(if bold { "<b>" } else { "</b>" });
            }
            c => out.push(c),
        }
    }
    // An unclosed marker in what somebody typed must not leak a tag into the rest of the file.
    if bold {
        out.push_str("</b>");
    }
    if code {
        out.push_str("</code>");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Snowflake;
    use crate::models::{TicketFile, TicketRecord, TicketStatus};

    fn ticket() -> TicketRecord {
        TicketRecord {
            id: 4,
            guild_id: Snowflake(10),
            channel_id: Snowflake(11),
            owner_id: Snowflake(12),
            claimed_by: None,
            status: TicketStatus::Open,
            subject: "broken onboarding".into(),
            reference: "AER-QQ3479".into(),
            created_at: 1_700_000_000,
            closed_at: None,
            message_count: 2,
            delete_at: None,
            channel_deleted_at: None,
        }
    }

    fn rows() -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({
                "at": 1, "seq": 1, "author": "alice", "authorId": "42", "content": "look at this <script>alert(1)</script>",
                "internal": false,
                "attachments": [{ "url": "https://cdn.discordapp.com/attachments/1/2/shot.png", "name": "shot.png", "kind": "image", "width": 800 }]
            }),
            serde_json::json!({ "at": 2, "seq": 2, "author": "mod", "authorId": "7", "content": "**checked** the logs", "internal": true, "attachments": [] }),
        ]
    }

    fn kept(name: &str, seq: i64, idx: i64, bytes: &[u8]) -> TicketFile {
        TicketFile {
            seq,
            idx,
            name: name.into(),
            content_type: "image/png".into(),
            width: 800,
            height: 600,
            bytes: bytes.to_vec(),
        }
    }

    /// The whole point of `ticket_files`: the picture is *in* the file the moderator keeps, so the
    /// document still shows it after the channel it came from is deleted. And the original URL is
    /// still the link around it, because while the message exists a moderator wants the real file.
    #[test]
    fn a_kept_picture_is_inside_the_document_and_still_links_the_original() {
        let bytes = [0x89u8, b'P', b'N', b'G', 1, 2, 3, 4];
        let body = html(
            &ticket(),
            &rows(),
            "Nine",
            &[kept("shot.png", 1, 0, &bytes)],
        );
        let want = format!("data:image/png;base64,{}", B64.encode(bytes));
        assert!(
            body.contains(&want),
            "the kept bytes must be a data URI, got a document of {} bytes without it",
            body.len()
        );
        assert!(
            body.contains("href=\"https://cdn.discordapp.com/attachments/1/2/shot.png\""),
            "the anchor around the picture is still the original file"
        );
        assert!(
            body.contains("shot.png &middot; 8 B inside this file")
                || body.contains("shot.png · 8 B inside this file"),
            "the caption says what is in here: {body}"
        );
        assert!(
            body.contains("1 of 1 file(s) are inside this document"),
            "the footer counts the copies"
        );
    }

    /// A copy is not required for the transcript to be correct, and a file we did not copy must not
    /// be presented as one we did: no data URI, no caption, and a footer that warns about the link.
    #[test]
    fn a_file_we_could_not_copy_stays_a_link_and_the_footer_says_so() {
        let body = html(&ticket(), &rows(), "Nine", &[]);
        assert!(!body.contains("data:image"), "nothing was kept: {body}");
        assert!(body.contains("shot.png"), "the file is still listed");
        assert!(
            body.contains("1 file(s) are linked to Discord"),
            "the footer names the risk instead of hiding it"
        );
        // The picture belongs to the row it was attached to. A renderer that put a kept file on the
        // wrong message would be worse than one that showed none, so the lookup is by (seq, idx)
        // and a row without a kept file gets no data URI even when the ticket has one.
        let misfiled = html(&ticket(), &rows(), "Nine", &[kept("shot.png", 7, 0, b"xx")]);
        assert!(
            !misfiled.contains("data:image"),
            "a copy for seq 7 must not paint row 1"
        );
    }

    /// A document that inlines `data:` URIs and ships a CSP that forbids them is a document with
    /// broken pictures and a console nobody reads.
    #[test]
    fn the_policy_permits_the_inlined_data_uris_it_just_wrote() {
        let body = html(&ticket(), &rows(), "Nine", &[]);
        let policy = body
            .lines()
            .find(|l| l.contains("Content-Security-Policy"))
            .expect("the document states its policy");
        assert!(policy.contains("img-src https: data:"), "{policy}");
        assert!(policy.contains("default-src 'none'"), "{policy}");
    }

    /// Only HTML carries the bytes. A `.txt` transcript stays text and a `.json` one stays
    /// machine-readable, which is what anybody parsing them expects.
    #[test]
    fn the_other_formats_are_untouched_by_a_kept_file() {
        let t = ticket();
        let files = [kept("shot.png", 1, 0, b"abcdef")];
        let (n, txt) = render_with_files(&t, &rows(), TranscriptFormat::Text, "Nine", &files);
        assert_eq!(n, "AER-QQ3479.txt");
        assert!(!txt.contains("base64"), "{txt}");
        assert!(
            txt.contains("file: shot.png https://cdn.discordapp.com"),
            "{txt}"
        );
        let (_, js) = render_with_files(&t, &rows(), TranscriptFormat::Json, "Nine", &files);
        let v: serde_json::Value = serde_json::from_str(&js).expect("json transcript parses");
        assert_eq!(v["messages"][0]["attachments"][0]["name"], "shot.png");
        assert!(
            js.len() < 4096,
            "no kilobytes of base64 in a json transcript"
        );
    }

    #[test]
    fn human_bytes_reads_like_a_file_size() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(3_221_225), "3.1 MB");
    }

    #[test]
    fn every_format_carries_the_header_and_every_message() {
        let t = ticket();
        for (name, (fname, body)) in [
            ("txt", render(&t, &rows(), TranscriptFormat::Text, "Nine")),
            ("json", render(&t, &rows(), TranscriptFormat::Json, "Nine")),
            ("html", render(&t, &rows(), TranscriptFormat::Html, "Nine")),
        ] {
            assert_eq!(
                fname,
                format!("AER-QQ3479.{name}"),
                "a file a browser can guess"
            );
            assert!(body.contains("AER-QQ3479"), "{name} lost the reference");
            assert!(body.contains("alice"), "{name} lost a speaker");
            assert!(
                body.contains("broken onboarding"),
                "{name} lost the subject"
            );
        }
    }

    #[test]
    fn the_html_shows_the_picture_and_marks_the_staff_note() {
        let (fname, body) = render(&ticket(), &rows(), TranscriptFormat::Html, "Nine");
        assert_eq!(fname, "AER-QQ3479.html");
        assert!(
            body.contains("<img src=\"https://cdn.discordapp.com/attachments/1/2/shot.png\""),
            "{body}"
        );
        assert!(
            body.contains("class=\"internal\""),
            "the staff note must be visibly a staff note"
        );
        assert!(body.contains("<b>alice</b>"), "{body}");
    }

    #[test]
    fn markup_a_member_typed_is_text_not_html() {
        let (_, body) = render(&ticket(), &rows(), TranscriptFormat::Html, "");
        assert!(
            !body.contains("<script>alert(1)</script>"),
            "a transcript must not run what it quotes"
        );
        assert!(body.contains("&lt;script&gt;"), "{body}");
        // The markdown the renderer does honour, it honours on the member's terms.
        assert!(body.contains("<b>checked</b> the logs"), "{body}");
    }

    #[test]
    fn a_ticket_nobody_spoke_in_says_so() {
        let (_, body) = render(&ticket(), &[], TranscriptFormat::Html, "Nine");
        assert!(body.contains("Nothing was said"), "{body}");
        let text = text(&ticket(), &[]);
        assert!(text.contains("no messages were recorded"), "{text}");
    }

    #[test]
    fn a_broken_attachment_list_is_no_attachments() {
        let rows = vec![
            serde_json::json!({ "at": 1, "author": "a", "content": "x", "internal": false, "attachments": "not a list" }),
        ];
        let (_, body) = render(&ticket(), &rows, TranscriptFormat::Html, "");
        assert!(body.contains(">x<"), "the message still renders: {body}");
    }
}
