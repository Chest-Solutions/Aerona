//! Route modules plus the small parsing helpers they all share.
//!
//! The helpers exist so that every handler validates input the same way: an
//! absent field, a non-numeric id, or an over-long string produces the same
//! `bad_request` envelope with a `details` object naming the field, on every route.

use std::collections::HashMap;

use axum::response::Response;
use serde_json::{json, Value};

use aerona_core::error::{Error, Result};
use aerona_core::identity::Snowflake;
use aerona_core::models::Feature;

pub mod account;
pub mod guild;
pub mod system;

pub(crate) type Params = HashMap<String, String>;

pub(crate) fn q<'a>(query: &'a Params, key: &str) -> Option<&'a str> {
    query.get(key).map(String::as_str).filter(|v| !v.is_empty())
}

/// Integer query parameter with a hard ceiling, so `?limit=999999999` cannot turn
/// into a full-table scan.
pub(crate) fn q_bounded(query: &Params, key: &str, default: u32, max: u32) -> Result<u32> {
    match q(query, key) {
        None => Ok(default),
        Some(raw) => raw
            .parse::<i64>()
            .map_err(|_| Error::Invalid(format!("{key} must be a number")))
            .and_then(|v| {
                if v < 0 {
                    return Err(Error::Invalid(format!("{key} must not be negative")));
                }
                Ok(v.min(max as i64) as u32)
            }),
    }
}

pub(crate) fn q_i64(query: &Params, key: &str) -> Result<Option<i64>> {
    match q(query, key) {
        None => Ok(None),
        Some(raw) => raw
            .parse::<i64>()
            .map(Some)
            .map_err(|_| Error::Invalid(format!("{key} must be an integer"))),
    }
}

pub(crate) fn q_bool(query: &Params, key: &str) -> bool {
    matches!(q(query, key), Some("1") | Some("true") | Some("yes"))
}

pub(crate) fn q_opt_sf(query: &Params, key: &str) -> Result<Option<Snowflake>> {
    match q(query, key) {
        None => Ok(None),
        Some(raw) => Snowflake::parse(raw)
            .map(Some)
            .map_err(|_| Error::Invalid(format!("{key} must be a discord id"))),
    }
}

pub(crate) fn q_status(query: &Params) -> Result<Option<aerona_core::models::TicketStatus>> {
    match q(query, "status") {
        None => Ok(None),
        Some(raw) => aerona_core::models::TicketStatus::parse(raw)
            .map(Some)
            .ok_or_else(|| Error::Invalid(format!("unknown ticket status {raw:?}"))),
    }
}

pub(crate) fn feature(raw: &str) -> Result<Feature> {
    Feature::parse(raw).ok_or_else(|| {
        Error::Invalid(format!(
            "unknown settings feature {raw:?}; expected one of {}",
            Feature::ALL
                .iter()
                .map(|f| f.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })
}

// -- body fields -------------------------------------------------------------

/// Named type for a validation message. `Value` has no `type_str`, and printing
/// `{:?}` of the value would echo the caller's payload back at it.
fn value_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

pub(crate) fn str_field(body: &Value, key: &str, max: usize) -> Result<String> {
    let raw = match body.get(key) {
        None | Some(Value::Null) => return Err(Error::Invalid(format!("{key} is required"))),
        Some(Value::String(s)) => s.clone(),
        // Accept a number where an id-ish string was sent (`{"user": 123}`), which
        // is what a hand-written curl call usually means; anything else is a
        // client bug we report rather than guess at.
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => {
            return Err(Error::Invalid(format!(
                "{key} must be a string, not {}",
                value_type(other)
            )))
        }
    };
    let cleaned = aerona_core::security::clean(&raw, max);
    if cleaned.is_empty() {
        return Err(Error::Invalid(format!("{key} must not be empty")));
    }
    Ok(cleaned)
}

pub(crate) fn opt_str_field(body: &Value, key: &str, max: usize) -> Result<Option<String>> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => str_field(body, key, max).map(Some),
    }
}

/// A field allowed to keep its line breaks.
///
/// `str_field` runs `security::clean`, which folds a newline into a space - right for a label,
/// wrong for the body of an embed or a ticket's opening text, where the lines are the point. This
/// reader only decides that the value is a string and not absurd, and hands the shaping to
/// `security::scrub_block` (twenty lines, three hundred characters each), which the model calls
/// anyway: a field that arrives cleaned twice is a field whose formatting was decided twice.
pub(crate) fn opt_block_field(body: &Value, key: &str, max: usize) -> Result<Option<String>> {
    let Some(value) = body.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let raw = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => {
            return Err(Error::Invalid(format!(
                "{key} must be a string, not {}",
                value_type(other)
            )))
        }
    };
    if raw.chars().count() > max {
        return Err(Error::Invalid(format!(
            "{key} is longer than {max} characters"
        )));
    }
    let kept = aerona_core::security::scrub_block(&raw, max);
    // Whitespace only is not a value; treating it as absent keeps "clear it" and "leave it alone"
    // from meaning the same thing in one direction and the other in the next.
    Ok((!kept.is_empty()).then_some(kept))
}

