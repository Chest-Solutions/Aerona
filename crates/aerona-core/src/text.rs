//! The AI text-moderation engine: custom rules, severity maths and the final
//! decision.
//!
//! This module is deliberately **pure** — no network, no database, no Discord.
//! Both `aerona-bot` (which must never slow down a message) and `aerona-api`
//! (which powers the dashboard's "test a sentence" box) call the same function,
//! so what you test in the dashboard is provably what the bot does in production.
//!
//! Severity model, end to end:
//!
//! | signal | score | severity |
//! |---|---|---|
//! | rule-based `low` | 0.35 | Low |
//! | rule-based `medium` | 0.60 | Medium |
//! | rule-based `high` / `extreme` | 0.85 | High |
//! | ML confidence `c` | `c` | ≥90 → Critical, ≥75 → High, ≥50 → Medium, ≥25 → Low |
//!
//! `strictness` shifts every threshold at once so an owner can go strict or
//! relaxed with one control instead of retuning twelve categories.
//!
//! A hit is *actionable* only when it clears **both** the per-category severity
//! floor and the guild-wide `min_severity`; `decide` then picks the action,
//! applies strikes, and escalates if the strike ledger is exhausted.

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::models::{
    AiCategoryRule, AiConfig, AiCustomMode, FilterAction, FilterVerdict, SeResponse, Severity,
};

/// Score attributed to each rule-based intensity label.
pub const INTENSITY_LOW: f64 = 0.35;
pub const INTENSITY_MEDIUM: f64 = 0.60;
pub const INTENSITY_HIGH: f64 = 0.85;

/// One matched piece of text, from either engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextHit {
    /// `profanity`, `spam`, `custom`, `toxic`, …
    pub category: String,
    /// Sub-type reported by the provider (`sexual`, `insulting`…) or the rule description.
    pub kind: String,
    pub severity: Severity,
    pub score: f64,
    /// The matched literal text. Empty when the guild enabled `privacy_redact`.
    pub matched: String,
    /// Byte range in the original message.
    pub start: usize,
    /// The action the matching *custom rule* asked for. `None` for provider hits,
    /// where the category config is the only opinion available. `#[serde(default)]`
    /// so a verdict cached before this field existed still deserialises.
    #[serde(default)]
    pub action: Option<FilterAction>,
    /// Strikes the matching rule requested (0 for provider hits, which means the
    /// ordinary one).
    #[serde(default)]
    pub rule_strikes: u8,
    pub end: usize,
    pub source: HitSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HitSource {
    /// Sightengine rule-based category match.
    Rules,
    /// Sightengine ML class above the category threshold.
    Ml,
    /// Local custom rule (no API call needed).
    Custom,
}

/// Outcome of running a message through the engine.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Nothing to check (too short, whitelisted, feature off…) — never logged.
    Skipped(&'static str),
    /// Checked and clean.
    Clean,
    /// Checked and actionable.
    Violation(Vec<TextHit>),
    /// Provider error / no credentials. `message` is operator-facing.
    Failed(String),
}

impl Outcome {
    pub fn is_violation(&self) -> bool {
        matches!(self, Outcome::Violation(_))
    }
}

/// Request body for `POST /api/v1/guilds/{id}/ai-filter/test`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FilterTestRequest {
    pub text: String,
    /// Test against unsaved edits: send the config you are looking at.
    #[serde(default)]
    pub config: Option<AiConfig>,
    /// `en`, `fr`, … forwarded to the provider.
    #[serde(default)]
    pub lang: String,
    /// Simulate this user's strike ledger.
    #[serde(default)]
    pub as_user: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FilterTestResponse {
    pub verdict: FilterVerdict,
    pub hits: Vec<TextHit>,
    /// What the engine would have sent to the provider.
    pub would_call: Option<ProviderCall>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderCall {
    pub url: &'static str,
    pub method: &'static str,
    pub fields: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// Severity maths
// ---------------------------------------------------------------------------

/// Shift applied to thresholds by `strictness` (-5..=5).
pub fn strictness_offset(strictness: i8) -> f64 {
    (strictness.clamp(-5, 5) as f64) * 0.04
}

/// Effective score floor for a category, in `0.0..=1.0`. Clamped so a hostile or
/// mistyped config can never make everything (or nothing) a violation.
pub fn effective_threshold(rule: &AiCategoryRule, strictness: i8) -> f64 {
    let base = f64::from(rule.ml_threshold) / 100.0;
    (base - strictness_offset(strictness)).clamp(0.05, 0.99)
}

pub fn severity_from_intensity(intensity: &str) -> Severity {
    match intensity.trim().to_ascii_lowercase().as_str() {
        "low" | "minor" => Severity::Low,
        "medium" | "moderate" => Severity::Medium,
        "high" | "strong" | "extreme" | "severe" => Severity::High,
        // Unlabelled/unknown: treat as the floor the operator asked for rather
        // than inventing severity we cannot justify.
        _ => Severity::Medium,
    }
}

pub fn score_from_intensity(intensity: &str) -> f64 {
    match intensity.trim().to_ascii_lowercase().as_str() {
        "low" | "minor" => INTENSITY_LOW,
        "medium" | "moderate" => INTENSITY_MEDIUM,
        _ => INTENSITY_HIGH,
    }
}

pub fn severity_from_score(score: f64) -> Severity {
    if score >= 0.90 {
        Severity::Critical
    } else if score >= 0.75 {
        Severity::High
    } else if score >= 0.50 {
        Severity::Medium
    } else if score >= 0.25 {
        Severity::Low
    } else {
        Severity::Info
    }
}

/// Severity of a provider match: intensity label drives the label, score drives
/// escalation so a `high`-intensity `0.99` ML hit can reach `critical`.
pub fn severity_for(intensity: &str, score: f64) -> Severity {
    severity_from_intensity(intensity).max(severity_from_score(score))
}

// ---------------------------------------------------------------------------
// Custom (local) rules
// ---------------------------------------------------------------------------

/// A compiled custom rule. Compiled once per config load, never per message.
#[derive(Clone)]
pub struct CompiledRule {
    pub kind: AiCustomMode,
    pub regex: Option<Regex>,
    /// Lowercased needle for `contains` / `whole_word` fast paths.
    pub needle: String,
    pub category: String,
    pub severity: Severity,
    pub action: FilterAction,
    pub strikes: u8,
    pub description: String,
}

impl std::fmt::Debug for CompiledRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledRule")
            .field("kind", &self.kind)
            .field("category", &self.category)
            .field("severity", &self.severity)
            .field("needle_len", &self.needle.len())
            .finish_non_exhaustive()
    }
}

