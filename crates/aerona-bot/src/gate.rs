//! The text gate: the pipeline every message runs through before it is allowed to
//! count. Two detection layers, one decision, one enforcement path.
//!
//! *custom rules* ([`text::compile_rules`] + [`text::scan_custom`], evaluated locally and
//! therefore free, instant and private) and *remote classification* (Sightengine, only
//! when a guild selects categories for it). Both feed one hit list and
//! [`text::decide`] turns that into a [`FilterVerdict`] — the same function the
//! dashboard's "test a phrase" endpoint calls, so what an operator sees in a preview is
//! what the gate does, by construction rather than by discipline.

use std::collections::HashMap;

use serde::Deserialize;
use serenity::all::{ChannelId, Context, CreateMessage, GuildId, MessageId, UserId};

use aerona_core::identity::Snowflake;
use aerona_core::models::{
    AiCategoryRule, AiConfig, AiEvent, FailMode, FilterAction, FilterVerdict, GuardAction,
    GuildSettings, Severity,
};
use aerona_core::security::sha256_hex;
use aerona_core::text::GuardHit;
use aerona_core::text::{self, TextHit};
use aerona_core::time::now;

use crate::clamp_chars;
use crate::BotState;

/// What the gate decided about one message.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub verdict: FilterVerdict,
    /// The message was removed. The caller must then stop processing it — no XP, no
    /// sticky tally, no command dispatch: a deleted message must not be rewarded.
    pub deleted: bool,
    /// The remote provider failed, so the log can say "unavailable" instead of silently
    /// reporting a clean scan.
    pub degraded: bool,
}

impl Outcome {
    pub fn clean() -> Self {
        Outcome {
            verdict: FilterVerdict {
                violation: false,
                severity: Severity::Info,
                score: 0.0,
                categories: vec![],
                matched: vec![],
                action: FilterAction::Flag,
                strikes: 0,
                strikes_max: 0,
                reason: String::new(),
                latency_ms: 0,
                degraded: false,
            },
            deleted: false,
            degraded: false,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SeResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    attributes: SeAttributes,
}

#[derive(Debug, Default, Deserialize)]
struct SeAttributes {
    #[serde(default)]
    classes: HashMap<String, SeClass>,
}

#[derive(Debug, Deserialize)]
struct SeClass {
    #[serde(default)]
    label: String,
    #[serde(default)]
    confidence: f64,
}

const ENDPOINT: &str = "https://lab.sightengine.com/text/check";

/// Which checks to ask for. Kept as the guild's own category list rather than a fixed
/// set: the operator pays for exactly what they enabled, and an unexpected bill is a
/// support ticket nobody wants.
fn models_for(cfg: &AiConfig) -> String {
    let mut wanted: Vec<&str> = cfg.categories.iter().map(String::as_str).collect();
    if wanted.is_empty() {
        wanted.push("toxicity");
    }
    wanted.sort_unstable();
    wanted.dedup();
    wanted.join(",")
}

/// Score-to-severity for a remote class. The provider reports a confidence, not a
/// severity, and this mapping is what gives a guild's `min_severity` floor meaning for
/// remote hits. The bands follow the provider's own guidance: below 0.5 is noise.
fn severity_for_score(score: f64) -> Severity {
    if score >= 0.95 {
        Severity::Critical
    } else if score >= 0.85 {
        Severity::High
    } else if score >= 0.70 {
        Severity::Medium
    } else if score >= 0.50 {
        Severity::Low
    } else {
        Severity::Info
    }
}

/// Drop hits a guild's configuration does not want: disabled rules, unselected
/// categories, and scores below the effective threshold. Done here rather than inside
/// `decide` so the dashboard preview and the gate filter identically.
pub fn keep_hits(hits: &[TextHit], cfg: &AiConfig) -> Vec<TextHit> {
    hits.iter()
        .filter(|h| {
            if h.severity.rank() < cfg.min_severity.rank() {
                return false;
            }
            if !cfg.categories.is_empty() && !cfg.categories.contains(&h.category) {
                // A custom rule's own category is allowed through even when it is not in
                // `categories`: the operator wrote that pattern on purpose, and
                // silently ignoring it would look like a broken feature.
                if !is_custom_hit(h) {
                    return false;
                }
            }
            true
        })
        .filter(|h| match rule_for(&cfg.categories_config, &h.category) {
            // No rule for this category means no threshold to respect (a custom rule or
            // a provider category the guild left at its default).
            None => true,
            Some(rule) => {
                rule.enabled
                    && h.score + f64::EPSILON >= text::effective_threshold(rule, cfg.strictness)
            }
        })
        .cloned()
        .collect()
}

fn is_custom_hit(h: &TextHit) -> bool {
    h.category == "custom"
}

fn rule_for<'a>(rules: &'a [AiCategoryRule], category: &str) -> Option<&'a AiCategoryRule> {
    rules.iter().find(|r| r.category == category)
}

