//! Domain models: the single source of truth for config shapes shared by the
//! bot, the API and the dashboard's generated types.
//!
//! Conventions that keep this codebase small and predictable:
//!
//! * Every id is a [`crate::identity::Snowflake`], serialized as a JSON *string*
//!   (20-digit ids are not representable as JS numbers).
//! * Every timestamp is epoch seconds (`i64`).
//! * Config structs derive `#[serde(default)]` so a partial `PATCH` from the
//!   dashboard cannot wipe fields the client did not send, and so a config row
//!   written by an older version still deserializes after an upgrade.
//! * Lists of ids/channels are stored as JSON inside the owning config row; the
//!   row itself stays relational, indexed and joinable.

use serde::{Deserialize, Serialize};

use crate::identity::Snowflake;

// ---------------------------------------------------------------------------
// Feature addressing
// ---------------------------------------------------------------------------

/// A configurable subsystem. Used for hot-reload signals between the API and the
/// bot, and as the audit-log subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    General,
    Moderation,
    Tickets,
    Sticky,
    AntiRaid,
    AiFilter,
    /// Deterministic link and invite rules. No model, no cost, same enforcement pass.
    LinkGuard,
    /// Rate and shape rules: flood, mentions, emoji, caps: on the bot's own windows.
    AntiSpam,
    /// What Aerona is called, and what it looks like, in *this* server.
    Identity,
    Autorole,
    Welcome,
    Logging,
    Commands,
    Keys,
    /// Minecraft cape releases: the channel they are announced in, and who is told.
    Capes,
}