/// Rejection reasons surfaced to the dashboard when a rule cannot be saved.
pub fn validate_pattern(pattern: &str, mode: AiCustomMode) -> Result<(), String> {
    if pattern.chars().count() > 512 {
        return Err("patterns are limited to 512 characters".into());
    }
    match mode {
        AiCustomMode::Regex => {
            // Reject the obvious ReDoS shapes before the regex engine ever sees
            // them: nested quantifiers are catastrophic backtracking in any
            // non-restartable engine, and our size-limited text does not excuse it.
            let p = pattern.trim();
            if p.contains("(?=") || p.contains("?!") || p.contains("(?<") {
                return Err(
                    "lookaround is not supported by the Rust regex engine; rewrite without it"
                        .into(),
                );
            }
            // Classic catastrophic shapes: `(a+)+`, `(a*)*`, `(a|b)*+` … A bounded
            // repetition like `\d{1,3}` must stay legal.
            let Ok(nested) = Regex::new(r"(?:\+|\*|\)|\})\s*(?:\+|\*|\{\d+,\s*\})") else {
                return Err(
                    "the safety checker itself failed to build; contact the operator".into(),
                );
            };
            if nested.is_match(p) {
                return Err(
                    "nested quantifiers can hang the matcher; anchor or simplify the pattern"
                        .into(),
                );
            }
            RegexBuilder::new(p)
                .case_insensitive(true)
                .size_limit(1 << 20)
                .dfa_size_limit(1 << 20)
                .build()
                .map_err(|e| format!("invalid regex: {}", short(&e.to_string())))?;
        }
        AiCustomMode::WholeWord | AiCustomMode::Contains | AiCustomMode::Whitelist => {
            if pattern.trim().is_empty() {
                return Err("pattern is empty".into());
            }
            if pattern
                .chars()
                .any(|c| c == '\u{0}' || c.is_control() && c != '\t')
            {
                return Err("patterns cannot contain control characters".into());
            }
        }
    }
    Ok(())
}

pub fn compile_rules(rules: &[crate::models::AiCustomRule]) -> Vec<CompiledRule> {
    rules
        .iter()
        .filter(|r| r.enabled)
        .filter_map(|r| {
            let regex = match r.mode {
                AiCustomMode::Regex => RegexBuilder::new(r.pattern.trim())
                    .case_insensitive(true)
                    .size_limit(1 << 20)
                    .dfa_size_limit(1 << 20)
                    .build()
                    .ok(),
                AiCustomMode::WholeWord => RegexBuilder::new(&regex::escape(r.pattern.trim()))
                    .case_insensitive(true)
                    .size_limit(1 << 20)
                    .build()
                    .ok(),
                AiCustomMode::Contains | AiCustomMode::Whitelist => None,
            };
            // An enabled regex rule that fails to compile must never silently
            // become "match everything" (None → fallthrough). We drop it and the
            // API already refuses to save invalid patterns.
            if matches!(r.mode, AiCustomMode::Regex | AiCustomMode::WholeWord) && regex.is_none() {
                return None;
            }
            Some(CompiledRule {
                kind: r.mode,
                regex,
                needle: r.pattern.trim().to_lowercase(),
                category: if r.category.is_empty() {
                    "custom".into()
                } else {
                    r.category.clone()
                },
                severity: r.severity,
                action: r.action,
                strikes: r.strikes,
                description: r.description.clone(),
            })
        })
        .collect()
}

/// Run local rules over the text. Returns `(whitelisted, hits)`: a whitelist hit
/// suppresses all *custom* hits (provider results are still evaluated, because a
/// whitelist entry is a false-positive guard for the local list, not a licence).
pub fn scan_custom(text: &str, rules: &[CompiledRule]) -> (bool, Vec<TextHit>) {
    let mut whitelisted = false;
    let mut hits = Vec::new();
    let lower = text.to_lowercase();
    for r in rules {
        if r.needle.is_empty() && matches!(r.kind, AiCustomMode::Contains | AiCustomMode::Whitelist)
        {
            continue; // an empty needle would "match" every message
        }
        let found: Option<(usize, usize)> = match r.kind {
            AiCustomMode::Contains | AiCustomMode::Whitelist => {
                lower.find(&r.needle).map(|i| (i, i + r.needle.len()))
            }
            AiCustomMode::WholeWord => r
                .regex
                .as_ref()
                .and_then(|re| re.find(text).map(|m| (m.start(), m.end()))),
            AiCustomMode::Regex => r
                .regex
                .as_ref()
                .and_then(|re| re.find(text).map(|m| (m.start(), m.end()))),
        };
        let Some((start, end)) = found else { continue };
        if r.kind == AiCustomMode::Whitelist {
            whitelisted = true;
            continue;
        }
        hits.push(TextHit {
            category: r.category.clone(),
            kind: if r.description.is_empty() {
                "custom".into()
            } else {
                r.description.clone()
            },
            severity: r.severity,
            // Custom rules are absolute: the operator wrote the list, so a hit is
            // a hit. Score pinned to the top of the band so thresholds cannot veto it.
            score: 1.0,
            matched: text[start.min(text.len())..end.min(text.len())].to_string(),
            start,
            end,
            source: HitSource::Custom,
            action: Some(r.action),
            rule_strikes: r.strikes,
        });
    }
    (whitelisted, hits)
}

