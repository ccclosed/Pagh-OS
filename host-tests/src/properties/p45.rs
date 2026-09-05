// Feature: TLS server authentication (issue #14), Property 45: the RFC 6125
// hostname/SAN matcher the verifier uses to authorize the connection —
//   * exact SAN entries match case-insensitively and ONLY when the label
//     lists are equal;
//   * a left-most `*` wildcard matches EXACTLY one label: constructed
//     host/pattern pairs accept, and any label-count change rejects;
//   * fail-closed by construction: a `*` anywhere except as the WHOLE
//     left-most SAN label (partial wildcards, deeper wildcards, wildcards on
//     the host side) never matches; non-ASCII never matches;
//   * IP literals classify per IPv4 rules and iPAddress entries compare as
//     raw octets only (never text, never across lengths).

use crate::hostname::{hostname_matches, ip_matches, is_ip_literal};
use proptest::prelude::*;

/// Random DNS label.
fn label() -> impl proptest::strategy::Strategy<Value = String> {
    proptest::string::string_regex("[a-z][a-z0-9-]{0,7}").unwrap()
}

/// Random 1..=4-label DNS name.
fn dns_name() -> impl proptest::strategy::Strategy<Value = String> {
    proptest::collection::vec(label(), 1..=4).prop_map(|ls| ls.join("."))
}

proptest! {
    /// Case is irrelevant: upper-/lower-casing either side of an EQUAL pair
    /// keeps the match, and unequal names never match regardless of case.
    #[test]
    fn case_insensitive_exact_match(name in dns_name()) {
        let upper = name.to_ascii_uppercase();
        let lower = name.to_ascii_lowercase();
        prop_assert!(hostname_matches(name.as_bytes(), upper.as_bytes()));
        prop_assert!(hostname_matches(upper.as_bytes(), lower.as_bytes()));
        // A different name (one extra label) never matches, in any case.
        let other = format!("x.{lower}");
        prop_assert!(!hostname_matches(name.as_bytes(), other.as_bytes()));
        prop_assert!(!hostname_matches(other.as_bytes(), name.as_bytes()));
    }

    /// The wildcard oracle: `*.{tail}` matches a host built from the SAME
    /// labels with ONE arbitrary first label, and rejects hosts with any
    /// other label count (deeper or shallower).
    #[test]
    fn wildcard_matches_exactly_one_label(
        tail_labels in proptest::collection::vec(label(), 1..=3),
        first in label(),
        extra in label(),
    ) {
        let tail = tail_labels.join(".");
        let host = format!("{first}.{tail}");
        let san = format!("*.{tail}");

        // Exactly one label matched.
        prop_assert!(hostname_matches(host.as_bytes(), san.as_bytes()));

        // One label deeper: never.
        let deeper = format!("{extra}.{host}");
        prop_assert!(!hostname_matches(deeper.as_bytes(), san.as_bytes()));

        // One label shallower (dropping the host's first label): never —
        // the wildcard must CONSUME a label, not vanish.
        let shallow = format!("*.{tail}").to_ascii_lowercase();
        let host_shallow = format!("{extra}.{tail}");
        prop_assert!(hostname_matches(host_shallow.as_bytes(), shallow.as_bytes()));
        prop_assert!(!hostname_matches(tail.as_bytes(), san.as_bytes()));
    }

    /// Fail-closed on misplaced wildcards: injecting `*` into any label of
    /// the SAN (so it is never the WHOLE left-most label) always rejects,
    /// and a wildcarded host never matches a wildcard-free SAN.
    #[test]
    fn misplaced_wildcards_fail_closed(
        labels in proptest::collection::vec(label(), 1..=3),
        label_idx in 0usize..3,
        star_pos in 0usize..10,
    ) {
        let mut ls = labels.clone();
        let i = label_idx % ls.len();
        let pos = star_pos % (ls[i].len() + 1);
        ls[i].insert(pos, '*');
        let san = ls.join(".");
        let host = labels.join(".");

        // The injected '*' is inside a label (partial) or a whole label that
        // is not left-most: both are outside the supported subset.
        let whole_leftmost = i == 0 && ls[0].as_bytes() == b"*";
        prop_assert!(!whole_leftmost);
        prop_assert!(!hostname_matches(host.as_bytes(), san.as_bytes()));

        // Host-side wildcard with a clean SAN: also always rejected.
        let mut hls = labels.clone();
        hls[0].insert(0, '*');
        let wild_host = hls.join(".");
        let clean_san = labels.join(".");
        prop_assert!(!hostname_matches(wild_host.as_bytes(), clean_san.as_bytes()));
    }

    /// IP literals: the classifier accepts exactly canonical-ish dotted
    /// quads, and iPAddress matching is raw-octet equality only.
    #[test]
    fn ip_classification_and_octet_equality(
        a in 0u32..=255,
        b in 0u32..=255,
        c in 0u32..=255,
        d in 0u32..=255,
        sa in 0u32..=255,
    ) {
        let s = format!("{a}.{b}.{c}.{d}");
        let five = format!("{a}.{b}.{c}.{d}.{sa}");
        let three = format!("{a}.{b}.{c}");
        let with_tld = format!("{s}.com");
        prop_assert!(is_ip_literal(s.as_bytes()));
        prop_assert!(!is_ip_literal(five.as_bytes()));
        prop_assert!(!is_ip_literal(three.as_bytes()));
        prop_assert!(!is_ip_literal(with_tld.as_bytes()));

        // Octets compare byte-for-byte; octet strings never match as text.
        let host = [a as u8, b as u8, c as u8, d as u8];
        let san = [a as u8, b as u8, c as u8, d as u8];
        prop_assert!(ip_matches(&host, &san));
        let other = [sa as u8, b as u8, c as u8, d as u8];
        prop_assert_eq!(ip_matches(&host, &other), host == other);
        prop_assert!(!ip_matches(s.as_bytes(), s.as_bytes()));
    }
}

// ---------------------------------------------------------------------------
// Fixed equivalence classes (cheap, exact)
// ---------------------------------------------------------------------------

#[test]
fn equivalence_classes() {
    // (host, san, expected)
    let cases = [
        ("www.example.com", "www.example.com", true),
        ("WWW.EXAMPLE.COM", "www.example.com", true),
        ("a.example.com", "*.example.com", true),
        ("b.a.example.com", "*.example.com", false),
        ("example.com", "*.example.com", false),
        ("www.example.com", "w*.example.com", false),
        ("www.example.com", "www.*.com", false),
        ("www.example.com.", "www.example.com", true),
        ("www..example.com", "www.example.com", false),
        ("пример.рф", "пример.рф", false),
        ("xn--e1afmkfd.xn--p1ai", "*.xn--p1ai", true),
        ("", "", false),
    ];
    for (host, san, want) in cases {
        assert_eq!(
            hostname_matches(host.as_bytes(), san.as_bytes()),
            want,
            "host={host:?} san={san:?}"
        );
    }
}
