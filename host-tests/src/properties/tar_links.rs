// Feature: ext2 links (issue #18) — host properties for the tar members that
// carry links (`src/pkg/tar.rs`) and for the pure install plan that decides how
// they are created (`src/pkg/install.rs`, contract `EXT2-LINKS.md` §5).
//
// The two questions these properties answer:
//
//   * **Does the parser see the link the archive really stores?** `'1'` and `'2'`
//     must be different kinds (a hard link needs an existing target, a symlink
//     does not), the target must come back verbatim, and a long path must not be
//     silently truncated — GNU `'L'`/`'K'` headers, the ustar `prefix` field and
//     pax `path=`/`linkpath=` records are all encodings dpkg-adjacent tools emit.
//   * **Is the resulting order executable?** Every hard link must be planned
//     after the member that creates its target (chains included), cycles must be
//     reported instead of silently becoming copies, and symlinks keep archive
//     order and verbatim targets.

use crate::install::{plan_install, InstallOp};
use crate::tar::{
    effective_path, read_tar, write_tar_members, TarEntry, TarError, TarFormat, TarMember, TarType,
};
use proptest::prelude::*;

/// The GNU magic name of an extension header.
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

// ─────────────────────────── the parser's view ───────────────────────────

#[test]
fn symlink_and_hardlink_are_distinct_kinds() {
    let members = [
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
    ];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("a written stream reads back");
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].kind, TarType::Regular);
    assert_eq!(entries[1].kind, TarType::Symlink);
    assert_eq!(entries[1].link_target, "../lib/real");
    assert_eq!(entries[2].kind, TarType::Hardlink);
    assert_eq!(entries[2].link_target, "usr/bin/real");
    // Only link members carry a target.
    assert_eq!(entries[0].link_target, "");
}

#[test]
fn gnu_longname_and_longlink_round_trip() {
    let path = long_name(140);
    let target = long_target(150);
    let members = [
        TarMember::File {
            path: "usr/bin/real",
            content: b"payload",
        },
        TarMember::Symlink {
            path: &path,
            target: &target,
        },
    ];
    let buf = write_tar_members(&members, TarFormat::GnuLongName);
    // Both extension headers are present and are not entries of their own.
    let text = core::str::from_utf8(&buf).unwrap_or("");
    assert!(text.contains(LONG_LINK));

    let entries = read_tar(&buf).expect("GNU long name stream reads back");
    assert_eq!(
        entries.len(),
        2,
        "extension headers are consumed, not returned"
    );
    assert_eq!(entries[1].kind, TarType::Symlink);
    assert_eq!(entries[1].path, path, "the full long path survives");
    assert_eq!(entries[1].link_target, target, "the long target survives");
}

#[test]
fn ustar_prefix_joins_into_the_effective_path() {
    let path = long_name(140);
    let members = [TarMember::File {
        path: &path,
        content: b"x",
    }];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("ustar prefix stream reads back");
    assert_eq!(entries.len(), 1);
    // The path is split across the two fields…
    assert!(!entries[0].prefix.is_empty(), "prefix must be used");
    assert_ne!(entries[0].path, path);
    // …and joining them restores it exactly.
    assert_eq!(effective_path(&entries[0]), path);
}

#[test]
fn pax_records_are_applied_and_malformed_ones_refuse_the_stream() {
    // Hand-assemble a pax `'x'` header followed by a short-named entry: the pax
    // `path=` is what the entry must end up with.
    let long = long_name(130);
    // The length prefix counts the whole record: its own digits, the space, the
    // `key=value` body and the trailing newline (`body` holds the last three).
    let body = format!("path={}\n", long);
    let total = body.len() + body.len().to_string().len() + 1;
    // `body` already ends with the record's newline: adding another one would
    // leave a stray byte after the record and the parser (correctly) refuses it.
    let record = format!("{} {}", total, body).into_bytes();

    let mut buf = Vec::new();
    buf.extend_from_slice(&pax_header(record.len()));
    buf.extend_from_slice(&record);
    let rem = record.len() % 512;
    if rem != 0 {
        buf.resize(buf.len() + (512 - rem), 0);
    }
    buf.extend_from_slice(&plain_header("short", 0, b'0'));
    buf.extend_from_slice(&[0u8; 1024]);

    let entries = read_tar(&buf).expect("a pax stream reads back");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, long.as_str());

    // A malformed record refuses the archive rather than guessing a path.
    let mut bad = Vec::new();
    let broken = b"not-a-length path=/etc/passwd\n";
    bad.extend_from_slice(&pax_header(broken.len()));
    bad.extend_from_slice(broken);
    let rem = broken.len() % 512;
    if rem != 0 {
        bad.resize(bad.len() + (512 - rem), 0);
    }
    bad.extend_from_slice(&plain_header("short", 0, b'0'));
    bad.extend_from_slice(&[0u8; 1024]);
    assert_eq!(read_tar(&bad), Err(TarError::BadExtension));
}

/// A ustar header with `typeflag` `'x'` and `size` bytes of pax payload.
fn pax_header(size: usize) -> [u8; 512] {
    plain_header("PaxHeaders/entry", size, b'x')
}

