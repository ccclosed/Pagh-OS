//! RFC 6125 hostname matching for the TLS server-certificate verifier
//! (issue #14 series) — pure `core` + `alloc`, self-contained, panic-free.
//!
//! The verifier calls [`hostname_matches`] against every `dNSName` SAN entry
//! of the server certificate and [`ip_matches`] against `iPAddress` entries
//! when the connection target was an IP literal ([`is_ip_literal`]).
//!
//! Supported subset (deliberately strict — anything outside it fails closed,
//! i.e. NO match, never a lax match):
//!
//!   * case-insensitive ASCII comparison per label (`[u8]::eq_ignore_ascii_case`);
//!   * wildcard `*` ONLY as the entire left-most label of the SAN pattern,
//!     matching exactly one label (`*.example.com` matches `a.example.com`,
//!     NOT `b.a.example.com`, NOT `example.com`);
//!   * partial-label wildcards (`w*.example.com`) and non-left-most
//!     wildcards (`www.*.com`) are unsupported → never match;
//!   * a `*` inside the HOST side is bogus input → never match;
//!   * a single trailing dot is tolerated on either side (DNS FQDN root);
//!     empty labels (`a..b.com`, leading/trailing dots beyond that) → no match;
//!   * non-ASCII bytes on either side → no match. IDN must reach us as
//!     A-labels (xn--…); a CA that emits raw UTF-8 dNSName entries is outside
//!     the supported surface.
//!
//! Known limitation (documented, fail-open only in the sense of availability):
//! `*.com`-style wildcards that a public-suffix-aware client would refuse are
//! accepted here — no PSL is carried in the kernel. Real CAs cannot issue
//! such certificates, so this does not weaken the default-mirror trust path.

#![allow(dead_code)] // consumed by the verifier later in the issue #14 series.

use alloc::vec::Vec;

/// Split a dotted name into labels; `None` when it carries an empty label
/// (`a..b.com`, leading dot, or a trailing dot beyond the one tolerated).
fn labels(name: &[u8]) -> Option<Vec<&[u8]>> {
    let ls: Vec<&[u8]> = name.split(|&b| b == b'.').collect();
    if ls.iter().any(|l| l.is_empty()) {
        return None;
    }
    Some(ls)
}

/// One tolerated trailing dot (RFC 6125 §6.4.2 allows the FQDN root form).
fn strip_trailing_dot(name: &[u8]) -> &[u8] {
    match name.strip_suffix(b".") {
        Some(s) => s,
        None => name,
    }
}

/// Printable-ASCII-only gate: non-ASCII (raw UTF-8 IDN) or spaces/controls
/// fail closed.
fn is_ascii_name(name: &[u8]) -> bool {
    name.iter().all(|&b| (0x21..=0x7e).contains(&b))
}

/// Case-insensitive equality of two label lists (all ASCII by this point).
fn labels_eq(a: &[&[u8]], b: &[&[u8]]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// Match a DNS `host` against a certificate `dNSName` SAN entry
/// (RFC 6125 §6.4). See the module docs for the supported subset.
pub fn hostname_matches(host: &[u8], san: &[u8]) -> bool {
    if !is_ascii_name(host) || !is_ascii_name(san) {
        return false;
    }
    let host = strip_trailing_dot(host);
    let san = strip_trailing_dot(san);
    let (hl, sl) = match (labels(host), labels(san)) {
        (Some(h), Some(s)) => (h, s),
        _ => return false,
    };
    if hl.is_empty() || sl.is_empty() {
        return false;
    }

    // A `*` on the HOST side is bogus input: the host is the name being
    // VERIFIED, never a pattern. This must hold before the wildcard branch —
    // otherwise host `a.*.example.com` would sail through the SAN pattern
    // `*.example.com` (only hl[1..] is compared there, hl[0] unchecked).
    if hl.iter().any(|l| l.contains(&b'*')) {
        return false;
    }

    // Wildcard: ONLY the entire left-most SAN label, matching exactly one
    // host label. A `*` anywhere else on the SAN side never matches either.
    if sl[0] == b"*" {
        return hl.len() == sl.len() && labels_eq(&hl[1..], &sl[1..]);
    }
    if sl.iter().any(|l| l.contains(&b'*')) {
        return false;
    }
    labels_eq(&hl, &sl)
}

/// Classify a connection-target `host` as an IPv4 literal (four dot-separated
/// decimal octets, each 0..=255, at most three digits).
pub fn is_ip_literal(host: &[u8]) -> bool {
    let parts: Vec<&[u8]> = host.split(|&b| b == b'.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.iter().all(u8::is_ascii_digit)
                && p.iter().fold(0u32, |v, &d| v * 10 + (d - b'0') as u32) <= 255
        })
}

