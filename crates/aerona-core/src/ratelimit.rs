//! Rate limiting primitives: a token bucket and a fixed sliding-window counter.
//!
//! Two independent jobs:
//!
//! * protecting *our* API from being hammered (and from being turned into an
//!   amplification vector against a third party), and
//! * protecting a guild from a join/message flood (anti-raid) and Discord's own
//!   429s from turning into a global convoy.
//!
//! Both are deliberately O(1) memory per key with an explicit sweep, so a
//! hostile client spraying fresh keys cannot grow us without bound.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use parking_lot::Mutex;

/// A refilling token bucket.
#[derive(Debug, Clone, Copy)]
pub struct Bucket {
    /// Sustained operations per second.
    pub rate: f64,
    /// Burst allowance.
    pub burst: u32,
    tokens: f64,
    last: Instant,
}

impl Bucket {
    pub fn new(rate: f64, burst: u32) -> Self {
        Self {
            rate: rate.max(0.0001),
            burst: burst.max(1),
            tokens: burst as f64,
            last: Instant::now(),
        }
    }

    /// Take `n` tokens if available. Returns `Err(retry_after)` when empty.
    pub fn try_take(&mut self, n: f64, now: Instant) -> Result<(), Duration> {
        let elapsed = now.duration_since(self.last).as_secs_f64();
        if elapsed > 0.0 {
            self.last = now;
            self.tokens = (self.tokens + elapsed * self.rate).min(self.burst as f64);
        }
        if self.tokens >= n {
            self.tokens -= n;
            Ok(())
        } else {
            let missing = n - self.tokens;
            Err(Duration::from_secs_f64((missing / self.rate).max(0.05)))
        }
    }

    /// Seconds until the bucket refills enough for one token (for `Retry-After`).
    /// Includes tokens accrued since the last top-up, so it never overstates.
    /// Tolerance: callers usually compute this a moment after a `try_take`, and
    /// float rounding there must not mint a spurious "wait 1s" on a full bucket.
    pub fn retry_after(&self, now: Instant) -> u32 {
        let accrued = now.duration_since(self.last).as_secs_f64() * self.rate;
        let available = (self.tokens + accrued).min(self.burst as f64);
        if available >= 1.0 - (1.0 / self.rate * 0.005) {
            return 0;
        }
        ((1.0 - available) / self.rate).ceil().max(1.0) as u32
    }
}

/// Keyed buckets with an age-out sweep. Used for "per user, per route".
pub struct BucketTable<K: std::cmp::Eq + std::hash::Hash + Clone> {
    map: DashMap<K, (Bucket, i64)>,
    rate: f64,
    burst: u32,
    /// Idle seconds after which a key is dropped on sweep.
    ttl_secs: i64,
    last_sweep: AtomicI64,
}

impl<K: std::cmp::Eq + std::hash::Hash + Clone> BucketTable<K> {
    pub fn new(rate: f64, burst: u32, ttl_secs: i64) -> Self {
        Self {
            map: DashMap::new(),
            rate,
            burst,
            ttl_secs,
            last_sweep: AtomicI64::new(0),
        }
    }

    pub fn check(&self, key: &K, cost: f64, now_epoch: i64) -> Result<(), u32> {
        let now = Instant::now();
        let mut entry = self
            .map
            .entry(key.clone())
            .or_insert_with(|| (Bucket::new(self.rate, self.burst), now_epoch));
        let outcome = entry.0.try_take(cost, now);
        entry.1 = now_epoch;
        drop(entry);
        match outcome {
            Ok(()) => {
                self.maybe_sweep(now_epoch);
                Ok(())
            }
            Err(d) => Err(d.as_secs_f64().ceil().max(1.0) as u32),
        }
    }

