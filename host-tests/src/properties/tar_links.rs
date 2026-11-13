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

use crate::install::{plan_install, InstallOp, UnsafeKind};
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

        // F-r2-vfs-1: no planned hard link may target an archive symlink — that
        // would be a second name for the link inode, a construct the installer
        // must refuse rather than plan.
        let symlink_paths: Vec<&str> = members
            .iter()
            .filter_map(|m| match m {
                TarMember::Symlink { path, .. } => Some(*path),
                _ => None,
            })
            .collect();
        for op in &plan.ops {
            if let InstallOp::Hardlink { path, target } = op {
                prop_assert!(
                    !symlink_paths.contains(&target.as_str()),
                    "hard link {} planned onto the archive symlink {}",
                    path,
                    target
                );
            }
        }

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

// ─────────── F-x86n-1: members below the archive's own symlinks ───────────
//
// The installer used to resolve later members' parent directories through *any*
// symlink in the tree. Because link targets are interpreted in the guest
// namespace, `./y -> /etc` maps to `/mnt/etc`, so `./y/autorun` (no `..` anywhere)
// wrote `/mnt/etc/autorun` — executed as root at next boot. The pure planner now
// refuses any member (or hard-link target) whose ancestor component is a symlink
// the same archive plans, and the effectful installer turns a non-empty refusal
// list into a whole-package rejection before writing anything.

/// The F-x86n-1 reproducer archive: `./y -> /etc` then `./y/autorun`.
#[test]
fn plan_refuses_member_below_the_archives_own_symlink() {
    let members = [
        TarMember::Symlink {
            path: "y",
            target: "/etc",
        },
        TarMember::File {
            path: "y/autorun",
            content: b"echo PWN-AUTORUN-VIA-PKG\n",
        },
        TarMember::File {
            path: "usr/bin/ok",
            content: b"ok\n",
        },
    ];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);

    assert_eq!(plan.unsafe_members.len(), 1, "exactly the child is refused");
    let refused = &plan.unsafe_members[0];
    assert_eq!(refused.path, "y/autorun");
    assert_eq!(refused.link, "y");
    assert_eq!(refused.kind, UnsafeKind::MemberBelowSymlink);
    assert!(
        !plan
            .ops
            .iter()
            .any(|op| matches!(op, InstallOp::File { path, .. } if path == "y/autorun")),
        "a refused member must not be planned"
    );
    // The harmless member is still planned; the installer refuses the package.
    assert!(plan.ops.iter().any(
        |op| matches!(op, InstallOp::File { path, .. } if path == "usr/bin/ok")
    ));
}

/// The same refusal when the archive's symlink target is relative and escapes the
/// root: the target string does not matter — descending through the link is what
/// is refused.
#[test]
fn plan_refuses_member_below_a_relative_escaping_symlink() {
    let members = [
        TarMember::Symlink {
            path: "x",
            target: "../../..",
        },
        TarMember::File {
            path: "x/pwned_root",
            content: b"PWN\n",
        },
    ];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);
    assert_eq!(plan.unsafe_members.len(), 1);
    assert_eq!(plan.unsafe_members[0].path, "x/pwned_root");
    assert_eq!(plan.unsafe_members[0].link, "x");
}

/// The check is against every symlink the archive plans, not only the ones that
/// precede the member: hard links execute after all file/symlink operations.
#[test]
fn plan_refuses_member_below_a_later_symlink() {
    let members = [
        TarMember::File {
            path: "x/pwned",
            content: b"PWN\n",
        },
        TarMember::Symlink {
            path: "x",
            target: "/",
        },
    ];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);
    assert_eq!(plan.unsafe_members.len(), 1);
    assert_eq!(plan.unsafe_members[0].path, "x/pwned");
}

/// A hard link whose **member path** is below the archive's own symlink.
#[test]
fn plan_refuses_hardlink_below_the_archives_own_symlink() {
    let members = [
        TarMember::Symlink {
            path: "y",
            target: "/etc",
        },
        TarMember::File {
            path: "usr/bin/real",
            content: b"data",
        },
        TarMember::Hardlink {
            path: "y/hard",
            target: "usr/bin/real",
        },
    ];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);
    assert_eq!(plan.unsafe_members.len(), 1);
    assert_eq!(plan.unsafe_members[0].path, "y/hard");
    assert_eq!(plan.unsafe_members[0].kind, UnsafeKind::MemberBelowSymlink);
    assert!(!plan
        .ops
        .iter()
        .any(|op| matches!(op, InstallOp::Hardlink { path, .. } if path == "y/hard")));
}