/// A minimal valid ustar header with `size` bytes of content and `typeflag`.
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

// ─────────────────────── the planner's executable order ───────────────────

/// Every hard link in the plan must come after the operation that creates its
/// target, or be reported (`deferred`/`unresolved`) — never silently become a
/// copy or a reordering hazard.
#[test]
fn hard_links_are_planned_after_their_targets() {
    let members = [
        TarMember::Hardlink {
            path: "a/hard2",
            target: "a/real",
        },
        TarMember::Symlink {
            path: "a/sym",
            target: "/usr/lib/real",
        },
        TarMember::File {
            path: "a/real",
            content: b"payload",
        },
    ];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);

    // The hard link's target appears earlier in the plan, even though the member
    // came first in the archive.
    let mut created: Vec<&str> = Vec::new();
    for op in &plan.ops {
        match op {
            InstallOp::File { path, .. } | InstallOp::Symlink { path, .. } => created.push(path),
            InstallOp::Hardlink { path, target } => {
                assert!(
                    created.contains(&target.as_str()),
                    "hard link {path} planned before its target {target}"
                );
                created.push(path);
            }
        }
    }
    assert!(plan.deferred.is_empty());
    assert!(plan.unresolved.is_empty());

    // A symlink keeps its verbatim target and is planned where it appeared.
    let sym = plan
        .ops
        .iter()
        .find_map(|op| match op {
            InstallOp::Symlink { target, .. } => Some(target.clone()),
            _ => None,
        })
        .expect("symlink op");
    assert_eq!(sym, "/usr/lib/real");
}

#[test]
fn hard_link_cycles_are_reported_not_copied() {
    let members = [
        TarMember::Hardlink {
            path: "a/one",
            target: "a/two",
        },
        TarMember::Hardlink {
            path: "a/two",
            target: "a/one",
        },
    ];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);
    assert_eq!(plan.unresolved.len(), 2, "both cycle members are reported");
    assert!(plan.deferred.is_empty());
    // No file operation was invented for them.
    assert!(!plan
        .ops
        .iter()
        .any(|op| matches!(op, InstallOp::File { .. })));
}

#[test]
fn hard_link_to_a_path_no_member_provides_is_deferred() {
    let members = [TarMember::Hardlink {
        path: "a/hard",
        target: "elsewhere/real",
    }];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);
    assert_eq!(plan.deferred, vec!["a/hard".to_string()]);
    assert!(plan.unresolved.is_empty());
    assert!(matches!(plan.ops[0], InstallOp::Hardlink { .. }));
}

#[test]
fn unsafe_and_empty_targets_are_counted() {
    let members = [
        TarMember::File {
            path: "../escape",
            content: b"x",
        },
        TarMember::Symlink {
            path: "a/empty",
            target: "",
        },
    ];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);
    assert_eq!(plan.skipped_unsafe, 1);
    assert_eq!(plan.skipped_empty_target, 1);
    assert!(plan.ops.is_empty());
}

// ─────────────────────────── a randomized cross-check ─────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Whatever mix of members is written, the plan is executable: each hard
    /// link either follows the creation of its target in the plan or is listed
    /// in `deferred`/`unresolved`, and file operations carry the archive's
    /// content verbatim.
    #[test]
    fn plan_is_executable_for_any_member_mix(
        kinds in prop::collection::vec(0u8..4, 1..10),
        payload in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut members: Vec<TarMember<'_>> = Vec::new();
        let names: Vec<String> = (0..kinds.len()).map(|i| format!("d/f{i}")).collect();
        for (i, kind) in kinds.iter().enumerate() {
            let path = names[i].as_str();
            match kind {
                0 => members.push(TarMember::File { path, content: &payload }),
                1 => members.push(TarMember::Directory { path }),
                2 => members.push(TarMember::Symlink { path, target: "../f0" }),
                _ => members.push(TarMember::Hardlink {
                    path,
                    // Half the time the target exists in the archive, half not.
                    target: if i % 2 == 0 { "d/f0" } else { "elsewhere/f" },
                }),
            }
        }
        let buf = write_tar_members(&members, TarFormat::UstarPrefix);
        let entries: Vec<TarEntry> = read_tar(&buf).expect("written stream reads back");
        let plan = plan_install(&entries);

        let mut created: Vec<String> = Vec::new();
        for op in &plan.ops {
            match op {
                InstallOp::File { path, content, .. } => {
                    // Extract the predicate: a nested `matches!` inside the macro
                    // argument confuses `prop_assert!`'s format-argument parsing.
                    let verbatim = members.iter().any(|m| match m {
                        TarMember::File { path: p, content: c } => {
                            *p == path.as_str() && *c == *content
                        }
                        _ => false,
                    });
                    prop_assert!(verbatim, "file op must carry the archive content");
                    created.push(path.clone());
                }
                InstallOp::Symlink { path, .. } => created.push(path.clone()),
                InstallOp::Hardlink { path, target } => {
                    prop_assert!(
                        created.iter().any(|c| c == target)
                            || plan.deferred.contains(path)
                            || plan.unresolved.contains(path),
                        "hard link {} has no earlier target and is not reported",
                        path
                    );
                    created.push(path.clone());
                }
            }
        }
    }
}