    /// Drop keys untouched for `ttl_secs`. Runs at most once every `ttl_secs`.
    fn maybe_sweep(&self, now_epoch: i64) {
        let prev = self.last_sweep.load(Ordering::Relaxed);
        if now_epoch - prev < self.ttl_secs.max(30) {
            return;
        }
        if self
            .last_sweep
            .compare_exchange(prev, now_epoch, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        self.map
            .retain(|_, (_, seen)| now_epoch - *seen < self.ttl_secs);
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn clear(&self) {
        self.map.clear();
    }
}

/// Fixed sliding window over `window_secs` - the shape anti-raid needs
/// ("how many joins in the last 10 seconds?"). Stores timestamps per key,
/// capped at `cap` so a flood cannot allocate without bound.
pub struct Window {
    inner: Mutex<DashMap<u64, Ring>>,
    window_secs: i64,
    cap: usize,
}

/// Fixed-capacity ring of timestamps for one key. Bounded by construction: a
/// flood cannot make us allocate, and the count is capped at the ring size.
#[derive(Debug, Clone)]
struct Ring {
    buf: Vec<i64>,
    /// Index of the oldest live sample.
    start: usize,
    len: usize,
}

impl Ring {
    fn with_cap(cap: usize) -> Self {
        Self {
            buf: vec![0; cap],
            start: 0,
            len: 0,
        }
    }

    /// Append `ts`, then evict everything outside the window. Returns the number
    /// of live samples, i.e. the current rate estimate.
    fn push_and_count(&mut self, ts: i64, window_secs: i64) -> usize {
        let cap = self.buf.len();
        if self.len < cap {
            self.buf[(self.start + self.len) % cap] = ts;
            self.len += 1;
        } else {
            // Full: overwrite the oldest sample and slide the window start.
            self.buf[self.start] = ts;
            self.start = (self.start + 1) % cap;
        }
        // Half-open window (ts - window_secs, ts]: a sample exactly window_secs
        // old has expired, so "N per Ns" can never admit N+1.
        let cutoff = ts - window_secs;
        while self.len > 0 && self.buf[self.start] <= cutoff {
            self.start = (self.start + 1) % cap;
            self.len -= 1;
        }
        self.len
    }
}

impl Window {
    pub fn new(window_secs: i64, cap: usize) -> Self {
        Self {
            inner: Mutex::new(DashMap::new()),
            window_secs: window_secs.max(1),
            cap: cap.clamp(16, 4096),
        }
    }

    /// Record an event for `key` and return how many fall inside the window.
    pub fn record(&self, key: u64, ts: i64) -> usize {
        let map = self.inner.lock();
        let w = self.window_secs;
        let mut q = map.entry(key).or_insert_with(|| Ring::with_cap(self.cap));
        let n = q.push_and_count(ts, w);
        // End the RefMut borrow before `map` (and therefore the entry) dies.
        drop(q);
        n
    }

    /// `key` is retired (e.g. guild became quiet) to keep memory flat.
    pub fn forget(&self, key: u64) {
        let map = self.inner.lock();
        map.remove(&key);
        drop(map);
    }

    pub fn len(&self) -> usize {
        let map = self.inner.lock();
        let n = map.len();
        drop(map);
        n
    }

    pub fn is_empty(&self) -> bool {
        let map = self.inner.lock();
        let n = map.is_empty();
        drop(map);
        n
    }
}

/// Exponential backoff counter for crash loops / retry storms.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    base_ms: u64,
    max_ms: u64,
    attempt: u32,
}

impl Backoff {
    pub fn new(base_ms: u64, max_ms: u64) -> Self {
        Self {
            base_ms: base_ms.max(50),
            max_ms: max_ms.max(base_ms),
            attempt: 0,
        }
    }