impl Feature {
    /// The row a feature's document is actually kept under.
    ///
    /// `autorole`, `welcome` and `commands` are three *screens* over one document, because
    /// the worker reads that one document for all three. Three rows would be three chances
    /// to disagree about whether the goodbye message is on: and a toggle nothing reads is
    /// worse than no toggle at all. Aliasing happens here, at the key, so the panel stays
    /// free to group settings the way an operator thinks about them.
    pub fn storage_key(&self) -> &'static str {
        match self {
            Feature::Autorole | Feature::Welcome | Feature::Commands => "autorole",
            other => other.as_str(),
        }
    }

    pub const ALL: [Feature; 15] = [
        Feature::General,
        Feature::Moderation,
        Feature::Tickets,
        Feature::Sticky,
        Feature::AntiRaid,
        Feature::AiFilter,
        Feature::LinkGuard,
        Feature::AntiSpam,
        Feature::Identity,
        Feature::Autorole,
        Feature::Welcome,
        Feature::Logging,
        Feature::Commands,
        Feature::Keys,
        Feature::Capes,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Feature::General => "general",
            Feature::Moderation => "moderation",
            Feature::Tickets => "tickets",
            Feature::Sticky => "sticky",
            Feature::AntiRaid => "anti_raid",
            Feature::AiFilter => "ai_filter",
            Feature::LinkGuard => "link_guard",
            Feature::AntiSpam => "anti_spam",
            Feature::Identity => "identity",
            Feature::Autorole => "autorole",
            Feature::Welcome => "welcome",
            Feature::Logging => "logging",
            Feature::Commands => "commands",
            Feature::Keys => "keys",
            Feature::Capes => "capes",
        }
    }

    pub fn parse(s: &str) -> Option<Feature> {
        Feature::ALL.into_iter().find(|f| f.as_str() == s)
    }

    /// Table that owns this feature's configuration, if it has one.
    pub fn table(self) -> Option<&'static str> {
        match self {
            Feature::Moderation => Some("mod_config"),
            Feature::Tickets => Some("ticket_config"),
            Feature::AntiRaid => Some("raid_config"),
            Feature::AiFilter => Some("ai_config"),
            Feature::Autorole | Feature::Welcome | Feature::Commands => Some("auto_config"),
            Feature::Logging => Some("log_config"),
            Feature::Sticky => Some("sticky_messages"),
            // The local guards and the guild's copy of the bot profile live in
            // `guild_config` like everything else; they have no second table to clear.
            Feature::General
            | Feature::Keys
            | Feature::LinkGuard
            | Feature::AntiSpam
            | Feature::Identity
            // The cape notifier's records are its own tables; the document needs no second place
            // to be cleared from, because there is nothing per-cape to reset.
            | Feature::Capes => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Guild
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuildSummary {
    pub id: Snowflake,
    pub name: String,
    pub icon: Option<String>,
    pub member_count: i64,
    /// Whether the *requesting user* may manage this guild (admin or `MANAGE_GUILD`).
    pub manageable: bool,
    /// Whether Aerona itself is present in the guild.
    pub bot_present: bool,
}

/// Row in `guild_state`: presence metadata, deliberately separate from config
/// so that config survives a re-invite.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuildState {
    pub guild_id: Snowflake,
    pub added_at: i64,
    pub synced_at: i64,
    pub bot_present: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GuildSettings {
    pub prefix: String,
    pub admin_role_ids: Vec<Snowflake>,
    pub mod_role_ids: Vec<Snowflake>,
    pub ignored_channel_ids: Vec<Snowflake>,
    pub ignored_role_ids: Vec<Snowflake>,
    /// Send DMs on bans, ticket replies, level-ups, etc.
    pub dms_enabled: bool,
    /// Bump guild member counts toward the level system.
    pub levels_enabled: bool,
}

impl Default for GuildSettings {
    fn default() -> Self {
        Self {
            prefix: "!".into(),
            admin_role_ids: vec![],
            mod_role_ids: vec![],
            ignored_channel_ids: vec![],
            ignored_role_ids: vec![],
            dms_enabled: true,
            levels_enabled: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Moderation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModConfig {
    /// Channel for mod-log embeds.
    pub log_channel_id: Option<Snowflake>,
    /// Minimal role allowed to run moderation commands.
    pub mod_role_id: Option<Snowflake>,
    /// Members above this role cannot be moderated by lower ones.
    pub hierarchy_check: bool,
    /// Warnings expire automatically.
    pub warn_expiry_days: i64,
    /// Number of active warnings that triggers the escalate action.
    pub strike_threshold: u8,
    pub escalate_action: StrikeAction,
    /// `!purge` limits, protecting the last N minutes of a channel.
    pub purge_max: u32,
    pub purge_min_age_secs: i64,
}

impl Default for ModConfig {
    fn default() -> Self {
        Self {
            log_channel_id: None,
            mod_role_id: None,
            hierarchy_check: true,
            warn_expiry_days: 0,
            strike_threshold: 0,
            escalate_action: StrikeAction::None,
            purge_max: 200,
            purge_min_age_secs: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrikeAction {
    /// Do nothing beyond logging.
    #[default]
    None,
    Timeout,
    Kick,
    Ban,
}

impl StrikeAction {
    pub fn as_str(self) -> &'static str {
        match self {
            StrikeAction::None => "none",
            StrikeAction::Timeout => "timeout",
            StrikeAction::Kick => "kick",
            StrikeAction::Ban => "ban",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "none" => StrikeAction::None,
            "timeout" => StrikeAction::Timeout,
            "kick" => StrikeAction::Kick,
            "ban" => StrikeAction::Ban,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WarnRecord {
    pub id: i64,
    pub guild_id: Snowflake,
    pub user_id: Snowflake,
    pub moderator_id: Snowflake,
    pub reason: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub active: bool,
    pub pardoned_at: Option<i64>,
    /// Message the warning was raised from, for context links.
    pub context_message: Option<Snowflake>,
}

// ---------------------------------------------------------------------------
// Tickets
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketStatus {
    #[default]
    Open,
    Pending,
    Closed,
}

impl TicketStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TicketStatus::Open => "open",
            TicketStatus::Pending => "pending",
            TicketStatus::Closed => "closed",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "open" => TicketStatus::Open,
            "pending" => TicketStatus::Pending,
            "closed" => TicketStatus::Closed,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TicketConfig {
    pub enabled: bool,
    /// Where the board itself goes: the message with one button per type. A type row may name its
    /// own channel, and then it wins; this is the default those rows fall back to, so an operator
    /// who sets it once and presses Save gets the board posted without having to name a channel
    /// for every type.
    pub board_channel_id: Option<Snowflake>,
    pub category_id: Option<Snowflake>,
    pub support_role_id: Option<Snowflake>,
    pub transcript_channel_id: Option<Snowflake>,
    /// Channel created inside the ticket for the user↔staff conversation.
    pub use_two_channels: bool,
    pub name_format: String,
    pub max_per_user: u8,
    pub claim_required: bool,
    pub auto_close_days: i64,
    pub ping_support_role: bool,
    pub default_message: String,
    /// The card posted in the ticket channel and the one posted when it closes. An empty title
    /// or body keeps the built-in text, so a guild that changes nothing still gets a working
    /// ticket rather than a blank card. `{user}`, `{ticket}` and `{reason}` are substituted.
    pub open_title: String,
    pub close_title: String,
    pub close_body: String,
    pub embed_color: u32,
    /// Buttons exposed on the panel.
    pub show_close: bool,
    pub show_claim: bool,
    /// The transcript button. It is the one control in a ticket channel that is useful after
    /// the conversation is over, so it is offered even in a guild that hides the other two.
    pub show_transcript: bool,
    /// Button wording. An empty label keeps the built-in one, the same rule the card text
    /// follows: a server that changes nothing gets a named button, not a blank one.
    pub close_label: String,
    pub claim_label: String,
    pub transcript_label: String,
    /// Link buttons under the card, for the things staff are asked about all day: the rules,
    /// a status page, an invite to a voice channel. Up to `TicketLink::MAX`.
    pub links: Vec<TicketLink>,
    pub transcript_format: TranscriptFormat,
    /// Minutes after a ticket closes before its channel is deleted. `0` keeps the channel, which
    /// is what a guild that has not configured anything gets, because deleting a channel is the
    /// one ticket action nobody can undo.
    ///
    /// The record and the transcript survive it: they are rows in Aerona's database, not messages
    /// in the channel, which is the only reason this can be offered at all.
    pub delete_after_minutes: u32,
    /// How long those copies are kept, in days. `0` keeps them as long as the ticket row does,
    /// which is the point of a transcript; a guild on a small disk can set 90 and the copies go,
    /// while the transcript keeps its words and says which links it lost.
    pub keep_ticket_files_days: u32,
    /// Copy the pictures a ticket contained into Aerona before the channel goes. On by default,
    /// and only ever applied to images: a Discord attachment is readable while its message is, and
    /// a closed ticket whose channel is deleted has taken its screenshots with it.
    pub keep_ticket_files: bool,
}

impl Default for TicketConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            board_channel_id: None,
            category_id: None,
            support_role_id: None,
            transcript_channel_id: None,
            use_two_channels: false,
            name_format: "ticket-{ref}-{user}".into(),
            max_per_user: 1,
            claim_required: false,
            auto_close_days: 0,
            ping_support_role: true,
            default_message: "Thanks for opening a ticket. A moderator will be with you shortly."
                .into(),
            open_title: "Ticket {ticket} for {user}".into(),
            close_title: "".into(),
            close_body: "".into(),
            embed_color: 0x58_65_F2,
            show_close: true,
            show_claim: true,
            show_transcript: true,
            close_label: String::new(),
            claim_label: String::new(),
            transcript_label: String::new(),
            links: Vec::new(),
            transcript_format: TranscriptFormat::Html,
            delete_after_minutes: 60,
            keep_ticket_files: true,
            keep_ticket_files_days: 0,
        }
    }
}

impl TicketConfig {
    /// The delay, in seconds, or `None` when this guild keeps closed channels. Clamped here rather
    /// than at the form, because the document can also be written by a key holder or an older
    /// panel: a stored `4_000_000` must not become "delete the channel in four years" and a
    /// `1` must not be a race with the transcript being written. A week is the longest anyone can
    /// ask for and one minute is the shortest that leaves time to read the ending.
    pub fn channel_delete_delay(&self) -> Option<i64> {
        match self.delete_after_minutes {
            0 => None,
            n => Some(i64::from(n.clamp(1, 10_080)) * 60),
        }
    }
}

/// The cap on what a transcript is allowed to keep of one file, and of one ticket. A byte limit is
/// the only sane boundary here: `size` is what Discord said the file was, and the copy is what we
/// actually hold.
impl TicketConfig {
    /// The days a kept file survives, or `None` for as long as the ticket row does.
    pub fn file_retention_secs(&self) -> Option<i64> {
        match self.keep_ticket_files_days {
            0 => None,
            d => Some(i64::from(d.clamp(1, 36_500)) * 86_400),
        }
    }

    /// The largest single file a transcript will hold.
    pub const FILE_MAX: u64 = 4 * 1024 * 1024;
    /// The largest total a ticket's files may reach together.
    pub const FILES_MAX: u64 = 8 * 1024 * 1024;
}

/// One file a ticket's transcript kept. This exists because a Discord attachment is not a record:
/// it is a link that stops working when its message does, and a closed ticket's channel is now
/// deleted on a timer. The bytes are ours, and the transcript they are inlined into outlives both.
#[derive(Debug, Clone)]
pub struct TicketFile {
    /// `ticket_messages.seq` this was attached to.
    pub seq: i64,
    /// Its position in that row's attachment list, which is the order the renderer walks.
    pub idx: i64,
    pub name: String,
    pub content_type: String,
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

/// One link button on a ticket card. `url` is checked where the document is written, because a
/// button Discord refuses takes the whole card down with it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TicketLink {
    pub label: String,
    pub url: String,
}

impl TicketLink {
    pub const LABEL_MAX: usize = 40;
    pub const URL_MAX: usize = 500;
    /// Discord's own limit for a row of buttons.
    pub const MAX: usize = 5;

    /// A button with no words or a scheme Discord does not speak is not a button, so neither
    /// the API nor the worker offers one.
    pub fn is_usable(&self) -> bool {
        !self.label.is_empty()
            && (self.url.starts_with("https://") || self.url.starts_with("http://"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptFormat {
    /// A file you open in a browser: the images are shown, names are real, and internal notes
    /// are marked. Text would lose every attachment, and JSON loses the reader.
    #[default]
    Html,
    Text,
    Json,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TicketPanel {
    pub id: i64,
    pub guild_id: Snowflake,
    pub channel_id: Snowflake,
    pub message_id: Snowflake,
    pub label: String,
    pub emoji: Option<String>,
    pub support_role_id: Option<Snowflake>,
    pub sort: i64,
    /// A panel is also a ticket *type*: the ticket it opens takes this colour and this subject
    /// instead of the guild's, which is how one board becomes "Report a player", "Billing" and
    /// "Server idea" without three configurations. `None` / empty means the guild's own value.
    pub color: Option<u32>,
    pub subject: String,
    pub intro: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TicketRecord {
    pub id: i64,
    pub guild_id: Snowflake,
    pub channel_id: Snowflake,
    pub owner_id: Snowflake,
    pub claimed_by: Option<Snowflake>,
    pub status: TicketStatus,
    pub subject: String,
    pub reference: String,
    pub created_at: i64,
    pub closed_at: Option<i64>,
    pub message_count: i64,
    /// When this ticket's channel is due to be deleted, and when it was. Both are read-only to
    /// the panel: the schedule is written by the worker at close, and a deadline a browser could
    /// move is not a deadline.
    pub delete_at: Option<i64>,
    pub channel_deleted_at: Option<i64>,
}

// ---------------------------------------------------------------------------
// Sticky messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StickyConfig {
    pub enabled: bool,
    /// Minimum seconds between reposts of the same message in one channel.
    pub cooldown_secs: u32,
    pub max_per_channel: u8,
}

impl Default for StickyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cooldown_secs: 120,
            max_per_channel: 3,
        }
    }
}

impl StickyConfig {
    /// A `repost` switch used to live here: edit the bot's own message in place, or send a fresh
    /// copy and delete the old one. The switch is gone because reposting is the only behaviour
    /// that matches what a sticky is for - the message at the bottom of the channel, right now -
    /// and an edited copy silently stops being the newest thing anybody reads. A document saved
    /// while the switch existed still carries the key, so `normalize` retires it on the next
    /// write instead of refusing every save after it.
    pub const RETIRED_KEYS: [&'static str; 1] = ["repost"];
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StickyRecord {
    pub id: i64,
    pub guild_id: Snowflake,
    pub channel_id: Snowflake,
    /// The message this note was copied from. `0` means none: the text below is the sticky, and
    /// the bot's own post is the only message involved.
    pub message_id: Snowflake,
    pub sticky_message_id: Option<Snowflake>,
    pub content: String,
    /// An optional embed, so a sticky can be more than a line of text. Every field is optional;
    /// any one of them - a colour on its own included - is enough to ask for a card, and the
    /// colour falls back to Discord's blurple when none was chosen.
    pub embed_title: Option<String>,
    pub embed_body: Option<String>,
    pub embed_color: Option<u32>,
    pub every_messages: u32,
    pub since_count: u32,
    pub created_by: Snowflake,
    pub created_at: i64,
    pub last_run_at: i64,
}

/// What it takes to author a sticky, from the panel or from a command. Everything the row needs
/// except the bookkeeping the database fills in itself.
#[derive(Debug, Clone, Default)]
pub struct StickyDraft {
    pub guild_id: Snowflake,
    pub channel_id: Snowflake,
    /// `Snowflake(0)`: not anchored to a message in Discord. The bot's own post is the sticky.
    pub message_id: Snowflake,
    pub content: String,
    pub embed_title: Option<String>,
    pub embed_body: Option<String>,
    pub embed_color: Option<u32>,
    pub every_messages: u32,
    pub author: Snowflake,
}

impl StickyDraft {
    /// Nothing to send is not a sticky.
    pub fn is_empty(&self) -> bool {
        self.content.trim().is_empty()
            && self
                .embed_title
                .as_deref()
                .unwrap_or_default()
                .trim()
                .is_empty()
            && self
                .embed_body
                .as_deref()
                .unwrap_or_default()
                .trim()
                .is_empty()
    }
}

impl StickyRecord {
    /// Whether this sticky is anchored to a message somebody picked in Discord.
    pub fn anchored(&self) -> bool {
        self.message_id.as_u64() != 0
    }

    /// Does this sticky want a card at all?
    ///
    /// A colour counts. The first version asked only for a title or a body, so the very natural
    /// move - write a line of text and set it white - posted the line with no embed, and the
    /// colour the operator picked was silently dropped on the floor.
    pub fn has_embed(&self) -> bool {
        self.embed_title.as_deref().unwrap_or_default().trim() != ""
            || self.embed_body.as_deref().unwrap_or_default().trim() != ""
            || self.embed_color.is_some()
    }

    /// The colour of the embed bar, with Discord's blurple as the fallback.
    pub fn color(&self) -> u32 {
        self.embed_color.unwrap_or(0x58_65_F2) & 0xff_ff_ff
    }
}

// ---------------------------------------------------------------------------
// Anti-raid
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RaidConfig {
    pub enabled: bool,
    pub detect_join_burst: bool,
    pub join_window_secs: u32,
    pub join_threshold: u32,
    pub detect_msg_flood: bool,
    pub msg_window_secs: u32,
    pub msg_threshold: u32,
    pub per_user_msgs_per_sec: u32,
    /// Ignore accounts older than this many days when sweeping.
    pub account_max_age_days: u32,
    pub require_avatar: bool,
    pub require_username_letters: bool,
    pub alert_channel_id: Option<Snowflake>,
    pub lockdown_slowmode_secs: u32,
    pub raise_verification: bool,
    pub auto_kick: bool,
    pub auto_ban: bool,
    pub mass_action_limit: u32,
    pub auto_recover_secs: u32,
    pub cooldown_secs: u32,
    /// Ignore these users when sweeping (verified bots, staff alts…).
    pub whitelist_user_ids: Vec<Snowflake>,
    pub whitelist_role_ids: Vec<Snowflake>,
}

impl Default for RaidConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            detect_join_burst: true,
            join_window_secs: 10,
            join_threshold: 8,
            detect_msg_flood: true,
            msg_window_secs: 6,
            msg_threshold: 30,
            per_user_msgs_per_sec: 5,
            account_max_age_days: 1,
            require_avatar: true,
            require_username_letters: true,
            alert_channel_id: None,
            lockdown_slowmode_secs: 300,
            raise_verification: true,
            auto_kick: false,
            auto_ban: false,
            mass_action_limit: 50,
            auto_recover_secs: 300,
            cooldown_secs: 120,
            whitelist_user_ids: vec![],
            whitelist_role_ids: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RaidEvent {
    pub id: i64,
    pub guild_id: Snowflake,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub trigger: String,
    pub peak: i64,
    pub window_secs: i64,
    pub flagged: i64,
    pub kicked: i64,
    pub banned: i64,
    pub actions: serde_json::Value,
}

// ---------------------------------------------------------------------------
// AI text filter (Sightengine)
// ---------------------------------------------------------------------------

/// Ordinal severity. `Ord` matters: the dashboard's "minimum severity to act"
/// slider compares these directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    #[default]
    Info = 0,
    Low = 1,
    Medium = 2,
    High = 3,
    Critical = 4,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "info" => Severity::Info,
            "low" => Severity::Low,
            "medium" => Severity::Medium,
            "high" => Severity::High,
            "critical" => Severity::Critical,
            _ => return None,
        })
    }
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Severity::Info,
            1 => Severity::Low,
            2 => Severity::Medium,
            3 => Severity::High,
            _ => Severity::Critical,
        }
    }
    pub fn rank(self) -> u8 {
        self as u8
    }
}

/// What to do with a message that crossed a threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterAction {
    /// Log only.
    #[default]
    Flag,
    Warn,
    /// Delete the message and tell the author why.
    Delete,
    DeleteStrike,
    DeleteKick,
    DeleteBan,
}

impl FilterAction {
    pub fn as_str(self) -> &'static str {
        match self {
            FilterAction::Flag => "flag",
            FilterAction::Warn => "warn",
            FilterAction::Delete => "delete",
            FilterAction::DeleteStrike => "delete_strike",
            FilterAction::DeleteKick => "delete_kick",
            FilterAction::DeleteBan => "delete_ban",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "flag" => FilterAction::Flag,
            "warn" => FilterAction::Warn,
            "delete" => FilterAction::Delete,
            "delete_strike" => FilterAction::DeleteStrike,
            "delete_kick" => FilterAction::DeleteKick,
            "delete_ban" => FilterAction::DeleteBan,
            _ => return None,
        })
    }
    pub fn deletes(self) -> bool {
        !matches!(self, FilterAction::Flag | FilterAction::Warn)
    }
    pub fn strikes(self) -> bool {
        matches!(
            self,
            FilterAction::DeleteStrike | FilterAction::DeleteKick | FilterAction::DeleteBan
        )
    }
}

/// Sightengine categories accepted by `text/check.json`.
pub const AI_CATEGORIES: [&str; 12] = [
    "profanity",
    "personal",
    "link",
    "drug",
    "weapon",
    "violence",
    "self-harm",
    "medical",
    "extremism",
    "spam",
    "content-trade",
    "money-transaction",
];

/// ML classes returned by `mode=ml`.
pub const ML_CLASSES: [&str; 6] = [
    "sexual",
    "discriminatory",
    "insulting",
    "violent",
    "toxic",
    "self-harm",
];

/// Per-category tuning row (table `ai_categories`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiCategoryRule {
    pub category: String,
    pub enabled: bool,
    /// Rule-based intensity required to count (low/medium/high).
    pub min_intensity: Severity,
    /// ML confidence required to count, 0..=100.
    pub ml_threshold: u8,
    /// Overrides the guild default when set.
    pub action_override: Option<FilterAction>,
    pub strikes_override: Option<u8>,
    pub dm_user: bool,
}