/// `**ee you` — mask a violation before it is stored or echoed.
pub fn redact(text: &str, hits: &[TextHit]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let mut sorted: Vec<&TextHit> = hits.iter().collect();
    sorted.sort_by_key(|h| h.start);
    for h in sorted {
        let s = h.start.min(text.len());
        let e = h.end.clamp(s, text.len());
        if s > cursor {
            out.push_str(&text[cursor..s]);
        }
        let span: String = text[s..e]
            .chars()
            .map(|c| if c.is_whitespace() { c } else { '*' })
            .collect();
        out.push_str(&span);
        cursor = cursor.max(e);
    }
    out.push_str(&text[cursor.min(text.len())..]);
    out
}

// ---------------------------------------------------------------------------
// Provider response → hits
// ---------------------------------------------------------------------------

/// Turn a Sightengine `text/check.json` payload into hits, honouring the
/// per-category floors.
pub fn hits_from_provider(resp: &SeResponse, config: &AiConfig) -> Vec<TextHit> {
    let mut hits = Vec::new();
    let enabled = |cat: &str| -> bool {
        config.categories.is_empty() || config.categories.iter().any(|c| c == cat)
    };
    let rule_for = |cat: &str| -> AiCategoryRule {
        config
            .categories_config
            .iter()
            .find(|c| c.category == cat)
            .cloned()
            .unwrap_or_else(|| AiCategoryRule {
                category: cat.into(),
                ..Default::default()
            })
    };

    for cat in crate::models::AI_CATEGORIES {
        if !enabled(cat) {
            continue;
        }
        let rule = rule_for(cat);
        if !rule.enabled {
            continue;
        }
        for m in resp.matches_for(cat) {
            let score = score_from_intensity(&m.intensity);
            let sev = severity_for(&m.intensity, score);
            // Rule-based matches are graded by the provider's own intensity label;
            // only that ladder applies here (the numeric threshold is the ML dial).
            if sev < rule.min_intensity {
                continue;
            }
            hits.push(TextHit {
                category: cat.to_string(),
                kind: if m.r#type.is_empty() {
                    cat.to_string()
                } else {
                    m.r#type.clone()
                },
                severity: sev,
                score,
                matched: m.match_.clone(),
                start: m.start,
                end: m.end,
                source: HitSource::Rules,
                action: None,
                rule_strikes: 0,
            });
        }
    }

    if config.mode.contains("ml") {
        if let Some(ml) = &resp.moderation_classes {
            for class in crate::models::ML_CLASSES {
                let rule = rule_for(class);
                if !rule.enabled {
                    continue;
                }
                let score = ml.score_for(class);
                if score < effective_threshold(&rule, config.strictness) {
                    continue;
                }
                hits.push(TextHit {
                    category: class.to_string(),
                    kind: "ml".into(),
                    severity: severity_for("high", score),
                    score,
                    matched: String::new(),
                    start: 0,
                    end: 0,
                    source: HitSource::Ml,
                    action: None,
                    rule_strikes: 0,
                });
            }
        }
    }
    hits
}

// ---------------------------------------------------------------------------
// Decision
// ---------------------------------------------------------------------------

/// Full decision. `strikes_before` is the author's current strike count so the
/// escalation edge (`== max`) can be computed without touching the database here.
pub fn decide(
    config: &AiConfig,
    _text: &str,
    hits: Vec<TextHit>,
    strikes_before: u8,
) -> FilterVerdict {
    let empty = FilterVerdict {
        violation: false,
        severity: Severity::Info,
        score: 0.0,
        categories: vec![],
        matched: vec![],
        action: FilterAction::Flag,
        strikes: strikes_before,
        strikes_max: config.strikes_max,
        reason: "no violations".into(),
        latency_ms: 0,
        degraded: false,
    };
    if hits.is_empty() {
        return empty;
    }

    let worst = hits
        .iter()
        .max_by(|a, b| {
            a.severity.cmp(&b.severity).then(
                a.score
                    .partial_cmp(&b.score)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        })
        .cloned()
        .expect("non-empty");

    if worst.severity < config.min_severity {
        return FilterVerdict {
            reason: format!(
                "detected {} (severity {}) below the minimum of {}",
                worst.category,
                worst.severity.as_str(),
                config.min_severity.as_str()
            ),
            severity: worst.severity,
            score: worst.score,
            ..empty
        };
    }

    let category_rule = config
        .categories_config
        .iter()
        .find(|c| c.category == worst.category)
        .cloned();
    // A custom rule's own action wins over the category default: the operator wrote
    // that pattern for a reason. `Flag` is "no opinion", so a rule that merely
    // matches must never *downgrade* a category that says delete.
    let rule_action = worst.action.filter(|a| *a != FilterAction::Flag);
    let action = rule_action
        .or_else(|| category_rule.as_ref().and_then(|r| r.action_override))
        .unwrap_or_else(|| default_action(config));

    let mut strikes = strikes_before;
    let mut action = action;
    let escalating = config.strikes_enabled && action.strikes();
    if escalating {
        // One strike normally; a rule that asked for more (a particularly bad list)
        // gets what it asked for.
        strikes = strikes.saturating_add(worst.rule_strikes.max(1));
        if config.strikes_max > 0 && strikes >= config.strikes_max {
            action = if config.ban_on_strikes {
                FilterAction::DeleteBan
            } else {
                FilterAction::Delete
            };
        }
    }

    FilterVerdict {
        violation: true,
        severity: worst.severity,
        score: worst.score,
        categories: {
            let mut v: Vec<String> = hits.iter().map(|h| h.category.clone()).collect();
            v.sort();
            v.dedup();
            v
        },
        matched: hits
            .iter()
            .map(|h| h.matched.clone())
            .filter(|s| !s.is_empty())
            .collect(),
        action,
        strikes,
        strikes_max: config.strikes_max,
        reason: format!(
            "{} hit at severity {} (score {:.2})",
            worst.category,
            worst.severity.as_str(),
            worst.score
        ),
        latency_ms: 0,
        degraded: false,
    }
}

fn default_action(config: &AiConfig) -> FilterAction {
    if config.warn_on_hit {
        FilterAction::Warn
    } else if config.auto_delete {
        if config.strikes_enabled {
            FilterAction::DeleteStrike
        } else {
            FilterAction::Delete
        }
    } else {
        FilterAction::Flag
    }
}

/// `rules,ml` → the provider `mode` field. Whitelisted to the documented values.
pub fn sanitize_mode(raw: &str) -> String {
    let parts: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|p| matches!(*p, "rules" | "ml" | "username"))
        .collect();
    if parts.is_empty() {
        "rules".into()
    } else {
        parts.join(",")
    }
}

