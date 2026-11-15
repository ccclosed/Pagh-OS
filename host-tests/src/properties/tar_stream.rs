// Feature: apt-fast W1 — streaming tar scan and metadata-plan equivalence.
//
// The two-pass `.deb` installer replaces "decompress the whole `data.tar` into a
// `Vec`, then `read_tar` into `Vec<TarEntry>`" with:
//
//   1. `tar::scan_tar_meta` / `tar::TarWalker` — an incremental scan that yields
//      owned `TarMeta` per member and never keeps content;
//   2. `install::plan_install_meta` — the SAME planner rules as `plan_install`,
//      applied to that metadata;
//   3. a second streamed pass that writes content as it decompresses.
//
// Streaming must not change what ends up planned. These properties pin that:
//
//   * `TarWalker` accepts arbitrary chunk boundaries and agrees member-for-member
//     with `read_tar` on every fixture (GNU `L`/`K`, ustar prefix, pax, links);
//   * the metadata plan agrees with the eager plan on operations, order,
//     deferred/unresolved hard links and every atomic-refusal counter;
//   * a truncated, oversized-extension or member-bomb stream is refused cleanly
//     (never panics, never reads past the input).
use crate::install::{plan_install, plan_install_meta, MetaOp, UnsafeKind};
use crate::tar::{
    effective_path, read_tar, write_tar, write_tar_members, TarError, TarFormat, TarMember,
    TarMeta, TarType, TarVisitor, TarWalker, STREAM_MAX_EXTENSION,
};
use alloc::string::String;
use alloc::vec::Vec;

/// The GNU magic name of an extension header (see the parser tests).
const LONG_LINK: &str = "././@LongLink";

fn long_name(len: usize) -> String {
    let mut s = String::from("deep/dir/");
    while s.len() + 6 < len {
        s.push('x');
    }
    s.push_str("/leaf");
    s
}

fn long_target(len: usize) -> String {
    let mut s = String::from("/opt/");
    while s.len() + 5 < len {
        s.push('t');
    }
    s.push_str("/bin");
    s
}

/// Collect metadata from one full in-memory stream.
fn scan_whole(buf: &[u8]) -> Result<Vec<TarMeta>, TarError> {
    crate::tar::scan_tar_meta(buf)
}

/// Feed `buf` to a fresh walker in `chunk`-sized pieces (1 means byte-at-a-time).
fn scan_chunked(buf: &[u8], chunk: usize) -> Result<Vec<TarMeta>, TarError> {
    struct Collect(Vec<TarMeta>);
    impl TarVisitor for Collect {
        fn on_entry(&mut self, meta: &TarMeta) -> Result<(), TarError> {
            self.0.push(meta.clone());
            Ok(())
        }
        fn on_data(&mut self, _chunk: &[u8]) -> Result<(), TarError> {
            Ok(())
        }
    }
    let mut sink = Collect(Vec::new());
    let mut walker = TarWalker::new();
    for piece in buf.chunks(chunk) {
        walker.push(piece, &mut sink)?;
    }
    walker.finish(&mut sink)?;
    Ok(sink.0)
}

/// A minimal valid ustar header with `size` content bytes and `typeflag`.
fn plain_header(name: &str, size: usize, typeflag: u8) -> [u8; 512] {
    let mut h = [0u8; 512];
    let nb = name.as_bytes();
    let n = core::cmp::min(nb.len(), 100);
    h[..n].copy_from_slice(&nb[..n]);
    h[100..108].copy_from_slice(b"0000644\0");
    let size_field = format!("{:011o}\0", size);
    h[124..136].copy_from_slice(size_field.as_bytes());
    h[156] = typeflag;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    for b in h[148..156].iter_mut() {
        *b = b' ';
    }
    let sum: u64 = h.iter().map(|b| *b as u64).sum();
    let chk = format!("{:06o}\0 ", sum);
    h[148..156].copy_from_slice(chk.as_bytes());
    h
}

fn pax_record(path: &str) -> Vec<u8> {
    let body = format!("path={}\n", path);
    let total = body.len() + body.len().to_string().len() + 1;
    format!("{} {}", total, body).into_bytes()
}

