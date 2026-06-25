// Feature: apt-large-index, Property 41: LARGE-SCALE pure-logic check for the
// streaming `Packages` index pipeline.
//
// The existing proptest suite only builds tiny indexes (<=8 stanzas), so a bug
// that only manifests at SCALE — index/offset arithmetic edges, a `u32`
// truncation in `StrRef`, a `Vec` capacity assumption, a sort/dedup divergence
// between the owned and streaming builders — would never be exercised. This
// module generates a realistic ~60k-stanza `Packages` document (well over 4 MiB
// of text), builds the index BOTH ways (the owned `from_records` path and the
// chunked streaming `StanzaParser::push_view` -> `PackageIndexBuilder` ->
// `from_builder` path), and asserts:
//
//   * neither path panics at scale,
//   * the two builds are query-equivalent (`get`/`get_provider`/`contains`/
//     `names`/`len` all agree), and
//   * the record count matches `parse_packages` length, and
//   * `apt_resolve::resolve_install` produces identical plans on both builds for
//     several targets.
//
// This reproduces (on the HOST, no QEMU) any pure-logic-at-scale crash in the
// decompress+parse pipeline that the in-kernel `apt update` exercises around a
// few thousand parsed packages / ~4 MiB decompressed.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use crate::apt_index::{parse_packages, PackageIndex, PackageIndexBuilder, StanzaParser};
use crate::apt_resolve::resolve_install;

/// Number of synthetic stanzas. 60k stanzas of realistic fields total well over
/// 4 MiB of `Packages` text — past the ~4 MiB / few-thousand-package point at
/// which the in-kernel fault was observed.
const N: usize = 60_000;

/// Generate a large, realistic synthetic `Packages` document as a UTF-8 string.
///
/// Each stanza carries varied `Package`/`Version`/`Architecture`/`Filename`/
/// `Depends`/`Provides`/`Size` fields, with dependency expressions that reference
/// other generated packages (with version constraints and `|` alternatives) and
/// some virtual `Provides` names, so the resolver and the provides table get a
/// real workout.
fn generate_packages_doc(n: usize) -> String {
    // Pre-size generously: ~120 bytes/stanza keeps reallocations down.
    let mut s = String::with_capacity(n * 128);
    let arches = ["amd64", "all", "i386", "arm64"];
    for i in 0..n {
        let arch = arches[i % arches.len()];
        // Package name (varied prefixes so names are not monotone-sorted as ints).
        s.push_str("Package: pkg-");
        push_name(&mut s, i);
        s.push('\n');

        s.push_str("Version: ");
        // e.g. 1:2.34.5-3
        push_usize(&mut s, i % 4);
        s.push(':');
        push_usize(&mut s, 1 + (i % 50));
        s.push('.');
        push_usize(&mut s, i % 40);
        s.push('.');
        push_usize(&mut s, i % 7);
        s.push('-');
        push_usize(&mut s, 1 + (i % 9));
        s.push('\n');

        s.push_str("Architecture: ");
        s.push_str(arch);
        s.push('\n');

        s.push_str("Maintainer: Synthetic Builder <build");
        push_usize(&mut s, i % 100);
        s.push_str("@example.invalid>\n");

        s.push_str("Filename: pool/main/p/pkg-");
        push_name(&mut s, i);
        s.push_str("/pkg-");
        push_name(&mut s, i);
        s.push('_');
        push_usize(&mut s, 1 + (i % 50));
        s.push('.');
        push_usize(&mut s, i % 7);
        s.push('_');
        s.push_str(arch);
        s.push_str(".deb\n");

        // Depends: reference up to a few earlier packages, with constraints and
        // an occasional OR alternative. Earlier-only refs keep names valid.
        if i > 3 {
            s.push_str("Depends: ");
            let d1 = i - 1;
            let d2 = i - 2 - (i % 3);
            s.push_str("pkg-");
            push_name(&mut s, d1);
            s.push_str(" (>= 1.0)");
            s.push_str(", pkg-");
            push_name(&mut s, d2);
            // Every few packages add an OR-alternative on a virtual name.
            if i % 5 == 0 {
                s.push_str(" | virt-feature-");
                push_usize(&mut s, i % 200);
            }
            s.push('\n');
        }

        // Pre-Depends occasionally (merged into Depends at parse time).
        if i % 11 == 0 && i > 0 {
            s.push_str("Pre-Depends: pkg-");
            push_name(&mut s, i - 1);
            s.push('\n');
        }

        // Provides: some packages provide a virtual feature name.
        if i % 5 == 0 {
            s.push_str("Provides: virt-feature-");
            push_usize(&mut s, i % 200);
            s.push_str(", virt-pkg-");
            push_usize(&mut s, i % 333);
            s.push('\n');
        }

        s.push_str("Size: ");
        push_usize(&mut s, 1024 + (i * 37) % 5_000_000);
        s.push('\n');

        s.push_str("Description: synthetic package number ");
        push_usize(&mut s, i);
        s.push_str(" for the large-index scale test\n");

        // Blank line: stanza separator.
        s.push('\n');
    }
    s
}