    pub fn next_delay(&mut self) -> Duration {
        let exp = self.attempt.min(16);
        self.attempt += 1;
        let ms = (self.base_ms.saturating_mul(1 << exp)).min(self.max_ms);
        Duration::from_millis(ms)
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_allows_burst_then_throttles() {
        let mut b = Bucket::new(1.0, 3);
        let now = Instant::now();
        for _ in 0..3 {
            assert!(b.try_take(1.0, now).is_ok());
        }
        let err = b.try_take(1.0, now).unwrap_err();
        assert!(err.as_millis() > 0);
        // After a full second of refill exactly one more fits.
        let later = now + Duration::from_millis(1005);
        assert!(b.try_take(1.0, later).is_ok());
        assert!(b.try_take(1.0, later).is_err());
    }

    #[test]
    fn bucket_caps_burst_and_refills_at_rate() {
        // burst 5, 100 tokens/s: the cap is real, and a drained bucket comes back
        // in ~10ms - `retry_after` may round up to a second, never down to zero.
        let mut b = Bucket::new(100.0, 5);
        let t = Instant::now();
        assert!(b.try_take(5.0, t).is_ok());
        assert!(b.try_take(1.0, t).is_err());
        assert_eq!(
            b.retry_after(t),
            1,
            "must not advertise zero wait while empty"
        );
        assert!(b.try_take(5.0, t + Duration::from_millis(1_000)).is_ok());
        // A full bucket must not advertise a wait.
        assert_eq!(b.retry_after(t + Duration::from_secs(10)), 0);
        // Idle time beyond the burst window never mints more than `burst`.
        let mut c = Bucket::new(100.0, 5);
        let far = Instant::now() + Duration::from_secs(10_000);
        assert!(c.try_take(5.0, far).is_ok());
        assert!(c.try_take(1.0, far).is_err());
    }

    #[test]
    fn table_sweeps_idle_keys() {
        let table: BucketTable<String> = BucketTable::new(1.0, 2, 30);
        for i in 0..50 {
            let _ = table.check(&format!("k{i}"), 1.0, 1_000);
        }
        assert_eq!(table.len(), 50);
        // Everything is now "old": next sweep with a much later clock must drop them.
        let _ = table.check(&"fresh".to_string(), 1.0, 1_000 + 600);
        assert!(table.len() <= 1, "sweep failed, len={}", table.len());
    }

    #[test]
    fn window_counts_only_in_window() {
        let w = Window::new(10, 64);
        assert_eq!(w.record(7, 100), 1);
        for i in 1..10 {
            // Every event from 100..=109 is within 10s of each other.
            assert_eq!(w.record(7, 100 + i), i as usize + 1);
        }
        // At ts 111 the cutoff is 101: 100 and 101 have expired, 102..=111 remain.
        assert_eq!(w.record(7, 111), 9);
        // Fresh key: exactly window_secs old ⇒ expired; one second less ⇒ live.
        assert_eq!(w.record(11, 200), 1);
        assert_eq!(w.record(11, 210), 1, "the 200 sample is exactly 10s back");
        assert_eq!(w.record(12, 300), 1);
        assert_eq!(w.record(12, 309), 2, "9s back is still inside a 10s window");
        assert_eq!(w.record(9, 500), 1, "other key independent");
        // Keys 11, 12 and 9 are still tracked; forgetting 7 must not disturb them.
        w.forget(7);
        assert_eq!(w.len(), 3);
        assert!(w.record(7, 900) == 1, "forgotten key restarts from zero");
    }

    #[test]
    fn window_is_bounded() {
        let w = Window::new(60, 16);
        for i in 0..100_000 {
            let c = w.record(1, 1_000 + i / 100);
            assert!(c <= 16, "count grew past capacity: {c}");
        }
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn backoff_caps() {
        let mut b = Backoff::new(500, 4_000);
        let mut prev = Duration::ZERO;
        for _ in 0..12 {
            let d = b.next_delay();
            assert!(d <= Duration::from_millis(4_000));
            assert!(d >= prev || d == Duration::from_millis(4_000));
            prev = d;
        }
        b.reset();
        assert_eq!(b.attempt(), 0);
        assert_eq!(b.next_delay(), Duration::from_millis(500));
    }
}