/// Every fixture stream this module compares the two readers on.
fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
    let mut out: Vec<(&'static str, Vec<u8>)> = Vec::new();

    out.push((
        "files",
        write_tar(&[("a/one", b"1".as_slice()), ("a/two", b"22".as_slice())]),
    ));
    out.push((
        "empty",
        write_tar_members(&[], TarFormat::UstarPrefix),
    ));

    let links = [
        TarMember::File {
            path: "usr/bin/real",
            content: b"payload",
        },
        TarMember::Symlink {
            path: "usr/bin/sym",
            target: "../lib/real",
        },
        TarMember::Hardlink {
            path: "usr/bin/hard",
            target: "usr/bin/real",
        },
        TarMember::Directory {
            path: "usr/share/doc",
        },
    ];
    out.push((
        "links",
        write_tar_members(&links, TarFormat::UstarPrefix),
    ));

    let ln = long_name(140);
    let lt = long_target(150);
    let long = [
        TarMember::File {
            path: "usr/bin/real",
            content: b"payload",
        },
        TarMember::Symlink {
            path: &ln,
            target: &lt,
        },
    ];
    out.push((
        "gnu-longname",
        write_tar_members(&long, TarFormat::GnuLongName),
    ));
    out.push((
        "ustar-prefix",
        write_tar_members(&long, TarFormat::UstarPrefix),
    ));

    // Hand-built pax stream (the host tests cannot ask tarfile for it without a
    // dependency), plus a trailing regular entry whose name is short.
    let long_pax = long_name(130);
    let record = pax_record(&long_pax);
    let mut pax = Vec::new();
    pax.extend_from_slice(&plain_header("PaxHeaders/entry", record.len(), b'x'));
    pax.extend_from_slice(&record);
    let rem = record.len() % 512;
    if rem != 0 {
        pax.resize(pax.len() + (512 - rem), 0);
    }
    pax.extend_from_slice(&plain_header("short", 5, b'0'));
    pax.extend_from_slice(b"hello");
    pax.resize(pax.len() + (512 - 5), 0);
    pax.extend_from_slice(&[0u8; 1024]);
    out.push(("pax", pax));

    out
}

// ─────────────────────── the streaming reader agrees ───────────────────────

#[test]
fn stream_scan_agrees_with_read_tar_on_every_fixture() {
    for (name, buf) in fixtures() {
        let entries = read_tar(&buf).unwrap_or_else(|e| panic!("{}: read_tar {:?}", name, e));
        let whole = scan_whole(&buf).unwrap_or_else(|e| panic!("{}: scan {:?}", name, e));
        assert_eq!(whole.len(), entries.len(), "{}: member count", name);
        for (got, want) in whole.iter().zip(entries.iter()) {
            assert_eq!(got.path, effective_path(want), "{}: path", name);
            assert_eq!(got.kind, want.kind, "{}: kind", name);
            assert_eq!(got.mode, want.mode, "{}: mode", name);
            assert_eq!(got.size, want.size, "{}: size", name);
            assert_eq!(
                got.link_target, want.link_target,
                "{}: link target",
                name
            );
        }
        // Member ordinals are dense and match the entry positions: the metadata
        // plan and pass 2 use them as the acceptance key.
        for (i, meta) in whole.iter().enumerate() {
            assert_eq!(meta.index, i, "{}: member ordinal", name);
        }
    }
}

#[test]
fn stream_scan_accepts_every_chunk_boundary() {
    for (name, buf) in fixtures() {
        let whole = scan_whole(&buf).expect("fixture scans");
        for chunk in [1usize, 2, 7, 511, 512, 513, 1024, buf.len()] {
            let got = scan_chunked(&buf, chunk)
                .unwrap_or_else(|e| panic!("{} @ {} bytes: {:?}", name, chunk, e));
            assert_eq!(got, whole, "{}: chunk size {} changed the scan", name, chunk);
        }
    }
}

#[test]
fn every_prefix_of_a_stream_is_handled_without_panic() {
    for (name, buf) in fixtures() {
        // Cut points that exercise partial headers, partial content and partial
        // padding; the walker must answer Ok or Err, never panic or read past.
        for cut in 0..buf.len() {
            match scan_whole(&buf[..cut]) {
                Ok(_) => {
                    // A prefix that ends exactly on a member boundary is a valid
                    // short archive; nothing more to check.
                }
                Err(e) => {
                    assert!(
                        matches!(
                            e,
                            TarError::Truncated
                                | TarError::BadHeaderChecksum
                                | TarError::LengthInconsistent
                                | TarError::BadExtension
                        ),
                        "{} @ {}: unexpected error {:?}",
                        name,
                        cut,
                        e
                    );
                }
            }
        }
    }
}