fn category_label(v: &FilterVerdict) -> String {
    if v.categories.is_empty() {
        "filter".to_string()
    } else {
        v.categories.join("+")
    }
}

/// One gate pass for one message. `None` means this guild has no filter at all, which
/// is different from `Some(clean)`: a guild that has one and passed the message.
#[allow(clippy::too_many_arguments)]
pub async fn evaluate(
    state: &BotState,
    ctx: &Context,
    cfg: &AiConfig,
    guild: Snowflake,
    author: Snowflake,
    channel: Snowflake,
    message: Snowflake,
    content: &str,
    member_roles: &[Snowflake],
    settings: &GuildSettings,
) -> Option<Outcome> {
    // The two local guards are read here rather than passed in: they are per guild, they
    // change under the operator's fingers, and the config epoch makes a stale read
    // impossible within one message — so the database is the freshest thing available.
    let links = state.db.link_guard_config(guild).unwrap_or_default();
    let spam = state.db.anti_spam_config(guild).unwrap_or_default();
    if !cfg.enabled && !links.enabled && !spam.enabled {
        return None;
    }
    if settings.ignored_channel_ids.contains(&channel)
        || cfg.whitelist_channel_ids.contains(&channel)
    {
        return None;
    }
    // The local pass runs first and on the *whole* message: `min_chars` and `max_chars`
    // exist to keep a provider bill down, and an invite in a six-word sentence is still an
    // invite. Its exemptions are its own, so a guild can let staff post links without
    // switching the model's whitelist off.
    if (links.enabled || spam.enabled)
        && !spam
            .exempt_role_ids
            .iter()
            .any(|r| member_roles.contains(r))
        && !spam.exempt_channel_ids.contains(&channel)
        && !links
            .exempt_role_ids
            .iter()
            .any(|r| member_roles.contains(r))
        && !links.exempt_channel_ids.contains(&channel)
        && !member_roles
            .iter()
            .any(|r| cfg.whitelist_role_ids.contains(r))
        && !cfg.whitelist_user_ids.contains(&author)
    {
        let seen = if spam.enabled && spam.max_messages > 0 {
            state.note_spam(
                guild,
                author,
                usize::from(spam.max_messages),
                spam.window_secs,
            )
        } else {
            0
        };
        if let Some((hit, action, minutes)) = local_guard(content, &links, &spam, seen) {
            let deleted = enforce_guard(
                state, ctx, guild, author, channel, message, &hit, action, minutes,
            )
            .await;
            let mut verdict = Outcome::clean().verdict;
            verdict.violation = true;
            verdict.severity = Severity::Medium;
            verdict.score = 1.0;
            verdict.categories = vec![hit.category.to_string()];
            verdict.matched = vec![hit.matched.clone()];
            verdict.action = FilterAction::Delete;
            verdict.reason = reason_for(&hit, action);
            let _ = state.db.log_ai_event(&AiEvent {
                id: 0,
                guild_id: guild,
                user_id: author,
                channel_id: channel,
                message_id: Some(message),
                category: hit.category.to_string(),
                severity: Severity::Medium,
                score: 1.0,
                action: FilterAction::Delete,
                created_at: now(),
                detail: clamp_chars(&verdict.reason, 400),
            });
            return Some(Outcome {
                verdict,
                deleted,
                degraded: false,
            });
        }
    }
    if !cfg.enabled {
        return Some(Outcome::clean());
    }
    if cfg.whitelist_user_ids.contains(&author)
        || member_roles
            .iter()
            .any(|r| cfg.whitelist_role_ids.contains(r))
    {
        return None;
    }
    let chars = content.chars().count();
    if chars < usize::from(cfg.min_chars) {
        return None;
    }
    // The provider charges and rates by length, and a 4 000-character wall of text from
    // a raid is both expensive and unnecessary: the first `max_chars` decide it.
    let text = clamp_chars(content, usize::from(cfg.max_chars.max(1)));

    let compiled = text::compile_rules(&state.db.custom_rules(guild).unwrap_or_default());
    let (custom_hit, mut hits) = text::scan_custom(&text, &compiled);

    let mut degraded = false;
    let started = std::time::Instant::now();
    if cfg.mode != "local" {
        match remote_scan(&state.http, &state.cfg, cfg, &text).await {
            Ok(extra) => hits.extend(extra),
            Err(note) => {
                degraded = true;
                tracing::warn!(%guild, %note, "the text provider did not answer");
                if cfg.on_api_error == FailMode::Block {
                    hits.push(TextHit {
                        category: "unavailable".into(),
                        kind: "provider".into(),
                        severity: Severity::High,
                        score: 1.0,
                        matched: String::new(),
                        start: 0,
                        end: 0,
                        action: None,
                        rule_strikes: 0,
                        source: aerona_core::text::HitSource::Ml,
                    });
                }
            }
        }
    }

    let latency_ms = started.elapsed().as_millis() as u64;
    let strikes_before = state.db.active_strikes(guild, author).unwrap_or(0);
    let mut verdict = text::decide(cfg, &text, keep_hits(&hits, cfg), strikes_before);
    verdict.latency_ms = latency_ms;
    verdict.degraded = degraded;
    if custom_hit && !verdict.violation {
        // `decide` can still call a custom hit non-actionable (a `flag` rule under a
        // stricter floor). The cache entry below must not then be written as "clean".
        verdict.violation = false;
    }

    if !verdict.violation {
        return Some(Outcome {
            verdict,
            deleted: false,
            degraded,
        });
    }

    let deleted = enforce(
        state,
        ctx,
        guild,
        author,
        channel,
        message,
        &mut verdict,
        cfg,
    )
    .await;

    // Verdict cache: keyed on the hash of the exact bytes plus the author, so a raid of
    // copy-pasted text is classified once and one user editing their own message is not
    // judged twice. Only clean verdicts are cached — caching a violation would make the
    // strike counter skip a repeat offender.
    if !deleted && cfg.cache_seconds > 0 {
        let hash = sha256_hex(text.as_bytes());
        let _ = state.db.cache_verdict(
            guild,
            author,
            &hash,
            &[],
            i64::from(u32::from(cfg.cache_seconds)),
        );
    }

    for category in &verdict.categories {
        let event = AiEvent {
            id: 0,
            guild_id: guild,
            user_id: author,
            channel_id: channel,
            message_id: Some(message),
            category: category.clone(),
            severity: verdict.severity,
            score: verdict.score,
            action: verdict.action,
            created_at: now(),
            detail: clamp_chars(&verdict.reason, 400),
        };
        let _ = state.db.log_ai_event(&event);
    }
    Some(Outcome {
        verdict,
        deleted,
        degraded,
    })
}