impl Default for AiCategoryRule {
    fn default() -> Self {
        Self {
            category: String::new(),
            enabled: true,
            min_intensity: Severity::Medium,
            ml_threshold: 80,
            action_override: None,
            strikes_override: None,
            dm_user: false,
        }
    }
}

/// Local pre-filter rule (table `ai_custom_rules`): matched before the API call,
/// so an obvious hit costs no quota and no latency.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiCustomRule {
    pub id: i64,
    pub pattern: String,
    pub is_regex: bool,
    pub mode: AiCustomMode,
    /// Rule type recorded in `ai_events`, e.g. `slur`.
    pub category: String,
    pub severity: Severity,
    pub action: FilterAction,
    pub strikes: u8,
    pub description: String,
    pub enabled: bool,
    pub hits: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiCustomMode {
    /// Match only if the pattern is a whole word.
    #[default]
    WholeWord,
    Contains,
    Regex,
    /// Allow-list: a hit cancels other rule matches.
    Whitelist,
}

impl AiCustomMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AiCustomMode::WholeWord => "whole_word",
            AiCustomMode::Contains => "contains",
            AiCustomMode::Regex => "regex",
            AiCustomMode::Whitelist => "whitelist",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "whole_word" => AiCustomMode::WholeWord,
            "contains" => AiCustomMode::Contains,
            "regex" => AiCustomMode::Regex,
            "whitelist" => AiCustomMode::Whitelist,
            _ => return None,
        })
    }
}

