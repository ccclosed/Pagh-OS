// Feature: apt-fast W3 — the compact, versioned `PackageIndex` encoding used by
// the persistent apt index cache.
//
// A second `apt update` against an unchanged signed `Release` must rebuild the
// index without downloading/decompressing/parsing `Packages`. The rebuild reads
// the index's *already compact* arena buffers from disk, so the encoding has to
// be:
//
//   * **lossless** — round-tripping must preserve every answer the index gives
//     (`get`, `get_provider`, `names`, dependency groups, digests, counters),
//   * **strict** — magic/version, every length, every `StrRef` range and every
//     record's dependency/provides range are validated, the stream must end
//     exactly, and any deviation returns `None` so the caller falls back to the
//     verified network path instead of trusting bytes at rest,
//   * **panic-free** — arbitrary corrupt bytes never panic and never make the
//     decoder allocate unboundedly.
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::apt_index::{
    parse_packages, PackageIndex, PackageIndexBuilder, StanzaParser, COMPACT_VERSION,
};

/// A small, varied `Packages` document: real names, a virtual provider, deps with
/// alternatives, an absent SHA256 and an absent Size.
fn sample_doc(n: usize) -> String {
    let mut s = String::new();
    for i in 0..n {
        s.push_str("Package: pkg-");
        s.push_str(&i.to_string());
        s.push('\n');
        s.push_str("Version: 1.");
        s.push_str(&(i % 40).to_string());
        s.push('\n');
        s.push_str("Architecture: amd64\n");
        s.push_str("Filename: pool/main/p/pkg-");
        s.push_str(&i.to_string());
        s.push_str("/pkg.deb\n");
        if i % 3 != 0 {
            s.push_str("Size: ");
            s.push_str(&(1000 + i).to_string());
            s.push('\n');
        }
        if i % 4 != 0 {
            s.push_str("SHA256: ");
            for k in 0..32 {
                s.push(char::from_digit(((i + k) % 16) as u32, 16).unwrap());
            }
            s.push('\n');
        }
        if i > 3 {
            s.push_str("Depends: pkg-");
            s.push_str(&(i - 1).to_string());
            s.push_str(" (>= 1.0), pkg-");
            s.push_str(&(i - 2).to_string());
            s.push_str(" | pkg-");
            s.push_str(&(i - 3).to_string());
            s.push('\n');
        }
        if i % 5 == 0 {
            s.push_str("Provides: virtual-");
            s.push_str(&i.to_string());
            s.push_str(", common-feature\n");
        }
        s.push_str("Description: synthetic package ");
        s.push_str(&i.to_string());
        s.push_str("\n continuation line\n\n");
    }
    s
}

/// Build the index through the production streaming path.
fn build(doc: &str) -> PackageIndex {
    let mut parser = StanzaParser::new();
    let mut builder = PackageIndexBuilder::new();
    parser.push_view(doc.as_bytes(), &mut builder);
    parser.finish_view(&mut builder);
    builder.finish()
}

/// Every observable answer of an index, as comparable strings.
fn snapshot(index: &PackageIndex) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!("len={}", index.len()));
    out.push(format!("footprint={}", index.footprint()));
    for name in index.names() {
        out.push(format!("name={}", name));
        let rec = index.get(name).expect("a listed name resolves");
        out.push(format!(
            "  rec v={} a={} f={} s={} sha={:?}",
            rec.version(),
            rec.arch(),
            rec.filename(),
            rec.size(),
            rec.sha256()
        ));
        for group in rec.depends() {
            let alts: Vec<&str> = group.alts().collect();
            out.push(format!("  dep {}", alts.join("|")));
        }
    }
    for i in 0..20usize {
        let virt = format!("virtual-{}", i * 5);
        if let Some(rec) = index.get_provider(&virt) {
            out.push(format!("provider {} -> {}", virt, rec.package()));
        }
    }
    out
}

#[test]
fn compact_round_trip_is_lossless() {
    let doc = sample_doc(400);
    let original = build(&doc);
    let bytes = original.to_compact_bytes();
    let decoded = PackageIndex::from_compact_bytes(&bytes).expect("a fresh encoding must decode");
    assert_eq!(snapshot(&decoded), snapshot(&original));
    // Encoding is deterministic (a re-encode of the decoded index is identical).
    assert_eq!(decoded.to_compact_bytes(), bytes);
    // And it really is the compact representation, not the text: well under the
    // Packages document size for this fixture.
    assert!(
        bytes.len() < doc.len(),
        "compact encoding ({}) must be smaller than the text ({})",
        bytes.len(),
        doc.len()
    );
}

