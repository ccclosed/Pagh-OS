//! PHASE 0 — cheap host-scale reproduction harness for the `apt update`
//! parse-stage crash (#14 PF, RIP=0x1) seen at ~5000+ packages.
//!
//! The existing property tests (p33-p40) only build indexes from tiny documents
//! (<=8 stanzas). The live crash is at ~5459 packages / ~4 MiB decompressed — a
//! scale never exercised on the host. This module generates a LARGE synthetic
//! `Packages` document (60,000 stanzas) and drives it through every path the
//! kernel uses, to flush out any pure-logic-at-scale defect (u32 cast/offset
//! overflow, Vec index, dep_group arithmetic, sort comparator, etc.).
//!
//! Marked clearly as a diagnostic repro harness; left in place for the follow-up
//! fix decision.

#![cfg(test)]

use alloc::string::String;
use alloc::vec::Vec;

use crate::apt_index::{
    parse_packages, PackageIndex, PackageIndexBuilder, StanzaParser,
};
use crate::deb::{self, Compression};

/// Number of synthetic stanzas. Well past the live-crash point (~5459) and into
/// real-`main`-index territory.
const N_STANZAS: usize = 60_000;

/// Build a large, realistic-ish `Packages` document with `n` stanzas. Each
/// stanza carries Package, Version, Architecture, Filename, Depends (a few
/// names with version constraints + an OR alternative), Provides, and Size, so
/// the dependency-group / provides side-table arithmetic is exercised at scale.
fn build_big_packages(n: usize) -> String {
    // Pre-size generously to avoid reallocation noise (~200+ bytes/stanza).
    let mut s = String::with_capacity(n * 256);
    for i in 0..n {
        // Package name, deterministic and unique.
        let pkg = alloc::format!("pkg-{:06}", i);
        s.push_str("Package: ");
        s.push_str(&pkg);
        s.push('\n');

        s.push_str("Version: ");
        s.push_str(&alloc::format!("{}.{}.{}-{}", i % 10, i % 100, i % 7, i % 3));
        s.push('\n');

        s.push_str("Architecture: amd64\n");

        s.push_str("Filename: ");
        s.push_str(&alloc::format!(
            "pool/main/p/{}/{}_{}.{}_amd64.deb",
            pkg, pkg, i % 10, i % 100
        ));
        s.push('\n');

        // Depends on a couple of earlier packages (so they resolve) plus an OR
        // group, each with a version constraint to exercise strip_atom.
        if i > 2 {
            s.push_str(&alloc::format!(
                "Depends: pkg-{:06} (>= 1.0), pkg-{:06} | pkg-{:06} (>= 2.0)\n",
                i - 1,
                i - 2,
                i - 3
            ));
        }

        // Every 5th package provides a virtual name.
        if i % 5 == 0 {
            s.push_str(&alloc::format!("Provides: virtual-{:06}, feature-x\n", i));
        }

        s.push_str("Maintainer: Pagh-OS <root@pagh>\n");

        // A continuation-line description, like real indexes.
        s.push_str("Description: synthetic package ");
        s.push_str(&pkg);
        s.push('\n');
        s.push_str(" This is a continuation line describing the package in detail\n");
        s.push_str(" across multiple physical lines for realism.\n");

        s.push_str("Size: ");
        s.push_str(&alloc::format!("{}", 1000 + (i as u64) * 37));
        s.push('\n');

        // Blank line: stanza separator.
        s.push('\n');
    }
    s
}

/// Feed `bytes` to the streaming `StanzaParser`/`PackageIndexBuilder` exactly the
/// way the kernel does: small fixed chunks via `push_view`, then `finish_view`.
fn build_index_streaming(bytes: &[u8], chunk: usize) -> PackageIndex {
    let mut parser = StanzaParser::new();
    let mut builder = PackageIndexBuilder::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let end = (pos + chunk).min(bytes.len());
        parser.push_view(&bytes[pos..end], &mut builder);
        pos = end;
    }
    parser.finish_view(&mut builder);
    PackageIndex::from_builder(builder)
}

#[test]
fn big_index_parse_packages_at_scale() {
    let doc = build_big_packages(N_STANZAS);
    let bytes = doc.as_bytes();
    eprintln!(
        "[bigindex] doc = {} bytes ({} KiB), {} stanzas",
        bytes.len(),
        bytes.len() / 1024,
        N_STANZAS
    );

    // 1. Whole-buffer parse_packages.
    let records = parse_packages(bytes);
    assert_eq!(records.len(), N_STANZAS, "parse_packages record count");

    // 2. PackageIndex::from_records (owned-record build + sort).
    let idx_from_records = PackageIndex::from_records(records);
    assert_eq!(idx_from_records.len(), N_STANZAS);

    // 3. Streaming build via push_view (the kernel apt-update path), in small
    //    8 KiB chunks like deb::decompress_stream delivers.
    let idx_stream = build_index_streaming(bytes, 8 * 1024);
    assert_eq!(idx_stream.len(), N_STANZAS);

    eprintln!(
        "[bigindex] footprint: from_records={} bytes, streaming={} bytes",
        idx_from_records.footprint(),
        idx_stream.footprint()
    );
}

