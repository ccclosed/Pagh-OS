// Feature: apt streaming index, Property 33: the incremental `StanzaParser`
// produces EXACTLY the same records as the whole-buffer `parse_packages`,
// regardless of how the input byte stream is split into chunks.
//
// This is the host-testable proof behind the bounded-memory `apt update` fix:
// the kernel decompresses the `Packages` index in fixed chunks and feeds each
// chunk to `StanzaParser`, never holding the whole decompressed index resident.
// For that to be correct the chunked/streaming parse must be byte-for-byte
// equivalent to the original whole-buffer parse — including across arbitrary
// (adversarial) chunk boundaries that split lines, CRLF pairs, stanza separators,
// and multi-byte UTF-8 sequences.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::apt_index::{parse_packages, PkgRecord, StanzaParser};
use proptest::prelude::*;

/// Run the streaming parser over `bytes`, feeding it in chunks of the given
/// `sizes` (cycled if shorter than the input), and collect the emitted records.
fn stream_parse_chunked(bytes: &[u8], sizes: &[usize]) -> Vec<PkgRecord> {
    let mut out = Vec::new();
    let mut parser = StanzaParser::new();
    let mut pos = 0;
    let mut si = 0;
    while pos < bytes.len() {
        // A chunk size of 0 would not make progress; treat it as 1.
        let want = sizes.get(si % sizes.len().max(1)).copied().unwrap_or(1).max(1);
        let end = (pos + want).min(bytes.len());
        parser.push(&bytes[pos..end], &mut |rec| out.push(rec));
        pos = end;
        si += 1;
    }
    parser.finish(&mut |rec| out.push(rec));
    out
}

/// Compare two records field-by-field (PkgRecord has no `PartialEq`).
fn records_eq(a: &PkgRecord, b: &PkgRecord) -> bool {
    a.package == b.package
        && a.version == b.version
        && a.arch == b.arch
        && a.filename == b.filename
        && a.provides == b.provides
        && a.size == b.size
        && a.depends.len() == b.depends.len()
        && a.depends
            .iter()
            .zip(b.depends.iter())
            .all(|(x, y)| x.alts == y.alts)
}

fn record_lists_eq(a: &[PkgRecord], b: &[PkgRecord]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| records_eq(x, y))
}

const FIXTURE: &str = "\
Package: hello
Version: 2.10-3
Architecture: amd64
Filename: pool/main/h/hello/hello_2.10-3_amd64.deb
Depends: libc6 (>= 2.34), foo | bar (>= 1.0)
Description: a friendly greeting
 This is a continuation line that extends the description field
 across several physical lines.
Size: 53000

Package: libc6
Version: 2.36-9
Architecture: amd64
Filename: pool/main/g/glibc/libc6_2.36-9_amd64.deb
Provides: libc-l10n, glibc
Pre-Depends: libgcc-s1
Size: 2800000

Package: bar
Version: 1.2
Architecture: amd64
Filename: pool/main/b/bar/bar_1.2_amd64.deb
Size: 1000
";

#[test]
fn fixture_streamed_byte_by_byte_matches_whole_buffer() {
    let bytes = FIXTURE.as_bytes();
    let whole = parse_packages(bytes);

    // Feed the fixture one byte at a time — the most adversarial chunking.
    let streamed = stream_parse_chunked(bytes, &[1]);
    assert!(record_lists_eq(&whole, &streamed), "byte-by-byte mismatch");

    // A few fixed chunk sizes including ones that straddle the CRLF/stanza breaks.
    for &sz in &[2usize, 3, 7, 13, 64, 4096] {
        let streamed = stream_parse_chunked(bytes, &[sz]);
        assert!(record_lists_eq(&whole, &streamed), "chunk size {sz} mismatch");
    }

    // CRLF variant, byte-by-byte (splits the \r\n pair across chunks).
    let crlf = FIXTURE.replace('\n', "\r\n");
    let crlf_whole = parse_packages(crlf.as_bytes());
    let crlf_streamed = stream_parse_chunked(crlf.as_bytes(), &[1]);
    assert!(
        record_lists_eq(&crlf_whole, &crlf_streamed),
        "CRLF byte-by-byte mismatch"
    );
    // Whole-buffer LF and CRLF parses agree on the records (CRLF tolerance).
    assert!(record_lists_eq(&whole, &crlf_whole));
}

/// Build a random but well-formed `Packages` document from `n` stanzas. Field
/// presence and ordering vary so the parser's optional-field and
/// continuation-line handling is exercised.
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
    #![proptest_config(ProptestConfig::with_cases(400))]

    /// For any `Packages` document and any sequence of chunk sizes, streaming the
    /// bytes through `StanzaParser` yields exactly the records the whole-buffer
    /// `parse_packages` produces.
    #[test]
    fn streaming_equals_whole_buffer_over_random_chunks(
        doc in packages_doc_strategy(),
        sizes in prop::collection::vec(1usize..40, 1..16),
    ) {
        let bytes = doc.as_bytes();
        let whole = parse_packages(bytes);
        let streamed = stream_parse_chunked(bytes, &sizes);
        prop_assert!(record_lists_eq(&whole, &streamed));
    }

    /// Arbitrary bytes (not necessarily valid UTF-8 or well-formed stanzas) must
    /// parse identically whole-buffer vs streamed, and never panic. This pins the
    /// "split on 0x0A before lossy decode == lossy decode then split on '\n'"
    /// equivalence even with malformed multi-byte sequences straddling chunks.
    #[test]
    fn streaming_equals_whole_buffer_on_arbitrary_bytes(
        bytes in prop::collection::vec(any::<u8>(), 0..512),
        sizes in prop::collection::vec(1usize..16, 1..8),
    ) {
        let whole = parse_packages(&bytes);
        let streamed = stream_parse_chunked(&bytes, &sizes);
        prop_assert!(record_lists_eq(&whole, &streamed));
    }
}