/// A hard link whose **target** is below the archive's own symlink (the target is
/// resolved on the installed tree, so it would be reached through the link).
#[test]
fn plan_refuses_hardlink_target_below_the_archives_own_symlink() {
    let members = [
        TarMember::Symlink {
            path: "y",
            target: "/etc",
        },
        TarMember::File {
            path: "y/real",
            content: b"data",
        },
        TarMember::Hardlink {
            path: "h",
            target: "y/real",
        },
    ];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);
    // The target is refused for descending through `y`; the hard link that needed
    // it is refused with it (not deferred, not copied).
    assert_eq!(plan.unsafe_members.len(), 2);
    assert_eq!(plan.unsafe_members[0].path, "y/real");
    assert_eq!(plan.unsafe_members[0].kind, UnsafeKind::MemberBelowSymlink);
    assert_eq!(plan.unsafe_members[1].path, "y/real");
    assert_eq!(
        plan.unsafe_members[1].kind,
        UnsafeKind::HardlinkTargetBelowSymlink
    );
    assert!(plan.deferred.is_empty());
    assert!(!plan
        .ops
        .iter()
        .any(|op| matches!(op, InstallOp::Hardlink { .. })));
}

/// The refusal is by path component: `xy` is not below the link `x`.
#[test]
fn plan_does_not_refuse_a_sibling_with_a_shared_prefix() {
    let members = [
        TarMember::Symlink {
            path: "x",
            target: "target",
        },
        TarMember::File {
            path: "xy/file",
            content: b"ok",
        },
    ];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);
    assert!(plan.unsafe_members.is_empty());
    assert!(plan
        .ops
        .iter()
        .any(|op| matches!(op, InstallOp::File { path, .. } if path == "xy/file")));
}

/// Negative control: the shipped link fixture (`links-pagh`) has symlinks but no
/// member below them, so the planner must accept it unchanged.
#[test]
fn plan_accepts_the_links_fixture() {
    let members = [
        TarMember::File {
            path: "usr/share/links-pagh/real",
            content: b"payload",
        },
        TarMember::Symlink {
            path: "usr/share/links-pagh/rel",
            target: "real",
        },
        TarMember::Symlink {
            path: "usr/share/links-pagh/abs",
            target: "/usr/share/links-pagh/real",
        },
        TarMember::Hardlink {
            path: "usr/share/links-pagh/hard",
            target: "usr/share/links-pagh/real",
        },
        TarMember::File {
            path: "usr/share/links-pagh/deep/file.txt",
            content: b"txt",
        },
    ];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);
    assert!(plan.unsafe_members.is_empty());
    assert_eq!(plan.ops.len(), 5);
    assert!(plan.skipped_unsafe == 0 && plan.skipped_empty_target == 0);
}

// ─────── F-r2-vfs-1: hard links that alias a symlink (round-2 audit) ───────
//
// `link(2)` does not follow the final component, so a tar hard-link member whose
// target is an archive symlink is a **second name for the symlink inode**. The
// round-2 PoC (`attack/r2/vfs/attack-alias.deb`) is
//
//     ./d/pwn       regular payload
//     ./d/y -> ../etc
//     ./d/h         HARDLINK -> ./d/y      (alias of the link inode)
//     ./d/h/autorun HARDLINK -> ./d/pwn    (parent is the alias)
//
// The old plan knew only `d/y`, accepted `d/h`, and the effectful resolver wrote
// `/mnt/etc/autorun`. The planner now treats aliases as links and refuses the
// whole package; these tests pin both the refusal classes and the boundaries.

/// The audit's four-member reproducer: the alias member and the member named
/// below it are refused; the harmless file and the link itself are still planned
/// so the effectful installer can refuse the package atomically.
#[test]
fn plan_refuses_the_round2_hardlink_alias_package() {
    let members = [
        TarMember::File {
            path: "d/pwn",
            content: b"echo R2-VFS-ALIAS-AUTORUN-EXECUTED\n",
        },
        TarMember::Symlink {
            path: "d/y",
            target: "../etc",
        },
        TarMember::Hardlink {
            path: "d/h",
            target: "d/y",
        },
        TarMember::Hardlink {
            path: "d/h/autorun",
            target: "d/pwn",
        },
    ];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);

    let alias = plan
        .unsafe_members
        .iter()
        .find(|m| m.path == "d/h")
        .expect("the hard link to the archive symlink is refused");
    assert_eq!(alias.kind, UnsafeKind::HardlinkAliasesSymlink);
    assert_eq!(alias.link, "d/y");

    let child = plan
        .unsafe_members
        .iter()
        .find(|m| m.path == "d/h/autorun")
        .expect("the member named below the alias is refused");
    assert_eq!(child.kind, UnsafeKind::MemberBelowSymlink);
    assert_eq!(child.link, "d/h");

    // The refused members never reach the executable plan; the file and the link
    // themselves are planned (the package is refused as a whole before any write).
    assert!(!plan
        .ops
        .iter()
        .any(|op| matches!(op, InstallOp::Hardlink { .. })));
    assert!(plan
        .ops
        .iter()
        .any(|op| matches!(op, InstallOp::File { path, .. } if path == "d/pwn")));
    assert!(plan
        .ops
        .iter()
        .any(|op| matches!(op, InstallOp::Symlink { path, .. } if path == "d/y")));
}