#[test]
fn big_index_queries_and_resolver_at_scale() {
    use alloc::collections::BTreeSet;

    let doc = build_big_packages(N_STANZAS);
    let bytes = doc.as_bytes();
    let idx = build_index_streaming(bytes, 8 * 1024);

    // Queries across the whole index.
    let mut found = 0usize;
    for i in (0..N_STANZAS).step_by(101) {
        let name = alloc::format!("pkg-{:06}", i);
        assert!(idx.get(&name).is_some(), "get({name})");
        assert!(idx.contains(&name), "contains({name})");
        found += 1;
    }
    eprintln!("[bigindex] verified {found} sampled package lookups");

    // Virtual-name provider lookup.
    assert!(idx.get_provider("virtual-000000").is_some());
    assert!(idx.get_provider("feature-x").is_some());

    // names()/len()
    assert_eq!(idx.len(), N_STANZAS);
    let name_count = idx.names().count();
    assert_eq!(name_count, N_STANZAS, "names() unique count");

    // Resolver over the big index: resolving the last package pulls a long
    // dependency chain (deep post-order DFS recursion).
    let already: BTreeSet<String> = BTreeSet::new();
    let target = alloc::format!("pkg-{:06}", N_STANZAS - 1);
    let plan = crate::apt_resolve::resolve_install(&idx, &target, &already)
        .expect("resolve_install");
    eprintln!("[bigindex] resolve plan length = {}", plan.len());
    assert!(!plan.is_empty());
    // Dependency-first: target appears last.
    assert_eq!(plan.last().unwrap(), &target);
}

/// Build a valid gzip stream (RFC 1952 wrapper around a raw DEFLATE payload) so
/// the doc can be pushed through `deb::decompress_stream` exactly like the kernel
/// decompresses a fetched `Packages.gz`.
fn gzip_encode(data: &[u8]) -> Vec<u8> {
    // Raw DEFLATE payload.
    let deflate = miniz_oxide::deflate::compress_to_vec(data, 6);

    let mut out = Vec::with_capacity(deflate.len() + 18);
    // Fixed 10-byte header: ID1 ID2 CM FLG MTIME(4) XFL OS.
    out.extend_from_slice(&[0x1f, 0x8b, 0x08, 0x00]);
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // mtime = 0
    out.extend_from_slice(&[0x00, 0xff]); // XFL, OS=unknown
    out.extend_from_slice(&deflate);
    // Trailer: CRC32(data) + ISIZE(data) mod 2^32, both little-endian.
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&((data.len() as u32).to_le_bytes()));
    out
}

/// Table-less CRC32 (IEEE 802.3), enough for a test fixture.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[test]
fn big_index_through_gzip_decompress_stream() {
    let doc = build_big_packages(N_STANZAS);
    let raw = doc.as_bytes();

    // Self-check the gzip helper on a tiny input first, so a construction bug
    // can't masquerade as a parse-stage defect.
    let probe = gzip_encode(b"hello world\n");
    let decoded = deb::decompress_bytes(&probe, Compression::Gzip)
        .expect("gzip helper round-trip");
    assert_eq!(decoded, b"hello world\n");

    let gz = gzip_encode(raw);
    eprintln!(
        "[bigindex] gzip: {} raw -> {} compressed bytes",
        raw.len(),
        gz.len()
    );

    // Mirror the kernel: decompress_stream feeds fixed chunks into push_view.
    let mut parser = StanzaParser::new();
    let mut builder = PackageIndexBuilder::new();
    let mut decompressed = 0usize;
    let total = deb::decompress_stream(
        &gz,
        Compression::Gzip,
        deb::MAX_INDEX_STREAM_BYTES,
        |chunk| {
            decompressed += chunk.len();
            parser.push_view(chunk, &mut builder);
            Ok(())
        },
    )
    .expect("decompress_stream");
    parser.finish_view(&mut builder);

    assert_eq!(total, raw.len(), "decompressed byte count");
    let idx = PackageIndex::from_builder(builder);
    assert_eq!(idx.len(), N_STANZAS, "package count via gzip stream");
    eprintln!(
        "[bigindex] gzip stream produced {} packages, {} decompressed bytes",
        idx.len(),
        decompressed
    );
}