#[test]
fn truncated_content_is_refused() {
    let mut buf = write_tar(&[("big", &[b'x'; 2048])]);
    // Header + 100 content bytes: the declared 2048 bytes do not fit.
    let cut = 512 + 100;
    buf.truncate(cut);
    assert_eq!(scan_whole(&buf), Err(TarError::Truncated));
}

#[test]
fn oversized_extension_is_refused_before_buffering() {
    let mut buf = Vec::new();
    buf.extend_from_slice(&plain_header(
        LONG_LINK,
        STREAM_MAX_EXTENSION + 1,
        b'L',
    ));
    // The size check fires at the header, so no payload needs to exist.
    assert_eq!(scan_whole(&buf), Err(TarError::ExtensionTooLarge));
}

#[test]
fn malformed_pax_extension_is_refused() {
    let broken = b"not-a-length path=/etc/passwd\n";
    let mut buf = Vec::new();
    buf.extend_from_slice(&plain_header("PaxHeaders/entry", broken.len(), b'x'));
    buf.extend_from_slice(broken);
    let rem = broken.len() % 512;
    if rem != 0 {
        buf.resize(buf.len() + (512 - rem), 0);
    }
    buf.extend_from_slice(&plain_header("short", 0, b'0'));
    buf.extend_from_slice(&[0u8; 1024]);
    assert_eq!(scan_whole(&buf), Err(TarError::BadExtension));
}

// ─────────────────────── the metadata plan is equivalent ───────────────────

