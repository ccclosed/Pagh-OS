// Feature: OpenPGP repository-metadata verification (issue #32), Property 55:
// every `Packages` record carries the `.deb` digest the signed index declares —
// through BOTH parser paths — and anything else is `None`, never a zero digest.
//
// `apt install` binds a downloaded `.deb` to the signed metadata through the
// stanza's `SHA256:`. Two mistakes would silently break that binding, and this
// property exists to catch both:
//
//   * the digest being dropped on the arena/streaming path (the one `apt update`
//     actually uses) while the owned path keeps it — the two paths must agree;
//   * a malformed or absent field being turned into a default (all-zero) digest,
//     which would then "verify" nothing and let any payload through.
//
// The zero-digest case is the dangerous one, so it is asserted directly: an
// absent, short, long or non-hex `SHA256:` yields `None`, and `None` is what
// makes `apt install` refuse the package.

use proptest::prelude::*;

use crate::apt_index::{parse_packages, PackageIndexBuilder, StanzaParser};

const D1: &str = "4f2c68d67001d595fbd343f6dbad44468953a51c4d9dec3a4803249f6e295940";
const D2: &str = "42c23aaca08e8225ed0449360724b92531952cbe4ce3bbf189298a7108f40a85";

fn digest(hex_str: &str) -> [u8; 32] {
    let bytes = hex_str.as_bytes();
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = (bytes[2 * i] as char).to_digit(16).unwrap();
        let lo = (bytes[2 * i + 1] as char).to_digit(16).unwrap();
        out[i] = ((hi << 4) | lo) as u8;
    }
    out
}

/// One realistic stanza with `Filename:`, `Size:`, `SHA256:` and a dependency.
fn packages_body() -> Vec<u8> {
    format!(
        "Package: hello-pagh\n\
         Version: 1.0\n\
         Architecture: amd64\n\
         Filename: pool/main/h/hello-pagh/hello-pagh_1.0_amd64.deb\n\
         Size: 4096\n\
         SHA256: {D1}\n\
         Depends: libc6 (>= 2.34)\n\
         Description: tiny hello binary\n\
         \n\
         Package: other-pkg\n\
         Version: 2.0\n\
         Filename: pool/main/o/other-pkg/other-pkg_2.0_amd64.deb\n\
         Size: 8192\n\
         SHA256: {D2}\n"
    )
    .into_bytes()
}

/// Feed a body through the STREAMING path `apt update` uses (incremental parser →
/// arena-backed index) in awkward chunk sizes, and return the index.
fn streaming_index(body: &[u8], chunk: usize) -> crate::apt_index::PackageIndex {
    let mut parser = StanzaParser::new();
    let mut builder = PackageIndexBuilder::new();
    for part in body.chunks(chunk.max(1)) {
        parser.push_view(part, &mut builder);
    }
    parser.finish_view(&mut builder);
    crate::apt_index::PackageIndex::from_builder(builder)
}

#[test]
fn owned_and_streaming_paths_carry_the_digest() {
    // Owned path (`parse_packages` returns `Vec<PkgRecord>`).
    let owned = parse_packages(&packages_body());
    let hello = owned
        .iter()
        .find(|r| r.package == "hello-pagh")
        .expect("hello-pagh in the owned records");
    assert_eq!(hello.sha256, Some(digest(D1)));
    assert_eq!(hello.size, 4096);
    assert_eq!(
        hello.filename,
        "pool/main/h/hello-pagh/hello-pagh_1.0_amd64.deb"
    );
    let other = owned
        .iter()
        .find(|r| r.package == "other-pkg")
        .expect("other-pkg in the owned records");
    assert_eq!(other.sha256, Some(digest(D2)));

    // Streaming path, at every chunk size including 1 byte (the parser's carry
    // logic must not lose or corrupt a field).
    for chunk in [1usize, 3, 7, 64, 4096] {
        let index = streaming_index(&packages_body(), chunk);
        let hello = index.get("hello-pagh").expect("hello-pagh in streaming index");
        assert_eq!(hello.sha256(), Some(digest(D1)), "chunk {chunk}");
        assert_eq!(hello.size(), 4096, "chunk {chunk}");
        let other = index.get("other-pkg").expect("other-pkg in streaming index");
        assert_eq!(other.sha256(), Some(digest(D2)), "chunk {chunk}");
    }
}

