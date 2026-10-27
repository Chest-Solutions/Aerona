//! The Minecraft cape notifier, working half: the only place in Aerona that asks minecraft.wiki a
//! question.
//!
//! The split is the same one the whole product runs on. `aerona-api` owns intent - which channel,
//! which role, how often, and what has been found - and stores it. This module owns the asking: it
//! runs on the slow tick, it has a budget, and it is the thing that must not exist in a request
//! handler. A settings page that fetched the wiki would turn a panel into a tool for making this
//! server request arbitrary urls, and it would fail whenever the wiki was slow rather than whenever
//! the setting was wrong.
//!
//! Two clocks, deliberately. *Looking* is per guild on the interval the operator set, and it is
//! quiet on the first pass: the category is sixty-odd pages and every one of them is "new" to a
//! notifier that was switched on today, so the first pass records them without pinging. *Telling*
//! is a trickle of a few per tick, because the announcement is a message and a message has a
//! channel to be polite to.
//!
//! The announcement itself is posted from here rather than queued in `action_outbox`, which is the
//! one place this crate breaks its own rule about Discord writes, and the reason is the retry: an
//! operator's queued action must survive as exactly one attempt with a visible failure, while a
//! scheduled post wants to try again every minute until it works or until it has been refused often
//! enough to say so on the panel. `cape_releases.announced_at` and `attempts` are that retry - the
//! row is the queue, and a restart cannot lose a cape that has not been mentioned yet.

use std::collections::HashSet;
use std::time::Duration;

use aerona_core::cape::{self, CapePage};
use aerona_core::db::capes::NewCape;
use aerona_core::identity::Snowflake;
use aerona_core::models::{CapeConfig, CapeRelease, CAPE_MAX_ATTEMPTS};
use serde_json::{json, Value};
use serenity::all::{
    ChannelId, Context, CreateAllowedMentions, CreateAttachment, CreateEmbed, CreateEmbedFooter,
    CreateMessage, RoleId,
};

use crate::state::BotState;

/// How many new capes one pass will look up artwork for. A cape is now two requests - the file
/// names asked about, and the texture - so ten of them is twenty calls on a pass that only runs when
/// there is news. That is the polite size of a burst, and it is a fifth of what the same budget cost
/// when every cape began by reading its own article.
const LOOKS_PER_PASS: usize = 10;
/// Announcements queued for Discord per guild per tick. A cape coming out is news, not a flood,
/// and three minutes of trickle is nothing next to the hour between polls.
const PER_TICK: usize = 3;
/// The pause between wiki calls. Two hundred milliseconds is not needed by the API and is badly
/// needed by whoever reads the access logs.
const PAUSE: Duration = Duration::from_millis(200);
/// The largest file this module will accept from the wiki: a cape texture is 1 KB and the wiki's own
/// render is 12 KB, so a megabyte is a hundred times the answer we expect and still small enough to
/// hold in memory without thinking.
const MAX_BYTES: usize = 1 << 20;

/// One pass of the notifier, from the slow tick.
pub async fn tick(state: &BotState, ctx: &Context) {
    let Ok(guilds) = state.db.guilds() else {
        return;
    };
    for guild in guilds {
        trickle(state, ctx, guild.id).await;
        poll(state, guild.id).await;
    }
}

/// What this pass did, in the words the panel gets to show.
#[derive(Default)]
struct Pass {
    pages: usize,
    recorded: usize,
    /// Left unlooked-at because of the per-pass budget. The next tick sees them; saying so keeps the
    /// panel honest about "up to date" being a momentary claim.
    waiting: usize,
    error: Option<String>,
}