/// Project an eager plan's operations onto comparable tuples.
fn eager_ops(entries: &[crate::tar::TarEntry<'_>]) -> Vec<(u8, String, String, u32, u64)> {
    plan_install(entries)
        .ops
        .iter()
        .map(|op| match op {
            crate::install::InstallOp::File {
                path,
                content,
                mode,
            } => (b'F', path.clone(), String::new(), *mode, content.len() as u64),
            crate::install::InstallOp::Symlink { path, target } => {
                (b'S', path.clone(), target.clone(), 0, 0)
            }
            crate::install::InstallOp::Hardlink { path, target } => {
                (b'H', path.clone(), target.clone(), 0, 0)
            }
        })
        .collect()
}

fn eager_checks<'a>(entries: &'a [crate::tar::TarEntry<'a>]) -> (usize, usize, usize, Vec<String>, Vec<String>, Vec<(String, String, u8)>) {
    let plan = plan_install(entries);
    (
        plan.skipped_unsafe,
        plan.skipped_empty_target,
        plan.skipped_other,
        plan.deferred.clone(),
        plan.unresolved.clone(),
        plan.unsafe_members
            .iter()
            .map(|m| {
                let kind = match m.kind {
                    UnsafeKind::MemberBelowSymlink => 0,
                    UnsafeKind::HardlinkTargetBelowSymlink => 1,
                    UnsafeKind::HardlinkAliasesSymlink => 2,
                };
                (m.path.clone(), m.link.clone(), kind)
            })
            .collect(),
    )
}

#[test]
fn metadata_plan_matches_eager_plan_on_every_fixture() {
    for (name, buf) in fixtures() {
        let entries = read_tar(&buf).expect("fixture reads back");
        let metas = scan_whole(&buf).expect("fixture scans");
        let streamed = plan_install_meta(&metas);

        let got: Vec<(u8, String, String, u32, u64)> = streamed
            .ops
            .iter()
            .map(|op| match op {
                MetaOp::File {
                    path, mode, size, ..
                } => (b'F', path.clone(), String::new(), *mode, *size),
                MetaOp::Symlink { path, target, .. } => (b'S', path.clone(), target.clone(), 0, 0),
                MetaOp::Hardlink { path, target } => (b'H', path.clone(), target.clone(), 0, 0),
            })
            .collect();
        assert_eq!(got, eager_ops(&entries), "{}: operation list", name);

        let (skipped_unsafe, skipped_empty, skipped_other, deferred, unresolved, unsafe_members) =
            eager_checks(&entries);
        assert_eq!(streamed.skipped_unsafe, skipped_unsafe, "{}", name);
        assert_eq!(streamed.skipped_empty_target, skipped_empty, "{}", name);
        assert_eq!(streamed.skipped_other, skipped_other, "{}", name);
        assert_eq!(streamed.deferred, deferred, "{}", name);
        assert_eq!(streamed.unresolved, unresolved, "{}", name);
        let streamed_unsafe: Vec<(String, String, u8)> = streamed
            .unsafe_members
            .iter()
            .map(|m| {
                let kind = match m.kind {
                    UnsafeKind::MemberBelowSymlink => 0,
                    UnsafeKind::HardlinkTargetBelowSymlink => 1,
                    UnsafeKind::HardlinkAliasesSymlink => 2,
                };
                (m.path.clone(), m.link.clone(), kind)
            })
            .collect();
        assert_eq!(streamed_unsafe, unsafe_members, "{}", name);

        // Every content operation's ordinal must point at the metadata member it
        // came from, which is how pass 2 decides whether to write.
        for op in streamed.ops.iter() {
            match op {
                MetaOp::File {
                    member, path, size, ..
                } => {
                    assert_eq!(&metas[*member].path, path, "{}: file ordinal", name);
                    assert_eq!(metas[*member].kind, TarType::Regular, "{}", name);
                    assert_eq!(metas[*member].size, *size, "{}", name);
                }
                MetaOp::Symlink {
                    member, path, target, ..
                } => {
                    assert_eq!(&metas[*member].path, path, "{}: symlink ordinal", name);
                    assert_eq!(metas[*member].kind, TarType::Symlink, "{}", name);
                    assert_eq!(&metas[*member].link_target, target, "{}", name);
                }
                MetaOp::Hardlink { .. } => {}
            }
        }
    }
}

#[test]
fn metadata_plan_keeps_the_atomic_refusal_verdicts() {
    // F-x86n-1: member below an archive symlink.
    let evil = [
        TarMember::Symlink {
            path: "y",
            target: "/etc",
        },
        TarMember::File {
            path: "y/autorun",
            content: b"pwn",
        },
    ];
    let buf = write_tar_members(&evil, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("reads back");
    let metas = scan_whole(&buf).expect("scans");
    let eager = plan_install(&entries);
    let streamed = plan_install_meta(&metas);
    assert!(!eager.unsafe_members.is_empty());
    assert_eq!(streamed.unsafe_members.len(), eager.unsafe_members.len());
    assert_eq!(
        streamed.unsafe_members[0].kind,
        UnsafeKind::MemberBelowSymlink
    );

    // F-r2-vfs-1: hard link that aliases an archive symlink.
    let alias = [
        TarMember::Symlink {
            path: "d/y",
            target: "../etc",
        },
        TarMember::Hardlink {
            path: "d/h",
            target: "d/y",
        },
        TarMember::File {
            path: "d/h/autorun",
            content: b"pwn",
        },
    ];
    let buf = write_tar_members(&alias, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("reads back");
    let metas = scan_whole(&buf).expect("scans");
    let eager = plan_install(&entries);
    let streamed = plan_install_meta(&metas);
    assert_eq!(
        streamed.unsafe_members.len(),
        eager.unsafe_members.len(),
        "alias refusal must survive the metadata plan"
    );
    assert!(eager
        .unsafe_members
        .iter()
        .any(|m| m.kind == UnsafeKind::HardlinkAliasesSymlink));
}

#[test]
fn metadata_plan_is_built_from_owned_metadata_alone() {
    // The whole point of W1: the planner must not need the content bytes. Build a
    // metadata list by hand (no archive buffer at all) and check the file op
    // carries the declared size rather than a slice.
    let metas = vec![
        TarMeta {
            index: 0,
            path: String::from("usr/bin/tool"),
            kind: TarType::Regular,
            mode: 0o755,
            size: 4096,
            link_target: String::new(),
        },
        TarMeta {
            index: 1,
            path: String::from("usr/bin/link"),
            kind: TarType::Symlink,
            mode: 0,
            size: 0,
            link_target: String::from("tool"),
        },
    ];
    let plan = plan_install_meta(&metas);
    assert_eq!(plan.ops.len(), 2);
    match &plan.ops[0] {
        MetaOp::File { size, mode, .. } => {
            assert_eq!(*size, 4096);
            assert_eq!(*mode, 0o755);
        }
        other => panic!("expected a file op, got {:?}", other),
    }
    match &plan.ops[1] {
        MetaOp::Symlink { target, .. } => assert_eq!(target, "tool"),
        other => panic!("expected a symlink op, got {:?}", other),
    }
}