/// Categories the provider accepts, filtered and joined.
pub fn sanitize_categories(raw: &[String]) -> String {
    let mut kept: Vec<&str> = raw
        .iter()
        .map(|c| c.trim())
        .filter(|c| crate::models::AI_CATEGORIES.contains(c))
        .collect();
    kept.sort_unstable();
    kept.dedup();
    if kept.is_empty() {
        crate::models::AI_CATEGORIES[..3].join(",")
    } else {
        kept.join(",")
    }
}

pub fn sanitize_lang(raw: &str) -> String {
    let mut kept: Vec<String> = raw
        .split(',')
        .map(|l| l.trim().to_ascii_lowercase())
        .filter(|l| (2..=5).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphabetic()))
        .collect();
    kept.sort();
    kept.dedup();
    kept.truncate(8);
    if kept.is_empty() {
        "en".into()
    } else {
        kept.join(",")
    }
}

fn short(s: &str) -> String {
    crate::security::scrub(s, 160)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AiCategoryRule, AiCustomRule};

    fn cfg() -> AiConfig {
        AiConfig {
            enabled: true,
            mode: "rules".into(),
            categories: vec!["profanity".into()],
            min_severity: Severity::Medium,
            auto_delete: true,
            dm_user: true,
            warn_on_hit: false,
            strikes_enabled: true,
            strikes_max: 3,
            ban_on_strikes: false,
            timeout_minutes: 60,
            strictness: 0,
            ..Default::default()
        }
    }

    fn resp(profanity_json: &str) -> SeResponse {
        let body = format!(r#"{{"status":"success","profanity":{{"matches":{profanity_json}}}}}"#);
        serde_json::from_str(&body).expect("valid test payload")
    }

    #[test]
    fn severity_bands_and_strictness() {
        assert_eq!(severity_from_intensity("high"), Severity::High);
        assert_eq!(severity_from_intensity("LOW"), Severity::Low);
        assert_eq!(severity_from_intensity("nonsense"), Severity::Medium);
        assert_eq!(severity_from_score(0.95), Severity::Critical);
        assert_eq!(severity_from_score(0.76), Severity::High);
        assert_eq!(severity_from_score(0.51), Severity::Medium);
        assert_eq!(severity_from_score(0.26), Severity::Low);
        assert_eq!(severity_from_score(0.1), Severity::Info);
        assert_eq!(
            severity_for("low", 0.95),
            Severity::Critical,
            "score must be able to escalate"
        );
        assert_eq!(
            severity_for("high", 0.10),
            Severity::High,
            "intensity is not discarded"
        );

        let rule = AiCategoryRule {
            ml_threshold: 80,
            ..Default::default()
        };
        let base = effective_threshold(&rule, 0);
        assert!(
            effective_threshold(&rule, 5) < base,
            "strict lowers the floor"
        );
        assert!(
            effective_threshold(&rule, -5) > base,
            "relaxed raises the floor"
        );
        assert!(
            (0.05..=0.99).contains(&effective_threshold(&rule, 5)),
            "clamped"
        );
        let zero = AiCategoryRule {
            ml_threshold: 0,
            ..Default::default()
        };
        assert!(
            effective_threshold(&zero, 0) >= 0.05,
            "never a free-for-all"
        );
        let hundred = AiCategoryRule {
            ml_threshold: 100,
            ..Default::default()
        };
        assert!(effective_threshold(&hundred, 0) <= 0.99, "never impossible");
    }

    #[test]
    fn a_rule_chooses_its_own_action_and_the_provider_does_not() {
        let base = AiConfig {
            enabled: true,
            strikes_enabled: true,
            strikes_max: 3,
            min_severity: Severity::Low,
            ..Default::default()
        };
        let hit = |action: Option<FilterAction>, strikes: u8| TextHit {
            category: "custom".into(),
            kind: "rule".into(),
            severity: Severity::High,
            score: 1.0,
            matched: "x".into(),
            start: 0,
            end: 1,
            source: HitSource::Custom,
            action,
            rule_strikes: strikes,
        };

        // A rule that says `delete_kick` is obeyed, not overridden by the default.
        let v = decide(&base, "x", vec![hit(Some(FilterAction::DeleteKick), 0)], 0);
        assert_eq!(v.action, FilterAction::DeleteKick);
        assert_eq!(
            v.strikes, 1,
            "kick/ban/sync are all strike actions in this model"
        );
        // `Flag` from a rule means "no opinion" and must not downgrade a category.
        let with_override = AiConfig {
            categories_config: vec![AiCategoryRule {
                category: "custom".into(),
                action_override: Some(FilterAction::Delete),
                ..Default::default()
            }],
            ..base.clone()
        };
        let v = decide(
            &with_override,
            "x",
            vec![hit(Some(FilterAction::Flag), 0)],
            0,
        );
        assert_eq!(
            v.action,
            FilterAction::Delete,
            "a flagging rule cannot weaken the category"
        );
        // Strike counts come from the rule when it asked for more than one.
        let v = decide(
            &base,
            "x",
            vec![hit(Some(FilterAction::DeleteStrike), 2)],
            1,
        );
        assert_eq!(v.strikes, 3, "one existing + two requested");
        let v = decide(&base, "x", vec![hit(Some(FilterAction::Warn), 0)], 0);
        assert_eq!(v.strikes, 0, "warn does not touch the ledger");
        // A provider hit has no rule opinion at all.
        let v = decide(
            &base,
            "x",
            vec![TextHit {
                source: HitSource::Ml,
                action: None,
                ..hit(None, 0)
            }],
            0,
        );
        assert_eq!(
            v.action,
            default_action(&base),
            "the config decides when the rule says nothing"
        );
        assert_eq!(
            v.strikes,
            if v.action.strikes() { 1 } else { 0 },
            "one strike for an escalating default"
        );
    }

    #[test]
    fn custom_rules_match_and_whitelist() {
        let rules = compile_rules(&[
            AiCustomRule {
                pattern: "shut up".into(),
                mode: AiCustomMode::WholeWord,
                category: "insult".into(),
                severity: Severity::Medium,
                action: FilterAction::Delete,
                strikes: 1,
                description: "don't tell people to shut up".into(),
                enabled: true,
                ..Default::default()
            },
            AiCustomRule {
                pattern: "shutup".into(),
                mode: AiCustomMode::Whitelist,
                enabled: true,
                ..Default::default()
            },
            AiCustomRule {
                pattern: r"giveaway\s*\d+".into(),
                mode: AiCustomMode::Regex,
                category: "spam".into(),
                severity: Severity::High,
                enabled: true,
                ..Default::default()
            },
        ]);
        assert_eq!(rules.len(), 3, "all three enabled rules must compile");
        let (wl, hits) = scan_custom("please SHUT UP and enter giveaway 42", &rules);
        assert!(!wl);
        assert_eq!(hits.len(), 2, "got {hits:?}");
        assert_eq!(hits[0].category, "insult");
        assert_eq!(hits[1].category, "spam");

        // A whitelist needle matches as a substring on purpose: its whole job is
        // to say "this string is fine here", so `I will shutup` is exempted too.
        for probe in ["shutup", "I will shutup"] {
            let (wl, hits) = scan_custom(probe, &rules);
            assert!(wl, "{probe:?} must trip the whitelist");
            assert!(
                hits.is_empty(),
                "a whitelisted message must not also fire a violation: {hits:?}"
            );
        }
        // Whole-word must not fire inside a longer word…
        let (_, hits4) = scan_custom("do not shutup now", &rules);
        assert!(
            hits4.iter().all(|h| h.category != "insult"),
            "substring leaked into whole-word: {hits4:?}"
        );
        // …but case-insensitivity must hold for real messages.
        let (_, hits5) = scan_custom("SHUT UP", &rules);
        assert_eq!(hits5.len(), 1, "case-insensitive whole-word");
    }

    #[test]
    fn disabled_rules_are_ignored() {
        let rules = compile_rules(&[AiCustomRule {
            pattern: "nope".into(),
            mode: AiCustomMode::Contains,
            enabled: false,
            ..Default::default()
        }]);
        assert!(rules.is_empty());
        let (_, hits) = scan_custom("nope", &rules);
        assert!(hits.is_empty());
    }

    #[test]
    fn broken_regex_is_never_treated_as_match_all() {
        let rules = compile_rules(&[AiCustomRule {
            pattern: "[unclosed".into(),
            mode: AiCustomMode::Regex,
            severity: Severity::Critical,
            enabled: true,
            ..Default::default()
        }]);
        assert!(
            rules.is_empty(),
            "uncompilable rule must be dropped, not widened"
        );
    }

    #[test]
    fn pattern_validation_rejects_dangerous_shapes() {
        assert!(
            validate_pattern("(a+)+$", AiCustomMode::Regex).is_err(),
            "nested quantifier"
        );
        assert!(
            validate_pattern(r"(?=foo)bar", AiCustomMode::Regex).is_err(),
            "lookahead"
        );
        assert!(
            validate_pattern("", AiCustomMode::Contains).is_err(),
            "empty"
        );
        assert!(
            validate_pattern(&"x".repeat(600), AiCustomMode::Contains).is_err(),
            "oversize"
        );
        assert!(validate_pattern(r"\bfoo\d{1,3}\b", AiCustomMode::Regex).is_ok());
        assert!(validate_pattern("normal words", AiCustomMode::WholeWord).is_ok());
    }

    #[test]
    fn redaction_masks_only_the_hit() {
        let hits = vec![TextHit {
            category: "profanity".into(),
            kind: "sexual".into(),
            severity: Severity::Medium,
            score: 0.6,
            matched: "damn".into(),
            start: 6,
            end: 10,
            source: HitSource::Custom,
            action: None,
            rule_strikes: 0,
        }];
        assert_eq!(redact("oh no damn it", &hits), "oh no **** it");
        assert_eq!(redact("nothing here", &[]), "nothing here");
    }

    #[test]
    fn provider_hits_respect_category_floors() {
        let mut c = cfg();
        c.categories_config = vec![AiCategoryRule {
            category: "profanity".into(),
            enabled: true,
            min_intensity: Severity::High,
            ml_threshold: 90,
            ..Default::default()
        }];
        // medium intensity is below the High floor → no hit at all.
        let r = resp(r#"[{"type":"sexual","intensity":"medium","match":"s*x","start":0,"end":3}]"#);
        assert!(hits_from_provider(&r, &c).is_empty(), "floor ignored");
        let r2 = resp(r#"[{"type":"sexual","intensity":"high","match":"s*x","start":0,"end":3}]"#);
        let hits = hits_from_provider(&r2, &c);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].severity, Severity::High);
        assert_eq!(hits[0].matched, "s*x");
        // Disabled category → nothing.
        c.categories_config[0].enabled = false;
        assert!(hits_from_provider(&r2, &c).is_empty());
    }

    #[test]
    fn ml_classes_only_count_in_ml_mode() {
        let raw = r#"{"status":"success","moderation_classes":{"sexual":0.9,"discriminatory":0.1,"insulting":0.95,"violent":0.0,"toxic":0.8,"self-harm":0.0}}"#;
        let r: SeResponse = serde_json::from_str(raw).unwrap();
        let mut c = cfg();
        c.mode = "rules".into();
        assert!(
            hits_from_provider(&r, &c).is_empty(),
            "rules-only mode must ignore ML scores"
        );
        c.mode = "rules,ml".into();
        c.categories = vec![];
        let hits = hits_from_provider(&r, &c);
        let cats: Vec<&str> = hits.iter().map(|h| h.category.as_str()).collect();
        assert!(cats.contains(&"insulting"), "{cats:?}");
        assert!(
            !cats.contains(&"discriminatory"),
            "0.1 is below the 80 default floor"
        );
    }

    #[test]
    fn decide_applies_guild_floor_and_actions() {
        let c = cfg();
        let hits = vec![TextHit {
            category: "profanity".into(),
            kind: "sexual".into(),
            severity: Severity::Medium,
            score: 0.6,
            matched: "damn".into(),
            start: 0,
            end: 4,
            source: HitSource::Rules,
            action: None,
            rule_strikes: 0,
        }];

        let v = decide(&c, "damn this", hits.clone(), 0);
        assert!(v.violation);
        assert_eq!(v.action, FilterAction::DeleteStrike);
        assert_eq!(v.strikes, 1, "strike ledger advanced");
        assert!(v.reason.contains("profanity"), "{}", v.reason);

        // Below the guild floor → clean, but the operator is told why.
        let mut strict = cfg();
        strict.min_severity = Severity::Critical;
        let v2 = decide(&strict, "damn this", hits.clone(), 0);
        assert!(!v2.violation);
        assert_eq!(
            v2.severity,
            Severity::Medium,
            "still reported for transparency"
        );
        assert!(v2.reason.contains("below the minimum"), "{}", v2.reason);

        // Flag mode when the owner asked for no deletion.
        let mut flag = cfg();
        flag.auto_delete = false;
        flag.strikes_enabled = false;
        let v3 = decide(&flag, "x", hits.clone(), 2);
        assert_eq!(v3.action, FilterAction::Flag);
        assert_eq!(v3.strikes, 2, "strikes must not move when disabled");
    }

    #[test]
    fn strikes_escalate_exactly_at_the_limit() {
        let mut c = cfg();
        c.strikes_max = 3;
        let hit = |sev: Severity| {
            vec![TextHit {
                category: "spam".into(),
                kind: "spam".into(),
                severity: sev,
                score: 0.9,
                matched: String::new(),
                start: 0,
                end: 0,
                source: HitSource::Ml,
                action: None,
                rule_strikes: 0,
            }]
        };
        assert_eq!(decide(&c, "x", hit(Severity::High), 1).strikes, 2);
        assert_eq!(
            decide(&c, "x", hit(Severity::High), 1).action,
            FilterAction::DeleteStrike
        );
        let last = decide(&c, "x", hit(Severity::High), 2);
        assert_eq!(last.strikes, 3);
        assert_eq!(
            last.action,
            FilterAction::Delete,
            "limit reached → escalate once"
        );
        c.ban_on_strikes = true;
        assert_eq!(
            decide(&c, "x", hit(Severity::High), 2).action,
            FilterAction::DeleteBan
        );
        // Strikes are saturating: 255 cannot overflow into 0.
        assert_eq!(
            decide(&c, "x", hit(Severity::High), u8::MAX).strikes,
            u8::MAX
        );
    }

    #[test]
    fn per_category_action_override_wins() {
        let mut c = cfg();
        c.categories_config = vec![AiCategoryRule {
            category: "link".into(),
            enabled: true,
            min_intensity: Severity::Low,
            ml_threshold: 50,
            action_override: Some(FilterAction::Flag),
            ..Default::default()
        }];
        let hits = vec![TextHit {
            category: "link".into(),
            kind: "url".into(),
            severity: Severity::High,
            score: 0.85,
            matched: "http://x".into(),
            start: 0,
            end: 8,
            source: HitSource::Rules,
            action: None,
            rule_strikes: 0,
        }];
        assert_eq!(decide(&c, "http://x", hits, 0).action, FilterAction::Flag);
    }

    #[test]
    fn empty_hits_are_clean() {
        let v = decide(&cfg(), "hello", vec![], 7);
        assert!(!v.violation);
        assert_eq!(v.severity, Severity::Info);
        assert_eq!(v.strikes, 7, "clean messages must not change the ledger");
    }

    #[test]
    fn provider_input_is_sanitized() {
        assert_eq!(sanitize_mode("ml"), "ml");
        assert_eq!(sanitize_mode("rules,ml"), "rules,ml");
        assert_eq!(
            sanitize_mode("rules; rm -rf /"),
            "rules",
            "unknown modes dropped"
        );
        assert_eq!(sanitize_mode("everything"), "rules", "falls back safely");
        assert_eq!(
            sanitize_categories(&["profanity".into(), "nope".into(), "link".into()]),
            "link,profanity"
        );
        assert_eq!(sanitize_categories(&[]), "profanity,personal,link");
        assert_eq!(
            sanitize_lang("EN,fr, ; DROP"),
            "en,fr",
            "only language tags survive"
        );
        assert_eq!(sanitize_lang(""), "en");
        assert_eq!(
            sanitize_lang(&"en,".repeat(50)),
            "en",
            "duplicates collapse"
        );
        assert_eq!(
            sanitize_lang("de,fr,en"),
            "de,en,fr",
            "normalized order, max 8"
        );
        assert_eq!(
            sanitize_lang("aa,bb,cc,dd,ee,ff,gg,hh,ii,jj"),
            "aa,bb,cc,dd,ee,ff,gg,hh",
            "capped at 8"
        );
    }

    #[test]
    fn test_request_accepts_partial_config() {
        let req: FilterTestRequest = serde_json::from_str(r#"{"text":"hi","lang":"en"}"#).unwrap();
        assert_eq!(req.text, "hi");
        assert!(req.config.is_none(), "absent means 'use what is saved'");
        assert!(req.as_user.is_none());
    }
}

// ---------------------------------------------------------------------------
// Local guards: links, invites, message shape
// ---------------------------------------------------------------------------
//
// These run without a provider and without a score. The reason they are here, in the pure
// module, is the same as the reason `decide` is: the dashboard's preview and the bot's
// message handler must not be allowed to have two ideas about what "a link" means.

/// One thing a guard disliked. There is no score and no offset, because there is nothing to
/// redact and nothing to weight: the rule either applies to this message or it does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardHit {
    /// `link` or `invite`. Also the string that shows up in the mod-log embed, so changing
    /// it is a user-visible change.
    pub category: &'static str,
    /// What matched, clamped for display.
    pub matched: String,
}

