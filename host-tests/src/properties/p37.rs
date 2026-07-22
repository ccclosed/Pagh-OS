// Feature: full-debian-apt-update, Property 4: monotonic forward progress.
//
// The kernel's `stream_parse_index` reports progress with two counters: the
// number of decompressed bytes consumed so far, and the number of package
// records accumulated so far (`PackageIndexBuilder::len()`). A progress display
// must never go backwards. This property models that loop — feeding bytes in
// arbitrary chunks via `StanzaParser::push_view` into a `PackageIndexBuilder` —
// and pins that both counters are monotonically non-decreasing across every
// observation, and that the final totals are exact: the decompressed counter
// equals the total input length, and the record count equals the byte-exact
// owned `parse_packages` record count.
//
// Validates: Requirements 3.2

use alloc::string::{String, ToString};

use crate::apt_index::{parse_packages, PackageIndexBuilder, StanzaParser};
use proptest::prelude::*;

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

    /// Feeding bytes in arbitrary chunks while observing the running
    /// `decompressed` byte counter and `builder.len()` after each push: both
    /// counters are non-decreasing across all observations, the final
    /// `decompressed` equals the total input length, and the final record count
    /// equals the byte-exact owned `parse_packages` record count.
    #[test]
    fn monotonic_forward_progress(
        doc in packages_doc_strategy(),
        sizes in prop::collection::vec(1usize..40, 1..16),
    ) {
        let bytes = doc.as_bytes();

        let mut builder = PackageIndexBuilder::new();
        let mut parser = StanzaParser::new();

        let mut decompressed: usize = 0;
        let mut prev_decompressed: usize = 0;
        let mut prev_len: usize = 0;

        let mut pos = 0;
        let mut si = 0;
        while pos < bytes.len() {
            let want = sizes.get(si % sizes.len().max(1)).copied().unwrap_or(1).max(1);
            let end = (pos + want).min(bytes.len());
            let chunk = &bytes[pos..end];

            parser.push_view(chunk, &mut builder);
            decompressed += chunk.len();

            let cur_len = builder.len();
            // Both progress counters never go backwards.
            prop_assert!(decompressed >= prev_decompressed);
            prop_assert!(cur_len >= prev_len);
            prev_decompressed = decompressed;
            prev_len = cur_len;

            pos = end;
            si += 1;
        }

        parser.finish_view(&mut builder);

        // finish_view may flush a final trailing stanza: len() must not regress.
        prop_assert!(builder.len() >= prev_len);

        // Totals are exact: every input byte was consumed, and the streamed
        // record count matches the byte-exact owned parser.
        prop_assert_eq!(decompressed, bytes.len());
        let owned = parse_packages(bytes);
        prop_assert_eq!(builder.len(), owned.len());
    }
}
