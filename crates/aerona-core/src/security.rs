//! Cryptographic and hygiene primitives.
//!
//! Threats this module exists to defuse:
//!
//! * **Timing oracles** on secrets - every comparison of a token/key/code is
//!   constant time, so no attacker can bisect a 256-bit key by measuring
//!   response latency.
//! * **Length-extension and cross-tenant forgery** - tokens are keyed with an
//!   HMAC over a per-install secret pepper, never bare-hashed.
//! * **Log injection** - anything that reaches a log line is scrubbed of
//!   newlines/controls and truncated.
//! * **Secret bleed** - `Secret` zeroes its memory on drop and redacts itself in
//!   `Debug`, so a stray `{:?}` cannot write a bot token to stdout.

use std::fmt;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

/// Bytes of entropy for opaque identifiers (256 bits).
pub const ID_BYTES: usize = 32;

/// Prefix for dashboard API keys. Grep-able, so a leaked key can be identified
/// and revoked by prefix alone in a log.
pub const KEY_PREFIX: &str = "aer_live_";

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

/// A string that must never be printed and must be wiped when released.
pub struct Secret(std::mem::ManuallyDrop<String>);

impl Secret {
    pub fn new(v: impl Into<String>) -> Self {
        Self(std::mem::ManuallyDrop::new(v.into()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Borrow for an outbound request. Callers must not store, log or clone this
    /// into a struct that outlives the request.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret(<redacted len={}>)", self.0.len())
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // SAFETY: `Secret` is never leaked (no `into_inner`/`Mem::forget`), so
        // this is the only take of the value, and it happens exactly on drop.
        unsafe { std::mem::ManuallyDrop::take(&mut self.0) }.zeroize();
    }
}

// ---------------------------------------------------------------------------
// Randomness
// ---------------------------------------------------------------------------

/// CSPRNG bytes. `rand::thread_rng` is a-ChaCha20 backed; there is no reason to
/// reach for anything else and every reason not to roll our own.
pub fn random_bytes(n: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut v = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut v);
    v
}

pub fn random_urlsafe(n: usize) -> String {
    base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        random_bytes(n),
    )
}

/// Uniform integer in `0..len` without modulo bias (rejection sampling).
pub fn random_below(len: usize) -> usize {
    use rand::Rng;
    assert!(len > 0, "random_below(0)");
    rand::thread_rng().gen_range(0..len)
}

/// Six digit numeric pairing code, for the first-run admin bootstrap.
pub fn numeric_code(digits: usize) -> String {
    (0..digits.clamp(4, 8))
        .map(|_| (random_below(10) as u8 + b'0') as char)
        .collect()
}

// ---------------------------------------------------------------------------
// Hashing
// ---------------------------------------------------------------------------

/// SHA-256 of arbitrary bytes, lowercase hex.
pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Keyed digest used for everything we might have to compare later: API keys,
/// session tokens, OAuth state. The pepper means a database dump alone is not
/// enough to recover or forge a credential.
pub fn token_mac(pepper: &[u8], token: &str) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(pepper).expect("hmac accepts any key length");
    mac.update(token.as_bytes());
    let out = mac.finalize().into_bytes();
    let mut key = [0u8; 32];
    key.copy_from_slice(&out);
    key
}

pub fn token_mac_hex(pepper: &[u8], token: &str) -> String {
    hex::encode(token_mac(pepper, token))
}

/// Constant-time comparison of two equal-length-hex digests. Length mismatch is
/// a hard `false` (and cannot be timed to learn anything: lengths are public).
pub fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Constant-time comparison of a candidate token against a stored MAC, without
/// ever materialising the expected value as a string.
pub fn verify_token(pepper: &[u8], candidate: &str, stored_mac_hex: &str) -> bool {
    let expected = token_mac(pepper, candidate);
    let given = match hex::decode(stored_mac_hex) {
        Ok(v) if v.len() == 32 => v,
        _ => return false,
    };
    expected.as_slice().ct_eq(&given).into()
}

/// Derive a 32-byte pepper from whatever entropy the operator supplied. We use
/// HKDF-Extract (HMAC over a fixed label) rather than trusting the raw string,
/// so a low-entropy secret still produces a uniformly distributed key.
pub fn derive_pepper(material: &str) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(b"aerona/dashboard/pepper/v1")
        .expect("fixed key length is valid");
    mac.update(material.as_bytes());
    let out = mac.finalize().into_bytes();
    let mut k = [0u8; 32];
    k.copy_from_slice(&out);
    k
}