/// A URL with a scheme, or a `www.` host. A bare `example.com` in prose is *not* a link
/// here: matching one would flag file names, domains inside code blocks and half of GitHub
/// chat, and an operator who wants that can add a custom rule for it.
fn link_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:https?://|www\.)[^\s<>\)\]]+")
            .expect("the link pattern is a fixed literal")
    })
}

fn invite_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)\b(?:discord\.gg|dsc\.gg|discord\.link|discordapp\.com/invite|discord\.com/invite)",
            r"/[a-z0-9-]+",
            r"|\bdiscord(?:app)?\.com/servers/[^\s<>\)\]]+"
        ))
        .expect("the invite pattern is a fixed literal")
    })
}

pub fn find_links(text: &str) -> Vec<GuardHit> {
    link_re()
        .find_iter(text)
        .map(|m| GuardHit {
            category: "link",
            matched: clamp(&m.as_str().to_lowercase()),
        })
        .collect()
}

pub fn find_invites(text: &str) -> Vec<GuardHit> {
    invite_re()
        .find_iter(text)
        .map(|m| GuardHit {
            category: "invite",
            matched: clamp(&m.as_str().to_lowercase()),
        })
        .collect()
}

/// The host of a matched URL, without a scheme, lowercased.
fn host_of(matched: &str) -> String {
    let rest = matched
        .strip_prefix("http://")
        .or_else(|| matched.strip_prefix("https://"))
        .unwrap_or(matched);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    host.split('@').next_back().unwrap_or(host).to_lowercase()
}