/// Guild-level AI filter configuration (table `ai_config`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    pub enabled: bool,
    /// `rules`, `ml`, or `rules,ml`.
    pub mode: String,
    pub categories: Vec<String>,
    /// Overall severity required before any action is taken.
    pub min_severity: Severity,
    /// Global sensitivity dial, -5 (relaxed) ..= 5 (paranoid). Shifts every
    /// category threshold at once so an owner never has to retune twelve sliders.
    pub strictness: i8,
    /// Delete the offending message.
    pub auto_delete: bool,
    /// DM the author with the reason.
    pub dm_user: bool,
    /// Issue a moderation warning alongside the delete.
    pub warn_on_hit: bool,
    /// Independent strike ledger that can escalate to a timeout.
    pub strikes_enabled: bool,
    pub strikes_max: u8,
    pub strike_expiry_days: i64,
    pub timeout_minutes: u32,
    pub ban_on_strikes: bool,
    /// Whitelisted roles skip the filter entirely.
    pub whitelist_role_ids: Vec<Snowflake>,
    pub whitelist_user_ids: Vec<Snowflake>,
    pub whitelist_channel_ids: Vec<Snowflake>,
    /// Do not store message text in `ai_events`.
    pub privacy_redact: bool,
    /// `allow` keeps the message if Sightengine errors; `block` fails closed.
    pub on_api_error: FailMode,
    /// Skip short messages entirely (quota guard).
    pub min_chars: u16,
    pub max_chars: u16,
    /// Reuse a verdict for identical text from the same author.
    pub cache_seconds: u16,
    pub apply_to_edits: bool,
    pub apply_to_embeds: bool,
    pub apply_to_bots: bool,
    /// Sightengine custom text list id, if the account uses one.
    pub list_id: Option<String>,
    pub categories_config: Vec<AiCategoryRule>,
    pub custom_rules: Vec<AiCustomRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailMode {
    /// Availability first: a provider outage must not silence a server.
    #[default]
    Allow,
    /// Safety first: block when we cannot check.
    Block,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: "rules".into(),
            categories: vec!["profanity".into(), "personal".into(), "spam".into()],
            min_severity: Severity::Medium,
            strictness: 0,
            auto_delete: true,
            dm_user: true,
            warn_on_hit: false,
            strikes_enabled: false,
            strikes_max: 3,
            strike_expiry_days: 30,
            timeout_minutes: 60,
            ban_on_strikes: false,
            whitelist_role_ids: vec![],
            whitelist_user_ids: vec![],
            whitelist_channel_ids: vec![],
            privacy_redact: false,
            on_api_error: FailMode::Allow,
            min_chars: 3,
            max_chars: 2000,
            cache_seconds: 60,
            apply_to_edits: false,
            apply_to_embeds: false,
            apply_to_bots: false,
            list_id: None,
            categories_config: vec![],
            custom_rules: vec![],
        }
    }
}

