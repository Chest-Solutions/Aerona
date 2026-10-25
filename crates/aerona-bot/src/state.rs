//! Process-wide state for the gateway worker: the database handle, a clone of the
//! serenity `Context` once it exists, and the small sliding windows the raid detector
//! needs. Deliberately *not* a mirror of Discord — everything permanent lives in
//! SQLite, so this can be thrown away at any moment.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use dashmap::DashMap;
use dashmap::DashSet;
use parking_lot::Mutex;
use serenity::all::Context;

use aerona_core::config::Config;
use aerona_core::db::Db;
use aerona_core::identity::Snowflake;
use aerona_core::time::now;

/// A sliding window of timestamps in one fixed interval. Two are kept per guild:
/// joins (raid burst) and messages (flood). Insert is O(1) amortised and the window is
/// bounded by its own age, so a raid cannot grow this structure without limit.
#[derive(Debug, Default)]
pub struct Window {
    hits: VecDeque<i64>,
}

impl Window {
    /// Records `now` and returns how many hits fall inside `window_secs`.
    pub fn note(&mut self, at: i64, window_secs: i64) -> usize {
        self.hits.push_back(at);
        let floor = at - window_secs.max(1);
        while let Some(&first) = self.hits.front() {
            if first < floor {
                self.hits.pop_front();
            } else {
                break;
            }
        }
        // A window that has drifted far into the past is dropped rather than kept
        // warm: an idle guild should cost nothing, and `VecDeque::clear` on read is
        // cheaper than a prune that runs forever.
        self.hits.len()
    }