pub(crate) fn sf_field(body: &Value, key: &str) -> Result<Snowflake> {
    let raw = str_field(body, key, 20)?;
    Snowflake::parse(&raw).map_err(|_| Error::Invalid(format!("{key} must be a discord id")))
}

pub(crate) fn opt_sf_field(body: &Value, key: &str) -> Result<Option<Snowflake>> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => sf_field(body, key).map(Some),
    }
}

pub(crate) fn int_field(body: &Value, key: &str, default: i64, min: i64, max: i64) -> Result<i64> {
    let v = match body.get(key) {
        None | Some(Value::Null) => default,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(default),
        Some(Value::String(s)) => match s.trim().parse::<i64>() {
            Ok(v) => v,
            Err(_) => return Err(Error::Invalid(format!("{key} must be an integer"))),
        },
        Some(other) => {
            return Err(Error::Invalid(format!(
                "{key} must be an integer, not {}",
                value_type(other)
            )))
        }
    };
    if v < min || v > max {
        return Err(Error::Invalid(format!(
            "{key} must be between {min} and {max}"
        )));
    }
    Ok(v)
}

pub(crate) fn bool_field(body: &Value, key: &str, default: bool) -> bool {
    match body.get(key) {
        None | Some(Value::Null) => default,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().map(|v| v != 0).unwrap_or(default),
        Some(Value::String(s)) => matches!(s.as_str(), "1" | "true" | "yes" | "on"),
        Some(_) => default,
    }
}

pub(crate) fn enum_field<T: Copy + PartialEq>(
    body: &Value,
    key: &str,
    default: T,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<T> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::String(s)) => {
            parse(s).ok_or_else(|| Error::Invalid(format!("unrecognised {key}")))
        }
        Some(_) => Err(Error::Invalid(format!("{key} must be a string"))),
    }
}

/// Whether a credential may perform an action requiring `scope`.
///
/// Sessions are the dashboard and carry every scope; an API key acts only inside
/// the scopes it was minted with. `read` is implied by every write scope, so a key
/// that can moderate can also list.
pub(crate) fn require_scope(
    principal: &crate::auth::Principal,
    scope: &str,
) -> std::result::Result<(), crate::respond::Invalid> {
    if principal.has_scope(scope) {
        Ok(())
    } else {
        Err(crate::respond::Invalid(Error::Forbidden(
            "this api key lacks the required scope",
        )))
    }
}

/// The audit entry shared by every successful mutation of one guild's config.
pub(crate) fn settings_scope(feature: Feature) -> &'static str {
    match feature {
        Feature::General | Feature::Keys => "config",
        Feature::Moderation | Feature::Tickets => "moderate",
        _ => "config",
    }
}

// -- settings documents ------------------------------------------------------

/// Deep-merge `patch` onto `base`: objects merge key by key, everything else
/// (including arrays) replaces wholesale. This is what makes `PATCH` on a config
/// document mean "change these fields" instead of "rewrite the feature".
pub(crate) fn merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                if v.is_null() {
                    // An explicit null clears a field rather than being ignored -
                    // that is the only way to unset an optional channel or role.
                    b.insert(k.clone(), Value::Null);
                    continue;
                }
                match b.get_mut(k) {
                    Some(slot) => merge(slot, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (slot, v) => *slot = v.clone(),
    }
}

