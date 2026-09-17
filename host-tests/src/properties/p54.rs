// Feature: OpenPGP repository-metadata verification (issue #32), Property 54:
// the `Release` parser binds `Packages` to the signed metadata — and never lets
// a digest from the wrong section (or a malformed line) stand in for one.
//
// The parser exists for a single decision: which SHA-256 and size the signature
// covers for the `Packages` file we are about to decompress and trust. A parser
// that scanned the whole file for "hex + size + path" would accept an `MD5Sum:`
// line (32 hex) as that digest, and a parser that ignored a malformed `SHA256:`
// line would turn "cannot verify" into "nothing to verify" — both fail open. The
// properties below pin the opposite: only the `SHA256:` section yields entries,
// an unreadable `Valid-Until:`/`Date:` is reported as *malformed* (so the caller
// refuses) rather than as absent, and no input panics.
//
// The main fixture is an excerpt of the live Debian `stable` `Release` file
// (fetched for the contract in OPENPGP-VERIFY-CONTRACT.md §12.1), including the
// same paths appearing in the `MD5Sum:` and `SHA256:` sections — the exact shape
// that tells the two apart.

use proptest::prelude::*;

use crate::release_file::{parse_date, parse_release, DateField, MAX_RELEASE_BYTES};

/// `Sat, 12 Sep 2026 07:55:41 UTC` from the live `stable` Release file.
const LIVE_DATE_EPOCH: i64 = 1_789_199_741;

/// A real-shape `Release` body: the live header fields, then an `MD5Sum:` section
/// and a `SHA256:` section that both mention the same three paths.
fn live_shaped_release() -> Vec<u8> {
    let body = "\
Origin: Debian
Label: Debian
Suite: stable
Version: 13.7
Codename: trixie
Date: Sat, 12 Sep 2026 07:55:41 UTC
Acquire-By-Hash: yes
Architectures: all amd64 arm64 armel armhf i386 ppc64el riscv64 s390x
Components: main contrib non-free-firmware
Description: Debian 13.7 Released 12 September 2026
MD5Sum:
 e08be5a2aa84e0f84bd976c3ce580092 56620099 main/binary-amd64/Packages
 1c7c4d1c34c2b2022bae744c346926f6 13332733 main/binary-amd64/Packages.gz
 2d47057a46b268c5f031facecf74b20a  9678380 main/binary-amd64/Packages.xz
SHA256:
 4f2c68d67001d595fbd343f6dbad44468953a51c4d9dec3a4803249f6e295940 56620099 main/binary-amd64/Packages
 42c23aaca08e8225ed0449360724b92531952cbe4ce3bbf189298a7108f40a85 13332733 main/binary-amd64/Packages.gz
 7778d3e3f303b7ddb8ce0fe7c8d57473a076c6bf2e8f241f75421d2396352498  9678380 main/binary-amd64/Packages.xz
";
    body.as_bytes().to_vec()
}

fn hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn live_shaped_release_parses_and_looks_up_by_path() {
    let release = parse_release(&live_shaped_release());
    assert_eq!(release.suite, "stable");
    assert_eq!(release.codename, "trixie");
    assert_eq!(release.version, "13.7");
    assert!(release.acquire_by_hash);
    assert_eq!(release.date, DateField::Parsed(LIVE_DATE_EPOCH));
    assert_eq!(release.valid_until, DateField::Absent);
    assert_eq!(release.components, vec!["main", "contrib", "non-free-firmware"]);
    assert!(release.architectures.contains(&"amd64".to_string()));
    assert!(!release.truncated);

    // Exactly the SHA256 section's three entries, with the values from the
    // signed file — not the MD5 ones that mention the same paths.
    assert_eq!(release.sha256.len(), 3);
    let gz = release
        .sha256_for("main/binary-amd64/Packages.gz")
        .expect("Packages.gz entry");
    assert_eq!(
        hex(&gz.sha256),
        "42c23aaca08e8225ed0449360724b92531952cbe4ce3bbf189298a7108f40a85"
    );
    assert_eq!(gz.size, 13_332_733);

    let plain = release
        .sha256_for("main/binary-amd64/Packages")
        .expect("Packages entry");
    assert_eq!(
        hex(&plain.sha256),
        "4f2c68d67001d595fbd343f6dbad44468953a51c4d9dec3a4803249f6e295940"
    );

    // A path only present in `MD5Sum:` has no SHA-256 entry: the parser cannot
    // mistake the 32-hex digest for a 64-hex one.
    assert!(release.sha256_for("main/binary-amd64/Release").is_none());
    // And an unknown path simply has no entry (the caller refuses).
    assert!(release.sha256_for("contrib/binary-amd64/Packages.gz").is_none());
}