    /// Keep the newest `keep` entries. A member sending a wall of messages must not grow
    /// the deque between prunes, and the count that decided the flood rule has already been
    /// read by then.
    pub fn truncate(&mut self, keep: usize) {
        while self.hits.len() > keep {
            self.hits.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.hits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    pub fn reset(&mut self) {
        self.hits.clear();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rate {
    /// The caller may proceed.
    Allowed,
    /// Too fast: retry after this many seconds.
    Throttled(u64),
}

/// Everything a handler needs that is not on the stack.
pub struct BotState {
    pub cfg: Arc<Config>,
    pub db: Db,
    /// One shared outbound client for every provider call. Its timeouts come from the
    /// config, because a hung third-party request must not pin a handler forever.
    pub http: reqwest::Client,
    pub started_at: i64,

    /// Set once from `ready`; every task and handler reads it through `ctx()`, which
    /// returns `None` before the gateway is up. `OnceLock` rather than a mutex because
    /// it is written exactly once and read on every event.
    ctx: OnceLock<Context>,
    /// Channel → the last time a sticky was reposted in it, so a burst of chatter
    /// cannot turn a pinned notice into a machine gun.
    pub last_sticky_edit: DashMap<Snowflake, i64>,

    /// Guilds the bot is a member of and is allowed to act in. Checked per event, in
    /// memory, so an unrelated server's traffic never reaches the database.
    pub managed: DashSet<Snowflake>,
    /// `(guild, config_epoch)` — the trigger to drop cached settings and re-register
    /// commands. Bumped by the API on every settings write.
    pub epochs: DashMap<Snowflake, i64>,

    pub joins: DashMap<Snowflake, Mutex<Window>>,
    pub floods: DashMap<Snowflake, Mutex<Window>>,
    /// Per-user message timestamps, keyed by `(guild, user)`. Only populated while a
    /// guild has anti-raid enabled, so a quiet server does not pay for the map.
    pub per_user: DashMap<(Snowflake, Snowflake), Mutex<Window>>,
    /// Channel → messages since that channel's stickies were last evaluated.
    /// `(guild, user)` → recent message timestamps, kept for the anti-spam flood rule.
    /// Only written while that guild has the guard on, so a quiet server pays nothing.
    pub spams: DashMap<(Snowflake, Snowflake), Mutex<Window>>,
    pub sticky_tally: DashMap<(Snowflake, Snowflake), u32>,
    /// `unix_until` per guild while a lockdown is in force.
    pub lockdown_until: DashMap<Snowflake, i64>,
    /// `unix_until` per `(guild, user, command)` for the configured command cooldown.
    pub cooldown: DashMap<(Snowflake, Snowflake, String), i64>,
    /// Guilds where a raid alert has been posted and not yet cleared, so recovery is
    /// announced once instead of on every tick.
    pub raid_alerted: DashMap<Snowflake, i64>,

    pub ws_latency_ms: Mutex<i64>,
    pub shards_total: Mutex<u32>,
    pub shards_ready: Mutex<u32>,
    pub events_min: Mutex<u64>,
    pub msgs_min: Mutex<u64>,
    pub actions_min: Mutex<u64>,
}

impl BotState {
    pub fn new(cfg: Arc<Config>, db: Db) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.http_timeout_secs))
            .connect_timeout(Duration::from_secs(cfg.http_connect_timeout_secs))
            .user_agent(format!("aerona-bot/{}", aerona_core::VERSION))
            .build()
            .unwrap_or_default();
        BotState {
            cfg,
            db,
            http,
            started_at: now(),
            ctx: OnceLock::new(),
            last_sticky_edit: DashMap::new(),
            managed: DashSet::new(),
            epochs: DashMap::new(),
            joins: DashMap::new(),
            floods: DashMap::new(),
            per_user: DashMap::new(),
            spams: DashMap::new(),
            sticky_tally: DashMap::new(),
            lockdown_until: DashMap::new(),
            cooldown: DashMap::new(),
            raid_alerted: DashMap::new(),
            ws_latency_ms: Mutex::new(0),
            shards_total: Mutex::new(0),
            shards_ready: Mutex::new(0),
            events_min: Mutex::new(0),
            msgs_min: Mutex::new(0),
            actions_min: Mutex::new(0),
        }
    }

    /// Called once from `ready`. A second call (a reconnect) is ignored on purpose: the
    /// `Context` serenity hands out is the same one for the life of the client.
    pub fn attach(&self, ctx: &Context) {
        let _ = self.ctx.set(ctx.clone());
    }

    pub fn ctx(&self) -> Option<&Context> {
        self.ctx.get()
    }

    pub fn bump(&self, which: &Mutex<u64>) -> u64 {
        let mut g = which.lock();
        *g += 1;
        *g
    }

    pub fn take_rate(&self, which: &Mutex<u64>) -> u64 {
        std::mem::replace(&mut *which.lock(), 0)
    }

    /// Messages from one member in the last `window_secs`, this one included. The caller
    /// compares the count with its own limit, which is why the note happens before the
    /// decision: an operator who sets "5 in 5 seconds" means the sixth message to be the
    /// one that trips, not the seventh.
    pub fn note_spam(
        &self,
        guild: Snowflake,
        user: Snowflake,
        limit: usize,
        window_secs: u8,
    ) -> usize {
        // Bound separately: `.or_default()` returns a `RefMut` that the temporary would
        // drop before the guard could be used.
        let slot = self.spams.entry((guild, user)).or_default();
        let mut w = slot.lock();
        let n = w.note(now(), i64::from(window_secs.max(1)));
        w.truncate(limit.saturating_mul(8).max(64));
        n
    }

    pub fn note_join(&self, guild: Snowflake, window_secs: u32) -> usize {
        let slot = self.joins.entry(guild).or_default();
        let mut guard = slot.lock();
        guard.note(now(), i64::from(window_secs))
    }

    pub fn note_message(
        &self,
        guild: Snowflake,
        user: Snowflake,
        window_secs: u32,
    ) -> (usize, usize) {
        let guild_count = {
            let slot = self.floods.entry(guild).or_default();
            let mut guard = slot.lock();
            guard.note(now(), i64::from(window_secs))
        };
        let user_count = {
            let slot = self.per_user.entry((guild, user)).or_default();
            let mut guard = slot.lock();
            guard.note(now(), i64::from(window_secs))
        };
        (guild_count, user_count)
    }

    pub fn forget_guild_windows(&self, guild: Snowflake) {
        self.joins.remove(&guild);
        self.floods.remove(&guild);
        self.per_user.retain(|k, _| k.0 != guild);
        self.spams.retain(|k, _| k.0 != guild);
        self.sticky_tally.retain(|k, _| k.0 != guild);
        self.lockdown_until.remove(&guild);
        self.raid_alerted.remove(&guild);
    }

    /// The command cooldown, in the "may I run this now" sense. `Allowed` also records
    /// the new expiry: a check-and-set in one step, because the alternative lets two
    /// concurrent invocations both pass.
    pub fn rate_limit_cmd(&self, guild: Snowflake, user: Snowflake, cmd: &str, secs: u32) -> Rate {
        if secs == 0 {
            return Rate::Allowed;
        }
        let key = (guild, user, cmd.to_string());
        let at = now();
        let mut entry = self.cooldown.entry(key).or_insert(at);
        let current = *entry;
        if current > at {
            return Rate::Throttled((current - at).max(1) as u64);
        }
        *entry = at + i64::from(secs);
        Rate::Allowed
    }

    /// Prune cooldown entries that have expired; called from the slow loop so the map
    /// cannot grow with one entry per user per command forever.
    pub fn prune_cooldowns(&self) -> usize {
        let at = now();
        let dead: Vec<_> = self
            .cooldown
            .iter()
            .filter(|e| *e.value() <= at)
            .map(|e| e.key().clone())
            .collect();
        let n = dead.len();
        for k in dead {
            self.cooldown.remove(&k);
        }
        n
    }

    /// Diff the guilds' `config_epoch` against what we last saw. Returns the guilds
    /// whose settings changed (including newly added ones), which is exactly the set
    /// that needs its command registrations refreshed.
    pub fn refresh_epochs(&self, seen: &[(Snowflake, i64)]) -> Vec<Snowflake> {
        let mut changed = Vec::new();
        for (guild, epoch) in seen {
            // `or_insert` alone would swallow the interesting case: a guild that has
            // never been seen is *new*, not unchanged, and must be reported so its
            // commands get registered.
            let known = self.epochs.contains_key(guild);
            let mut entry = self.epochs.entry(*guild).or_insert(*epoch);
            if !known || *entry != *epoch {
                *entry = *epoch;
                changed.push(*guild);
            }
        }
        let live: Vec<_> = self.epochs.iter().map(|e| *e.key()).collect();
        for guild in live {
            if !seen.iter().any(|(g, _)| *g == guild) {
                self.epochs.remove(&guild);
                self.forget_guild_windows(guild);
                self.managed.remove(&guild);
            }
        }
        changed
    }

    pub fn uptime_secs(&self) -> i64 {
        now() - self.started_at
    }
}

/// Resident set in KiB, read from procfs. No crate for this: `/proc/self/statm` is one
/// read, and on a platform without it the answer is simply unknown (0), which the
/// dashboard renders as a flat line rather than a lie.
pub fn rss_kb() -> i64 {
    match std::fs::read_to_string("/proc/self/statm") {
        Ok(s) => s
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.parse::<i64>().ok())
            .map(|pages| pages * 4)
            .unwrap_or(0),
        Err(_) => 0,
    }
}