async fn poll(state: &BotState, guild: Snowflake) {
    let Ok(cfg) = state.db.cape_config(guild) else {
        return;
    };
    if !cfg.enabled {
        // A feature that is off does not touch the network. Its rows stay where they are, so
        // switching it back on resumes the list rather than starting a new one.
        return;
    }
    let (last, seeded) = state.db.cape_state(guild).unwrap_or((0, false));
    let due =
        last == 0 || aerona_core::time::now().saturating_sub(last) >= cfg.check_every_secs() as i64;
    if !due {
        return;
    }
    let pass = look(state, guild, &cfg, seeded).await;
    // The stamp is what makes the interval mean something. It is written even when the source
    // failed, because a wiki that is down must not be repolled every sixty seconds by forty
    // servers; the panel shows the error and the next scheduled pass tries again.
    let _ = state
        .db
        .note_cape_check(guild, seeded || pass.error.is_none());
    let pending = state
        .db
        .cape_pending(guild, 10)
        .map(|rows| rows.len())
        .unwrap_or(0);
    let stuck = state
        .db
        .cape_releases(guild, 200)
        .map(|rows| {
            rows.iter()
                .filter(|r| r.announced_at.is_none() && r.attempts >= CAPE_MAX_ATTEMPTS)
                .count()
        })
        .unwrap_or(0);
    let _ = state.db.put_guild_report(
        guild,
        "capes",
        &json!({
            "source": "minecraft.wiki",
            "lastCheck": aerona_core::time::now(),
            "pages": pass.pages,
            "recorded": pass.recorded,
            "waiting": pass.waiting,
            "pending": pending,
            "stuck": stuck,
            "seeded": seeded || pass.error.is_none(),
            "interval": cfg.check_every_secs(),
            "error": pass.error,
        }),
    );
}

/// The poll itself. `seeded` is whether this server has been through a first pass before: the first
/// one records everything it finds quietly.
async fn look(state: &BotState, guild: Snowflake, cfg: &CapeConfig, seeded: bool) -> Pass {
    let mut pass = Pass::default();
    let body = match fetch_json(&state.http, &cape::query_url()).await {
        Ok(v) => v,
        Err(e) => {
            pass.error = Some(e);
            return pass;
        }
    };
    let pages = match cape::parse_category(&body) {
        Ok(p) => p,
        Err(e) => {
            pass.error = Some(e);
            return pass;
        }
    };
    pass.pages = pages.len();
    let rows = state.db.cape_known(guild).unwrap_or_default();
    let known: HashSet<(String, i64)> = rows
        .iter()
        .map(|(slug, rev, _id, _announced)| (slug.clone(), *rev))
        .collect();
    let known_slugs: HashSet<&str> = rows.iter().map(|(slug, _, _, _)| slug.as_str()).collect();
    let quiet = !seeded;
    let mut looked = 0usize;
    for page in &pages {
        let slug = cape::slug(page);
        if known_slugs.contains(slug.as_str()) {
            // The page may have been edited since: a new revision is not a new cape, so this is a
            // silent refresh of the row - the panel shows the current wording and, if the page has
            // gained artwork in the meantime, a picture it could not have had before. It is never a
            // second ping, and it is inside the same budget as a new cape because it costs the same
            // two requests.
            if !quiet && !known.contains(&(slug.clone(), page.last_rev)) && looked < LOOKS_PER_PASS
            {
                looked += 1;
                let mut refresh = NewCape::from_page(page, true);
                attach_artwork(state, page, &mut refresh).await;
                if let Some((_slug, _rev, id, _announced)) =
                    rows.iter().find(|(s, _, _, _)| s == &slug)
                {
                    let _ = state.db.refresh_cape(*id, &refresh);
                }
            }
            continue;
        }
        let mut new = NewCape::from_page(page, quiet);
        if quiet {
            // The seed pass writes rows, not pictures: a cape that came out before this server
            // switched the notifier on gets no render, which is what "we did not know about it"
            // looks like when somebody asks a week later.
            if state
                .db
                .remember_cape(guild, &new)
                .unwrap_or(None)
                .is_some()
            {
                pass.recorded += 1;
            }
            continue;
        }
        if looked >= LOOKS_PER_PASS {
            pass.waiting += 1;
            continue;
        }
        looked += 1;
        attach_artwork(state, page, &mut new).await;
        if state
            .db
            .remember_cape(guild, &new)
            .unwrap_or(None)
            .is_some()
        {
            pass.recorded += 1;
        }
        if looked < LOOKS_PER_PASS {
            tokio::time::sleep(PAUSE).await;
        }
    }
    let _ = cfg;
    pass
}

