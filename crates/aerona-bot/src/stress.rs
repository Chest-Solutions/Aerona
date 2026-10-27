//! The shipped handlers, driven under the settings the panel can produce, at a scale a real
//! server reaches.
//!
//! This exists because "no feature works" was reported against code that compiled and passed its
//! unit tests: the failures were at the seams, an off switch that still recorded, a picker that was
//! never filled, a transcript renderer that disagreed with the one the panel downloaded. So the
//! assertions here are about seams. Nothing reaches Discord: the sends are the one part a test
//! cannot drive without a gateway, so every test asserts the decision and the built payload, which
//! is exactly what a handler computes before it sends.
use std::collections::HashMap;
use std::sync::Arc;

use serde_json::json;

use aerona_core::db::logging::LogEntry;
use aerona_core::identity::Snowflake;
use aerona_core::models::{
    GuildSettings, LogConfig, TicketConfig, TicketRecord, TicketStatus, TranscriptFormat,
    LOG_EVENTS,
};

use crate::logger;
use crate::notify;
use crate::raid;
use crate::state::BotState;

const GUILD: u64 = 7;

/// A bot with a real database in a temp dir, held open by the returned guard.
fn bot() -> (BotState, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("a temp dir for the stress database");
    let mut env = HashMap::new();
    env.insert(
        "AERONA_TOKEN".to_string(),
        "MTIzNDU2Nzg5.ABCDEFGHIJKLMNOPQRSTUVWXYZ.abcdefghijklmnopqrstuvwxyz01".to_string(),
    );
    env.insert("AERONA_PORT".to_string(), "8799".to_string());
    let cfg = Arc::new(
        aerona_core::config::Config::parse_env(&env, dir.path(), true)
            .expect("a config to build a state"),
    );
    let db =
        aerona_core::db::Db::open_path(&dir.path().join("stress.db")).expect("an opened database");
    (BotState::new(cfg, db), dir)
}

fn guild() -> Snowflake {
    Snowflake(GUILD)
}

fn entry(i: usize) -> LogEntry {
    LogEntry::new(
        LOG_EVENTS[i % LOG_EVENTS.len()],
        format!("row {i} of the flood"),
    )
    .user(Snowflake(1000 + i as u64))
    .channel(Snowflake(2000 + i as u64))
}

/// The complaint was that switching logging off did not stop it. The channel *is* the switch, so
/// this asserts both halves: nothing is wanted without one, and everything is wanted with it.
#[test]
fn no_channel_picked_means_nothing_is_recorded() {
    let (state, _dir) = bot();
    let cfg = LogConfig {
        channel_id: None,
        events: LOG_EVENTS.iter().map(|e| (*e).to_string()).collect(),
        ..LogConfig::default()
    };
    state
        .db
        .put_log_config(guild(), &cfg, None)
        .expect("the config saves");

    for i in 0..LOG_EVENTS.len() {
        let e = entry(i);
        assert!(
            logger::wanted(&state, guild(), &e).is_none(),
            "{} was logged with no channel picked",
            e.event
        );
    }
    assert_eq!(
        state.db.list_log_events(guild(), 500, None).unwrap().len(),
        0,
        "a guild that picked no channel has no reason to hold rows"
    );

    // And the same list, with a channel, does record every event, so the off switch above is a
    // decision rather than a dead code path.
    state
        .db
        .put_log_config(
            guild(),
            &LogConfig {
                channel_id: Some(Snowflake(4242)),
                ..cfg
            },
            None,
        )
        .expect("the second config saves");
    for name in LOG_EVENTS.iter() {
        let e = LogEntry::new(name, "anything").channel(Snowflake(1));
        assert!(
            logger::wanted(&state, guild(), &e).is_some(),
            "{name} never reached the log"
        );
        state.db.record_log_event(guild(), &e).unwrap();
    }
    assert_eq!(
        state.db.list_log_events(guild(), 500, None).unwrap().len(),
        LOG_EVENTS.len()
    );
}