/// One row in `ai_events`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiEvent {
    pub id: i64,
    pub guild_id: Snowflake,
    pub user_id: Snowflake,
    pub channel_id: Snowflake,
    /// `None` when the violation was found on a message we could not act on
    /// (e.g. an edit whose message was already deleted).
    pub message_id: Option<Snowflake>,
    pub category: String,
    pub severity: Severity,
    pub score: f64,
    pub action: FilterAction,
    pub created_at: i64,
    pub detail: String,
}

/// Response the dashboard's "test a sentence" box renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterVerdict {
    pub violation: bool,
    pub severity: Severity,
    pub score: f64,
    pub categories: Vec<String>,
    pub matched: Vec<String>,
    pub action: FilterAction,
    /// Strike count *after* this message, when strikes apply.
    pub strikes: u8,
    pub strikes_max: u8,
    /// Why the message passed or failed, for the operator.
    pub reason: String,
    /// Provider latency in ms, so a slow account is visible in the UI.
    pub latency_ms: u64,
    /// Set when the provider was unreachable and `on_api_error` decided the case.
    pub degraded: bool,
}

// ---------------------------------------------------------------------------
// Automation (autorole / welcome / commands)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoConfig {
    pub autoroles_enabled: bool,
    pub autorole_ids: Vec<Snowflake>,
    /// Wait N minutes after join before assigning roles (deters instant alt raids).
    pub autorole_delay_mins: u32,
    pub welcome_enabled: bool,
    pub welcome_channel_id: Option<Snowflake>,
    pub welcome_message: String,
    pub welcome_direct_dm: bool,
    /// The greeting as an embed, in addition to the line above. Both fields are optional:
    /// an empty title *and* an empty description send plain text only, so turning the card
    /// on is a choice an operator makes rather than a migration the code forces.
    pub welcome_embed_title: String,
    pub welcome_embed_description: String,
    /// A Discord colour as `0xRRGGBB`, kept as a number because that is what the API takes
    /// and what the panel writes: a `#hex` string here would be a second format to parse.
    pub welcome_embed_color: u32,
    /// Thumbnail the joiner's avatar on the card. Costs no extra request: the same
    /// snowflake and hash that make the mention build the CDN URL.
    pub welcome_show_avatar: bool,
    /// Do not greet bots. Another server's automation, a verification app and a music bot
    /// all "join", and a greeting to each of them is noise the staff channel has to scroll
    /// past. Also applies to the goodbye line, for the same reason.
    pub welcome_ignore_bots: bool,
    /// Delete the greeting after N seconds, so a busy #general keeps its last page of
    /// conversation. 0 leaves it up.
    pub welcome_delete_after_secs: u32,
    pub goodbye_enabled: bool,
    pub goodbye_channel_id: Option<Snowflake>,
    pub goodbye_message: String,
    pub command_cooldown_secs: u32,
    pub disabled_commands: Vec<String>,
    pub disabled_channels: Vec<Snowflake>,
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self {
            autoroles_enabled: false,
            autorole_ids: vec![],
            autorole_delay_mins: 0,
            welcome_enabled: true,
            welcome_channel_id: None,
            welcome_message: "Welcome {user} to {server}: you are member #{count}.".into(),
            welcome_direct_dm: false,
            welcome_embed_title: String::new(),
            welcome_embed_description: String::new(),
            // Blurple, grouped as the RGB it is. The default is allowed to be pretty.
            welcome_embed_color: 0x58_65_F2,
            welcome_show_avatar: true,
            welcome_ignore_bots: true,
            welcome_delete_after_secs: 0,
            goodbye_enabled: false,
            goodbye_channel_id: None,
            goodbye_message: "{user} left the server.".into(),
            command_cooldown_secs: 3,
            disabled_commands: vec![],
            disabled_channels: vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// Local guards and per-guild identity
// ---------------------------------------------------------------------------

/// What a local guard does with an offending message. Narrower than the AI filter's
/// action list on purpose: these rules are deterministic, so there is no "flag and let a
/// human decide" tier: a rule that fires has already decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardAction {
    Delete,
    #[default]
    DeleteWarn,
    DeleteTimeout,
}

impl GuardAction {
    pub fn as_str(self) -> &'static str {
        match self {
            GuardAction::Delete => "delete",
            GuardAction::DeleteWarn => "delete_warn",
            GuardAction::DeleteTimeout => "delete_timeout",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "delete" => Some(GuardAction::Delete),
            "delete_warn" => Some(GuardAction::DeleteWarn),
            "delete_timeout" => Some(GuardAction::DeleteTimeout),
            _ => None,
        }
    }

    pub fn warns(self) -> bool {
        matches!(self, GuardAction::DeleteWarn)
    }
}

/// Links and invites, checked locally on every message the AI gate is also asked about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkGuardConfig {
    pub enabled: bool,
    /// Any URL or bare domain is a violation.
    pub block_links: bool,
    /// `discord.gg/x`, `discord.com/invite/x`, `discordapp.com/invite/x`.
    pub block_invites: bool,
    /// Domains that are always fine. Compared on the registrable suffix, so
    /// `discord.com` allows `discord.com/invite/x`? No: it allows `docs.discord.com`,
    /// which is why invites are matched *before* the allowlist: an allowlisted domain
    /// never turns an invite into an exception.
    pub allowed_domains: Vec<String>,
    pub exempt_role_ids: Vec<Snowflake>,
    pub exempt_channel_ids: Vec<Snowflake>,
    pub action: GuardAction,
    /// Only read when `action` is `delete_timeout`.
    pub timeout_minutes: u32,
}

impl Default for LinkGuardConfig {
    fn default() -> Self {
        Self {
            // Invites only: the thing every server wants on day one, and the one rule
            // that cannot be argued with. A server that wants to block all links flips
            // `block_links` on for itself.
            enabled: true,
            block_links: false,
            block_invites: true,
            allowed_domains: vec![],
            exempt_role_ids: vec![],
            exempt_channel_ids: vec![],
            action: GuardAction::Delete,
            timeout_minutes: 10,
        }
    }
}