/// The requests that turn a page into a picture, and the render that makes the rest.
///
/// One, in the normal case. The wiki's file naming for a cape is `File:<page> (texture).png` and its
/// java/bedrock variants, so the page's own name is enough to ask `prop=imageinfo` whether the file
/// exists - and `imageinfo` answers for up to fifty titles at once, so the nine candidate names of
/// one cape cost one request. Asking the *article* what files it lists used to come first, and it is
/// a wasted round trip most of the time: the cape pages carry their artwork through an infobox
/// template rather than by transcluding it, so `prop=images` answers empty for 56 of the 62 pages in
/// the category (measured 2026-10-26, same day as the file-name hit rate of 43 in 40 titles). The
/// page listing is still consulted, second, because a cape filed under a name the convention does not
/// predict exists, and a notifier that only speaks the convention drops its artwork silently.
///
/// Failure here is not failure of the announcement: whatever goes wrong - no file by any of those
/// names, a texture that is not laid out like a cape, a render that cannot be drawn - leaves the row
/// with no frames, and the cape is still announced with its name and its article. That is deliberate.
/// A notifier that swallowed news because an image failed would be a notifier an operator cannot
/// trust, and the silence is what makes it so.
async fn attach_artwork(state: &BotState, page: &CapePage, new: &mut NewCape) {
    let predicted = cape::wanted_files(&page.title);
    let mut infos = ask_about_files(state, &predicted).await;
    if infos.is_empty() {
        // The fallback costs two requests for one page. It only runs on the cape whose files the
        // naming did not predict, which is the case where it is worth it.
        let listed: Vec<String> = match fetch_json(
            &state.http,
            &cape::images_url(std::slice::from_ref(&page.title)),
        )
        .await
        {
            Ok(body) => cape::about_page(&cape::page_files(&body, &page.title), &page.title),
            Err(e) => {
                tracing::debug!(%e, page = %page.title, "the page would not say what files it lists");
                Vec::new()
            }
        };
        if !listed.is_empty() {
            infos = ask_about_files(state, &listed).await;
        }
    }
    if infos.is_empty() {
        return;
    }
    let have: Vec<String> = infos.iter().map(|i| i.file.clone()).collect();
    let picks = cape::cape_images_from(&page.title, &have);
    let found = |name: &str| {
        infos
            .iter()
            .find(|i| i.file.eq_ignore_ascii_case(name))
            .cloned()
    };
    if let Some(img) = picks
        .texture
        .as_deref()
        .and_then(found)
        .filter(|i| i.can_drape())
    {
        if let Ok(bytes) = fetch_bytes(&state.http, &img.url).await {
            match cape::render_frames(&bytes) {
                Ok(frames) => {
                    new.texture_file = img.file.clone();
                    new.texture_url = img.url.clone();
                    new.render_kind = "drape".into();
                    new.frames = frames;
                    return;
                }
                Err(e) => {
                    tracing::warn!(%e, file = %img.file, "cape render refused; announcing without a picture");
                    new.texture_file = img.file.clone();
                    new.texture_url = img.url.clone();
                }
            }
        }
    }
    if let Some(img) = picks
        .render
        .as_deref()
        .and_then(found)
        .filter(|i| i.is_portrait_render())
    {
        if let Ok(bytes) = fetch_bytes(&state.http, &img.url).await {
            new.texture_file = img.file.clone();
            new.texture_url = img.url.clone();
            new.render_kind = "wiki".into();
            new.frames = vec![bytes];
        }
    }
}