/// HMAC-SHA256 over a canonical payload, base64url - used for the stateless half
/// of the session CSRF token binding (no signing of user data, only of bindings).
pub fn sign(pepper: &[u8], payload: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(pepper).expect("hmac accepts any key length");
    mac.update(payload.as_bytes());
    base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        mac.finalize().into_bytes(),
    )
}

pub fn verify_signature(pepper: &[u8], payload: &str, signature_b64url: &str) -> bool {
    let expected = sign(pepper, payload);
    // Both sides are base64url without padding: fixed length for a fixed input.
    ct_eq(&expected, signature_b64url)
}

// ---------------------------------------------------------------------------
// Hygiene
// ---------------------------------------------------------------------------

/// Replace control characters and collapse whitespace so user-supplied strings
/// can never forge a log line, break out of a table cell, or smuggle an ANSI
/// escape into a terminal.
pub fn scrub(input: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(input.len().min(max_chars * 3));
    let mut prev_space = false;
    for ch in input.chars() {
        if out.chars().count() >= max_chars {
            out.push('…');
            break;
        }
        if ch == '\x1b' {
            out.push_str("<esc>");
            prev_space = false;
            continue;
        }
        if ch.is_control() || ch == '\u{202e}' || ch == '\u{200e}' || ch == '\u{200f}' {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
            continue;
        }
        // Homoglyph-ish bidi/tricks are rare; stripping the obvious ones is cheap.
        if ch == ' ' {
            if prev_space {
                continue;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

/// [`scrub`] for the two places an operator is allowed to break a line: an embed body and a
/// ticket card's intro. Each line is filtered on its own and the block is capped in lines as well
/// as characters, because a settings document that accepts unlimited lines turns a card into a
/// wall. Blank lines are dropped rather than kept: a paragraph break in Discord is one `\n`, and
/// pasting text with two of them should not double the height of a card.
pub fn scrub_block(input: &str, max_chars: usize) -> String {
    const MAX_LINES: usize = 20;
    const LINE_MAX: usize = 300;
    let mut out = String::new();
    for line in input.lines().take(MAX_LINES) {
        let one = scrub(line, LINE_MAX);
        if one.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&one);
        if out.chars().count() >= max_chars {
            let cut = out
                .char_indices()
                .nth(max_chars)
                .map(|(i, _)| i)
                .unwrap_or(out.len());
            out.truncate(cut);
            break;
        }
    }
    out
}

/// Like [`scrub`], but truncates by *characters* with no ellipsis - for stored
/// content where an appended `…` would corrupt the text (transcripts, notes).
pub fn clean(input: &str, max_chars: usize) -> String {
    let mut s = scrub(input, max_chars);
    if s.ends_with('…') {
        s.pop();
    }
    s
}

/// Escape the five SQLite-string-dangerous characters for use inside a `LIKE`
/// pattern (we still bind the value; this only neutralises `%` and `_` wildcards).
pub fn escape_like(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    for ch in pattern.chars() {
        match ch {
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            c => out.push(c),
        }
    }
    out
}

/// Normalise a client IP for rate-limit keys and audit rows. Strips the port,
/// lowercases IPv6, and collapses `::ffff:a.b.c.d` to `a.b.c.d` so an attacker
/// cannot dodge a limiter by alternating v4-mapped forms.
pub fn normalize_ip(raw: &str) -> String {
    let raw = raw.trim();
    // `[::1]:8443` and `::ffff:1.2.3.4:8443` are the shapes that actually arrive.
    let host = if let Some(rest) = raw.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        let h = raw.split('%').next().unwrap_or("");
        match h.rsplit_once(':') {
            Some((head, port))
                if port.bytes().all(|b| b.is_ascii_digit()) && !head.contains(':') =>
            {
                head
            }
            _ => h,
        }
    };
    let host = host.split('%').next().unwrap_or(host);
    if let Some(v4) = host.strip_prefix("::ffff:") {
        if v4.contains('.') {
            return v4.to_ascii_lowercase();
        }
    }
    if host.contains(':') {
        // Keep only the /64 for v6 limits: providers hand out whole subnets.
        // Keep the leading 4 hextets ("the /64"): providers hand out whole
        // subnets to one customer, so per-address limits on v6 are trivially
        // bypassed. Splitting on `::` first makes collapsed forms fold to a
        // single bucket (`::1` → `::*`) instead of leaking the suffix.
        let head: Vec<&str> = host
            .split("::")
            .next()
            .unwrap_or("")
            .split(':')
            .take(4)
            .collect();
        return format!("{}::*", head.join(":"));
    }
    host.to_ascii_lowercase()
}

/// Parse `user:secret` style credentials, tolerant of `=`/URL-ish separators and
/// surrounding quotes, and never echo the secret back on failure.
pub fn split_credential(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim().trim_matches('"').trim_matches('\'');
    let sep = [':', ',', ' ', '\t']
        .iter()
        .find_map(|c| trimmed.find(*c))?;
    let (a, b) = trimmed.split_at(sep);
    let b = b[1..].trim_start_matches([':', ',', ' ', '\t']);
    if a.trim().is_empty() || b.is_empty() {
        return None;
    }
    Some((a.trim().to_string(), b.to_string()))
}

/// A CSP + header set is only as strong as its strictest policy; we keep the
/// literal in one place so the Rust server and the dashboard's meta tag agree.
///
/// No `upgrade-insecure-requests`, deliberately. That directive puts the page in
/// "protected mode", in which a browser rewrites *every* `http://` subresource to
/// `https://` - including `/assets/index-*.js` and `/api/v1/...` on the origin the page
/// itself was served from. A game panel gives you a bare TCP port with no TLS in front of
/// it, which is where most of these installs live, and there the upgrade turns a working
/// dashboard into a blank page whose every request dies with `ERR_SSL_PROTOCOL_ERROR` -
/// the same symptom as reaching out to `https://` on the http port. It also protects
/// nothing here: every source above is `'self'`, `data:`, or an explicit `https://` host,
/// so there is no mixed content to upgrade. If you terminate TLS in front of Aerona,
/// `AERONA_BASE_URL` should say `https://` and the browser is already in a secure context.
pub fn csp_header(nonce: Option<&str>) -> String {
    let script = match nonce {
        Some(n) => format!("'self' 'nonce-{n}'"),
        None => "'self'".to_string(),
    };
    format!(
        "default-src 'self'; \
         script-src {script}; \
         style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: https://cdn.discordapp.com; \
         font-src 'self'; \
         connect-src 'self' https://discord.com https://cdn.discordapp.com; \
         form-action 'self'; frame-ancestors 'none'; base-uri 'self'; \
         object-src 'none'; manifest-src 'self'; worker-src 'none'"
    )
}

/// Headers that must appear on *every* response, success or error.
/// Headers that must appear on *every* response, success or error.
///
/// `secure` is whether the install is reached over a *trustworthy* origin in the browser's
/// sense - https, or localhost. It gates `Cross-Origin-Opener-Policy` alone, and the reason is
/// the operator's console rather than the threat model: on a plain-http origin a browser ignores
/// that header and prints a line saying that it is ignoring it, once per document. A control that
/// can never apply is noise, and the common Aerona deployment is a tunnel in front of
/// `http://127.0.0.1:25154`. An https install still gets the header.
pub fn security_headers(nonce: Option<&str>, secure: bool) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("content-security-policy", csp_header(nonce)),
        (
            "strict-transport-security",
            "max-age=63072000; includeSubDomains; preload".into(),
        ),
        ("x-content-type-options", "nosniff".into()),
        ("x-frame-options", "DENY".into()),
        ("referrer-policy", "no-referrer".into()),
        (
            "permissions-policy",
            // A name the browser does not implement is not a denied capability - it is a console
            // error ("Unrecognized feature '…' in 'Permissions-Policy'") once per page load, for
            // a policy the parser dropped. The platform retired half of the original list:
            // `document-domain` was reverted, and `sync-xhr`, `ambient-light-sensor`, `battery`,
            // `navigation-override` and the two `execution-while-*` directives no longer exist.
            // Anything *else* you see in that message came from a proxy in front of Aerona adding
            // its own directives, which is not ours to name or to audit here.
            "accelerometer=(), autoplay=(), camera=(), clipboard-read=(), clipboard-write=(), \
             cross-origin-isolated=(), display-capture=(), encrypted-media=(), fullscreen=(), \
             geolocation=(), gyroscope=(), hid=(), idle-detection=(), local-fonts=(), \
             magnetometer=(), microphone=(), midi=(), payment=(), picture-in-picture=(), \
             publickey-credentials-get=(), screen-wake-lock=(), serial=(), usb=(), web-share=(), \
             window-management=(), xr-spatial-tracking=()"
                .into(),
        ),
        ("cross-origin-resource-policy", "same-origin".into()),
        ("x-dns-prefetch-control", "off".into()),
        ("x-permitted-cross-domain-policies", "none".into()),
        ("vary", "Accept-Encoding".into()),
    ];
    if secure {
        headers.push(("cross-origin-opener-policy", "same-origin".into()));
    }
    headers
}