/// The default document for a feature, as JSON. Used both as the merge base and as
/// the accepted key list for unknown-field rejection.
pub(crate) fn default_doc(feature: Feature) -> Value {
    use aerona_core::models::*;
    match feature {
        Feature::General => json!(GuildSettings::default()),
        Feature::Moderation => json!(ModConfig::default()),
        Feature::Tickets => json!(TicketConfig::default()),
        Feature::Sticky => json!(StickyConfig::default()),
        Feature::AntiRaid => json!(RaidConfig::default()),
        Feature::AiFilter => json!(AiConfig::default()),
        Feature::Autorole | Feature::Welcome | Feature::Commands => json!(AutoConfig::default()),
        Feature::LinkGuard => json!(LinkGuardConfig::default()),
        Feature::AntiSpam => json!(AntiSpamConfig::default()),
        Feature::Identity => json!(IdentityConfig::default()),
        Feature::Logging => json!(LogConfig::default()),
        Feature::Capes => json!(CapeConfig::default()),
        // API keys are managed by /keys, where creation and revocation are audited
        // and scoped. A raw settings write would bypass all of that.
        Feature::Keys => Value::Null,
    }
}

/// Validate a merged settings document by round-tripping it through the typed
/// model, and reject unknown keys.
///
/// The strictness matters: every config struct is `#[serde(default)]`, so a typo
/// (`join_threshhold`) would deserialize happily, store a document that still says
/// `30`, and answer `200`. The operator would then wonder for a week why the raid
/// sensitivity did not change.
/// Keys a feature used to have and no longer models. They are removed from an incoming document
/// before validation, so an install that saved them while the setting existed can keep saving it.
/// A feature says which by listing them on itself (`StickyConfig::RETIRED_KEYS`), so the model is
/// still the only answer to what a document may contain.
fn retire_keys(feature: Feature, doc: &mut Value) {
    if feature != Feature::Sticky {
        return;
    }
    if let Some(obj) = doc.as_object_mut() {
        for key in aerona_core::models::StickyConfig::RETIRED_KEYS {
            obj.remove(key);
        }
    }
}

