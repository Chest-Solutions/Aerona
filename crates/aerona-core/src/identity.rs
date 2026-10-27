//! Discord snowflakes and user-facing identifiers.
//!
//! A snowflake is `((timestamp_ms - DISCORD_EPOCH) << 22) | ...` - decoding it
//! gives us creation time for free, which is exactly what anti-raid needs to
//! spot freshly created alt accounts without an extra API round-trip.

use crate::error::{Error, Result};

/// Discord's epoch: 2015-01-01T00:00:00Z in milliseconds.
pub const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;

/// A validated 64-bit Discord identifier. Parsing through this type instead of
/// `u64` means a malformed ID can never reach the HTTP layer or a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Snowflake(pub u64);

impl Snowflake {
    /// Accepts 1..=20 decimal digits only. Rejects `+1`, `007`, whitespace, and
    /// anything that would silently wrap into a different ID.
    pub fn parse(raw: &str) -> Result<Self> {
        let s = raw.trim();
        if s.is_empty() || s.len() > 20 || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::Invalid("ids must be 1–20 digit numbers".into()));
        }
        // Leading zeros are rejected: `007` and `7` must never both resolve to
        // the same guild/user in a request that passed validation.
        if s.len() > 1 && s.starts_with('0') {
            return Err(Error::Invalid("ids must not have leading zeros".into()));
        }
        s.parse::<u64>()
            .map(Snowflake)
            .map_err(|_| Error::Invalid("id is out of range for a snowflake".into()))
    }

    /// Optional variant used by `PATCH` payloads where `null` means "unchanged".
    pub fn parse_opt(raw: Option<&str>) -> Result<Option<Self>> {
        match raw.map(str::trim) {
            None | Some("") | Some("null") => Ok(None),
            Some(v) => Ok(Some(Self::parse(v)?)),
        }
    }

    /// Creation timestamp of the object, in unix milliseconds.
    pub fn created_ms(self) -> i64 {
        DISCORD_EPOCH_MS + ((self.0 >> 22) as i64)
    }

    /// Creation timestamp in unix seconds.
    pub fn created_secs(self) -> i64 {
        self.created_ms() / 1000
    }

    /// Whole minutes of account age. `-1` if the clock says it is from the future
    /// (happens on VMs with a skewed clock - treat as suspicious, not impossible).
    pub fn age_minutes(self, now_ms: i64) -> i64 {
        let delta = now_ms - self.created_ms();
        if delta < 0 {
            return -1;
        }
        delta / 60_000
    }

    /// The `Default` sentinel. `0` is not a valid Discord id, so "unset", "no
    /// guild attached" and "cleared" are all represented by it - and nothing may
    /// treat it as an id.
    pub fn is_zero(self) -> bool {
        self.0 == 0
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }

    pub fn as_i64(self) -> i64 {
        self.0 as i64
    }

    pub fn as_str(self) -> String {
        self.0.to_string()
    }
}

impl std::fmt::Display for Snowflake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl serde::Serialize for Snowflake {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        // Always a JSON string: 20-digit numbers lose precision in JavaScript.
        s.serialize_str(&self.0.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for Snowflake {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Snowflake;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a discord snowflake as a string or integer")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Snowflake, E> {
                Snowflake::parse(v).map_err(|e| E::custom(e.safe_message()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Snowflake, E> {
                Ok(Snowflake(v))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Snowflake, E> {
                u64::try_from(v)
                    .map(Snowflake)
                    .map_err(|_| E::custom("snowflake must be positive"))
            }
        }
        d.deserialize_any(V)
    }
}

/// `Snowflake(0)` is the "unset" marker used by container-level `#[serde(default)]`.
/// `0` is never a real Discord id, so it can only ever match nothing in a query;
/// callers that must reject it explicitly can check `id.0 == 0`.
/// (`parse("0")` still succeeds - the API validates it separately as `not_found`
/// rather than pretending it was a malformed string.)
impl Default for Snowflake {
    fn default() -> Self {
        Snowflake(0)
    }
}

/// Opaque, URL-safe, high-entropy identifier used for API keys, sessions,
/// OAuth state and ticket references. 24 bytes → 32 base64url characters.
pub fn opaque_id(bytes: usize) -> String {
    crate::security::random_urlsafe(bytes)
}

/// Human-friendly ticket code: `AER-4F2K` style. Collision-tolerant - the
/// `(guild, reference)` unique index makes a clash a retryable conflict.
pub fn ticket_reference() -> String {
    const ALPHABET: &[u8] = b"ACDEFHJKLMNPQRTUVWXY3479"; // no I/O/0/1 ambiguity
    let raw = crate::security::random_bytes(6);
    let mut out = String::with_capacity(10);
    out.push_str("AER-");
    for b in &raw[..6] {
        out.push(ALPHABET[(*b as usize) % ALPHABET.len()] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_garbage_ids() {
        for bad in [
            "",
            " ",
            "abc",
            "1.5",
            "-1",
            "1e3",
            " +1",
            " +7",
            "007",
            &"9".repeat(21),
            "1_000",
        ] {
            assert!(Snowflake::parse(bad).is_err(), "accepted {bad:?}");
        }
        // Surrounding whitespace from a query param is fine; leading zeros are not.
        assert_eq!(
            Snowflake::parse(" 123456789012345678 ").unwrap().0,
            123456789012345678
        );
        assert_eq!(
            Snowflake::parse(" 1 ").unwrap().0,
            1,
            "benign padding is fine"
        );
        assert_eq!(
            Snowflake::parse("0").unwrap().0,
            0,
            "single zero is a legal u64, just not a real snowflake"
        );
        assert_eq!(Snowflake::parse("7").unwrap().0, 7);
    }

    #[test]
    fn age_from_snowflake() {
        // Build a snowflake for a known instant: 2026-10-23T00:00:00Z.
        let ms = 1_792_771_200_000i64;
        let id = (((ms - DISCORD_EPOCH_MS) as u64) << 22) | 1;
        let s = Snowflake(id);
        assert_eq!(s.created_ms(), ms);
        assert_eq!(s.age_minutes(ms + 5 * 60_000), 5);
        assert_eq!(s.age_minutes(ms - 1000), -1, "future ids must not look old");
    }

    #[test]
    fn serializes_as_string() {
        // 20 digits: exceeds i64/Javascript safe-integer range, so it must stay a string.
        let s = Snowflake(18_446_744_073_709_551_610);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.starts_with('"') && json.ends_with('"'), "{json}");
        let back: Snowflake = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        // also accepts a raw number
        let back2: Snowflake = serde_json::from_str("123456789").unwrap();
        assert_eq!(back2.0, 123456789);
    }

    #[test]
    fn ticket_reference_shape() {
        let r = ticket_reference();
        assert_eq!(r.len(), 10, "{r}");
        assert!(r.starts_with("AER-"));
        assert!(!r.contains(['I', 'O', '0', '1']));
        assert_ne!(r, ticket_reference());
    }

    #[test]
    fn opaque_ids_are_unique_and_long() {
        let a = opaque_id(24);
        let b = opaque_id(24);
        assert_eq!(a.len(), 32, "{a}");
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(a, b);
    }
}