/// Suffix match on label boundaries, so allowing `example.com` covers `a.b.example.com` and
/// does not cover `evil-example.com`. That second property is the whole point.
pub fn host_allowed(host: &str, allowed: &[String]) -> bool {
    let host = host.trim_end_matches('.').to_lowercase();
    allowed.iter().any(|a| {
        let a = a
            .trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_start_matches("www.")
            .trim_end_matches('/')
            .trim_end_matches('.')
            .to_lowercase();
        !a.is_empty() && (host == a || host.ends_with(&format!(".{a}")))
    })
}

/// Every link the guard should still complain about, after the allowlist. Invites are
/// checked *before* the allowlist is applied: a server that allows `discord.com` for its
/// docs link must not thereby allow every invite on it.
pub fn link_violations(
    text: &str,
    block_links: bool,
    block_invites: bool,
    allowed: &[String],
) -> Vec<GuardHit> {
    let mut out = Vec::new();
    if block_invites {
        out.extend(find_invites(text));
    }
    if block_links {
        for mut hit in find_links(text) {
            let host = host_of(&hit.matched);
            if host_allowed(&host, allowed) {
                continue;
            }
            hit.matched = host;
            out.push(hit);
        }
    }
    out
}

pub fn caps_percent(text: &str) -> u8 {
    let (mut upper, mut alpha) = (0u32, 0u32);
    for c in text.chars() {
        if c.is_alphabetic() {
            alpha += 1;
            if c.is_uppercase() {
                upper += 1;
            }
        }
    }
    if alpha == 0 {
        return 0;
    }
    ((upper as f64 / alpha as f64) * 100.0).round() as u8
}