/// Rate and shape limits. The counters are in the gateway worker's memory, which is
/// what makes them free; nothing here is persisted, and a restart resets a flood.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AntiSpamConfig {
    pub enabled: bool,
    /// Messages from one member inside `window_secs`. `0` disables the rule.
    pub max_messages: u8,
    pub window_secs: u8,
    /// `<@id>` mentions in a single message. `0` disables the rule.
    pub max_mentions: u8,
    /// Distinct emoji in a single message. `0` disables the rule.
    pub max_emoji: u8,
    /// Uppercase share of an all-caps message, in percent. `0` disables the rule.
    pub max_caps_percent: u8,
    /// Caps and emoji rules ignore anything shorter than this, so "WOW" stays.
    pub min_chars: u8,
    pub exempt_role_ids: Vec<Snowflake>,
    pub exempt_channel_ids: Vec<Snowflake>,
    pub action: GuardAction,
    pub timeout_minutes: u32,
}

impl Default for AntiSpamConfig {
    fn default() -> Self {
        Self {
            // Off by default: a flood limit that is too tight mutes a conversation during
            // an announcement, and the operator learns about it from their members.
            enabled: false,
            max_messages: 5,
            window_secs: 5,
            max_mentions: 6,
            max_emoji: 15,
            max_caps_percent: 75,
            min_chars: 12,
            exempt_role_ids: vec![],
            exempt_channel_ids: vec![],
            action: GuardAction::DeleteWarn,
            timeout_minutes: 5,
        }
    }
}

/// The bot's face in one server. `nick` and `bio` live on the *guild member*, so they change
/// nothing for any other server and clearing one restores the account-wide value. The pictures and
/// the decorations are account-wide - Discord keeps them on the user - which is why the apply
/// splits them across two endpoints rather than PATCHing one member document with all of it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IdentityConfig {
    /// Shown instead of the username in this server. 1-32 characters; `null` clears it.
    pub nick: Option<String>,
    /// The profile text Discord shows for the member here. Max 190; `null` clears it.
    pub bio: Option<String>,
    /// Never the bytes: a 1 MB base64 string inside a settings document would be written
    /// to `config_history` on every save and read back on every page load. The image goes
    /// to `PUT /identity/avatar`, which stages it for one apply and records its digest
    /// here, so the document says "an avatar is waiting" without carrying one.
    pub avatar: Option<String>,
    /// `sha256:<16 hex>` of the staged image, or of the one Discord has. The dashboard
    /// shows it so two saves of the same file are visibly the same thing.
    pub avatar_hash: Option<String>,
    /// The banner across the top of the bot's profile. Same arrangement as the avatar: bytes are
    /// staged once through `PUT /identity/banner`, and only their digest is kept here. Bots carry
    /// the same Nitro cosmetics as any other account, so there is no reason for the panel to offer
    /// an avatar and stop there.
    pub banner_hash: Option<String>,
    /// Two decoration SKUs, sent to Discord as `avatar_decoration_id` and `banner_decoration_id`.
    ///
    /// Discord publishes no catalogue of these for a bot to read, so the panel's picker offers
    /// what it *can* see - the decorations in use by members of this server, gathered by the
    /// worker into `picker.decorations` - and takes an id typed in for anything else. An id that
    /// the account does not own is refused by Discord, and the refusal is what the apply report
    /// says; nothing here pretends otherwise.
    pub avatar_decoration_id: Option<String>,
    pub banner_decoration_id: Option<String>,
    /// Not a Discord field: the one-line description the dashboard and `/{prefix}help`
    /// show for Aerona in this server.
    pub about: Option<String>,
}

// The derive *is* the intent: every field is an `Option`, so "the bot's default identity in a
// server" is "no field set, everything falls back to the account": a hand-written impl could only
// drift from that.
impl IdentityConfig {
    /// Every field unset: nothing to ask Discord for.
    pub fn is_empty(&self) -> bool {
        self.nick.is_none() && self.bio.is_none() && self.avatar_hash.is_none()
    }

    /// Discord's own limits, restated for the dashboard's copy. `bio` and `avatar` need no
    /// permission; `nick` needs Change nickname, which `permissions=8` carries.
    pub fn needs_change_nick(&self) -> bool {
        self.nick.as_deref().is_some_and(|s| !s.is_empty())
    }

    pub const NICK_MAX: usize = 32;
    pub const BIO_MAX: usize = 190;
    /// Base64 inflates by ~4/3; 1 MB of bytes is just over 1.4 MB of text, and Discord's
    /// image routes stop at 8 MB per request, so this leaves the headroom to spare.
    pub const AVATAR_MAX_CHARS: usize = 1_500_000;
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

/// Event keys the logger can toggle, and the list is exactly the set the bot emits.
///
/// `username_change` is deliberately absent even though Discord sends it: an account name is not a
/// server event, the same change shows up in every guild at once, and a switch that could only ever
/// log a duplicate is a switch nobody should have to reason about.
pub const LOG_EVENTS: [&str; 10] = [
    "message_edit",
    "message_delete",
    "message_bulk_delete",
    "member_join",
    "member_leave",
    "member_ban",
    "member_unban",
    "role_change",
    "nickname_change",
    "channel_change",
];

/// The channel and the event list are the whole switch: a logger with no channel has nowhere to
/// post, and one with no events checked posts nothing. A master toggle on top of those two would
/// only ever be left on, so it is not here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    pub channel_id: Option<Snowflake>,
    /// Keys from [`LOG_EVENTS`].
    pub events: Vec<String>,
    pub ignore_bots: bool,
    /// How many of this guild's messages to keep the text of, so an edit or a delete can quote
    /// what changed instead of reporting that something changed.
    pub max_cached_messages: u32,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            channel_id: None,
            // The four that make a self-hosted mod log worth having. The rest are one tick away,
            // and a guild that never ticks anything should not be posting nothing forever either.
            events: [
                "message_edit",
                "message_delete",
                "member_join",
                "member_leave",
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
            ignore_bots: true,
            max_cached_messages: 500,
        }
    }
}

impl LogConfig {
    /// Should this event be posted? Only true with somewhere to post it, so a caller that asks
    /// `tracks` before building anything never does work for a discarded card.
    pub fn tracks(&self, event: &str) -> bool {
        self.channel_id.is_some() && self.events.iter().any(|e| e == event)
    }

    /// Should this guild's text be cached at all, independent of whether anything is posted.
    pub fn caches(&self) -> bool {
        self.max_cached_messages > 0
    }
}

// ---------------------------------------------------------------------------
// Minecraft cape notifier
// ---------------------------------------------------------------------------