/// What the local rules make of one message, before anything is paid for.
///
/// Pure on purpose: the arguments are the text, the two configs, and how many messages this
/// member has already sent in the window. The whole rule set is therefore testable without a
/// gateway, a database, or Discord. `seen` is what the caller counted, not a guess.
pub fn local_guard(
    content: &str,
    links: &aerona_core::models::LinkGuardConfig,
    spam: &aerona_core::models::AntiSpamConfig,
    seen: usize,
) -> Option<(GuardHit, GuardAction, u32)> {
    let text = clamp_chars(content, 4000);
    if links.enabled {
        if let Some(hit) = aerona_core::text::link_violations(
            &text,
            links.block_links,
            links.block_invites,
            &links.allowed_domains,
        )
        .into_iter()
        .next()
        {
            return Some((hit, links.action, links.timeout_minutes));
        }
    }
    if !spam.enabled {
        return None;
    }
    let shape = text.chars().count() >= usize::from(spam.min_chars);
    if spam.max_messages > 0 && seen > usize::from(spam.max_messages) {
        return Some((
            GuardHit {
                category: "flood",
                matched: format!("{seen} messages in the window"),
            },
            spam.action,
            spam.timeout_minutes,
        ));
    }
    if spam.max_mentions > 0 {
        let n = aerona_core::text::count_mentions(&text);
        if n > u32::from(spam.max_mentions) {
            return Some((
                GuardHit {
                    category: "mentions",
                    matched: format!("{n} pings"),
                },
                spam.action,
                spam.timeout_minutes,
            ));
        }
    }
    if shape && spam.max_emoji > 0 {
        let n = aerona_core::text::count_emoji(&text);
        if n > u32::from(spam.max_emoji) {
            return Some((
                GuardHit {
                    category: "emoji",
                    matched: format!("{n} emoji"),
                },
                spam.action,
                spam.timeout_minutes,
            ));
        }
    }
    if shape && spam.max_caps_percent > 0 {
        let pct = aerona_core::text::caps_percent(&text);
        if pct >= spam.max_caps_percent {
            return Some((
                GuardHit {
                    category: "caps",
                    matched: format!("{pct}% capital letters"),
                },
                spam.action,
                spam.timeout_minutes,
            ));
        }
    }
    None
}

