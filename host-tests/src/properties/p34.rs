// Feature: full-debian-apt-update, Property 1 (Validates R6.3): the compact
// byte arena is a faithful, panic-free string store, and the query surface that
// reads back through it never faults.
//
// Two halves, both pinned here:
//   * `Arena::intern`/`Arena::resolve` round-trips every string (including the
//     empty string -> `StrRef::EMPTY` -> "" and arbitrary Unicode), and
//     `resolve` is *total* — a wildly out-of-bounds `StrRef` resolves to "" with
//     no panic rather than indexing past the backing bytes.
//   * Built end to end, `PackageIndex::from_records(parse_packages(doc))` reads
//     back through the same arena: every enumerated name resolves to a record
//     whose `package()` equals that name, and walking every `PkgRef` field and
//     every `depends().alts()` never panics (the bounds half of R6.3 at the
//     public query surface).

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::apt_index::{parse_packages, Arena, PackageIndex, StrRef};
use proptest::prelude::*;

/// A grab-bag of strings to intern: the empty string, plain ASCII package-ish
/// names, arbitrary Unicode runs, and a fixed multi-byte sentinel — so the arena
/// round-trip is exercised across `StrRef::EMPTY`, ASCII, and multi-byte UTF-8.
fn strings_strategy() -> impl Strategy<Value = Vec<String>> {
    let one = prop_oneof![
        Just(String::new()),
        "[a-z0-9.+:_-]{0,16}".prop_map(|s| s),
        "[\\p{L}\\p{N} ]{0,10}".prop_map(|s| s),
        Just("naïve-π-pkg".to_string()),
    ];
    prop::collection::vec(one, 0..24)
}

/// Build a random but well-formed `Packages` document from `n` stanzas. Field
/// presence and ordering vary so the parser's optional-field and
/// continuation-line handling is exercised. (Copied from p33 — per-file copying
/// matches the existing property-test style.)
fn packages_doc_strategy() -> impl Strategy<Value = String> {
    let name = "[a-z][a-z0-9.+-]{0,12}";
    let ver = "[0-9]{1,2}\\.[0-9]{1,3}(-[0-9]{1,2})?";
    let stanza = (
        name,
        ver,
        prop::option::of("[a-z0-9 ,|()>=.:+-]{0,40}"), // Depends
        prop::option::of("[a-z0-9 ,.+-]{0,30}"),        // Provides
        prop::option::of(0u64..9_000_000),              // Size
        any::<bool>(),                                   // include a continuation Description
    )
        .prop_map(|(pkg, version, deps, provides, size, desc)| {
            let mut s = String::new();
            s.push_str("Package: ");
            s.push_str(&pkg);
            s.push('\n');
            s.push_str("Version: ");
            s.push_str(&version);
            s.push('\n');
            s.push_str("Architecture: amd64\n");
            s.push_str("Filename: pool/main/x/");
            s.push_str(&pkg);
            s.push_str(".deb\n");
            if let Some(d) = deps {
                s.push_str("Depends: ");
                s.push_str(&d);
                s.push('\n');
            }
            if let Some(p) = provides {
                s.push_str("Provides: ");
                s.push_str(&p);
                s.push('\n');
            }
            if desc {
                s.push_str("Description: short\n a continued description line\n another line\n");
            }
            if let Some(sz) = size {
                s.push_str("Size: ");
                s.push_str(&sz.to_string());
                s.push('\n');
            }
            s
        });
    prop::collection::vec(stanza, 0..8).prop_map(|stanzas| stanzas.join("\n"))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Property 1: arena intern/resolve round-trip + bounds, and the built-index
    /// query surface reads back through the arena without panicking.
    #[test]
    fn arena_round_trip_and_query_bounds(
        strings in strings_strategy(),
        doc in packages_doc_strategy(),
    ) {
        // --- Arena intern/resolve round-trip -------------------------------
        let mut arena = Arena::new();
        let refs: Vec<StrRef> = strings.iter().map(|s| arena.intern(s)).collect();
        for (s, r) in strings.iter().zip(refs.iter()) {
            // Every interned string resolves back to exactly itself.
            prop_assert_eq!(arena.resolve(*r), s.as_str());
            // The empty string is the canonical EMPTY ref resolving to "".
            if s.is_empty() {
                prop_assert_eq!(*r, StrRef::EMPTY);
                prop_assert_eq!(arena.resolve(*r), "");
            }
        }

        // EMPTY resolves to "" regardless of arena contents.
        prop_assert_eq!(arena.resolve(StrRef::EMPTY), "");

        // resolve is TOTAL: a wildly out-of-bounds range yields "" (no panic).
        let bogus = StrRef { off: 1_000_000, len: 1_000_000 };
        prop_assert_eq!(arena.resolve(bogus), "");

        // --- Query-surface bounds via a fully built index ------------------
        let index = PackageIndex::from_records(parse_packages(doc.as_bytes()));

        for n in index.names() {
            // Every enumerated name resolves to a record owning that name.
            let pref = index.get(n);
            prop_assert!(pref.is_some());
            let pref = pref.unwrap();
            prop_assert_eq!(pref.package(), n);

            // Reading every field must not panic; touch each accessor.
            let _ = pref.version();
            let _ = pref.arch();
            let _ = pref.filename();
            let _ = pref.size();

            // Walking every dependency group's alternatives must not panic.
            let mut alt_count = 0usize;
            for group in pref.depends() {
                for alt in group.alts() {
                    // `alt` is a resolved &str slice into the arena.
                    alt_count += alt.len();
                }
            }
            let _ = alt_count;
        }
    }
}