/// What the cape notifier posts, and how often it asks.
///
/// The source is the wiki category every documented cape belongs to (see [`crate::cape`] for why
/// that is the right thing to poll and why it is not Mojang); this document is only about what to do
/// with what it says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CapeConfig {
    pub enabled: bool,
    /// Where the announcement goes. `None` is not a misconfiguration to be refused at save time: the
    /// notifier keeps recording what it finds, so an operator can turn the feature on, read the list
    /// a week later, and point it at a channel without having missed anything. The page says so.
    pub channel_id: Option<Snowflake>,
    /// The role to ping. `None` posts the card silently, which is what a server that only wants the
    /// news in the channel picks.
    pub ping_role_id: Option<Snowflake>,
    /// How often to ask the wiki. The floor is not politeness theatre: the category is 60-odd pages
    /// and the answer changes a few times a year, so a check faster than a quarter of an hour is
    /// traffic for nothing. Clamped on use, not refused on save.
    pub check_every_minutes: u32,
    /// Attach the 3D render of the cape. It is drawn here, from the texture, so this is a question
    /// about whether the message carries a picture at all - and the lever to pull if an operator
    /// decides they want the text and the link only.
    pub show_render: bool,
    /// The bar on the left of the card.
    pub embed_color: u32,
}

impl Default for CapeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            channel_id: None,
            ping_role_id: None,
            check_every_minutes: 20,
            show_render: true,
            embed_color: 0x58_65_F2,
        }
    }
}

impl CapeConfig {
    /// The interval to sleep between polls, in seconds, clamped into the range that is worth having.
    pub fn check_every_secs(&self) -> u64 {
        (self.check_every_minutes.clamp(5, 1440) as u64) * 60
    }

    /// Is there anywhere to post? The list is written either way, so this decides the ping and not
    /// the work.
    pub fn announces(&self) -> bool {
        self.enabled && self.channel_id.is_some()
    }
}

/// One cape the notifier has seen, as the panel lists it. Written by the worker, read by the
/// dashboard, and the row that answers "did it really come out, and when did you notice".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapeRelease {
    pub id: i64,
    /// The page's id as the source spells it, which is what a rename cannot fake.
    pub slug: String,
    /// The page title, verbatim, for the link and for anybody comparing the two.
    pub title: String,
    /// The name the card shows.
    pub name: String,
    pub article_url: String,
    /// The file the texture came from. Empty means the page had no flat artwork, which is why
    /// `has_render` exists rather than a placeholder image.
    pub texture_file: String,
    /// Which picture the row carries: `drape` for the render made from the flat texture here,
    /// `wiki` for the wiki's own render of the cape worn, and `none` when neither was available.
    /// The panel shows this when an operator wonders why one cape has four angles and another has
    /// one, and it is why a missing picture never became a missing announcement.
    pub render_kind: String,
    pub frames: u32,
    /// Whether there is a picture to show at all, which is what `frames > 0` means. Kept because
    /// every caller so far wants the yes-or-no rather than the count.
    pub has_render: bool,
    /// The revision the announcement was made from, so a page that is merely edited is updated
    /// quietly instead of pinging twice for one cape.
    pub last_rev: i64,
    pub found_at: i64,
    pub announced_at: Option<i64>,
    /// Posts attempted and refused, for a row that never made it to the channel.
    pub attempts: i32,
}

/// How many angles of each cape are kept. The announcement shows one; the panel turns it.
/// How many frames one cape has: the drape views plus the sheet's own art, flat. Kept as a
/// re-export of the renderer's own count so a frame added there cannot be missing here.
pub const CAPE_FRAMES: usize = crate::cape::FRAMES;

/// How many times the notifier will try to post one cape before it gives up and says so on the
/// panel. A channel that refuses the bot is a configuration mistake, and five minutes of trying is
/// a generous read of "the operator is asleep"; past that the row stays unannounced and the panel
/// carries the reason, which is a thing a person can fix.
pub const CAPE_MAX_ATTEMPTS: i32 = 5;

// ---------------------------------------------------------------------------
// API surface: keys, sessions, audit, stats
// ---------------------------------------------------------------------------

// `ApiKeyRecord`, `Session` and `AuditEntry` live in `db::security` rather than
// here: they are row shapes with no bot-side counterpart, and keeping them next to
// the SQL that fills them means a schema change touches exactly one file.
// `StatsSnapshot` stays because the bot writes it and the API reads it.

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatsSnapshot {
    pub at: i64,
    pub process: String,
    pub rss_kb: i64,
    pub cpu_pct: f64,
    pub guilds: i64,
    pub users_seen: i64,
    pub ws_latency_ms: i64,
    pub messages_per_min: i64,
    pub db_size_kb: i64,
}

// ---------------------------------------------------------------------------
// Sightengine wire types
// ---------------------------------------------------------------------------

