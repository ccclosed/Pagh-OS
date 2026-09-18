// Feature: procfs (issue #11), contract `docs/procfs.md` §4.6/§5.3/§7.2 (property
// topic P55). `/proc/self/maps` is the address-space map read by sanitizers,
// `addr2line`-style symbolizers, Go's `runtime.dumpregs` and debuggers. Those
// consumers parse the fixed fields positionally and binary-search the range list,
// so the line shape, the ordering and the absence of overlap are all contract.
//
// The same file covers the procfs path/inode table (identity must be stable across
// `readdir`/`stat`) and the chroot predicate that keeps `/proc` reachable
// (`docs/procfs.md` §1.3).

use crate::io::guest_path_keeps_root;
use crate::procfs_format::{
    format_maps, MapsRegion, PROC_FILES, PROC_INO_BASE, PROC_ROOT_ORDER, PROC_SELF_FILES,
    PROT_EXEC, PROT_READ, PROT_WRITE,
};
use proptest::prelude::*;

/// Parse one maps line the way a symbolizer does: address range, perms, offset,
/// `major:minor`, inode, optional pathname.
fn parse_line(line: &str) -> Option<(u64, u64, String, u64, (u8, u8), u64, Option<&str>)> {
    let mut it = line.splitn(6, ' ');
    let range = it.next()?;
    let perms = it.next()?;
    let offset = it.next()?;
    let dev = it.next()?;
    let ino = it.next()?;
    let path = it.next();

    let (start, end) = range.split_once('-')?;
    if start.len() < 8 || end.len() < 8 {
        return None;
    }
    if start.contains("0x") || end.contains("0x") {
        return None;
    }
    let start = u64::from_str_radix(start, 16).ok()?;
    let end = u64::from_str_radix(end, 16).ok()?;

    if perms.len() != 4
        || !matches!(perms.as_bytes()[0], b'r' | b'-')
        || !matches!(perms.as_bytes()[1], b'w' | b'-')
        || !matches!(perms.as_bytes()[2], b'x' | b'-')
        || perms.as_bytes()[3] != b'p'
    {
        return None;
    }
    if offset.len() != 8 || u64::from_str_radix(offset, 16).is_err() {
        return None;
    }
    let (maj, min) = dev.split_once(':')?;
    if maj.len() != 2 || min.len() != 2 {
        return None;
    }
    let maj = u8::from_str_radix(maj, 16).ok()?;
    let min = u8::from_str_radix(min, 16).ok()?;
    let ino: u64 = ino.parse().ok()?;
    Some((
        start,
        end,
        perms.to_string(),
        offset.parse().ok()?,
        (maj, min),
        ino,
        path,
    ))
}

fn region() -> impl Strategy<Value = MapsRegion<'static>> {
    (
        0u64..(1u64 << 40),
        1u64..(1u64 << 20),
        prop::sample::select(vec![
            0u32,
            PROT_READ,
            PROT_READ | PROT_WRITE,
            PROT_READ | PROT_EXEC,
        ]),
        prop::option::of(prop::sample::select(vec![
            "[heap]",
            "[stack]",
            "/mnt/bin/app",
        ])),
    )
        .prop_map(|(start, len, prot, path)| MapsRegion {
            start,
            end: start + len,
            prot,
            file_offset: 0,
            dev_major: 0,
            dev_minor: 0,
            ino: 0,
            path,
        })
}

proptest! {
    /// Every line is parseable, ranges are ascending and disjoint, and each input
    /// region is covered by the output.
    #[test]
    fn maps_lines_are_sorted_disjoint_and_cover_the_input(regions in prop::collection::vec(region(), 0..8)) {
        let out = format_maps(&regions);
        let text = core::str::from_utf8(&out).unwrap();
        if regions.is_empty() {
            prop_assert!(out.is_empty());
            return Ok(());
        }
        prop_assert!(text.ends_with('\n'));

        let mut prev_end = 0u64;
        for line in text.trim_end_matches('\n').lines() {
            let (start, end, _perms, _off, _dev, _ino, _path) =
                parse_line(line).unwrap_or_else(|| panic!("unparseable maps line {line:?}"));
            prop_assert!(start < end);
            prop_assert!(start >= prev_end, "ranges must not overlap: {:?}", line);
            prev_end = end;
        }

        // Every input address is inside some emitted range.
        let lines: Vec<(u64, u64)> = text
            .trim_end_matches('\n')
            .lines()
            .map(|l| {
                let (s, e, ..) = parse_line(l).unwrap();
                (s, e)
            })
            .collect();
        for r in &regions {
            prop_assert!(
                lines.iter().any(|(s, e)| *s <= r.start && r.end <= *e),
                "region {:x}-{:x} not covered",
                r.start,
                r.end
            );
        }
    }

    /// An anonymous line ends at the inode — no trailing space and no invented
    /// pathname.
    #[test]
    fn anonymous_lines_have_no_trailing_space(regions in prop::collection::vec(region(), 1..4)) {
        let out = format_maps(&regions);
        let text = core::str::from_utf8(&out).unwrap();
        for line in text.trim_end_matches('\n').lines() {
            let (_s, _e, _p, _o, _d, _i, path) = parse_line(line).unwrap();
            if path.is_none() {
                // Anonymous: exactly the five fixed fields, no trailing space.
                prop_assert_eq!(line.split(' ').count(), 5, "anonymous line {:?}", line);
                prop_assert!(!line.ends_with(' '));
            }
        }
    }
}

