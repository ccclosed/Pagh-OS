// Feature: full-debian-apt-update, Property 3: streaming-into-arena equivalence
// across chunk splits (panic-free).
//
// The kernel `apt update` path never holds the whole decompressed `Packages`
// index resident: it decompresses the body in fixed chunks and feeds each chunk
// through `StanzaParser::push_view` straight into a `PackageIndexBuilder`, which
// interns every stanza into its arena. This property pins that the compact index
// produced incrementally — across ARBITRARY chunk boundaries — is QUERY-equivalent
// to the one produced by a single whole-buffer push, and that the build is
// panic-free even on arbitrary (malformed) bytes. It also cross-checks the
// compact builder's name set against the byte-exact owned `parse_packages` path,
// since both share the same underlying parser.
//
// Validates: Requirements 1.1, 2.1, 2.5

use alloc::collections::BTreeSet;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::apt_index::{parse_packages, PackageIndex, PackageIndexBuilder, StanzaParser};
use proptest::prelude::*;

/// Build the compact index by feeding `bytes` to `StanzaParser::push_view` in
/// chunks cycled from `sizes` (each `.max(1)`), interning straight into a
/// `PackageIndexBuilder`, then `finish_view` + `from_builder`.
fn build_chunked(bytes: &[u8], sizes: &[usize]) -> PackageIndex {
    let mut builder = PackageIndexBuilder::new();
    let mut parser = StanzaParser::new();
    let mut pos = 0;
    let mut si = 0;
    while pos < bytes.len() {
        let want = sizes.get(si % sizes.len().max(1)).copied().unwrap_or(1).max(1);
        let end = (pos + want).min(bytes.len());
        parser.push_view(&bytes[pos..end], &mut builder);
        pos = end;
        si += 1;
    }
    parser.finish_view(&mut builder);
    PackageIndex::from_builder(builder)
}

/// Build the same compact index whole-buffer: a single `push_view` of the entire
/// slice, then `finish_view` + `from_builder`.
fn build_whole(bytes: &[u8]) -> PackageIndex {
    let mut builder = PackageIndexBuilder::new();
    let mut parser = StanzaParser::new();
    parser.push_view(bytes, &mut builder);
    parser.finish_view(&mut builder);
    PackageIndex::from_builder(builder)
}

/// Assert two indexes are QUERY-equivalent: same `len()`, same `names()`
/// sequence, and over the union of their names (plus any `extra_probes`) the
/// `get`/`get_provider` package strings and `contains` all agree. The internals
/// are private, so everything is compared through the public query surface.
fn assert_query_equivalent(a: &PackageIndex, b: &PackageIndex, extra_probes: &[&str]) {
    assert_eq!(a.len(), b.len(), "len() differs");

    let names_a: Vec<&str> = a.names().collect();
    let names_b: Vec<&str> = b.names().collect();
    assert_eq!(names_a, names_b, "names() sequence differs");

    let mut probes: BTreeSet<&str> = BTreeSet::new();
    probes.extend(names_a.iter().copied());
    probes.extend(names_b.iter().copied());
    probes.extend(extra_probes.iter().copied());

    for n in probes {
        let ga = a.get(n).map(|r| r.package().to_string());
        let gb = b.get(n).map(|r| r.package().to_string());
        assert_eq!(ga, gb, "get({n:?}) differs");

        let pa = a.get_provider(n).map(|r| r.package().to_string());
        let pb = b.get_provider(n).map(|r| r.package().to_string());
        assert_eq!(pa, pb, "get_provider({n:?}) differs");

        assert_eq!(a.contains(n), b.contains(n), "contains({n:?}) differs");
    }
}

/// Build a random but well-formed `Packages` document from `n` stanzas. Field
/// presence and ordering vary so the parser's optional-field and
/// continuation-line handling is exercised. (Per-file copy of the P33 strategy,
/// matching the existing house style.)
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

    /// Over well-formed `Packages` docs and arbitrary chunk-size vectors, the
    /// chunked-streamed compact index and the whole-buffer compact index are
    /// query-equivalent, and the chunked builder's package-name set equals the
    /// set of `rec.package` from the byte-exact owned `parse_packages`.
    #[test]
    fn streaming_into_arena_equivalent_over_docs(
        doc in packages_doc_strategy(),
        sizes in prop::collection::vec(1usize..40, 1..16),
    ) {
        let bytes = doc.as_bytes();

        let chunked = build_chunked(bytes, &sizes);
        let whole = build_whole(bytes);
        assert_query_equivalent(&chunked, &whole, &[]);

        // Cross-check against the owned byte-exact path: same name SET.
        let owned = parse_packages(bytes);
        let owned_names: BTreeSet<String> =
            owned.iter().map(|r| r.package.clone()).collect();
        let chunked_names: BTreeSet<String> =
            chunked.names().map(|n| n.to_string()).collect();
        prop_assert_eq!(owned_names, chunked_names);
    }

    /// Arbitrary bytes (not necessarily valid UTF-8 or well-formed stanzas) must
    /// build without panicking, and the chunked vs whole compact indexes must
    /// still be query-equivalent — probed over the names each produced plus a few
    /// random byte-strings.
    #[test]
    fn streaming_into_arena_equivalent_on_arbitrary_bytes(
        bytes in prop::collection::vec(any::<u8>(), 0..512),
        sizes in prop::collection::vec(1usize..16, 1..8),
        probes in prop::collection::vec("[\\x00-\\x7f]{0,16}", 0..4),
    ) {
        let chunked = build_chunked(&bytes, &sizes);
        let whole = build_whole(&bytes);

        let probe_refs: Vec<&str> = probes.iter().map(|s| s.as_str()).collect();
        assert_query_equivalent(&chunked, &whole, &probe_refs);

        // The owned byte-exact path agrees on the name SET even on garbage bytes.
        let owned = parse_packages(&bytes);
        let owned_names: BTreeSet<String> =
            owned.iter().map(|r| r.package.clone()).collect();
        let chunked_names: BTreeSet<String> =
            chunked.names().map(|n| n.to_string()).collect();
        prop_assert_eq!(owned_names, chunked_names);
    }
}