/// Match an IP connection target against an `iPAddress` SAN entry: both are
/// raw octets (4 for IPv4, 16 for IPv6) compared byte-for-byte.
pub fn ip_matches(host: &[u8], san: &[u8]) -> bool {
    (host.len() == 4 || host.len() == 16) && host == san
}

// ───────────────────────────── self-tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn m(host: &str, san: &str) -> bool {
        hostname_matches(host.as_bytes(), san.as_bytes())
    }

    #[test]
    fn exact_and_case() {
        assert!(m("www.example.com", "www.example.com"));
        assert!(m("WWW.EXAMPLE.COM", "www.example.com"));
        assert!(m("www.example.com", "WWW.Example.Com"));
        assert!(!m("www.example.com", "www.example.org"));
        assert!(!m("www.example.com", "example.com"));
    }

    #[test]
    fn wildcard_leftmost_single_label() {
        assert!(m("a.example.com", "*.example.com"));
        assert!(m("A.EXAMPLE.COM", "*.example.com"));
        // Exactly ONE label: no deeper, no shallower.
        assert!(!m("b.a.example.com", "*.example.com"));
        assert!(!m("example.com", "*.example.com"));
        // Wildcard itself is case-insensitive.
        assert!(m("a.example.com", "*.EXAMPLE.com"));
    }

    #[test]
    fn unsupported_wildcards_never_match() {
        // Partial-label wildcard.
        assert!(!m("www.example.com", "w*.example.com"));
        // Non-left-most wildcard.
        assert!(!m("www.example.com", "www.*.com"));
        assert!(!m("www.example.com", "www.example.*"));
        // Bogus wildcard on the host side — INCLUDING against a wildcard
        // SAN whose tail lines up: the host is verified, never a pattern.
        assert!(!m("*.example.com", "*.example.com"));
        assert!(!m("a.*.example.com", "*.example.com"));
        assert!(!m("*.example.com", "a.example.com"));
        assert!(!m("a.*.com", "a.*.com"));
    }

    #[test]
    fn trailing_dot_and_empty_labels() {
        assert!(m("www.example.com.", "www.example.com"));
        assert!(m("www.example.com", "www.example.com."));
        assert!(m("WWW.EXAMPLE.COM.", "www.EXAMPLE.com."));
        // Double dots / leading dots produce empty labels → no match.
        assert!(!m("www..example.com", "www.example.com"));
        assert!(!m(".www.example.com", "www.example.com"));
        assert!(!m("www.example.com", ".www.example.com"));
        // Empty inputs.
        assert!(!m("", ""));
        assert!(!m("a", ""));
    }

    #[test]
    fn non_ascii_fails_closed() {
        // Raw UTF-8 IDN must not match anything (A-labels are the surface).
        assert!(!m("пример.рф", "пример.рф"));
        assert!(!hostname_matches(b"a\xffb.com", b"a\xffb.com"));
        // A-labels are fine.
        assert!(m("xn--e1afmkfd.xn--p1ai", "*.xn--p1ai"));
    }

    #[test]
    fn ip_classification_and_match() {
        assert!(is_ip_literal(b"10.0.2.15"));
        assert!(is_ip_literal(b"127.0.0.1"));
        assert!(!is_ip_literal(b"999.1.1.1"));
        assert!(!is_ip_literal(b"1.2.3.4.5"));
        assert!(!is_ip_literal(b"1.2.3"));
        assert!(!is_ip_literal(b"example.com"));
        assert!(!is_ip_literal(b""));

        // iPAddress entries compare as raw octets, never as text.
        assert!(ip_matches(&[10, 0, 2, 15], &[10, 0, 2, 15]));
        assert!(!ip_matches(&[10, 0, 2, 15], &[10, 0, 2, 16]));
        // An IPv4 host never matches a 16-byte (IPv6) SAN entry.
        assert!(!ip_matches(&[10, 0, 2, 15], &[0u8; 16]));
        // Text forms must NOT be compared through ip_matches.
        assert!(!ip_matches(b"10.0.2.15", b"10.0.2.15"));
    }
}