#[test]
fn suite_matching_accepts_the_codename() {
    let release = parse_release(&live_shaped_release());
    assert!(release.matches_suite("stable"));
    assert!(release.matches_suite("trixie"));
    assert!(!release.matches_suite("bookworm"));
    // A body with neither field is not a mismatch (some mirrors trim them).
    let bare = parse_release(b"Date: Sat, 12 Sep 2026 07:55:41 UTC\n");
    assert!(bare.matches_suite("stable"));
}

#[test]
fn malformed_digest_lines_yield_no_entry() {
    let cases = [
        // 63 hex characters.
        " 42c23aaca08e8225ed0449360724b92531952cbe4ce3bbf189298a7108f40a8 13332733 main/binary-amd64/Packages.gz",
        // non-hex character.
        " 42c23aaca08e8225ed0449360724b92531952cbe4ce3bbf189298a7108f40a8z 13332733 main/binary-amd64/Packages.gz",
        // size not a number.
        " 42c23aaca08e8225ed0449360724b92531952cbe4ce3bbf189298a7108f40a85 x13332733 main/binary-amd64/Packages.gz",
        // no path.
        " 42c23aaca08e8225ed0449360724b92531952cbe4ce3bbf189298a7108f40a85 13332733",
    ];
    for case in cases {
        let body = format!("Suite: stable\nSHA256:\n{case}\n");
        let release = parse_release(body.as_bytes());
        assert!(
            release.sha256_for("main/binary-amd64/Packages.gz").is_none(),
            "malformed entry must not become an entry: {case}"
        );
    }
}

#[test]
fn digest_sections_are_not_interchangeable() {
    // A well-formed entry in `MD5Sum:` (or `SHA512:`), and even in a section the
    // parser does not know, must not produce a SHA-256 entry — the signature only
    // covers `SHA256:`.
    let digest64 = "42c23aaca08e8225ed0449360724b92531952cbe4ce3bbf189298a7108f40a85";
    for section in ["MD5Sum", "SHA1", "SHA512", "Orig-Release-SHA256"] {
        let body = format!("Suite: stable\n{section}:\n {digest64} 13332733 main/binary-amd64/Packages.gz\n");
        let release = parse_release(body.as_bytes());
        assert!(
            release.sha256_for("main/binary-amd64/Packages.gz").is_none(),
            "section {section} must not yield a SHA-256 entry"
        );
    }
    // The SHA256 section does.
    let body = format!("Suite: stable\nSHA256:\n {digest64} 13332733 main/binary-amd64/Packages.gz\n");
    let release = parse_release(body.as_bytes());
    assert_eq!(
        release
            .sha256_for("main/binary-amd64/Packages.gz")
            .map(|e| hex(&e.sha256)),
        Some(digest64.to_string())
    );
}