pub(crate) fn normalize(feature: Feature, mut doc: Value) -> Result<Value> {
    use aerona_core::models::*;
    if feature == Feature::Keys {
        return Err(Error::Invalid(
            "api keys are managed under /api/v1/keys".into(),
        ));
    }
    if !doc.is_object() {
        return Err(Error::Invalid(
            "a settings document must be a json object".into(),
        ));
    }
    // An old document is validated against the model that exists *now*, so a key a previous
    // version wrote has to be dropped here first. Refusing it would be a permanent lock rather than
    // a review: the stored document carries the key whether or not anybody is typing it, and
    // every save after an upgrade would answer "sticky has no setting(s): repost" forever. That is
    // the same class of bug as a handler writing a key the model does not have, met from the other
    // side, and the fix is the same - the document and the model agree, or nothing is written.
    retire_keys(feature, &mut doc);
    let default = default_doc(feature);
    if let (Some(got), Some(want)) = (doc.as_object(), default.as_object()) {
        let unknown: Vec<&String> = got
            .keys()
            .filter(|k| !want.contains_key(k.as_str()))
            .collect();
        if !unknown.is_empty() {
            return Err(Error::Invalid(format!(
                "{} has no setting(s): {}",
                feature.as_str(),
                unknown
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    // Deserialize into the real type so range/enum errors are caught here, then
    // re-serialize so the stored document is always complete and canonical.
    let stored = match feature {
        Feature::General => serde_json::from_value::<GuildSettings>(doc).map(|v| json!(v)),
        Feature::Moderation => serde_json::from_value::<ModConfig>(doc).map(|v| json!(v)),
        Feature::Tickets => {
            serde_json::from_value::<TicketConfig>(doc).map(|v| json!(normalise_tickets(v)))
        }
        Feature::Sticky => serde_json::from_value::<StickyConfig>(doc).map(|v| json!(v)),
        Feature::AntiRaid => serde_json::from_value::<RaidConfig>(doc).map(|v| json!(v)),
        Feature::AiFilter => serde_json::from_value::<AiConfig>(doc).map(|v| json!(v)),
        Feature::Autorole | Feature::Welcome | Feature::Commands => {
            serde_json::from_value::<AutoConfig>(doc).map(|v| json!(v))
        }
        Feature::LinkGuard => {
            serde_json::from_value::<LinkGuardConfig>(doc).map(|v| json!(normalise_links(v)))
        }
        Feature::AntiSpam => serde_json::from_value::<AntiSpamConfig>(doc).map(|v| json!(v)),
        // The identity limits are Discord's, and they are turned into a validation error
        // through serde's own channel so one failure path covers both shapes of complaint.
        Feature::Identity => serde_json::from_value::<IdentityConfig>(doc).and_then(|v| {
            normalise_identity(v)
                .map(|v| json!(v))
                .map_err(<serde_json::Error as serde::de::Error>::custom)
        }),
        Feature::Logging => serde_json::from_value::<LogConfig>(doc).map(|v| json!(v)),
        Feature::Capes => {
            serde_json::from_value::<CapeConfig>(doc).map(|v| json!(normalise_capes(v)))
        }
        Feature::Keys => unreachable!("rejected above"),
    }
    .map_err(|e| Error::Invalid(describe(&e)))?;
    Ok(stored)
}

/// What a cape notifier document is allowed to say.
///
/// Both limits are the ones the worker enforces anyway, applied on the way in so the panel reads
/// back the number that is actually used rather than the one that was typed. A one-minute poll is
/// traffic for nothing - the category is sixty-odd pages and changes a few times a year - and an
/// embed colour wider than 24 bits is not a colour Discord can draw.
fn normalise_capes(mut v: aerona_core::models::CapeConfig) -> aerona_core::models::CapeConfig {
    v.check_every_minutes = v.check_every_minutes.clamp(15, 1440);
    v.embed_color &= 0xff_ff_ff;
    v
}

/// A ticket card's wording and buttons, kept inside what Discord accepts.
///
/// Labels are clamped rather than refused: an operator who typed 90 characters wants the button,
/// and cutting it to Discord's 80 is a smaller surprise than a save that fails on a count nobody
/// can see. A link whose URL Discord would reject is dropped instead of stored, because a card
/// that fails to build takes the whole ticket open down with it.
fn normalise_tickets(
    mut v: aerona_core::models::TicketConfig,
) -> aerona_core::models::TicketConfig {
    use aerona_core::models::TicketLink;
    let label = |s: &str| -> String {
        let t = aerona_core::security::clean(s, 80);
        if t == "Clear" || t == "Close ticket" || t == "Claim" || t == "Transcript" {
            // Wording that equals the built-in one is not a change; storing it would make the
            // default impossible to recognise as a default.
            String::new()
        } else {
            t
        }
    };
    v.close_label = label(&v.close_label);
    v.claim_label = label(&v.claim_label);
    v.transcript_label = label(&v.transcript_label);
    let mut seen: Vec<String> = Vec::new();
    v.links.retain(|l| {
        let url = l.url.trim();
        let ok = l.is_usable() && !seen.iter().any(|u| u == url);
        if ok {
            seen.push(url.to_string());
        }
        ok
    });
    v.links.truncate(TicketLink::MAX);
    for link in &mut v.links {
        link.label = aerona_core::security::clean(&link.label, TicketLink::LABEL_MAX);
        link.url = aerona_core::security::clean(link.url.trim(), TicketLink::URL_MAX);
    }
    v
}

/// Domains are matched as hosts, so they are stored as hosts: lowercase, no scheme, no
/// path, no wildcard. A `*.example.com` typed by an operator means the same thing as
/// `example.com` to this product, and saying so by rewriting it beats rejecting it.
fn normalise_links(
    mut v: aerona_core::models::LinkGuardConfig,
) -> aerona_core::models::LinkGuardConfig {
    let mut seen: Vec<String> = Vec::new();
    for raw in &v.allowed_domains {
        let d = raw
            .trim()
            .to_lowercase()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_start_matches("www.")
            .trim_start_matches("*.")
            .trim_end_matches('/')
            .trim_end_matches('.')
            .chars()
            .take(180)
            .collect::<String>();
        if d.is_empty() || d.contains('/') || d.contains(':') || d.contains(' ') {
            continue;
        }
        if !seen.contains(&d) {
            seen.push(d);
        }
        if seen.len() == 100 {
            break;
        }
    }
    v.allowed_domains = seen;
    v
}

/// The identity document is the only one whose values go *to Discord* verbatim, so the
/// limits are Discord's and are enforced here rather than discovered by a 400 in a worker
/// log. An empty string is a real value: it means "clear this field on Discord".
fn normalise_identity(
    v: aerona_core::models::IdentityConfig,
) -> Result<aerona_core::models::IdentityConfig> {
    use aerona_core::models::IdentityConfig as I;
    let clean = |s: &Option<String>, max: usize, name: &str| -> Result<Option<String>> {
        match s {
            None => Ok(None),
            Some(raw) => {
                let t = aerona_core::security::clean(raw, max + 40);
                if t.is_empty() {
                    return Ok(Some(String::new()));
                }
                if t.chars().count() > max {
                    return Err(Error::Invalid(format!(
                        "{name} is {} characters; Discord allows {max}",
                        t.chars().count()
                    )));
                }
                Ok(Some(t))
            }
        }
    };
    let nick = clean(&v.nick, I::NICK_MAX, "nickname")?;
    if nick
        .as_deref()
        .is_some_and(|s| !s.is_empty() && s.chars().count() < 2)
    {
        return Err(Error::Invalid("a nickname needs 2 characters".into()));
    }
    // A decoration is an id or nothing. Discord has no endpoint that lists them for a bot, so the
    // only validation available is the shape: digits, and not zero. An empty string is kept,
    // because that is how the panel says "take it off", and the apply turns it into a null.
    let deco = |s: &Option<String>, name: &str| -> Result<Option<String>> {
        let Some(raw) = s else { return Ok(None) };
        let id = raw.trim();
        if id.is_empty() {
            return Ok(Some(String::new()));
        }
        if id.len() > 20 || id == "0" || !id.chars().all(|c| c.is_ascii_digit()) {
            return Err(Error::Invalid(format!(
                "{name} must be the digits of a Discord id, not {id:?}"
            )));
        }
        Ok(Some(id.to_string()))
    };
    Ok(aerona_core::models::IdentityConfig {
        nick,
        bio: clean(&v.bio, I::BIO_MAX, "bio")?,
        // The bytes never live in a settings document: see `identity_avatar`.
        avatar: None,
        avatar_hash: v.avatar_hash.filter(|h| h.len() >= 8 && h.len() <= 80),
        banner_hash: v.banner_hash.filter(|h| h.len() >= 8 && h.len() <= 80),
        avatar_decoration_id: deco(&v.avatar_decoration_id, "the avatar decoration")?,
        banner_decoration_id: deco(&v.banner_decoration_id, "the banner decoration")?,
        about: clean(&v.about, 300, "about")?,
    })
}

fn describe(e: &serde_json::Error) -> String {
    // serde's message names the field and the expected shape, which is exactly
    // what the dashboard needs to highlight; it never contains paths or values
    // from the database.
    let m = e.to_string();
    m.chars().take(200).collect()
}

pub(crate) fn settings_for(
    db: &aerona_core::db::Db,
    guild: Snowflake,
    feature: Feature,
) -> Result<Value> {
    // A stored document is always the full normalized one (`PATCH` writes what
    // `normalize` produced), so there is nothing to merge with defaults here - and
    // merging would risk showing a field the guild explicitly set.
    match db.raw_settings(guild, feature)? {
        Some(v) => Ok(v),
        None => Ok(default_doc(feature)),
    }
}

/// A `200` with a JSON value, for the many read-only endpoints.
pub(crate) fn json_ok(v: Value) -> Response {
    crate::respond::ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aerona_core::models::Feature;

    #[test]
    fn query_parsers_are_bounded_and_strict() {
        let mut q = Params::new();
        assert_eq!(
            q_bounded(&q, "limit", 25, 200).unwrap(),
            25,
            "absent means default"
        );
        q.insert("limit".into(), "1000".into());
        assert_eq!(
            q_bounded(&q, "limit", 25, 200).unwrap(),
            200,
            "clamped, not rejected"
        );
        q.insert("limit".into(), "-1".into());
        assert!(
            q_bounded(&q, "limit", 25, 200).is_err(),
            "negative is a client bug"
        );
        q.insert("limit".into(), "many".into());
        assert!(q_bounded(&q, "limit", 25, 200).is_err());
        q.insert("limit".into(), "".into());
        assert_eq!(
            q_bounded(&q, "limit", 25, 200).unwrap(),
            25,
            "empty is absent"
        );

        q.insert("guild".into(), "12345".into());
        assert_eq!(q_opt_sf(&q, "guild").unwrap(), Some(Snowflake(12345)));
        q.insert("guild".into(), "abc".into());
        assert!(q_opt_sf(&q, "guild").is_err());
        q.insert("flag".into(), "yes".into());
        assert!(q_bool(&q, "flag"));
        assert!(!q_bool(&q, "missing"));
        assert!(q_i64(&q, "guild").is_err());
    }

    #[test]
    fn body_fields_reject_the_right_things() {
        let body = json!({"reason": "  spam  ", "n": 7, "b": true, "ids": [1], "user": 12345, "empty": "   "});
        assert_eq!(str_field(&body, "reason", 80).unwrap(), "spam");
        assert_eq!(int_field(&body, "n", 0, 0, 10).unwrap(), 7);
        assert!(
            int_field(&body, "missing", 5, 0, 10).unwrap() == 5,
            "default for absent"
        );
        assert!(int_field(&body, "n", 0, 0, 5).is_err(), "out of range");
        assert!(
            int_field(&body, "b", 0, 0, 5).is_err(),
            "a bool is not an int"
        );
        assert!(bool_field(&body, "b", false));
        assert!(!bool_field(&body, "missing", false));
        assert_eq!(
            sf_field(&body, "user").unwrap(),
            Snowflake(12345),
            "numbers are accepted as ids"
        );
        assert!(
            str_field(&body, "empty", 10).is_err(),
            "whitespace-only is empty"
        );
        assert!(
            str_field(&body, "ids", 10).is_err(),
            "an array is not a string"
        );
        assert!(
            str_field(&body, "absent", 10).is_err(),
            "required field missing"
        );
        assert_eq!(opt_str_field(&body, "absent", 10).unwrap(), None);
    }

    #[test]
    fn merge_is_deep_for_objects_and_replace_for_arrays() {
        let mut base =
            json!({"a": 1, "nested": {"x": 1, "y": 2}, "list": [1, 2, 3], "nil": "keep"});
        merge(
            &mut base,
            &json!({"nested": {"y": 9}, "list": [7], "nil": null, "new": true}),
        );
        assert_eq!(base["a"], 1, "untouched");
        assert_eq!(base["nested"]["x"], 1, "sibling key survives");
        assert_eq!(base["nested"]["y"], 9, "patched key wins");
        assert_eq!(base["list"], json!([7]), "arrays replace wholesale");
        assert_eq!(base["nil"], Value::Null, "explicit null clears");
        assert_eq!(base["new"], true, "new key added");
    }

    #[test]
    fn normalize_validates_strictly_and_canonicalises() {
        // A partial patch is filled from the defaults, so the dashboard always
        // reads a complete document.
        let doc = normalize(Feature::AntiRaid, json!({"join_threshold": 25})).unwrap();
        assert_eq!(doc["join_threshold"], 25);
        assert_eq!(
            doc["join_window_secs"],
            json!(aerona_core::models::RaidConfig::default().join_window_secs)
        );
        assert!(doc.as_object().unwrap().len() > 10, "full document");
        // Typo: refused, not silently dropped.
        let err = normalize(Feature::AntiRaid, json!({"join_threshhold": 25})).unwrap_err();
        assert!(err.to_string().contains("join_threshhold"), "{err}");
        // Wrong type: refused by the typed deserialize.
        assert!(normalize(Feature::AntiRaid, json!({"join_threshold": "lots"})).is_err());
        // Enum values are validated too.
        assert!(normalize(Feature::Moderation, json!({"escalate_action": "vaporise"})).is_err());
        assert!(normalize(Feature::Moderation, json!({"escalate_action": "timeout"})).is_ok());
        // Not every feature accepts a settings write.
        assert!(normalize(Feature::Keys, json!({})).is_err());
        assert!(normalize(Feature::General, json!("a string")).is_err());
    }

    #[test]
    fn default_docs_cover_every_writable_feature() {
        for f in Feature::ALL {
            let d = default_doc(f);
            if f == Feature::Keys {
                assert!(d.is_null(), "keys has no settings document");
                continue;
            }
            assert!(d.is_object(), "{:?} must have an object default", f);
            assert!(
                !d.as_object().unwrap().is_empty(),
                "{:?} default must list its keys",
                f
            );
        }
    }

    #[test]
    fn feature_names_roundtrip() {
        for f in Feature::ALL {
            assert_eq!(feature(f.as_str()).unwrap(), f);
        }
        assert!(feature("nope").is_err());
        // The error lists the valid names, so a wrong one is self-explanatory.
        assert!(feature("nope")
            .unwrap_err()
            .to_string()
            .contains("anti_raid"));
    }
}