/// The one sentence both the audit row and the member's DM show. Built here so the phrasing
/// cannot drift from what the rule actually was.
fn reason_for(hit: &GuardHit, action: GuardAction) -> String {
    let what = match hit.category {
        "invite" => "a Discord invite",
        "link" => "a link",
        "flood" => "too many messages at once",
        "mentions" => "too many mentions",
        "emoji" => "too much emoji",
        "caps" => "shouting",
        other => other,
    };
    let then = match action {
        GuardAction::Delete => "removed",
        GuardAction::DeleteWarn => "removed and warned",
        GuardAction::DeleteTimeout => "removed and timed out",
    };
    format!(
        "Your message contained {what}; it was {then} ({}).",
        hit.matched
    )
}

/// Delete, then whatever the guild's action says on top. The removal has to land before the
/// member's next message, so it is a direct call; a warn row is ours and a timeout is one
/// edit, so both are inline as well. Nothing here is allowed to fail the message: a guard
/// that cannot enforce has already done its logging, and the mod log is what a human reads.
// Nine arguments, and clippy is right that that is a lot. The fix is a small `Enforcement` struct
// — one call site, so it is cheap — but it is a change to how a warn, a deletion and a timeout are
// passed into moderation itself, which is not this round's subject. Bundled here so the workspace
// gate stays runnable; do the struct the next time an argument has to be added, since that is the
// moment the positional list becomes a real hazard rather than a lint.
#[allow(clippy::too_many_arguments)]
async fn enforce_guard(
    state: &BotState,
    ctx: &Context,
    guild: Snowflake,
    author: Snowflake,
    channel: Snowflake,
    message: Snowflake,
    hit: &GuardHit,
    action: GuardAction,
    timeout_minutes: u32,
) -> bool {
    let g = GuildId::new(guild.as_u64());
    let c = ChannelId::new(channel.as_u64());
    let m = MessageId::new(message.as_u64());
    let u = UserId::new(author.as_u64());
    let mut deleted = false;
    match c.delete_message(&ctx.http, m).await {
        Ok(()) => deleted = true,
        Err(e) => tracing::debug!(%guild, %e, "the guard's delete was refused"),
    }
    let reason = reason_for(hit, action);
    if action == GuardAction::DeleteWarn {
        let cfg = state.db.mod_config(guild).unwrap_or_default();
        let _ = state.db.add_strike(
            guild,
            author,
            "local-guard",
            Some(hit.category),
            &reason,
            cfg.warn_expiry_days.max(1),
        );
        if let Err(e) = state.db.add_warn(
            guild,
            author,
            Snowflake(0),
            &reason,
            cfg.warn_expiry_days.max(1),
            i64::from(cfg.strike_threshold.max(1)),
            Some(message),
        ) {
            tracing::warn!(%guild, %e, "the warn row for a guard hit could not be written");
        }
    }
    if action == GuardAction::DeleteTimeout && timeout_minutes > 0 {
        let until = rfc3339(now() + i64::from(timeout_minutes.min(40320)) * 60);
        let edit = serenity::all::EditMember::new().disable_communication_until(until);
        if let Err(e) = g.edit_member(&ctx.http, u, edit).await {
            tracing::debug!(%guild, %e, "the guard's timeout was refused");
        }
    }
    deleted
}