#[test]
fn date_fields_distinguish_absent_from_unreadable() {
    // Live format.
    assert_eq!(
        parse_date("Sat, 12 Sep 2026 07:55:41 UTC"),
        DateField::Parsed(LIVE_DATE_EPOCH)
    );
    // Weekday optional, GMT accepted.
    assert_eq!(
        parse_date("12 Sep 2026 07:55:41 GMT"),
        DateField::Parsed(LIVE_DATE_EPOCH)
    );
    // A `Valid-Until` a day later.
    assert_eq!(
        parse_date("Sun, 13 Sep 2026 07:55:41 UTC"),
        DateField::Parsed(LIVE_DATE_EPOCH + 86_400)
    );
    // Present but unusable -> Malformed (the caller refuses; "unreadable" is not
    // "absent").
    for bad in [
        "",
        "garbage",
        "Sat, 12 Sep 2026 07:55:41 CEST",
        "Sat, 32 Sep 2026 07:55:41 UTC",
        "Sat, 12 Foo 2026 07:55:41 UTC",
        "Sat, 12 Sep 2026 25:55:41 UTC",
        "Sat, 12 Sep 2026 07:55 UTC",
    ] {
        assert_eq!(parse_date(bad), DateField::Malformed, "case {bad:?}");
    }
}

#[test]
fn valid_until_and_date_are_read_separately() {
    let body = "\
Suite: stable
Date: Sat, 12 Sep 2026 07:55:41 UTC
Valid-Until: Sun, 13 Sep 2026 07:55:41 UTC
SHA256:
 42c23aaca08e8225ed0449360724b92531952cbe4ce3bbf189298a7108f40a85 13332733 main/binary-amd64/Packages.gz
";
    let release = parse_release(body.as_bytes());
    assert_eq!(release.date, DateField::Parsed(LIVE_DATE_EPOCH));
    assert_eq!(
        release.valid_until,
        DateField::Parsed(LIVE_DATE_EPOCH + 86_400),
        "Valid-Until must not be confused with Date"
    );
    assert!(release.valid_until.is_present());

    // A `Valid-Until:` that cannot be read is Malformed (caller refuses), while
    // an absent one is Absent (caller proceeds).
    let broken = parse_release(b"Suite: stable\nValid-Until: whenever\n");
    assert_eq!(broken.valid_until, DateField::Malformed);
    assert!(!broken.valid_until.is_present() || broken.valid_until.value().is_none());
    assert_eq!(parse_release(b"Suite: stable\n").valid_until, DateField::Absent);
}

#[test]
fn crlf_and_long_bodies_are_handled() {
    // CRLF line endings (some mirrors/proxies) parse identically.
    let crlf: Vec<u8> = live_shaped_release()
        .into_iter()
        .flat_map(|b| if b == b'\n' { vec![b'\r', b'\n'] } else { vec![b] })
        .collect();
    let release = parse_release(&crlf);
    assert_eq!(release.suite, "stable");
    assert_eq!(release.sha256.len(), 3);

    // An over-long body parses what it can but reports `truncated`, so the caller
    // refuses rather than acting on a partial digest list.
    let mut huge = vec![b'x'; MAX_RELEASE_BYTES + 16];
    huge[0] = b'\n';
    let release = parse_release(&huge);
    assert!(release.truncated, "an oversized body must be reported as truncated");
}

proptest! {
    /// Arbitrary bytes never panic the parser.
    #[test]
    fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..2000)) {
        let release = parse_release(&data);
        // Whatever it found, every entry is a well-formed 64-hex digest with a
        // path: the invariant the trust chain relies on.
        for entry in &release.sha256 {
            prop_assert_eq!(entry.path.is_empty(), false);
            prop_assert_eq!(hex(&entry.sha256).len(), 64);
        }
    }

    /// A digest that is in the right section and well formed always round-trips,
    /// whatever the hex, path and size are.
    #[test]
    fn well_formed_entries_round_trip(
        digest in proptest::collection::vec(any::<u8>(), 32),
        size in 0u64..u32::MAX as u64,
        path in "[a-z0-9/._-]{1,48}",
    ) {
        let d = {
            let mut out = [0u8; 32];
            out.copy_from_slice(&digest);
            out
        };
        let body = format!("SHA256:\n {} {size} {path}\n", hex(&d));
        let release = parse_release(body.as_bytes());
        let entry = release.sha256_for(&path);
        prop_assert!(entry.is_some(), "well-formed entry must be found");
        prop_assert_eq!(entry.map(|e| e.sha256), Some(d));
        prop_assert_eq!(entry.map(|e| e.size), Some(size));
    }
}