pub fn count_mentions(text: &str) -> u32 {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"<@[!&]?\d{1,25}>").expect("fixed literal"));
    // `@everyone` and `@here` are the ping a member cannot opt out of, so each counts as
    // one mention even though no id is involved.
    let ids = re.find_iter(text).count() as u32;
    let everyone = text.to_lowercase().matches("@everyone").count() as u32;
    let here = text.to_lowercase().matches("@here").count() as u32;
    ids + everyone + here
}

/// Visible emoji, counted as *runs*, so a family made of five code points is one emoji.
/// Text presentation selectors and skin-tone modifiers are absorbed into the run they
/// follow; `count_emoji("👍🏽")` is 1, not 2.
pub fn count_emoji(text: &str) -> u32 {
    let chars: Vec<char> = text.chars().collect();
    let mut runs = 0u32;
    let mut i = 0usize;
    while i < chars.len() {
        if !is_emoji_char(chars[i]) {
            i += 1;
            continue;
        }
        runs += 1;
        i += 1;
        // Swallow the rest of the run: variation selectors and skin tones, plus whatever a
        // zero-width joiner glues on afterwards. Two emoji side by side are two runs, which
        // is the difference between "😀" and a wall of them.
        while i < chars.len() && is_emoji_joiner(chars[i]) {
            let glued = chars[i] == '\u{200D}';
            i += 1;
            if glued && i < chars.len() && is_emoji_char(chars[i]) {
                i += 1;
            }
        }
    }
    runs
}