/// One rule-based hit. `intensity` maps onto [`Severity`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SeMatch {
    pub r#type: String,
    pub intensity: String,
    /// The literal snippet that matched. JSON key is `match`.
    #[serde(rename = "match", default)]
    pub match_: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SeMatchList {
    pub matches: Vec<SeMatch>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SeMlClasses {
    pub available: Vec<String>,
    pub sexual: f64,
    pub discriminatory: f64,
    pub insulting: f64,
    pub violent: f64,
    pub toxic: f64,
    /// The provider spells this one `self-harm`; a hyphen cannot be a Rust
    /// identifier, so it needs an explicit rename. Reading it via `flatten`
    /// would silently yield 0.0 and never moderate self-harm content.
    #[serde(rename = "self-harm", default)]
    pub self_harm: f64,
    /// Anything the provider adds later, so a new class is visible in the
    /// dashboard instead of being silently dropped.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl SeMlClasses {
    /// Confidence for a category name the dashboard uses.
    pub fn score_for(&self, class: &str) -> f64 {
        match class {
            "sexual" => self.sexual,
            "discriminatory" => self.discriminatory,
            "insulting" => self.insulting,
            "violent" => self.violent,
            "toxic" => self.toxic,
            "self-harm" => self.self_harm,
            _ => 0.0,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SeResponse {
    pub status: String,
    pub error: Option<SeError>,
    pub profanity: Option<SeMatchList>,
    pub personal: Option<SeMatchList>,
    pub link: Option<SeMatchList>,
    pub drug: Option<SeMatchList>,
    pub weapon: Option<SeMatchList>,
    pub violence: Option<SeMatchList>,
    #[serde(rename = "self-harm", default)]
    pub self_harm: Option<SeMatchList>,
    pub medical: Option<SeMatchList>,
    pub extremism: Option<SeMatchList>,
    pub spam: Option<SeMatchList>,
    #[serde(rename = "content-trade", default)]
    pub content_trade: Option<SeMatchList>,
    #[serde(rename = "money-transaction", default)]
    pub money_transaction: Option<SeMatchList>,
    #[serde(rename = "moderation_classes", default)]
    pub moderation_classes: Option<SeMlClasses>,
}

impl SeResponse {
    /// Match list for a category we know about.
    pub fn matches_for(&self, category: &str) -> &[SeMatch] {
        let list = match category {
            "profanity" => &self.profanity,
            "personal" => &self.personal,
            "link" => &self.link,
            "drug" => &self.drug,
            "weapon" => &self.weapon,
            "violence" => &self.violence,
            "self-harm" => &self.self_harm,
            "medical" => &self.medical,
            "extremism" => &self.extremism,
            "spam" => &self.spam,
            "content-trade" => &self.content_trade,
            "money-transaction" => &self.money_transaction,
            _ => return &[],
        };
        list.as_ref().map(|l| l.matches.as_slice()).unwrap_or(&[])
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SeError {
    pub message: String,
    pub code: Option<String>,
    pub type_: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_roundtrip_and_tables() {
        for f in Feature::ALL {
            assert_eq!(
                Feature::parse(f.as_str()),
                Some(f),
                "roundtrip {}",
                f.as_str()
            );
            let json = serde_json::to_string(&f).unwrap();
            assert!(json.contains('"'), "{json}");
        }
        assert_eq!(Feature::Moderation.table(), Some("mod_config"));
        assert_eq!(Feature::General.table(), None);
        assert_eq!(Feature::parse("nonsense"), None);
    }

    #[test]
    fn defaults_are_conservative() {
        let ai = AiConfig::default();
        // Off by default: a bot must never start deleting messages nobody asked for.
        assert!(!ai.enabled);
        assert!(ai.auto_delete);
        assert_eq!(
            ai.on_api_error,
            FailMode::Allow,
            "outage must not silence a server"
        );
        assert!(ai.categories.contains(&"profanity".to_string()));
        assert!(!ai.ban_on_strikes, "never auto-ban on first install");

        let raid = RaidConfig::default();
        assert!(raid.enabled);
        assert!(
            raid.join_threshold > 2,
            "threshold 1-2 would fire on normal servers"
        );
        assert!(!raid.auto_ban, "mass bans must be opt-in");
        assert!(raid.mass_action_limit <= 200, "bounded mass action");

        let tickets = TicketConfig::default();
        assert!(!tickets.enabled);
        assert!(tickets.max_per_user >= 1);

        assert!(
            ModConfig::default().hierarchy_check,
            "no upside-down mod chains"
        );
    }

    #[test]
    fn severity_is_ordinal() {
        assert!(Severity::Low < Severity::Medium);
        assert!(Severity::Medium < Severity::High);
        assert!(Severity::High < Severity::Critical);
        assert_eq!(Severity::from_u8(9), Severity::Critical);
        for s in [
            Severity::Info,
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ] {
            assert_eq!(Severity::parse(s.as_str()), Some(s));
            assert_eq!(Severity::from_u8(s.rank()), s);
        }
    }

    #[test]
    fn filter_action_capabilities() {
        assert!(!FilterAction::Flag.deletes() && !FilterAction::Warn.deletes());
        assert!(FilterAction::Delete.deletes(), "delete must delete");
        assert!(
            !FilterAction::Delete.strikes(),
            "plain delete is not a strike"
        );
        assert!(FilterAction::DeleteStrike.strikes());
        assert!(FilterAction::DeleteBan.deletes() && FilterAction::DeleteBan.strikes());
    }

    #[test]
    fn serde_defaults_tolerate_partial_config() {
        // A PATCH that only touches one field must not reset the rest to zero.
        let partial = serde_json::json!({"enabled": true, "min_severity": "high"});
        let cfg: AiConfig = serde_json::from_value(partial).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.min_severity, Severity::High);
        assert_eq!(cfg.strikes_max, AiConfig::default().strikes_max);
        assert_eq!(cfg.mode, "rules");

        // Unknown keys are ignored rather than fatal, so a newer dashboard can
        // talk to an older binary without 400s.
        let with_extra = serde_json::json!({"enabled": true, "image_moderation": true});
        let cfg2: AiConfig = serde_json::from_value(with_extra).unwrap();
        assert!(cfg2.enabled);
    }

    #[test]
    fn snowflake_fields_serialize_as_strings() {
        let rec = WarnRecord {
            id: 1,
            guild_id: Snowflake(123_456_789_012_345_678),
            user_id: Snowflake(42),
            moderator_id: Snowflake(7),
            reason: "be nice".into(),
            created_at: 0,
            expires_at: 0,
            active: true,
            pardoned_at: None,
            context_message: None,
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(
            json.contains("\"guild_id\":\"123456789012345678\""),
            "{json}"
        );
    }

    #[test]
    fn sightengine_lookup_maps_names() {
        let raw = r#"{
            "status": "success",
            "profanity": {"matches": [{"type":"sexual","intensity":"medium","match":"sex","start":5,"end":9}]},
            "personal": {"matches": []},
            "moderation_classes": {"available":["toxic"],"sexual":0.9,"discriminatory":0.1,
                                    "insulting":0.2,"violent":0.05,"toxic":0.7,"self-harm":0.02}
        }"#;
        let r: SeResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(r.matches_for("profanity").len(), 1);
        assert_eq!(r.matches_for("profanity")[0].intensity, "medium");
        assert_eq!(r.matches_for("profanity")[0].match_, "sex");
        assert!(r.matches_for("link").is_empty());
        assert!(r.matches_for("not-a-category").is_empty());
        let ml = r.moderation_classes.as_ref().unwrap();
        assert!((ml.score_for("sexual") - 0.9).abs() < 1e-9);
        assert!((ml.score_for("self-harm") - 0.02).abs() < 1e-9);
        assert_eq!(ml.score_for("unknown"), 0.0);
    }

    #[test]
    fn sightengine_error_payload_parses() {
        let r: SeResponse = serde_json::from_str(
            r#"{"status":"error","error":{"message":"No credentials provided","code":"no-auth"}}"#,
        )
        .unwrap();
        assert_eq!(r.status, "error");
        assert!(r.error.as_ref().unwrap().message.contains("credentials"));
        assert!(r.profanity.is_none());
    }

    #[test]
    fn log_config_tracks_only_when_it_has_somewhere_to_post() {
        let mut c = LogConfig {
            events: vec!["message_edit".into()],
            max_cached_messages: 0,
            ..Default::default()
        };
        // No channel: nothing to post to, so nothing to build. This is the half of the old
        // `enabled` switch that meant something, and it is why the switch itself is gone.
        assert!(
            !c.tracks("message_edit"),
            "a log with no channel logs nothing"
        );
        assert!(
            !c.caches(),
            "the default cache size of 500 is not what this sets"
        );
        c.channel_id = Some(Snowflake(5));
        c.max_cached_messages = 500;
        assert!(c.tracks("message_edit"));
        assert!(c.caches());
        assert!(
            !c.tracks("member_ban"),
            "an unchecked event stays unchecked"
        );
    }
}