/// Append `i` rendered as a fixed lowercase-letter base-26 suffix so package
/// names are realistic and collision-free. base-26 over `a..z` is an injective
/// encoding of `i` (distinct integers -> distinct strings), so every generated
/// `pkg-<name>` is unique.
fn push_name(s: &mut String, i: usize) {
    const ALPH: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    let start = s.len();
    let mut v = i;
    loop {
        s.push(ALPH[v % ALPH.len()] as char);
        v /= ALPH.len();
        if v == 0 {
            break;
        }
    }
    // Reverse the freshly pushed digits to most-significant-first. Reversal is a
    // bijection on strings, so uniqueness of the base-26 encoding is preserved.
    unsafe {
        s.as_mut_vec()[start..].reverse();
    }
}

/// Append `n` in decimal without allocating a temporary `String`.
fn push_usize(s: &mut String, n: usize) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let mut v = n;
    if v == 0 {
        s.push('0');
        return;
    }
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    s.push_str(core::str::from_utf8(&buf[i..]).unwrap());
}

/// Build the index via the chunked streaming path: feed the document to a
/// `StanzaParser` in many small chunks, interning into a `PackageIndexBuilder`,
/// then finish. `chunk` is intentionally small so line/stanza boundaries fall
/// inside chunks (exercising the carry buffer) the way the 64 KiB kernel decode
/// chunks split a real index.
fn build_streaming(doc: &[u8], chunk: usize) -> PackageIndex {
    let mut parser = StanzaParser::new();
    let mut builder = PackageIndexBuilder::new();
    let mut off = 0;
    while off < doc.len() {
        let end = core::cmp::min(off + chunk, doc.len());
        parser.push_view(&doc[off..end], &mut builder);
        off = end;
    }
    parser.finish_view(&mut builder);
    PackageIndex::from_builder(builder)
}

#[test]
fn large_index_owned_and_streaming_are_equivalent() {
    let doc = generate_packages_doc(N);
    // Sanity: the document really is large (well over 4 MiB).
    assert!(
        doc.len() > 4 * 1024 * 1024,
        "doc only {} bytes; expected > 4 MiB",
        doc.len()
    );

    // The owned whole-buffer parse (records in file order).
    let records = parse_packages(doc.as_bytes());
    assert_eq!(records.len(), N, "parse_packages dropped/added stanzas");

    // Build BOTH ways.
    let idx_owned = PackageIndex::from_records(parse_packages(doc.as_bytes()));
    let idx_stream = build_streaming(doc.as_bytes(), 73); // odd small chunk

    // len() matches parse_packages length on both.
    assert_eq!(idx_owned.len(), N);
    assert_eq!(idx_stream.len(), N);

    // names().count() matches between builds (unique names; all pkg-* are unique
    // here so this also equals N).
    let names_owned: Vec<&str> = idx_owned.names().collect();
    let names_stream: Vec<&str> = idx_stream.names().collect();
    assert_eq!(names_owned.len(), N, "unique name count != N");
    assert_eq!(
        names_owned, names_stream,
        "names() iterators diverge between owned and streaming builds"
    );

    // Full query-equivalence over every real package name.
    for &name in &names_owned {
        let a = idx_owned.get(name).expect("owned get hit");
        let b = idx_stream.get(name).expect("stream get hit");
        assert_eq!(a.package(), b.package(), "package mismatch for {}", name);
        assert_eq!(a.version(), b.version(), "version mismatch for {}", name);
        assert_eq!(a.arch(), b.arch(), "arch mismatch for {}", name);
        assert_eq!(a.filename(), b.filename(), "filename mismatch for {}", name);
        assert_eq!(a.size(), b.size(), "size mismatch for {}", name);

        // Depends groups (flattened to owned Vec<Vec<String>>) must match.
        let da: Vec<Vec<String>> = a
            .depends()
            .map(|g| g.alts().map(|s| s.to_string()).collect())
            .collect();
        let db: Vec<Vec<String>> = b
            .depends()
            .map(|g| g.alts().map(|s| s.to_string()).collect())
            .collect();
        assert_eq!(da, db, "depends mismatch for {}", name);

        // contains() agrees.
        assert!(idx_owned.contains(name));
        assert!(idx_stream.contains(name));
    }

    // Virtual-name (Provides) equivalence: get_provider on virtual names must
    // resolve to the same providing package on both builds.
    for v in 0..200usize {
        let mut vname = String::from("virt-feature-");
        push_usize(&mut vname, v);
        let pa = idx_owned.get_provider(&vname).map(|r| r.package().to_string());
        let pb = idx_stream.get_provider(&vname).map(|r| r.package().to_string());
        assert_eq!(pa, pb, "get_provider mismatch for virtual {}", vname);
        // Equal contains() result too.
        assert_eq!(
            idx_owned.contains(&vname),
            idx_stream.contains(&vname),
            "contains mismatch for virtual {}",
            vname
        );
    }

    // resolve_install must agree between builds for several targets (including
    // the last package, which has the deepest dependency chain).
    let already = BTreeSet::new();
    let mut targets: Vec<String> = Vec::new();
    for &t in &[1usize, 100, 5_000, 25_000, 59_999] {
        let mut name = String::from("pkg-");
        push_name(&mut name, t);
        targets.push(name);
    }
    for t in &targets {
        let plan_a = resolve_install(&idx_owned, t, &already);
        let plan_b = resolve_install(&idx_stream, t, &already);
        match (plan_a, plan_b) {
            (Ok(pa), Ok(pb)) => assert_eq!(pa, pb, "resolve plan mismatch for {}", t),
            (Err(_), Err(_)) => {}
            _ => panic!("resolve_install Ok/Err divergence for {}", t),
        }
    }
}