fn is_emoji_char(c: char) -> bool {
    let u = c as u32;
    (0x1F000..=0x1FAFF).contains(&u)
        || (0x2600..=0x27BF).contains(&u)
        || matches!(u, 0x2B00..=0x2BFF | 0x2764)
        || flags_and_keycaps(u)
}

/// Flags and keycaps are emoji and live outside the planes above.
fn flags_and_keycaps(u: u32) -> bool {
    matches!(u, 0x1F1E6..=0x1F1FF | 0x23F1..=0x23FA | 0x24C2 | 0x25AA | 0x25AB | 0x25FB..=0x25FE | 0x2B1B | 0x2B1C)
}

fn is_emoji_joiner(c: char) -> bool {
    matches!(c as u32, 0xFE0F | 0xFE0E | 0x200D | 0x1F3FB..=0x1F3FF)
}

fn clamp(s: &str) -> String {
    s.chars().take(80).collect()
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    #[test]
    fn links_are_found_with_their_host_and_nothing_else() {
        let hits = find_links("read https://docs.rs/a and www.example.com/x ok");
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert!(hits[0].matched.starts_with("https://docs.rs"));
        assert_eq!(hits[1].matched, "www.example.com/x");
        assert_eq!(host_of("www.example.com/x"), "www.example.com");
        assert_eq!(
            host_of("https://a.b.example.com:8443/p"),
            "a.b.example.com:8443"
        );
    }

    #[test]
    fn a_bare_domain_in_prose_is_not_a_link() {
        assert!(find_links("we run example.com since 2019").is_empty());
        assert!(find_links("node.js and deno.js are fine").is_empty());
    }

    #[test]
    fn invites_are_caught_in_every_shape_discord_ships() {
        for s in [
            "join discord.gg/aerona now",
            "https://discord.com/invite/12",
            "https://discordapp.com/invite/abc-",
            "dsc.gg/short",
            "https://discord.com/servers/friends-123",
            "https://discord.link/x",
        ] {
            assert!(!find_invites(s).is_empty(), "{s} should be an invite");
        }
        assert!(find_invites("discord is nice").is_empty());
        assert!(find_invites("my guild on discord.gg is called x").is_empty());
    }

    #[test]
    fn the_allowlist_matches_on_label_boundaries_only() {
        let allowed = vec!["example.com".to_string()];
        assert!(host_allowed("example.com", &allowed));
        assert!(host_allowed("a.b.example.com", &allowed));
        assert!(!host_allowed("evil-example.com", &allowed));
        assert!(!host_allowed("example.com.evil.net", &allowed));
    }

    #[test]
    fn an_allowlisted_domain_never_lets_an_invite_through() {
        let allowed = vec!["discord.com".to_string()];
        let hits = link_violations("https://discord.com/invite/leak", false, true, &allowed);
        assert_eq!(
            hits.len(),
            1,
            "the invite must survive the allowlist: {hits:?}"
        );
        assert_eq!(hits[0].category, "invite");
        // …and a non-invite link on the same domain is allowed.
        assert!(link_violations("https://discord.com/help", true, false, &allowed).is_empty());
    }

    #[test]
    fn each_rule_can_be_turned_off_independently() {
        let text = "go https://a.example.com";
        assert!(link_violations(text, false, false, &[]).is_empty());
        assert_eq!(link_violations(text, true, false, &[]).len(), 1);
        assert_eq!(link_violations(text, false, true, &[]).len(), 0);
    }

    #[test]
    fn caps_are_a_percentage_of_letters_not_of_the_message() {
        assert_eq!(caps_percent("THIS IS ALL CAPS"), 100);
        assert_eq!(caps_percent("this is all lower"), 0);
        // Digits and punctuation are not letters and must not dilute the ratio.
        assert_eq!(caps_percent("WOW 123!!!"), 100);
        assert!(caps_percent("CaP") > 30);
        assert_eq!(caps_percent("12345"), 0);
    }

    #[test]
    fn mentions_count_users_roles_and_the_two_everyone_pings() {
        assert_eq!(count_mentions("<@123> hi"), 1);
        assert_eq!(count_mentions("<@1> <@2> <@&3>"), 3);
        assert_eq!(count_mentions("@everyone @here"), 2);
        assert_eq!(count_mentions("no pings at all"), 0);
        // A username typed in prose is not a ping, and must not trip a mention limit.
        assert_eq!(count_mentions("@steve said hi"), 0);
    }

    #[test]
    fn an_emoji_run_counts_once() {
        assert_eq!(count_emoji("👍"), 1);
        assert_eq!(count_emoji("👍🏽"), 1);
        assert_eq!(count_emoji("👨‍👩‍👧"), 1);
        assert_eq!(count_emoji("😀😀😀"), 3);
        assert_eq!(count_emoji(":notanemoji:"), 0);
        assert_eq!(count_emoji("😀️"), 1);
    }
}
