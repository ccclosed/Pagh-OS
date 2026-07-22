// Feature: full-debian-apt-update, Property 6 (Validates R5.1): the streaming
// compression decoder behind `apt update` (`deb::decompress_stream`) decodes a
// real gzip / zstd / xz body into exactly the bytes that, once parsed, yield the
// same `Packages` records as parsing the original uncompressed document.
//
// `apt update` never holds the whole decompressed index in RAM: it feeds the
// compressed body to `deb::decompress_stream`, which delivers the decompressed
// output to a sink in fixed-size chunks. Here the sink simply concatenates the
// chunks into a `Vec`, and we assert the round-trip `parse_packages(decompress(
// compress(doc))) == parse_packages(doc)` field-by-field.
//
// Evidence per codec:
//   * gzip — PRIMARY (>=256 cases). We build a *real* RFC 1952 gzip container
//     (`1f 8b 08` header + raw DEFLATE body + CRC32 + ISIZE trailer) because the
//     decoder's `gzip_payload_offset` validates the two magic bytes and the
//     DEFLATE method byte before handing the raw DEFLATE payload to miniz_oxide's
//     raw inflater. The DEFLATE body is produced by `miniz_oxide::deflate::
//     compress_to_vec` (raw DEFLATE, matching the raw inflater the decoder uses).
//   * zstd — (>=128 cases) via ruzstd's pure-Rust encoder, mirroring p29.
//   * xz — EXAMPLE/fixture only: xz4rust is decode-only, so an arbitrary doc
//     cannot be xz-encoded at runtime. The authoritative `.xz` fixtures from
//     `p29_fixtures` are streamed through `deb::decompress_stream(.., Xz)` and
//     asserted to equal the known payloads, proving the xz STREAM path feeds
//     bytes through correctly. (xz round-trip over arbitrary docs is therefore
//     covered by fixtures, not a proptest, because xz4rust is decode-only.)

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::apt_index::{parse_packages, PkgRecord};
use crate::deb::{self, Compression};
use proptest::prelude::*;

use super::p29_fixtures as fx;

/// Drive `deb::decompress_stream` over `bytes` under `comp`, concatenating every
/// delivered output chunk into one `Vec` (the test-side sink). Panics if the
/// decoder reports an error, since every input here is a valid stream.
fn stream_decompress_to_vec(bytes: &[u8], comp: Compression) -> Vec<u8> {
    let mut out = Vec::new();
    deb::decompress_stream(bytes, comp, deb::MAX_INDEX_STREAM_BYTES, |chunk| {
        out.extend_from_slice(chunk);
        Ok(())
    })
    .expect("valid stream must decompress without error");
    out
}

/// IEEE CRC-32 (reflected, polynomial 0xEDB88320) — the checksum a gzip trailer
/// carries. Implemented inline so the test owns the exact container it builds.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            // Branchless reduce: subtract the polynomial iff the low bit is set.
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Wrap `data` in a real RFC 1952 gzip container that `deb`'s Gzip path accepts:
/// a 10-byte fixed header (`1f 8b`, CM=8 DEFLATE, no flags, OS=0xff), the raw
/// DEFLATE body, then the 4-byte CRC32 and 4-byte ISIZE (`data.len()`) trailer,
/// both little-endian.
fn gzip_wrap(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0xff]);
    // Raw DEFLATE (no zlib header) — matches the raw inflater `deb` uses.
    let body = miniz_oxide::deflate::compress_to_vec(data, 6);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

/// Compare two records field-by-field (PkgRecord has no `PartialEq`).
/// (Copied from p33 — per-file copying matches the existing property-test style.)
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

/// xz EXAMPLE (fixtures): stream each authoritative `.xz` fixture through
/// `deb::decompress_stream(.., Xz)` and assert it equals the known payload. This
/// is the documented xz evidence — xz round-trip over *arbitrary* docs cannot be
/// a proptest because xz4rust is decode-only (no runtime xz encoder), so the xz
/// STREAM path is proven via these authoritative fixtures instead.
#[test]
fn xz_stream_decodes_authoritative_fixtures() {
    let cases: &[(&str, &[u8], &[u8])] = &[
        ("EMPTY", fx::XZ_EMPTY, fx::PAYLOAD_EMPTY),
        ("HELLO", fx::XZ_HELLO, fx::PAYLOAD_HELLO),
        ("TEXT", fx::XZ_TEXT, fx::PAYLOAD_TEXT),
        ("LARGE", fx::XZ_LARGE, fx::PAYLOAD_LARGE),
    ];
    for (name, xz, payload) in cases {
        let got = stream_decompress_to_vec(xz, Compression::Xz);
        assert_eq!(&got, payload, "xz fixture {name} stream mismatch");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Feature: full-debian-apt-update, Property 6 (gzip round-trip).
    ///
    /// For any `Packages` document, wrapping it in a real gzip container and
    /// decoding it through `deb::decompress_stream(.., Gzip)` yields bytes that
    /// parse to exactly the same records as parsing the original document.
    #[test]
    fn gzip_stream_round_trip_into_index(doc in packages_doc_strategy()) {
        let original = doc.as_bytes();
        let gz = gzip_wrap(original);
        let decompressed = stream_decompress_to_vec(&gz, Compression::Gzip);
        prop_assert!(record_lists_eq(
            &parse_packages(original),
            &parse_packages(&decompressed),
        ));
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Feature: full-debian-apt-update, Property 6 (zstd round-trip).
    ///
    /// For any `Packages` document, compressing it to a real zstd frame and
    /// decoding it through `deb::decompress_stream(.., Zstd)` yields bytes that
    /// parse to exactly the same records as parsing the original document.
    #[test]
    fn zstd_stream_round_trip_into_index(doc in packages_doc_strategy()) {
        use ruzstd::encoding::{compress_to_vec, CompressionLevel};
        let original = doc.as_bytes();
        let frame = compress_to_vec(original, CompressionLevel::Fastest);
        let decompressed = stream_decompress_to_vec(&frame, Compression::Zstd);
        prop_assert!(record_lists_eq(
            &parse_packages(original),
            &parse_packages(&decompressed),
        ));
    }
}