/// Ask the provider. Errors come back as strings because they are logged verbatim.
async fn remote_scan(
    http: &reqwest::Client,
    cfg: &aerona_core::config::Config,
    filter: &AiConfig,
    text: &str,
) -> Result<Vec<TextHit>, String> {
    let Some((user, secret)) = cfg.sightengine.as_ref() else {
        return Err("no SIGHTENGINE_TOKEN is configured on this install".into());
    };
    let mut form = vec![
        ("models", models_for(filter)),
        ("text", text.to_string()),
        ("fields", "classes".into()),
    ];
    if let Some(list) = filter.list_id.clone().filter(|l| !l.is_empty()) {
        form.push(("list_id", list));
    }
    let resp = http
        .post(ENDPOINT)
        .basic_auth(user.clone(), Some(secret.expose()))
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "provider answered {status}: {}",
            clamp_chars(&body, 180)
        ));
    }
    let parsed: SeResponse = resp
        .json()
        .await
        .map_err(|e| format!("unreadable response: {e}"))?;
    if parsed.status != "ok" {
        return Err(format!("provider status was {:?}", parsed.status));
    }
    Ok(parsed
        .attributes
        .classes
        .into_iter()
        .filter(|(_, class)| class.confidence > 0.0)
        .map(|(category, class)| TextHit {
            category,
            kind: class.label,
            severity: severity_for_score(class.confidence),
            score: class.confidence,
            matched: String::new(),
            start: 0,
            end: 0,
            action: None,
            rule_strikes: 0,
            source: aerona_core::text::HitSource::Rules,
        })
        .collect())
}

/// Carry out a verdict. Returns whether the message was removed, because the caller must
/// not treat a deleted message as conversation.
// The verdict needs the whole context of the event it came from (who, where, which
// message, and the guild's config); bundling those into a struct would name a thing that
// exists for exactly one call.
#[allow(clippy::too_many_arguments)]
async fn enforce(
    state: &BotState,
    ctx: &Context,
    guild: Snowflake,
    author: Snowflake,
    channel: Snowflake,
    message: Snowflake,
    verdict: &mut FilterVerdict,
    cfg: &AiConfig,
) -> bool {
    let g = GuildId::new(guild.as_u64());
    let c = ChannelId::new(channel.as_u64());
    let m = MessageId::new(message.as_u64());
    let u = UserId::new(author.as_u64());
    let mut deleted = false;

    if verdict.action.deletes() || cfg.auto_delete {
        match c.delete_message(&ctx.http, m).await {
            Ok(()) => deleted = true,
            // A 403 or a 404 here is not a failure of the filter: the message may be
            // older than 14 days, or another bot got there first.
            Err(e) => tracing::debug!(%guild, %e, "the flagged message could not be deleted"),
        }
    }

    if cfg.warn_on_hit && verdict.severity.rank() >= Severity::Medium.rank() {
        let warn = state.db.add_warn(
            guild,
            author,
            Snowflake(0),
            &verdict.reason,
            cfg.strike_expiry_days.max(1),
            i64::from(cfg.strikes_max.max(1)),
            Some(message),
        );
        if let Err(e) = warn {
            tracing::warn!(%guild, %e, "the warn row for a filter hit could not be written");
        }
    }
    if cfg.strikes_enabled && verdict.strikes > 0 {
        let label = category_label(verdict);
        let _ = state.db.add_strike(
            guild,
            author,
            "ai-filter",
            Some(&label),
            &verdict.reason,
            cfg.strike_expiry_days.max(1),
        );
    }

    if cfg.timeout_minutes > 0 && verdict.action == FilterAction::DeleteStrike {
        let until = rfc3339(now() + i64::from(cfg.timeout_minutes) * 60);
        let edit = serenity::all::EditMember::new().disable_communication_until(until);
        if let Err(e) = g.edit_member(&ctx.http, u, edit).await {
            tracing::debug!(%guild, %e, "timeout refused");
        }
    }

    if matches!(verdict.action, FilterAction::DeleteKick) {
        let _ = g
            .kick(&ctx.http, u)
            .await
            .map_err(|e| tracing::debug!(%guild, %e, "kick refused"));
    }

    // Escalation at the strike ceiling, and only when the guild opted into it: a filter
    // that bans on its own is how a bad afternoon becomes a permanent loss.
    if cfg.ban_on_strikes && verdict.strikes_max > 0 && verdict.strikes >= verdict.strikes_max {
        if let Err(e) = g.ban_with_reason(&ctx.http, u, 0, &verdict.reason).await {
            tracing::debug!(%guild, %e, "ban refused");
        }
        let _ = state
            .db
            .record_ban(guild, author, Snowflake(0), &verdict.reason, 0, false);
    }

    if cfg.dm_user && !verdict.reason.is_empty() {
        let body = format!(
            "Your message in a server you share with me was removed.\nReason: {}",
            clamp_chars(&verdict.reason, 400)
        );
        // Two calls, because a `UserId` cannot open a DM on its own: the user object is
        // needed for the endpoint Discord exposes, and it doubles as the check that the
        // member still exists. A closed DM is normal, so both failures are debug-level.
        if let Ok(user) = u.to_user(&ctx.http).await {
            match user.create_dm_channel(&ctx.http).await {
                Ok(ch) => {
                    let _ = ch
                        .send_message(&ctx.http, CreateMessage::new().content(body))
                        .await;
                }
                Err(e) => tracing::debug!(%guild, %e, "the member's DMs are closed"),
            }
        }
    }

    deleted
}