#[test]
fn absent_or_malformed_digest_is_none_never_zero() {
    let cases: [(&str, &str); 7] = [
        ("absent", ""),
        // 63 hex characters.
        ("short", " SHA256: 4f2c68d67001d595fbd343f6dbad44468953a51c4d9dec3a4803249f6e29594\n"),
        // 65 hex characters.
        ("long", " SHA256: 4f2c68d67001d595fbd343f6dbad44468953a51c4d9dec3a4803249f6e2959400\n"),
        // non-hex character.
        ("non-hex", " SHA256: zz2c68d67001d595fbd343f6dbad44468953a51c4d9dec3a4803249f6e295940\n"),
        // empty value.
        ("empty", " SHA256:\n"),
        // wrong key spelling (the parser's key match is case-sensitive).
        ("wrong-case", " sha256: 4f2c68d67001d595fbd343f6dbad44468953a51c4d9dec3a4803249f6e295940\n"),
        // a `SHA512:` field is a different algorithm and must not be used.
        (
            "sha512",
            " SHA512: 4f2c68d67001d595fbd343f6dbad44468953a51c4d9dec3a4803249f6e2959404f2c68d67001d595fbd343f6dbad44468953a51c4d9dec3a4803249f6e295940\n",
        ),
    ];
    for (label, extra) in cases {
        let body = format!(
            "Package: hello-pagh\nVersion: 1.0\nFilename: pool/main/h/hello-pagh.deb\nSize: 4096\n{extra}\n"
        );
        for chunk in [1usize, 4096] {
            let index = streaming_index(body.as_bytes(), chunk);
            let rec = index.get("hello-pagh").expect("record is still indexed");
            assert_eq!(rec.sha256(), None, "{label} (chunk {chunk})");
            assert_ne!(
                rec.sha256(),
                Some([0u8; 32]),
                "{label}: a missing digest must never become the zero digest"
            );
        }
        let owned = parse_packages(body.as_bytes());
        assert_eq!(
            owned.iter().find(|r| r.package == "hello-pagh").and_then(|r| r.sha256),
            None,
            "{label} (owned path)"
        );
    }
}

#[test]
fn a_repeated_digest_key_is_last_wins() {
    // Debian stanzas never repeat `SHA256:`, but the parser's rule (last-wins,
    // matching the old BTreeMap builder) must hold here too, so a mirror cannot
    // smuggle an earlier digest past a later one.
    let body = format!(
        "Package: hello-pagh\nFilename: a.deb\nSHA256: {D1}\nSHA256: {D2}\n\n"
    );
    for chunk in [1usize, 4096] {
        let index = streaming_index(body.as_bytes(), chunk);
        assert_eq!(
            index.get("hello-pagh").and_then(|r| r.sha256()),
            Some(digest(D2)),
            "chunk {chunk}"
        );
    }
    assert_eq!(
        parse_packages(body.as_bytes())
            .iter()
            .find(|r| r.package == "hello-pagh")
            .and_then(|r| r.sha256),
        Some(digest(D2))
    );
}

#[test]
fn continuation_lines_do_not_invent_a_digest() {
    // A digest split across a continuation line is not a digest: the parser joins
    // continuations with a single space, and the resulting value is not 64 hex.
    let body = format!("Package: hello-pagh\nSHA256: {}\n {}\n\n", &D1[..32], &D1[32..]);
    let index = streaming_index(body.as_bytes(), 4096);
    let rec = index.get("hello-pagh").expect("record indexed");
    assert_eq!(
        rec.sha256(),
        None,
        "a split digest must not be reassembled into a valid-looking one"
    );
}

proptest! {
    /// A well-formed random digest always survives the streaming path intact.
    #[test]
    fn random_well_formed_digests_round_trip(
        raw in proptest::collection::vec(any::<u8>(), 32),
        chunk in 1usize..512,
    ) {
        let mut d = [0u8; 32];
        d.copy_from_slice(&raw);
        let hexd: String = d.iter().map(|b| format!("{b:02x}")).collect();
        let body = format!("Package: p\nFilename: p.deb\nSize: 7\nSHA256: {hexd}\n\n");
        let index = streaming_index(body.as_bytes(), chunk);
        let rec = index.get("p").expect("record indexed");
        prop_assert_eq!(rec.sha256(), Some(d));
        prop_assert_eq!(rec.size(), 7);
    }

    /// Arbitrary bytes never panic either parser path, and no record ever
    /// reports a digest that is not one of the 64-hex values in the input.
    #[test]
    fn arbitrary_bodies_never_panic_or_invent_digests(
        data in proptest::collection::vec(any::<u8>(), 0..600),
        chunk in 1usize..300,
    ) {
        let index = streaming_index(&data, chunk);
        for name in index.names() {
            if let Some(rec) = index.get(name) {
                if let Some(d) = rec.sha256() {
                    let hexd: String = d.iter().map(|b| format!("{b:02x}")).collect();
                    let as_text = String::from_utf8_lossy(&data);
                    prop_assert!(
                        as_text.to_lowercase().contains(&hexd),
                        "a digest appeared that is not in the input"
                    );
                }
            }
        }
        let _ = parse_packages(&data);
    }
}