#[test]
fn compact_round_trip_matches_owned_builder_path() {
    let doc = sample_doc(60);
    let records = parse_packages(doc.as_bytes());
    let owned = PackageIndex::from_records(records);
    let streaming = build(&doc);
    assert_eq!(snapshot(&owned), snapshot(&streaming));
    let decoded = PackageIndex::from_compact_bytes(&owned.to_compact_bytes()).unwrap();
    assert_eq!(snapshot(&decoded), snapshot(&owned));
}

#[test]
fn empty_index_round_trips() {
    let index = build("");
    assert_eq!(index.len(), 0);
    let decoded = PackageIndex::from_compact_bytes(&index.to_compact_bytes()).unwrap();
    assert_eq!(decoded.len(), 0);
    assert_eq!(decoded.footprint(), index.footprint());
}

#[test]
fn every_truncation_is_rejected_without_panic() {
    let bytes = build(&sample_doc(20)).to_compact_bytes();
    for cut in 0..bytes.len() {
        assert_eq!(
            PackageIndex::from_compact_bytes(&bytes[..cut]).map(|_| ()),
            None,
            "truncation at {} must be rejected",
            cut
        );
    }
}

#[test]
fn structural_corruptions_are_rejected() {
    let bytes = build(&sample_doc(10)).to_compact_bytes();

    // Bad magic.
    let mut bad = bytes.clone();
    bad[0] ^= 0xff;
    assert!(PackageIndex::from_compact_bytes(&bad).is_none());

    // Wrong version.
    let mut bad = bytes.clone();
    bad[8..12].copy_from_slice(&(COMPACT_VERSION + 1).to_le_bytes());
    assert!(PackageIndex::from_compact_bytes(&bad).is_none());

    // Absurd arena length (larger than the remaining bytes).
    let mut bad = bytes.clone();
    bad[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(PackageIndex::from_compact_bytes(&bad).is_none());

    // Trailing byte: the stream must end exactly.
    let mut bad = bytes.clone();
    bad.push(0);
    assert!(PackageIndex::from_compact_bytes(&bad).is_none());

    // First record's `Package` StrRef pointed outside the arena.
    // Layout: 8 magic + 4 version + 4 arena_len + arena + 4 records_len.
    let arena_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let rec_off = 16 + arena_len + 4;
    let mut bad = bytes.clone();
    bad[rec_off..rec_off + 4].copy_from_slice(&(arena_len as u32 + 7).to_le_bytes());
    assert!(PackageIndex::from_compact_bytes(&bad).is_none());

    // Absurd record count.
    let mut bad = bytes.clone();
    bad[16 + arena_len..20 + arena_len].copy_from_slice(&4_000_001u32.to_le_bytes());
    assert!(PackageIndex::from_compact_bytes(&bad).is_none());
}

#[test]
fn arbitrary_byte_flips_never_panic() {
    let bytes = build(&sample_doc(25)).to_compact_bytes();
    // Flipping any single byte may produce another structurally valid index (the
    // arena is opaque text), but it must never panic and any structural break
    // must be a clean `None`.
    for i in 0..bytes.len() {
        let mut bad = bytes.clone();
        bad[i] ^= 0x80;
        let _ = PackageIndex::from_compact_bytes(&bad);
    }
    // Same for garbage of assorted lengths.
    let junk = [0u8; 7];
    for len in 0..junk.len() {
        assert!(PackageIndex::from_compact_bytes(&junk[..len]).is_none());
    }
}

#[test]
fn record_count_and_ordinals_are_consistent_after_round_trip() {
    // 1000 packages: past any tiny-fixture edge in offset/counter arithmetic.
    let doc = sample_doc(1000);
    let original = build(&doc);
    assert_eq!(original.len(), 1000);
    let decoded = PackageIndex::from_compact_bytes(&original.to_compact_bytes()).unwrap();
    assert_eq!(decoded.len(), 1000);
    assert_eq!(decoded.names().count(), original.names().count());
    for name in original.names() {
        let a = original.get(name).unwrap();
        let b = decoded.get(name).unwrap();
        assert_eq!(a.package(), b.package());
        assert_eq!(a.version(), b.version());
        assert_eq!(a.filename(), b.filename());
        assert_eq!(a.size(), b.size());
        assert_eq!(a.sha256(), b.sha256());
        let da: Vec<String> = a
            .depends()
            .flat_map(|g| g.alts().map(|s| s.to_string()).collect::<Vec<_>>())
            .collect();
        let db: Vec<String> = b
            .depends()
            .flat_map(|g| g.alts().map(|s| s.to_string()).collect::<Vec<_>>())
            .collect();
        assert_eq!(da, db);
    }
}