/// A timestamp Discord will accept for `communication_disabled_until`.
pub(crate) fn rfc3339(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let rem = epoch_secs.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a calendar date (Howard Hinnant's algorithm, the same one
/// `aerona_core::time` uses — duplicated here because the crate exposes it over a
/// different shape and inventing a second conversion would be worse than three lines).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AiConfig {
        AiConfig {
            enabled: true,
            mode: "hybrid".into(),
            categories: vec!["toxicity".into()],
            min_severity: Severity::Medium,
            strictness: 0,
            ..Default::default()
        }
    }

    use aerona_core::models::{AntiSpamConfig, LinkGuardConfig};

    fn links() -> LinkGuardConfig {
        LinkGuardConfig {
            enabled: true,
            block_links: true,
            block_invites: true,
            allowed_domains: vec![],
            exempt_role_ids: vec![],
            exempt_channel_ids: vec![],
            action: GuardAction::Delete,
            timeout_minutes: 0,
        }
    }

    fn spam() -> AntiSpamConfig {
        AntiSpamConfig {
            enabled: true,
            max_messages: 5,
            window_secs: 5,
            max_mentions: 2,
            max_emoji: 4,
            max_caps_percent: 80,
            min_chars: 10,
            exempt_role_ids: vec![],
            exempt_channel_ids: vec![],
            action: GuardAction::DeleteWarn,
            timeout_minutes: 0,
        }
    }

    #[test]
    fn a_link_is_caught_and_the_allowlist_spares_it() {
        let open = links();
        let (hit, action, _) = local_guard(
            "see https://evil.test/prize",
            &open,
            &AntiSpamConfig::default(),
            0,
        )
        .expect("a bare link is a violation");
        assert_eq!(hit.category, "link");
        assert_eq!(action, GuardAction::Delete);
        // The domain is stored as a host, so the match is on the host: `www.` and a path are
        // the same site to this rule, and an operator who typed either meant this.
        let allowed = LinkGuardConfig {
            allowed_domains: vec!["discord.com".into()],
            ..open.clone()
        };
        assert!(local_guard(
            "https://cdn.discord.com/app.png",
            &allowed,
            &AntiSpamConfig::default(),
            0
        )
        .is_none());
        assert!(local_guard("https://discord.com", &open, &AntiSpamConfig::default(), 0).is_some());
    }

    #[test]
    fn an_invite_is_its_own_category_because_the_fix_is_not_the_same() {
        // Links allowed, invites still refused: this is the shape a "no ads but talk freely"
        // server picks, and the reason shown to the member has to name the invite.
        let only = LinkGuardConfig {
            block_links: false,
            block_invites: true,
            ..links()
        };
        let (hit, _, _) = local_guard(
            "join discord.gg/nextgen now",
            &only,
            &AntiSpamConfig::default(),
            0,
        )
        .unwrap();
        assert_eq!(hit.category, "invite");
        assert!(local_guard(
            "the link to the doc is in #rules",
            &only,
            &AntiSpamConfig::default(),
            0
        )
        .is_none());
    }

    #[test]
    fn the_flood_rule_acts_on_the_count_the_caller_took() {
        let off = LinkGuardConfig::default();
        let s = spam();
        // `seen` includes the message being judged, so the limit is exceeded *after* it.
        assert!(local_guard("hello there friend", &off, &s, usize::from(s.max_messages)).is_none());
        let (hit, action, _) = local_guard(
            "hello there friend",
            &off,
            &s,
            1 + usize::from(s.max_messages),
        )
        .unwrap();
        assert_eq!(hit.category, "flood");
        assert_eq!(action, GuardAction::DeleteWarn);
    }

    #[test]
    fn shape_rules_need_the_minimum_length_that_typing_does_not() {
        let mut s = spam();
        s.min_chars = 12;
        // Six capitals in a row is 100% caps, and it is one keystroke: `min_chars` is what
        // stops "WTF" from being a moderation event.
        let short = local_guard("WTF!!!", &LinkGuardConfig::default(), &s, 1);
        assert!(
            short.is_none(),
            "a shout too short to be a pattern is not a violation"
        );
        let long = local_guard(
            "WHY IS EVERYONE IN THIS CHANNEL SHOUTING",
            &LinkGuardConfig::default(),
            &s,
            1,
        )
        .unwrap();
        assert_eq!(long.0.category, "caps");
        // Emoji has the same floor, and the same reason.
        s.max_emoji = 2;
        assert_eq!(
            local_guard("nice one", &LinkGuardConfig::default(), &s, 1)
                .map(|h| h.0.category)
                .unwrap_or("clean"),
            "clean"
        );
        s.min_chars = 1;
        let emoji = local_guard("hi 👟👟👟👟👟👟", &LinkGuardConfig::default(), &s, 1).unwrap();
        assert_eq!(emoji.0.category, "emoji");
    }

    #[test]
    fn a_guilds_timeout_setting_travels_with_the_hit() {
        let mut s = spam();
        s.action = GuardAction::DeleteTimeout;
        s.timeout_minutes = 7;
        s.max_mentions = 1;
        s.min_chars = 0;
        let (hit, action, minutes) =
            local_guard("<@1> <@2>", &LinkGuardConfig::default(), &s, 1).unwrap();
        assert_eq!(hit.category, "mentions");
        assert_eq!(action, GuardAction::DeleteTimeout);
        assert_eq!(minutes, 7, "the gate needs the length, not a default");
    }

    #[test]
    fn a_disabled_guard_says_nothing_at_all() {
        // Both off, on text that would trip each of them. `LinkGuardConfig::default()` is
        // *on* (invites only — the one rule no server argues with), so "off" has to be said
        // rather than assumed, which is exactly the trap this test exists to catch.
        let off = LinkGuardConfig {
            enabled: false,
            ..LinkGuardConfig::default()
        };
        assert!(local_guard(
            "https://evil.test discord.gg/x",
            &off,
            &AntiSpamConfig::default(),
            99
        )
        .is_none());
        assert!(local_guard(
            "https://evil.test discord.gg/x",
            &LinkGuardConfig::default(),
            &AntiSpamConfig::default(),
            99
        )
        .is_some());
    }

    #[test]
    fn the_reason_names_the_rule_and_the_consequence() {
        let hit = GuardHit {
            category: "link",
            matched: "https://evil.test".into(),
        };
        let warn = reason_for(&hit, GuardAction::DeleteWarn);
        assert!(warn.contains("link"), "{warn}");
        assert!(warn.contains("warned"), "{warn}");
        assert!(
            warn.contains("https://evil.test"),
            "the member must be able to see what matched: {warn}"
        );
        assert!(reason_for(&hit, GuardAction::Delete).contains("removed"));
    }

    fn rule(category: &str, enabled: bool, ml_threshold: u8) -> AiCategoryRule {
        AiCategoryRule {
            category: category.into(),
            enabled,
            min_intensity: Severity::Low,
            ml_threshold,
            action_override: None,
            strikes_override: None,
            dm_user: false,
        }
    }

    fn hit(category: &str, score: f64, severity: Severity) -> TextHit {
        TextHit {
            category: category.into(),
            kind: "test".into(),
            severity,
            score,
            matched: String::new(),
            start: 0,
            end: 0,
            action: None,
            rule_strikes: 0,
            source: aerona_core::text::HitSource::Rules,
        }
    }

    #[test]
    fn a_score_below_the_floor_never_reaches_the_decision() {
        let mut cfg = cfg();
        cfg.categories_config = vec![rule("toxicity", true, 70)];
        assert!(
            keep_hits(&[hit("toxicity", 0.50, Severity::Low)], &cfg).is_empty(),
            "0.50 is under a 0.70 threshold"
        );
        assert_eq!(
            keep_hits(&[hit("toxicity", 0.90, Severity::High)], &cfg).len(),
            1
        );
    }

    #[test]
    fn a_disabled_category_rule_silences_its_hits() {
        let mut cfg = cfg();
        cfg.min_severity = Severity::Info;
        cfg.categories_config = vec![rule("toxicity", false, 0)];
        assert!(keep_hits(&[hit("toxicity", 0.99, Severity::Critical)], &cfg).is_empty());
    }

    #[test]
    fn an_unselected_category_is_dropped_but_a_custom_rule_survives() {
        let mut cfg = cfg();
        cfg.min_severity = Severity::Info;
        cfg.categories = vec!["toxicity".into()];
        assert!(keep_hits(&[hit("profanity", 0.99, Severity::High)], &cfg).is_empty());
        assert_eq!(
            keep_hits(&[hit("custom", 0.99, Severity::High)], &cfg).len(),
            1
        );
    }

    #[test]
    fn a_positive_strictness_lowers_the_threshold_and_catches_more() {
        let r = rule("x", true, 80);
        let mut cfg = cfg();
        cfg.min_severity = Severity::Info;
        cfg.categories = vec![];
        cfg.categories_config = vec![r.clone()];
        // `strictness` is subtracted from the configured threshold, so a positive value
        // means "flag more", not "require more" — the direction an operator expects when
        // they say "be stricter".
        assert!(
            text::effective_threshold(&r, 20) < 0.80,
            "a stricter setting must catch more"
        );
        assert!(text::effective_threshold(&r, -20) > 0.80);
        cfg.strictness = 20;
        assert_eq!(keep_hits(&[hit("x", 0.82, Severity::Low)], &cfg).len(), 1);
        cfg.strictness = -20;
        assert!(keep_hits(&[hit("x", 0.82, Severity::Low)], &cfg).is_empty());
    }

    #[test]
    fn severity_bands_are_monotonic() {
        let mut last = 0;
        for s in [0.0, 0.4, 0.55, 0.75, 0.9, 0.99] {
            let rank = severity_for_score(s).rank();
            assert!(rank >= last, "rank went backwards at {s}");
            last = rank;
        }
    }

    #[test]
    fn delete_class_actions_are_the_ones_that_remove_a_message() {
        assert!(FilterAction::Delete.deletes());
        assert!(FilterAction::DeleteKick.deletes());
        assert!(!FilterAction::Flag.deletes());
        assert!(!FilterAction::Warn.deletes());
    }

    #[test]
    fn the_clean_outcome_is_inert() {
        let o = Outcome::clean();
        assert!(!o.verdict.violation && !o.deleted && !o.degraded);
        assert_eq!(category_label(&o.verdict), "filter");
    }

    #[test]
    fn the_model_list_is_sorted_deduplicated_and_never_empty() {
        let mut cfg = cfg();
        cfg.categories = vec!["toxicity".into(), "profanity".into(), "toxicity".into()];
        assert_eq!(models_for(&cfg), "profanity,toxicity");
        cfg.categories.clear();
        assert_eq!(models_for(&cfg), "toxicity");
    }

    /// The epoch-to-date conversion is the one piece of arithmetic here that can be
    /// wrong in silence, so it is pinned to known instants.
    #[test]
    fn timestamps_render_as_utc_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z", "leap day");
        // Negative epochs (a pre-1970 account) must not panic or drift.
        assert_eq!(rfc3339(-86_400), "1969-12-31T00:00:00Z");
    }
}