/// CPU percentage between successive calls, from jiffies in `/proc/self/stat`. The
/// sampler calls this every 30 s; keeping the previous reading in a static is what
/// makes it a rate rather than a lifetime average.
pub fn cpu_pct() -> f64 {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering::Relaxed;
    static PREVIOUS: AtomicU64 = AtomicU64::new(0);
    static PREV_AT: AtomicU64 = AtomicU64::new(0);

    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return 0.0;
    };
    // The comm field is in parentheses and can contain spaces, so count from the
    // closing paren, not from the first.
    let Some(end) = stat.rfind(')') else {
        return 0.0;
    };
    let fields: Vec<&str> = stat[end + 1..].split_whitespace().collect();
    // utime and stime are fields 14 and 15 of the whole line, i.e. 12 and 13 after
    // `pid (comm) state`.
    let jiffies = |i: usize| {
        fields
            .get(i)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let spent = (jiffies(11) + jiffies(12)) * 1000 / 100; // ms, assuming 100 Hz
    let at = std::time::UNIX_EPOCH
        .elapsed()
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64;
    let prev = PREVIOUS.swap(spent, Relaxed);
    let prev_at = PREV_AT.swap(at, Relaxed);
    if prev == 0 || at <= prev_at || spent < prev {
        return 0.0;
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(1);
    let busy_ms = spent - prev;
    let wall_ms = at - prev_at;
    f64::from(100) * (busy_ms as f64 / wall_ms as f64) / cores as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use aerona_core::config::Config;
    use aerona_core::db::Db;

    /// Every test gets its own database file. Sharing one directory between tests that
    /// run on threads in the same process is how a migration ends up racing itself.
    fn state() -> (BotState, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().expect("a temp dir for the test database");
        let mut env = std::collections::HashMap::new();
        env.insert(
            "AERONA_TOKEN".to_string(),
            "MTIzNDU2Nzg5.ABCDEFGHIJKLMNOPQRSTUVWXYZ.abcdefghijklmnopqrstuvwxyz01".to_string(),
        );
        env.insert("AERONA_PORT".to_string(), "8799".to_string());
        let cfg = Arc::new(Config::parse_env(&env, dir.path(), true).unwrap());
        let db = Db::open_path(&dir.path().join("t.db")).unwrap();
        (BotState::new(cfg, db), dir)
    }

    #[test]
    fn window_counts_only_hits_inside_the_interval() {
        let mut w = Window::default();
        assert_eq!(w.note(100, 10), 1);
        assert_eq!(w.note(105, 10), 2);
        assert_eq!(w.note(109, 10), 3);
        // 100 has fallen out of a 10-second window by t=111.
        assert_eq!(w.note(111, 10), 3);
        assert_eq!(w.note(120, 10), 2);
        assert!(!w.is_empty());
        w.reset();
        assert!(w.is_empty());
    }

    #[test]
    fn a_zero_second_window_still_counts_the_current_hit() {
        let mut w = Window::default();
        assert_eq!(
            w.note(500, 0),
            1,
            "window_secs is clamped to 1 so it cannot divide to a negative floor"
        );
    }

    #[test]
    fn cooldown_allows_once_then_throttles() {
        let (s, _keep) = state();
        let g = Snowflake(1);
        let u = Snowflake(2);
        assert_eq!(s.rate_limit_cmd(g, u, "warn", 30), Rate::Allowed);
        match s.rate_limit_cmd(g, u, "warn", 30) {
            Rate::Throttled(secs) => assert!((1..=30).contains(&secs), "got {secs}"),
            Rate::Allowed => panic!("the second call should be throttled"),
        }
        // A different command is independent, and a zero cooldown never throttles.
        assert_eq!(s.rate_limit_cmd(g, u, "ban", 30), Rate::Allowed);
        assert_eq!(s.rate_limit_cmd(g, u, "stats", 0), Rate::Allowed);
    }

    #[test]
    fn epoch_diff_reports_new_and_changed_and_drops_removed() {
        let (s, _keep) = state();
        let a = Snowflake(10);
        let b = Snowflake(20);
        s.managed.insert(a);
        s.managed.insert(b);
        assert_eq!(
            s.refresh_epochs(&[(a, 1)]),
            vec![a],
            "first sight counts as changed"
        );
        assert!(
            s.refresh_epochs(&[(a, 1)]).is_empty(),
            "an unchanged epoch is silent"
        );
        assert_eq!(s.refresh_epochs(&[(a, 2), (b, 1)]), vec![a, b]);
        // Guild b vanished from the table: it must stop being managed and lose its windows.
        let changed = s.refresh_epochs(&[(a, 2)]);
        assert_eq!(changed, Vec::new());
        assert!(
            !s.managed.contains(&b),
            "a guild with no row is no longer managed"
        );
        assert!(s.epochs.get(&b).is_none());
    }

    #[test]
    fn per_user_windows_are_isolated_per_guild() {
        let (s, _keep) = state();
        let u = Snowflake(7);
        let (g1, _) = s.note_message(Snowflake(1), u, 60);
        let (g2, u2) = s.note_message(Snowflake(2), u, 60);
        assert_eq!((g1, g2, u2), (1, 1, 1));
        let (_, u_again) = s.note_message(Snowflake(1), u, 60);
        assert_eq!(u_again, 2, "the same user in the same guild accumulates");
    }

    #[test]
    fn pruning_removes_only_expired_cooldowns() {
        let (s, _keep) = state();
        s.cooldown
            .insert((Snowflake(1), Snowflake(1), "warn".into()), now() - 5);
        s.cooldown
            .insert((Snowflake(1), Snowflake(2), "warn".into()), now() + 300);
        assert_eq!(s.prune_cooldowns(), 1);
        assert_eq!(s.cooldown.len(), 1);
    }

    #[test]
    fn procfs_readers_never_panic_and_are_sane() {
        // On Linux both must be > 0 for a live process; elsewhere 0 is the documented
        // answer. Either way the numbers have to be finite and non-negative.
        let rss = rss_kb();
        let cpu = cpu_pct();
        assert!(rss >= 0, "rss {rss}");
        assert!(cpu >= 0.0 && cpu.is_finite(), "cpu {cpu}");
        if cfg!(target_os = "linux") {
            assert!(
                rss > 100,
                "a running process reports at least 100 KB, got {rss}"
            );
        }
    }
}