/// `Cache-Control` for API responses: never store, never share, always revalidate.
pub const API_CACHE_CONTROL: &str = "no-store, no-cache, must-revalidate, private, max-age=0";

#[cfg(test)]
mod tests {
    #[test]
    fn scrub_block_keeps_line_breaks_and_nothing_else() {
        assert_eq!(scrub_block("one\ntwo", 100), "one\ntwo");
        assert_eq!(scrub_block("a\u{202e}b\n\u{7}x", 100), "a b\nx");
        assert_eq!(
            scrub_block("  \n\n", 100),
            "",
            "blank lines are not content"
        );
        let long = scrub_block(&"y".repeat(500), 50);
        assert_eq!(long.chars().count(), 50, "the whole block, not each line");
        let many = (1..=25)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(scrub_block(&many, 500).lines().count(), 20);
    }

    use super::*;

    #[test]
    fn secret_redacts_and_zeroizes() {
        let s = Secret::new("Bot.abc123.supersecret");
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("abc123"), "{dbg}");
        assert_eq!(format!("{s}"), "<redacted>");
        assert_eq!(s.expose(), "Bot.abc123.supersecret");
    }

    #[test]
    fn token_roundtrip_and_rejection() {
        let pepper = derive_pepper("install-secret");
        let tok = random_urlsafe(ID_BYTES);
        let stored = token_mac_hex(&pepper, &tok);
        assert!(verify_token(&pepper, &tok, &stored));
        assert!(!verify_token(&pepper, &format!("{tok}x"), &stored));
        assert!(!verify_token(&derive_pepper("other"), &tok, &stored));
        assert!(!verify_token(&pepper, &tok, "not-hex"));
        assert!(
            !verify_token(&pepper, &tok, &"ab".repeat(8)),
            "wrong length mac"
        );
    }

    #[test]
    fn mac_is_not_bare_sha_and_is_stable() {
        let pepper = derive_pepper("p1");
        let a = token_mac_hex(&pepper, "same");
        assert_eq!(a, token_mac_hex(&pepper, "same"));
        assert_ne!(a, sha256_hex("same"), "must be keyed");
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn signatures_verify() {
        let pepper = derive_pepper("p");
        let sig = sign(&pepper, "session|42|1700000000");
        assert!(verify_signature(&pepper, "session|42|1700000000", &sig));
        assert!(!verify_signature(&pepper, "session|43|1700000000", &sig));
        assert!(!sig.contains('='), "must be unpadded base64url");
    }

    #[test]
    fn scrub_blocks_log_injection() {
        let nasty = "alice\u{0a}INFO fake daemon\u{0d}[31mred\u{1b}x\u{202e}y";
        let clean = scrub(nasty, 64);
        assert!(!clean.contains('\n') && !clean.contains('\r'));
        assert!(!clean.contains('\u{1b}') && !clean.contains('\u{202e}'));
        assert!(clean.contains("<esc>"));
        assert_eq!(scrub("a   b", 10), "a b");
        assert_eq!(scrub("abcd", 2), "ab…");
    }

    #[test]
    fn clean_truncates_without_ellipsis() {
        assert_eq!(clean("abcd", 2), "ab");
        assert_eq!(clean("ok", 10), "ok");
        assert_eq!(clean("a\nb", 10), "a b");
        // Character-based, not byte-based: an emoji must not consume the budget twice.
        assert_eq!(clean("🇦🇧🇨🇩", 2), "🇦🇧");
    }

    #[test]
    fn like_wildcards_escaped() {
        assert_eq!(escape_like("100%_x\\y"), "100\\%\\_x\\\\y");
    }

    #[test]
    fn ip_normalization() {
        assert_eq!(normalize_ip("203.0.113.9:5511"), "203.0.113.9");
        assert_eq!(normalize_ip("::ffff:203.0.113.9"), "203.0.113.9");
        assert_eq!(
            normalize_ip("2001:0db8:1111:2222:3333::9"),
            "2001:0db8:1111:2222::*"
        );
        // Loopback collapses to a /64 too: consistent keys beat cleverness here.
        assert_eq!(
            normalize_ip("[::1]:8080"),
            "::*",
            "loopback folds to one bucket"
        );
        assert_eq!(normalize_ip("fe80::1%eth0"), "fe80::*");
        // No port, IPv4 stays intact.
        assert_eq!(normalize_ip("198.51.100.7"), "198.51.100.7");
    }

    #[test]
    fn credential_splitting() {
        assert_eq!(
            split_credential("user123:sec456").unwrap(),
            ("user123".into(), "sec456".into())
        );
        // tolerant of the "user , secret" style people paste from a dashboard
        assert_eq!(split_credential("u , s").unwrap(), ("u".into(), "s".into()));
        assert_eq!(
            split_credential("  u1:s1  ").unwrap(),
            ("u1".into(), "s1".into())
        );
        assert!(split_credential("onlyuser").is_none());
        assert!(split_credential("  :x").is_none());
        assert!(split_credential("u:").is_none());
    }

    #[test]
    fn codes_have_expected_shape() {
        let c = numeric_code(6);
        assert_eq!(c.len(), 6);
        assert!(c.bytes().all(|b| b.is_ascii_digit()));
        assert_ne!(numeric_code(6).len(), 4);
    }

    #[test]
    fn opener_policy_is_only_claimed_where_it_applies() {
        let names = |secure: bool| {
            security_headers(None, secure)
                .into_iter()
                .map(|(n, _)| n)
                .collect::<Vec<_>>()
        };
        assert!(names(true).contains(&"cross-origin-opener-policy"));
        assert!(!names(false).contains(&"cross-origin-opener-policy"));
        // Everything else is unconditional: a plain-http install still refuses framing, sniffing
        // and cross-origin reads. What it cannot get is *isolation*, and the browser says so once
        // per document - which is the whole reason the header is dropped rather than kept.
        for n in [
            "content-security-policy",
            "x-content-type-options",
            "referrer-policy",
            "permissions-policy",
        ] {
            assert!(
                names(false).contains(&n),
                "{n} missing on a plain-http origin"
            );
        }
    }

    #[test]
    fn permissions_policy_denies_only_features_that_exist() {
        // The message this keeps out of the operator's console is `Unrecognized feature '…' in
        // 'Permissions-Policy' header: feature is not present in spec` - once per page load per
        // bogus name. Checked against a list of retired features rather than against a copy of the
        // header: a self-comparing test is exactly what let them in.
        let policy = security_headers(None, true)
            .into_iter()
            .find(|(n, _)| *n == "permissions-policy")
            .expect("permissions-policy header")
            .1;
        for retired in [
            "document-domain",
            "sync-xhr",
            "ambient-light-sensor",
            "battery",
            "navigation-override",
            "execution-while-not-rendered",
            "execution-while-out-of-viewport",
            "interest-cohort",
            "speaker",
            "vr",
        ] {
            assert!(
                !policy.contains(retired),
                "`{retired}` is not a feature a current browser parses: a console error, not a control"
            );
        }
        for live in [
            "camera",
            "microphone",
            "geolocation",
            "payment",
            "fullscreen",
            "usb",
            "clipboard-write",
        ] {
            assert!(
                policy.contains(&format!("{live}=()")),
                "{live} is live and should be denied"
            );
        }
        assert!(
            policy.split(", ").all(|d| d.ends_with("=()")),
            "every directive must deny the feature: {policy}"
        );
    }

    #[test]
    fn csp_covers_the_dangerous_directives() {
        let csp = csp_header(None);
        for d in [
            "default-src 'self'",
            "frame-ancestors 'none'",
            "object-src 'none'",
            "base-uri 'self'",
        ] {
            assert!(csp.contains(d), "missing {d} in {csp}");
        }
        assert!(csp_header(Some("abc")).contains("'nonce-abc'"));
        assert!(!csp_header(None).contains("unsafe-eval"));
        assert!(!csp_header(Some("n")).contains("http://"));
        // Locked in because it looks like a hardening miss and is the opposite:
        // `upgrade-insecure-requests` puts the page in protected mode, where the browser
        // rewrites *same-origin* `http://` requests to `https://`. A game panel allocates a
        // bare TCP port with no TLS behind it, so the directive turns a working install into
        // a blank page whose every asset and API call dies with ERR_SSL_PROTOCOL_ERROR.
        // Nothing is lost by omitting it: every source above is 'self', `data:`, or an
        // explicit `https://` host, so there is no mixed content to upgrade.
        assert!(
            !csp.contains("upgrade-insecure-requests"),
            "an http deployment must still be able to load its own assets: {csp}"
        );
    }
}