/// "Does a file by this name exist, and what is it?" in as few calls as the API allows: forty titles
/// per request, because the endpoint answers at most fifty and past that the reply is an error rather
/// than a file list - which, read as an empty answer, looks exactly like a cape with no art.
async fn ask_about_files(state: &BotState, names: &[String]) -> Vec<cape::CapeImage> {
    let mut out = Vec::new();
    for chunk in names.chunks(40) {
        if let Ok(body) = fetch_json(&state.http, &cape::texture_url(chunk)).await {
            out.extend(cape::image_infos(&body));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// asking the wiki
// ---------------------------------------------------------------------------

/// A GET that must answer with json, in one place so that the user agent and the size limit are the
/// same for every call the notifier makes. A contact address in the agent is the difference between
/// a poll and a scrape, and it is the only thing that makes a wiki admin's question answerable.
async fn fetch_json(http: &reqwest::Client, url: &str) -> Result<Value, String> {
    let text = fetch_text(http, url).await?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| format!("the wiki answered with something that is not json: {e}"))?;
    // A MediaWiki failure is a 200 carrying an `error` member and no `query`, so without this check a
    // rejected or rate-limited request reads as "this page has no pictures". Say what it is instead:
    // the pass reports it, and an operator sees an outage rather than a quiet notifier.
    if let Some(err) = value.get("error") {
        return Err(format!(
            "the wiki said: {}",
            err.get("info")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
        ));
    }
    Ok(value)
}

async fn fetch_text(http: &reqwest::Client, url: &str) -> Result<String, String> {
    let bytes = fetch_bytes(http, url).await?;
    String::from_utf8(bytes)
        .map_err(|e| format!("the wiki answered in bytes that are not utf-8: {e}"))
}

/// The bytes of a wiki answer, with the size checked twice: once from the header, so a response
/// that announces itself as enormous is refused before it is read, and once from the length,
/// because a server can lie about the first. This process also holds the moderation log's
/// connection pool, and a notifier is not a reason to run out of memory.
async fn fetch_bytes(http: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let resp = http
        .get(url)
        .header(reqwest::header::USER_AGENT, cape::USER_AGENT)
        .send()
        .await
        .map_err(|e| format!("the wiki could not be reached: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("the wiki answered {status}"));
    }
    if resp
        .content_length()
        .is_some_and(|n| usize::try_from(n).unwrap_or(usize::MAX) > MAX_BYTES)
    {
        return Err(format!("the wiki offered more than {MAX_BYTES} bytes"));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("the answer did not finish arriving: {e}"))?;
    if bytes.len() > MAX_BYTES {
        return Err(format!("the wiki sent more than {MAX_BYTES} bytes"));
    }
    Ok(bytes.to_vec())
}

// ---------------------------------------------------------------------------
// telling people about it
// ---------------------------------------------------------------------------

/// The trickle. Reads the rows the poll has found and not yet said, posts them, and marks them.
async fn trickle(state: &BotState, ctx: &Context, guild: Snowflake) {
    let Ok(cfg) = state.db.cape_config(guild) else {
        return;
    };
    if !cfg.announces() {
        // No channel to say it in. The rows stay pending rather than being marked as told, so an
        // operator who sets the channel an hour later gets the capes that came out in between -
        // which is the difference between "record only" and "record nothing".
        return;
    }
    let Ok(pending) = state.db.cape_pending(guild, PER_TICK) else {
        return;
    };
    let queued = pending.len();
    if queued == 0 {
        return;
    }
    let Some(channel) = cfg.channel_id else {
        return;
    };
    for release in pending {
        let picture = hero_picture(state, guild, &release, &cfg);
        let message = announcement(&release, &cfg, picture);
        let sent = ChannelId::new(channel.as_u64())
            .send_message(&ctx.http, message)
            .await;
        match sent {
            Ok(_) => {
                let _ = state.db.mark_cape_announced(release.id);
            }
            Err(e) => {
                // One failure stops the pass: a channel the bot cannot post in will refuse all
                // three, and the retry is next tick rather than a loop here.
                tracing::warn!(%e, guild = %guild, cape = %release.slug, "cape announcement refused");
                let _ = state.db.note_cape_attempt(release.id);
                return;
            }
        }
        if queued > 1 {
            tokio::time::sleep(PAUSE).await;
        }
    }
}

/// The bytes to attach, if the row has a picture and the settings say to show one. The drape has
/// four angles and the hero frame is the one that reads as cloth; the wiki's render is a single
/// frame and frame 0 is all there is.
fn hero_picture(
    state: &BotState,
    guild: Snowflake,
    release: &CapeRelease,
    cfg: &CapeConfig,
) -> Option<Vec<u8>> {
    if release.frames == 0 || !cfg.show_render {
        return None;
    }
    let frame = if release.render_kind == "drape" {
        cape::HERO_FRAME as u8
    } else {
        0
    };
    state
        .db
        .cape_render(guild, &release.slug, frame)
        .ok()
        .flatten()
}

/// The file name the embed points at. Derived from the slug so the message and the attachment
/// cannot disagree, and stripped to what a filename accepts because a wiki title is not one.
fn attachment_name(slug: &str) -> String {
    let safe: String = slug
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let safe = if safe.is_empty() { "cape".into() } else { safe };
    format!("{safe}.png")
}

/// The message itself, in one pure function, because this is the part a person judges and the part
/// a test can hold still.
///
/// The role ping goes in the content and nowhere else: an embed does not mention, and a mention in
/// the embed description is text. The article link appears twice on purpose - the embed title is
/// clickable and the content line carries the url for a reader on mobile notifications, where only
/// the content survives - with `<>` around the second one, which is how Discord is told not to turn
/// it into a second embed.
fn announcement(
    release: &CapeRelease,
    cfg: &CapeConfig,
    picture: Option<Vec<u8>>,
) -> CreateMessage {
    let name = if release.name.is_empty() {
        release.title.clone()
    } else {
        release.name.clone()
    };
    let ping = cfg
        .ping_role_id
        .map(|role| format!("<@&{}> ", role))
        .unwrap_or_default();
    let article = if release.article_url.is_empty() {
        "no article link was on the page".to_string()
    } else {
        // Angle brackets are how Discord is told to link without also embedding: without them the
        // same url arrives twice, once as the card and once as a second card of its own.
        format!("<{}>", release.article_url)
    };
    let content = format!("{ping}**{name}** is out in Minecraft. {article}");

    let filename = attachment_name(&release.slug);
    let mut embed = CreateEmbed::new()
        .color(cfg.embed_color & 0xff_ff_ff)
        .title(name)
        .description(if release.render_kind == "none" || release.frames == 0 {
            "No artwork for this one on the wiki, so here is the page.".to_string()
        } else if release.render_kind == "wiki" {
            "The wiki's own render of it.".to_string()
        } else {
            "Draped from the texture file, so you can see the fold.".to_string()
        })
        .footer(CreateEmbedFooter::new("Minecraft capes · minecraft.wiki"));
    if !release.article_url.is_empty() {
        embed = embed.url(release.article_url.clone());
    }
    if picture.is_some() {
        // The embed refers to the upload by name, which is how a file sent in the same message
        // becomes the picture *inside* the card rather than a thumbnail under it. Both halves are
        // built from one `filename`, so they cannot disagree - and a name that disagreed is a card
        // with a broken image and nothing in the log to say why.
        embed = embed.image(format!("attachment://{filename}"));
    }
    let mut message = CreateMessage::new().content(content).embed(embed);
    if let Some(bytes) = picture {
        message = message.add_file(CreateAttachment::bytes(
            bytes,
            attachment_name(&release.slug),
        ));
    }
    // Only the role is mentionable in this message. Nothing else in it names a user, and the rule
    // that a bot's own messages cannot ping people is worth more than the convenience of not
    // setting it.
    message = match cfg.ping_role_id {
        Some(role) => message
            .allowed_mentions(CreateAllowedMentions::new().roles([RoleId::new(role.as_u64())])),
        None => message.allowed_mentions(CreateAllowedMentions::new()),
    };
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use aerona_core::cape::CapePage;

    fn release(kind: &str, frames: u32) -> CapeRelease {
        let page = CapePage {
            title: "Aurora Cape".into(),
            page_id: 42,
            last_rev: 900,
            url: "https://minecraft.wiki/w/Aurora_Cape".into(),
        };
        let mut new = NewCape::from_page(&page, false);
        new.render_kind = kind.into();
        CapeRelease {
            id: 1,
            slug: new.slug,
            title: new.title,
            name: new.name,
            article_url: new.article_url,
            texture_file: new.texture_file,
            render_kind: new.render_kind,
            frames,
            has_render: frames > 0,
            last_rev: new.last_rev,
            found_at: 1_700_000_000,
            announced_at: None,
            attempts: 0,
        }
    }

    fn cfg(role: Option<Snowflake>) -> CapeConfig {
        CapeConfig {
            enabled: true,
            channel_id: Some(Snowflake(555)),
            ping_role_id: role,
            check_every_minutes: 60,
            show_render: true,
            embed_color: 0x58_65_F2,
        }
    }

    fn wire(message: &CreateMessage) -> Value {
        serde_json::to_value(message).expect("a CreateMessage serialises")
    }

    #[test]
    fn the_ping_is_a_role_and_the_role_is_all_it_may_ping() {
        let role = Snowflake(900);
        let out = wire(&announcement(&release("drape", 4), &cfg(Some(role)), None));
        let content = out["content"].as_str().expect("content");
        assert!(
            content.starts_with("<@&900> **Aurora Cape** is out in Minecraft."),
            "the ping leads the line, and the name is what a notification shows: {content}"
        );
        assert!(
            content.contains("<https://minecraft.wiki/w/Aurora_Cape>"),
            "the article rides in the content, angle-bracketed so it does not become a second embed: {content}"
        );
        assert_eq!(
            out["allowed_mentions"]["roles"].as_array().map(Vec::len),
            Some(1),
            "one role and nothing else: {out}"
        );
        assert_eq!(
            out["allowed_mentions"]["parse"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0),
            0,
            "no @everyone and no users, whatever the content says: {out}"
        );

        // With no role, the message is still sent and nothing is mentioned.
        let out = wire(&announcement(&release("drape", 4), &cfg(None), None));
        assert!(!out["content"].as_str().unwrap().starts_with("<@&"));
        assert!(
            out["allowed_mentions"]["roles"]
                .as_array()
                .map_or(true, |r| r.is_empty()),
            "{out}"
        );
    }

    #[test]
    fn the_card_is_the_cape_and_the_link_goes_to_the_article() {
        let out = wire(&announcement(
            &release("drape", 4),
            &cfg(None),
            Some(vec![1, 2, 3]),
        ));
        let embed = &out["embeds"][0];
        assert_eq!(embed["title"], "Aurora Cape");
        assert_eq!(embed["url"], "https://minecraft.wiki/w/Aurora_Cape");
        assert_eq!(embed["color"], 0x58_65_F2);
        assert_eq!(
            embed["image"]["url"], "attachment://mw42.png",
            "the picture is the file attached to this message: {embed}"
        );
        assert_eq!(
            out["attachments"][0]["filename"], "mw42.png",
            "the file carries the name the embed points at"
        );
        assert!(
            embed["description"]
                .as_str()
                .unwrap_or_default()
                .contains("Draped from the texture"),
            "{embed}"
        );

        // No picture: no attachment, and the card says so instead of showing a broken frame.
        let out = wire(&announcement(&release("none", 0), &cfg(None), None));
        assert_eq!(out["embeds"][0]["title"], "Aurora Cape");
        assert!(
            out["attachments"].is_null()
                || out["attachments"].as_array().map_or(true, |a| a.is_empty())
        );
        assert!(out["embeds"][0]["image"].is_null(), "{out}");
        assert!(
            out["embeds"][0]["description"]
                .as_str()
                .unwrap_or_default()
                .contains("No artwork"),
            "{out}"
        );
    }

    #[test]
    fn a_name_that_is_not_a_filename_cannot_escape_one() {
        // The slug is built from a page id, so it is always `mw<number>`; the filter is what keeps
        // that from ever becoming an assumption a title could break.
        assert_eq!(attachment_name("mw42"), "mw42.png");
        assert_eq!(
            attachment_name("../../etc/passwd"),
            "etcpasswd.png",
            "no path, no dot-dot"
        );
        assert_eq!(attachment_name(""), "cape.png", "and never an empty name");
    }
}
