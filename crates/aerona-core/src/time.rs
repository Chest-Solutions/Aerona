//! Second-resolution time helpers.
//!
//! Aerona stores every timestamp as `INTEGER` epoch-seconds rather than using
//! `TIMESTAMP`/`DATETIME` text. It is 4 bytes instead of 27, compares correctly
//! with plain `<` / `>`, needs no timezone interpretation, and keeps us from
//! pulling a date library (and its transitive MSRV churn) into the tree.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch. Monotonic enough for config and audit data; we
/// never use it for interval measurement (those use `Instant`).
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// Milliseconds since the Unix epoch — used for latency and sub-second windows.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// `2026-10-23 14:05:09Z` in UTC, computed with civil-from-days so we need no
/// calendar crate. Used only for human-facing rendering.
pub fn format(ts: i64) -> String {
    if ts <= 0 {
        return "—".into();
    }
    let days = ts.div_euclid(86_400);
    let sod = ts.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}Z")
}

/// Compact duration for dashboards: `3d 4h`, `12m 09s`, `41s`.
pub fn human_delta(seconds: i64) -> String {
    let s = seconds.max(0);
    let (d, h, m, sec) = (s / 86_400, (s % 86_400) / 3600, (s % 3600) / 60, s % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{sec}s"),
        (0, 0, _) => format!("{m:02}:{sec:02}"),
        (0, _, _) => format!("{h}h {m:02}m"),
        _ => format!("{d}d {h}h"),
    }
}

/// Howard Hinnant's `civil_from_days`, public-domain variant.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
        // 2000-02-29 (leap day) is day 11016; 2026-01-01 is day 20454.
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_454), (2026, 1, 1));
        // Pre-epoch must work too (audit rows never go here, but the maths must hold).
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn formats_utc() {
        assert_eq!(format(1_766_624_400), "2025-12-25 01:00:00Z");
        assert_eq!(format(0), "—");
    }

    #[test]
    fn human_delta_tiers() {
        assert_eq!(human_delta(0), "0s");
        assert_eq!(human_delta(41), "41s");
        assert_eq!(human_delta(129), "02:09");
        assert_eq!(human_delta(3_725), "1h 02m");
        assert_eq!(human_delta(90_000), "1d 1h");
        assert_eq!(human_delta(-5), "0s");
    }

    #[test]
    fn now_is_sane() {
        let t = now();
        assert!(t > 1_700_000_000 && t < 4_000_000_000);
        assert!(now_ms() > t * 1000 - 5000);
    }
}