/// The alias alone is enough: a hard link to an archive symlink is refused even
/// when no member descends through it (the primitive itself is the violation).
#[test]
fn plan_refuses_a_hardlink_to_an_archive_symlink() {
    let members = [
        TarMember::Symlink {
            path: "y",
            target: "target",
        },
        TarMember::Hardlink {
            path: "h",
            target: "y",
        },
    ];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);
    assert!(plan.unsafe_members.iter().any(|m| {
        m.path == "h" && m.link == "y" && m.kind == UnsafeKind::HardlinkAliasesSymlink
    }));
    assert!(!plan
        .ops
        .iter()
        .any(|op| matches!(op, InstallOp::Hardlink { .. })));
}

/// Alias of alias, with the aliases stored before their target (archive order
/// must not matter): `h2 -> h1 -> y`.
#[test]
fn plan_refuses_alias_of_alias_and_members_below_the_chain() {
    let members = [
        TarMember::Symlink {
            path: "d/y",
            target: "../etc",
        },
        TarMember::Hardlink {
            path: "d/h2",
            target: "d/h1",
        },
        TarMember::Hardlink {
            path: "d/h1",
            target: "d/y",
        },
        TarMember::File {
            path: "d/h2/leaf",
            content: b"PWN-R2-ALIAS-CHAIN\n",
        },
    ];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);

    let refused: Vec<(&str, UnsafeKind)> = plan
        .unsafe_members
        .iter()
        .map(|m| (m.path.as_str(), m.kind))
        .collect();
    assert!(refused.contains(&("d/h1", UnsafeKind::HardlinkAliasesSymlink)));
    assert!(refused.contains(&("d/h2", UnsafeKind::HardlinkAliasesSymlink)));
    assert!(refused.contains(&("d/h2/leaf", UnsafeKind::MemberBelowSymlink)));
    assert!(!plan
        .ops
        .iter()
        .any(|op| matches!(op, InstallOp::Hardlink { .. })));
}

/// `..` components are normalized before the alias check, on both the member path
/// and the hard-link target: `d/x/../h -> d/x/../y` is the same construct.
#[test]
fn plan_refuses_dotdot_normalized_alias_paths() {
    let members = [
        TarMember::Symlink {
            path: "d/y",
            target: "../etc",
        },
        TarMember::Hardlink {
            path: "d/x/../h",
            target: "d/x/../y",
        },
        TarMember::File {
            path: "d/q/../h/autorun",
            content: b"PWN-R2-ALIAS-DOTDOT\n",
        },
    ];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);
    assert!(plan.unsafe_members.iter().any(|m| {
        m.path == "d/h" && m.link == "d/y" && m.kind == UnsafeKind::HardlinkAliasesSymlink
    }));
    assert!(plan
        .unsafe_members
        .iter()
        .any(|m| m.path == "d/h/autorun" && m.kind == UnsafeKind::MemberBelowSymlink));
}

/// Boundary: a hard link to a **regular file** is a normal hard link, not an
/// alias, and keeps its place in the executable plan.
#[test]
fn plan_keeps_normal_hardlinks_to_regular_files() {
    let members = [
        TarMember::File {
            path: "usr/bin/real",
            content: b"payload",
        },
        TarMember::Hardlink {
            path: "usr/bin/hard",
            target: "usr/bin/real",
        },
    ];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);
    assert!(plan.unsafe_members.is_empty());
    assert_eq!(plan.ops.len(), 2);
    assert!(plan.ops.iter().any(|op| matches!(
        op, InstallOp::Hardlink { path, target }
            if path == "usr/bin/hard" && target == "usr/bin/real"
    )));
}

/// Boundary: the planner cannot know about links that already exist on the
/// installed tree, so a hard link to an unknown target is deferred, never
/// guessed into a refusal.
#[test]
fn plan_defers_a_hardlink_to_a_link_it_did_not_create() {
    let members = [TarMember::Hardlink {
        path: "h",
        target: "lib64",
    }];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);
    assert!(plan.unsafe_members.is_empty());
    assert_eq!(plan.deferred, vec!["h".to_string()]);
}

/// The alias refusal is by exact path, not by a shared prefix: a hard link to
/// `y2` is not an alias of the archive link `y`.
#[test]
fn plan_does_not_refuse_a_hardlink_to_a_sibling_of_the_link() {
    let members = [
        TarMember::Symlink {
            path: "y",
            target: "target",
        },
        TarMember::File {
            path: "y2",
            content: b"regular",
        },
        TarMember::Hardlink {
            path: "h",
            target: "y2",
        },
    ];
    let buf = write_tar_members(&members, TarFormat::UstarPrefix);
    let entries = read_tar(&buf).expect("stream reads back");
    let plan = plan_install(&entries);
    assert!(plan.unsafe_members.is_empty());
    assert!(plan
        .ops
        .iter()
        .any(|op| matches!(op, InstallOp::Hardlink { path, .. } if path == "h")));
}