/// Five thousand events through the same path a busy server sends in a day, asserting the row and
/// the card for each, and that a `bot` row is dropped only when the guild asked for that.
#[test]
fn a_day_of_traffic_becomes_rows_and_cards() {
    let (state, _dir) = bot();
    state
        .db
        .put_log_config(
            guild(),
            &LogConfig {
                channel_id: Some(Snowflake(9)),
                events: LOG_EVENTS.iter().map(|e| (*e).to_string()).collect(),
                ignore_bots: true,
                max_cached_messages: 500,
            },
            None,
        )
        .expect("the config saves");

    for i in 0..5000 {
        let mut e = entry(i);
        if i % 3 == 0 {
            e = e.bot(true);
        }
        let wanted = logger::wanted(&state, guild(), &e);
        assert_eq!(
            wanted.is_some(),
            i % 3 != 0,
            "ignore_bots flipped the wrong way at {i}"
        );
        if let Some(cfg) = wanted {
            assert_eq!(cfg.channel_id, Some(Snowflake(9)));
            let card = serde_json::to_value(logger::card("Nine", guild(), &e))
                .expect("a card that serialises");
            let title = card["title"].as_str().unwrap_or_default().to_string();
            assert!(!title.is_empty(), "an event with no card title at {i}");
            assert!(
                card["description"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("flood"),
                "the summary vanished from the card at {i}"
            );
            assert!(card["color"].is_number(), "a card with no colour at {i}");
            state.db.record_log_event(guild(), &e).unwrap();
        }
    }

    let rows = state.db.list_log_events(guild(), 500, None).unwrap();
    assert_eq!(rows.len(), 500, "the page asks for 500 and gets 500");
    assert!(
        rows.windows(2)
            .all(|w| w[0]["at"].as_i64().unwrap_or(0) >= w[1]["at"].as_i64().unwrap_or(0)),
        "a log that is not newest first is not a log"
    );
    // The keys are part of the contract: the panel reads them by name, and a rename in one place
    // is a blank table in the other.
    let first = &rows[0];
    for key in ["at", "event", "summary", "channelId", "userId"] {
        assert!(first.get(key).is_some(), "a log row lost {key}: {first}");
    }

    // One kind, filtered, which is what the page's filter buttons ask for.
    let edits = state
        .db
        .list_log_events(guild(), 500, Some("message_edit"))
        .unwrap();
    assert!(
        edits.iter().all(|r| r["event"] == "message_edit"),
        "the filter did not filter"
    );
}

/// DMs are the guild's to allow and the action's to deserve. There is no per-action reason switch,
/// and this is where that promise is kept.
#[test]
fn a_member_is_told_why_unless_the_guild_closed_dms() {
    let open = GuildSettings {
        dms_enabled: true,
        ..GuildSettings::default()
    };
    let shut = GuildSettings {
        dms_enabled: false,
        ..GuildSettings::default()
    };
    for kind in [
        notify::Kind::Warn,
        notify::Kind::Timeout,
        notify::Kind::Kick,
        notify::Kind::Ban,
        notify::Kind::Unban,
    ] {
        let a = notify::Action {
            kind,
            target: Snowflake(77),
            target_name: "rae",
            actor: "mod",
            reason: "posting banned sites in #general",
            detail: Some("45 minutes"),
            fallback_channel: None,
        };
        assert!(
            notify::wants_dm(kind, &open),
            "{kind:?} owes the member a DM"
        );
        assert!(
            !notify::wants_dm(kind, &shut),
            "{kind:?} ignored the guild's switch"
        );
        let card =
            serde_json::to_value(notify::dm_card(&a, "Nine")).expect("a dm card that serialises");
        let text = format!(
            "{} {} {}",
            card["title"].as_str().unwrap_or_default(),
            card["description"].as_str().unwrap_or_default(),
            serde_json::to_string(&card["fields"]).unwrap_or_default()
        );
        assert!(
            text.contains("banned sites"),
            "the reason is missing from {kind:?}: {text}"
        );
        assert!(
            text.contains("Nine"),
            "the server is missing from {kind:?}: {text}"
        );
    }
    for kind in [notify::Kind::Purge, notify::Kind::Note] {
        let a = notify::Action {
            kind,
            target: Snowflake(77),
            target_name: "rae",
            actor: "mod",
            reason: "cleanup",
            detail: None,
            fallback_channel: None,
        };
        assert!(!notify::wants_dm(kind, &open), "{kind:?} is not a DM");
        // Not a DM is not a free pass: staff still get the card, with the reason on it, or the
        // action happened in the dark.
        let card =
            serde_json::to_value(notify::log_card(&a, "Nine")).expect("a log card that serialises");
        // The reason is a field on this card rather than part of its description, so the whole
        // payload is what has to carry it: staff can always see why something happened.
        let wire = card.to_string();
        assert!(
            wire.contains("cleanup"),
            "{kind:?} logged without its reason: {wire}"
        );
        assert!(
            !card["title"].as_str().unwrap_or_default().is_empty(),
            "{kind:?} logged with no title"
        );
    }
}

/// The ticket cards are the member's first impression and the staff's last one, so they must hold
/// up on input nobody sanitised: a six-kilobyte subject, unbalanced tokens, control characters.
#[test]
fn ticket_cards_hold_up_on_the_worst_text_a_member_can_type() {
    let nasty = format!(
        "{} {} {}",
        "u".repeat(6000),
        "``<b>{user}{</b> {ticket}",
        "\u{200b}\u{0007} emoji 🎉".repeat(200)
    );
    let cfg = TicketConfig {
        open_title: "Ticket {ticket} for {user} about {subject}".into(),
        default_message: "{server} staff are on their way. {nonsense}".into(),
        close_title: "Closed {ticket}".into(),
        close_body: "{user}: {subject}".into(),
        embed_color: 0x00_ff_00,
        ..TicketConfig::default()
    };
    let open = serde_json::to_value(crate::events::ticket_intro(
        &cfg,
        123,
        "AER-QQ3479",
        &nasty,
        "Nine",
        None,
    ))
    .expect("an opening card that serialises");
    let title = open["title"].as_str().unwrap_or_default().to_string();
    assert!(
        title.contains("AER-QQ3479"),
        "the reference vanished: {title}"
    );
    assert!(title.contains("<@123>"), "the mention vanished: {title}");
    assert!(
        title.chars().count() <= 256,
        "a card title Discord would reject: {}",
        title.chars().count()
    );
    assert!(
        !title.contains("{subject}"),
        "a token was left unexpanded: {title}"
    );

    let record = TicketRecord {
        id: 4,
        guild_id: guild(),
        channel_id: Snowflake(5),
        owner_id: Snowflake(123),
        claimed_by: Some(Snowflake(9)),
        status: TicketStatus::Closed,
        subject: nasty.clone(),
        reference: "AER-QQ3479".into(),
        created_at: 1_700_000_000,
        closed_at: Some(1_700_000_600),
        message_count: 37,
        delete_at: None,
        channel_deleted_at: None,
    };
    let closed = serde_json::to_value(crate::events::ticket_close_card(
        &cfg,
        &record,
        "Nine",
        37,
        &[],
    ))
    .expect("a close card that serialises");
    let body = closed["description"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    // The reference belongs to the title and the message count to a field; a guild that writes its
    // own body without `{ticket}` still gets a card that says which ticket it is.
    let close_title = closed["title"].as_str().unwrap_or_default().to_string();
    assert!(
        close_title.contains("AER-QQ3479"),
        "the close card lost its reference: {close_title}"
    );
    assert!(body.contains("uuuu"), "the body went missing: {body}");
    let fields = serde_json::to_string(&closed["fields"]).unwrap_or_default();
    assert!(
        fields.contains("37"),
        "the count of messages is on the card: {fields}"
    );
    // A guild that deletes closed channels says so on the card, in words, at the moment it decides.
    let mut timer = cfg.clone();
    timer.delete_after_minutes = 90;
    let card = serde_json::to_value(crate::events::ticket_close_card(
        &timer,
        &record,
        "Nine",
        37,
        &[],
    ))
    .expect("a close card with a deadline");
    let fields = serde_json::to_string(&card["fields"]).unwrap_or_default();
    assert!(
        fields.contains("deleted in 1 hour 30 minutes"),
        "the deadline is stated, and not rounded away: {fields}"
    );
    assert_eq!(
        crate::events::human_duration(3600),
        "1 hour",
        "an even hour is one number"
    );
    assert_eq!(
        crate::events::human_duration(45),
        "45 seconds",
        "a minute of grace is not '1 minute'"
    );
    assert!(
        body.chars().count() <= 1500,
        "a description Discord would reject: {}",
        body.chars().count()
    );
    assert_eq!(
        closed["color"].as_i64(),
        Some(0x00_ff_00),
        "the guild's own colour must be the card's"
    );

    // Nothing set at all still says something: the defaults are the promise.
    let bare = TicketConfig::default();
    let plain = serde_json::to_value(crate::events::ticket_intro(
        &bare, 123, "AER-1", "help", "Nine", None,
    ))
    .unwrap();
    assert!(
        plain["title"]
            .as_str()
            .unwrap_or_default()
            .contains("AER-1"),
        "a default card with no reference"
    );
}

/// RED-15: the log card has to show both halves of a change, in a form that survives the text
/// being markdown itself. A card that folds them into one sentence is a card where the answer to
/// "what did they change it to" has to be parsed out of prose.
#[test]
fn the_log_card_holds_both_halves_apart() {
    let edit = LogEntry::new("message_edit", "Message edited in #general")
        .channel(Snowflake(GUILD))
        .user(Snowflake(42))
        .was("before **the** edit")
        .is_now("after `the` edit");
    let card = serde_json::to_value(logger::card("Nine", Snowflake(GUILD), &edit)).expect("a card");
    let fields = card["fields"].as_array().unwrap();
    // Both halves first, then where and who: the change is what a reader came for, the two
    // inline facts underneath are what makes it actionable.
    assert_eq!(fields.len(), 4, "before, after, and the two facts: {card}");
    assert_eq!(fields[0]["name"], "Before", "{card}");
    assert_eq!(fields[1]["name"], "After", "{card}");
    assert_eq!(fields[2]["name"], "Channel", "{card}");
    assert_eq!(fields[2]["value"], "<#7>", "{card}");
    assert_eq!(fields[3]["name"], "Member", "{card}");
    assert_eq!(fields[3]["value"], "<@42>", "{card}");
    // Fenced, so a message of asterisks stays legible instead of becoming bold soup - and a
    // message that is itself a code block cannot end the fence early.
    for f in &fields[..2] {
        let value = f["value"].as_str().unwrap_or_default();
        assert!(
            value.starts_with("```") && value.ends_with("```"),
            "{value:?}"
        );
        assert!(
            value.matches("```").count() == 2,
            "a fence inside the quoted text is neutered: {value:?}"
        );
    }
    assert!(fields[0]["value"]
        .as_str()
        .unwrap()
        .contains("before **the** edit"));
    assert!(
        card["description"]
            .as_str()
            .unwrap_or_default()
            .contains("Message edited in #general"),
        "the sentence is still the headline: {card}"
    );

    // One half missing is no field, not an empty one: "we do not know" and "it was nothing" are
    // different answers, and a card must not invent the second.
    let leave = LogEntry::new("member_leave", "A member left")
        .now_opt(Some("no longer a member"))
        .was_opt(None::<String>);
    let card = serde_json::to_value(logger::card("Nine", Snowflake(GUILD), &leave)).unwrap();
    let fields = card["fields"].as_array().expect("a field list");
    assert_eq!(fields.len(), 1, "only the half that is known: {card}");
    assert_eq!(fields[0]["name"], "After", "{card}");

    // A delete is a change too, and the card shape follows: the kept text is what a reader came for,
    // so it is in the body under the headline, and there is no "After" field to hold a lie.
    let gone = LogEntry::new(
        "message_delete",
        "Message deleted in #general by @neko (the text was kept from when it arrived)",
    )
    .channel(Snowflake(7))
    .was("the text that is now gone");
    let card = serde_json::to_value(logger::card("Nine", Snowflake(GUILD), &gone)).unwrap();
    let body = card["description"].as_str().unwrap_or_default();
    assert!(
        body.contains("the text that is now gone"),
        "a deletion quotes what was lost: {body}"
    );
    assert!(
        body.contains("(the text was kept from when it arrived)"),
        "{body}"
    );
    let fields: Vec<&str> = card["fields"]
        .as_array()
        .map(|a| a.iter().filter_map(|f| f["name"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        !fields.contains(&"Before") && !fields.contains(&"After"),
        "the pair is an edit's shape: {fields:?}"
    );

    // The sentence is still the headline, so nothing about the card has to be guessed.
    assert!(body.starts_with("Message deleted in #general"), "{body}");
}

/// The complaint in the user's own words: a delete "still dont show the before" and an edit does
/// not appear at all. The half that was missing is a lookup, so this drives the whole lookup
/// through the real database - the row written when a message arrives, the quote a delete reads
/// back from it, the entry built from that pair, and the row the panel lists. A unit test of the
/// handler alone could not tell these apart from a working install.
#[test]
fn a_delete_quotes_the_text_the_bot_kept_when_it_arrived() {
    let (state, _dir) = bot();
    state.managed.insert(guild());
    let ch = Snowflake(7);
    let msg = Snowflake(900);

    assert_eq!(
        state.db.recalled_message(guild(), msg).unwrap(),
        None,
        "a cold cache invents a quote for nobody"
    );
    state
        .db
        .remember_message(guild(), ch, msg, Snowflake(42), "the **old** text")
        .unwrap();
    let quote = state
        .db
        .recalled_message(guild(), msg)
        .unwrap()
        .map(|(text, author, _at)| (text, author));
    let entry = crate::events::delete_entry(ch, msg, quote.as_ref(), true);
    assert!(state.db.record_log_event(guild(), &entry).unwrap() > 0);

    let rows = state.db.list_log_events(guild(), 20, None).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["event"], "message_delete", "{rows:?}");
    assert_eq!(rows[0]["before"], "the **old** text", "{rows:?}");
    assert_eq!(
        rows[0]["after"].as_str(),
        None,
        "a delete has no after half, and the row does not invent one to fill the space: {rows:?}"
    );
    assert_eq!(rows[0]["userId"], "42", "{rows:?}");
    // The card a reader sees is built from the same entry, so the pair is not lost between the row
    // and the log channel - which was the other half of the report.
    let card = serde_json::to_value(logger::card("Nine", ch, &entry)).expect("a card");
    // The kept text is the body of a delete card, not a side field: that is what a moderator
    // actually came to read, and it is the half the old "(deleted)" field crowded out.
    let body = card["description"].as_str().unwrap_or_default();
    assert!(body.contains("the **old** text"), "{body}");
    // The card fences the text, so the asterisks a member typed survive as what they typed.
    assert!(body.contains("```"), "fenced, not interpreted: {body}");
    let fields: Vec<serde_json::Value> = card["fields"].as_array().cloned().unwrap_or_default();
    let names: Vec<&str> = fields.iter().filter_map(|f| f["name"].as_str()).collect();
    assert!(
        !names.contains(&"Before") && !names.contains(&"After"),
        "a delete is one half of a pair, and saying so twice is the bug: {names:?}"
    );

    // Once the message is gone the kept text goes with it, so a delete cannot be quoted twice.
    state.db.forget_message(guild(), msg).unwrap();
    assert_eq!(state.db.recalled_message(guild(), msg).unwrap(), None);
    let bare = crate::events::delete_entry(ch, msg, None, true);
    state.db.record_log_event(guild(), &bare).unwrap();
    let rows = state.db.list_log_events(guild(), 20, None).unwrap();
    assert_eq!(
        rows.len(),
        2,
        "a delete is logged with or without a quote: {rows:?}"
    );
    assert_eq!(
        rows[0]["before"].as_str(),
        None,
        "without the cache the row says it could not keep the text, and makes nothing up"
    );
}

/// How an edit came to be dropped entirely: the update event's guild is optional, and the handler
/// asked it and nothing else. A channel the cache knows is a second, equally good answer.
#[test]
fn a_guild_comes_from_the_channel_when_the_payload_has_none() {
    let (state, _dir) = bot();
    state.managed.insert(guild());
    let here = serenity::model::id::GuildId::new(GUILD);
    let elsewhere = serenity::model::id::GuildId::new(4242);

    assert_eq!(
        crate::events::guild_of(&state, Some(here), None),
        Some(guild())
    );
    assert_eq!(
        crate::events::guild_of(&state, None, Some(here)),
        Some(guild()),
        "an event with no guild is still ours when its channel says so"
    );
    assert_eq!(crate::events::guild_of(&state, None, None), None);
    assert_eq!(
        crate::events::guild_of(&state, Some(here), Some(elsewhere)),
        Some(guild()),
        "the payload is the nearer fact"
    );
    // A server nobody added to this install is not ours to log, however the id reached us.
    assert_eq!(crate::events::guild_of(&state, Some(elsewhere), None), None);
    assert_eq!(crate::events::guild_of(&state, None, Some(elsewhere)), None);
}

/// Two thousand messages with attachments, through the renderer the bot and the panel share. This
/// is the one place the "export as HTML that shows the images" promise is checked end to end.
#[test]
fn a_two_thousand_row_transcript_still_shows_every_picture() {
    let record = TicketRecord {
        id: 9,
        guild_id: guild(),
        channel_id: Snowflake(5),
        owner_id: Snowflake(7),
        claimed_by: None,
        status: TicketStatus::Open,
        subject: "the file will not upload".into(),
        reference: "AER-4T9X".into(),
        created_at: 1_700_000_000,
        closed_at: None,
        message_count: 2000,
        delete_at: None,
        channel_deleted_at: None,
    };
    let rows: Vec<_> = (0..2000)
        .map(|i| {
            json!({
                "at": 1_700_000_000 + i as i64,
                "author": format!("member{}", i % 40),
                "content": if i % 500 == 0 {
                    format!("**looked** at this <script>alert({i})</script>")
                } else {
                    format!("message number {i}")
                },
                "internal": i % 7 == 0,
                "attachments": if i % 3 == 0 {
                    json!([
                        { "url": "https://cdn.discordapp.com/attachments/1/2/shot.png", "name": "shot.png", "kind": "image", "width": 800 },
                        { "url": "https://cdn.discordapp.com/attachments/1/2/clip.mp4", "name": "clip.mp4", "kind": "video" },
                    ])
                } else {
                    json!([])
                },
            })
        })
        .collect();

    let (name, html) =
        aerona_core::transcript::render(&record, &rows, TranscriptFormat::Html, "Nine");
    assert_eq!(name, "AER-4T9X.html", "a file a browser can recognise");
    // 667 attachment images plus one avatar per message: the avatars are why this is 2,667, and a
    // transcript that lost them would still look plausible in a test that only counted `<img>`.
    // Each attachment is referenced three times (link, image, caption), and every message carries
    // an avatar, so counting `<img>` alone would pass on a transcript with no pictures at all.
    assert_eq!(
        html.matches("shot.png").count(),
        667 * 3,
        "one image per third row, named"
    );
    assert_eq!(
        html.matches("<img src=\"https://cdn.discordapp.com/attachments")
            .count(),
        667,
        "the image the member posted is in the file"
    );
    assert_eq!(
        html.matches("/embed/avatars/").count(),
        2000,
        "and every speaker has their avatar"
    );
    assert_eq!(html.matches("<video ").count(), 667, "and the videos too");
    assert_eq!(
        html.matches("staff note").count(),
        286,
        "every internal line is marked"
    );
    assert!(
        !html.contains("<script>alert"),
        "the transcript runs what a member typed"
    );
    assert!(
        html.contains("&lt;script&gt;"),
        "the text is there, escaped"
    );
    assert!(html.contains("AER-4T9X"), "and it says which ticket it is");

    let (tname, text) =
        aerona_core::transcript::render(&record, &rows, TranscriptFormat::Text, "Nine");
    assert_eq!(tname, "AER-4T9X.txt");
    // 1,996 of the 2,000 rows are plain text; the four every-500th row carry the markup sample.
    assert_eq!(
        text.matches("message number ").count(),
        1996,
        "one line per message"
    );
    assert!(
        text.contains("message number 1999"),
        "the newest row is in it"
    );
    assert_eq!(
        text.matches("(internal)").count(),
        286,
        "a staff note is marked in text too"
    );
    assert!(
        text.contains(": message number 1998 (internal)")
            || text.contains("member38: message number 1998"),
        "a text line must read as someone speaking"
    );

    let (jname, body) =
        aerona_core::transcript::render(&record, &rows, TranscriptFormat::Json, "Nine");
    assert_eq!(jname, "AER-4T9X.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&body).expect("the json transcript parses");
    assert_eq!(parsed["messages"].as_array().map(|a| a.len()), Some(2000));
    assert_eq!(parsed["reference"], "AER-4T9X");
}

/// The bug was an expiry timer that released the slowmode and left verification raised, so the
/// server stayed shut with nothing on screen saying why. What the release needs to remember is a
/// report row, and that row has to survive the round trip.
#[test]
fn a_release_remembers_exactly_what_the_lock_changed() {
    let (state, _dir) = bot();
    let fresh = raid::saved(&state, guild());
    assert!(
        !fresh.raised,
        "a guild that was never locked has nothing to undo"
    );
    assert_eq!(fresh.previous_verification, 0);

    let saved = raid::Saved {
        raised: true,
        previous_verification: 2,
        minutes: 15,
        had_notice: true,
    };
    state
        .db
        .put_guild_report(
            guild(),
            raid::REPORT,
            &serde_json::to_value(&saved).unwrap(),
        )
        .unwrap();
    let read = raid::saved(&state, guild());
    assert_eq!(read.raised, saved.raised);
    assert_eq!(
        read.previous_verification, saved.previous_verification,
        "verification must go back to what it was, not to a guess"
    );
    assert_eq!(read.minutes, saved.minutes);
    assert_eq!(read.had_notice, saved.had_notice);

    // A release deletes the report, so a second release cannot act on stale numbers: an unreadable
    // blob degrades to "nothing to undo" rather than to a wrong restore.
    state
        .db
        .put_guild_report(guild(), raid::REPORT, &json!({ "raised": "yes" }))
        .unwrap();
    assert!(
        !raid::saved(&state, guild()).raised,
        "a malformed report must not lock a server out"
    );
}

/// The picker the panel offers is built from the cache the bot keeps, so the cache window has to
/// bound it: a guild that remembers nothing must not be shown a list of things that are not there.
#[test]
fn the_message_window_the_log_configures_is_the_window_the_picker_shows() {
    let (state, _dir) = bot();
    for i in 0..600 {
        state
            .db
            .remember_message(
                guild(),
                Snowflake(30),
                Snowflake(9000 + i),
                Snowflake(50 + i % 7),
                &format!("text {i}"),
            )
            .unwrap();
    }
    let rows = state.db.recent_messages(guild(), 500).unwrap();
    assert_eq!(rows.len(), 500, "the cap is honoured");
    assert!(
        rows.windows(2)
            .all(|w| w[0]["at"].as_i64().unwrap_or(0) >= w[1]["at"].as_i64().unwrap_or(0)),
        "newest first, or the picker shows the wrong hour"
    );
    assert_eq!(
        rows[0]["id"], "9599",
        "the newest message is the one a moderator just posted"
    );
    assert!(rows[0]["content"]
        .as_str()
        .unwrap_or_default()
        .starts_with("text 599"));

    state.db.prune_log_messages(50).unwrap();
    let kept = state.db.recent_messages(guild(), 500).unwrap();
    assert_eq!(
        kept.len(),
        50,
        "housekeeping keeps the window the guild asked for"
    );
}

/// The API's list and the bot's cache must agree about one thing: a channel that exists is
/// addressable by the id the panel saved. A stale report is what made a picker look broken.
#[test]
fn a_picker_report_round_trips_through_the_database() {
    let (state, _dir) = bot();
    let payload = json!({
        "reportedAt": 1_700_000_000_i64,
        "channels": [{ "id": "30", "name": "general", "kind": "text", "category": null }],
        "roles": [{ "id": "77", "name": "Moderator", "color": 5793266, "assignable": true, "iconUrl": null }],
        "seen": 2,
    });
    state
        .db
        .put_guild_report(guild(), "picker", &payload)
        .unwrap();
    state.db.put_guild_report(guild(), "messages", &json!([
        { "id": "9599", "channelId": "30", "authorId": "50", "content": "text 599", "at": 1_700_000_000 }
    ]))
    .unwrap();

    let all = state.db.guild_reports(guild()).unwrap();
    assert_eq!(all["picker"]["channels"][0]["name"], "general");
    assert_eq!(all["picker"]["roles"][0]["color"], 5793266);
    assert_eq!(
        all["messages"][0]["id"], "9599",
        "the messages kind is a plain array"
    );
    // Both kinds the panel reads, in one fetch, which is what keeps the page a single request.
    for kind in ["picker", "messages"] {
        assert!(
            all.get(kind).is_some(),
            "{kind} is missing from the report the panel fetches"
        );
    }
}
