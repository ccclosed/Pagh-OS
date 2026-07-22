// Feature: full-debian-apt-update, Property 5 (Validates R2.4, R6.1, R6.2): the
// `PackageIndex::footprint()` accounting identity is deterministic and monotone,
// and the streaming builder's package count never decreases as stanzas arrive.
//
// `footprint()` is the Resident_Index_Footprint accounting the design pins
// against the 128 MiB ceiling, summed purely from the component buffer lengths.
// We pin:
//   * Determinism — two builds from the same document report an equal footprint
//     (and the empty index has a stable baseline footprint across builds).
//   * Monotonicity — appending stanzas to a document can only grow the footprint
//     and the record count; any non-empty index has a strictly positive
//     footprint.
//   * Incremental non-decrease — feeding stanzas into a `PackageIndexBuilder`
//     leaves `builder.len()` non-decreasing across pushes.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::apt_index::{parse_packages, PackageIndex, PackageIndexBuilder, StanzaParser};
use proptest::prelude::*;

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

    /// Property 5: footprint determinism, monotonicity, and builder count
    /// non-decrease.
    #[test]
    fn footprint_accounting_and_monotonicity(
        base in packages_doc_strategy(),
        extra in packages_doc_strategy(),
    ) {
        // --- Determinism: same doc -> equal footprint ----------------------
        let smaller = PackageIndex::from_records(parse_packages(base.as_bytes()));
        let smaller2 = PackageIndex::from_records(parse_packages(base.as_bytes()));
        prop_assert_eq!(smaller.footprint(), smaller2.footprint());

        // Empty index baseline is deterministic.
        let empty_a = PackageIndex::from_records(Vec::new());
        let empty_b = PackageIndex::from_records(Vec::new());
        prop_assert_eq!(empty_a.footprint(), empty_b.footprint());

        // --- Monotonicity: appending stanzas only grows ---------------------
        // Concatenate the two documents (blank line keeps stanzas separated).
        let bigger_doc = if base.is_empty() {
            extra.clone()
        } else {
            let mut s = base.clone();
            s.push_str("\n\n");
            s.push_str(&extra);
            s
        };
        let bigger = PackageIndex::from_records(parse_packages(bigger_doc.as_bytes()));

        prop_assert!(bigger.footprint() >= smaller.footprint());
        prop_assert!(bigger.len() >= smaller.len());

        // Any non-empty index has a strictly positive footprint.
        if !smaller.is_empty() {
            prop_assert!(smaller.footprint() > 0);
        }
        if !bigger.is_empty() {
            prop_assert!(bigger.footprint() > 0);
        }

        // --- Incremental builder: len() is non-decreasing -------------------
        let bytes = bigger_doc.as_bytes();
        let mut builder = PackageIndexBuilder::new();
        let mut parser = StanzaParser::new();
        let mut prev = builder.len();
        let chunk = (bytes.len() / 5).max(1);
        let mut pos = 0;
        while pos < bytes.len() {
            let end = (pos + chunk).min(bytes.len());
            parser.push_view(&bytes[pos..end], &mut builder);
            let now = builder.len();
            prop_assert!(now >= prev);
            prev = now;
            pos = end;
        }
        parser.finish_view(&mut builder);
        prop_assert!(builder.len() >= prev);

        // The fully-streamed builder agrees on record count with the
        // whole-buffer build of the same document.
        let streamed = PackageIndex::from_builder(builder);
        prop_assert_eq!(streamed.len(), bigger.len());
    }
}