/// A labelled region keeps exactly one space before the label, as Linux emits.
#[test]
fn labelled_lines_have_one_space_before_the_path() {
    let out = format_maps(&[
        MapsRegion {
            start: 0x7fff_0000,
            end: 0x7fff_1000,
            prot: PROT_READ | PROT_WRITE,
            file_offset: 0,
            dev_major: 0,
            dev_minor: 0,
            ino: 0,
            path: Some("[stack]"),
        },
        MapsRegion {
            start: 0x400000,
            end: 0x401000,
            prot: PROT_READ | PROT_EXEC,
            file_offset: 0,
            dev_major: 8,
            dev_minor: 0,
            ino: 42,
            path: Some("/mnt/bin/app"),
        },
    ]);
    let text = core::str::from_utf8(&out).unwrap();
    assert!(
        text.contains("7fff0000-7fff1000 rw-p 00000000 00:00 0 [stack]\n"),
        "got:\n{text}"
    );
    assert!(
        text.contains("00400000-00401000 r-xp 00000000 08:00 42 /mnt/bin/app\n"),
        "got:\n{text}"
    );
}

/// Overlapping inputs are coalesced instead of producing a range list a
/// binary-searching consumer would misread.
#[test]
fn overlapping_regions_are_merged() {
    let mk = |start: u64, end: u64, prot: u32| MapsRegion {
        start,
        end,
        prot,
        file_offset: 0,
        dev_major: 0,
        dev_minor: 0,
        ino: 0,
        path: None,
    };
    let out = format_maps(&[
        mk(0x1000, 0x3000, PROT_READ),
        mk(0x2000, 0x4000, PROT_WRITE),
    ]);
    let text = core::str::from_utf8(&out).unwrap();
    assert_eq!(text.lines().count(), 1, "got:\n{text}");
    // `{:08x}` is a minimum width: an 8-digit address is not zero-padded to 16.
    assert!(text.starts_with("00001000-00004000 rw-p"), "got:\n{text}");
}

/// The path/inode table: names as agreed, one identity per path, all inodes
/// distinct, none inside the synthetic FNV range.
#[test]
fn procfs_table_is_stable_and_unique() {
    assert_eq!(PROC_ROOT_ORDER, ["self", "cpuinfo", "meminfo", "uptime"]);
    assert_eq!(
        PROC_FILES.iter().map(|e| e.name).collect::<Vec<_>>(),
        ["cpuinfo", "meminfo", "uptime"]
    );
    assert_eq!(
        PROC_SELF_FILES.iter().map(|e| e.name).collect::<Vec<_>>(),
        ["exe", "cmdline", "status", "maps"]
    );
    assert_eq!(
        crate::procfs_format::proc_file("cpuinfo").unwrap().ino,
        PROC_INO_BASE + 6
    );
    assert!(crate::procfs_format::proc_file("1").is_none());
    assert!(crate::procfs_format::proc_file("stat").is_none());
    assert!(crate::procfs_format::proc_self_file("fd").is_none());
    assert!(crate::procfs_format::proc_self_file("status/x").is_none());
    assert!(crate::procfs_format::proc_self_file("").is_none());

    let mut inos: Vec<u64> = Vec::new();
    for e in PROC_FILES.iter().chain(PROC_SELF_FILES.iter()).copied() {
        assert!(
            e.ino & (1 << 63) == 0,
            "{} must stay below the FNV range",
            e.name
        );
        assert!(
            e.ino > 0x0054_0000,
            "{} must stay above the ramfs range",
            e.name
        );
        assert!(!inos.contains(&e.ino), "duplicate inode for {}", e.name);
        inos.push(e.ino);
    }
}

/// The chroot predicate: only a whole first component keeps the VFS root, which
/// is what makes absolute `/proc/...` reach the synthetic tree.
#[test]
fn chroot_predicate_is_component_exact() {
    for keep in [
        "/proc",
        "/proc/self/status",
        "/mnt",
        "/dev/null",
        "/tmp/x",
        "/sys",
    ] {
        assert!(guest_path_keeps_root(keep), "{keep} must keep the root");
    }
    for map in [
        "/",
        "/usr/bin/x",
        "/process",
        "/procfoo",
        "/devices",
        "/etc/resolv.conf",
    ] {
        assert!(!guest_path_keeps_root(map), "{map} must map under /mnt");
    }
    // The procfs entry point specifically: never remapped.
    assert!(guest_path_keeps_root("/proc"));
    assert!(guest_path_keeps_root("/proc/meminfo"));
}
