// Feature: procfs (issue #11), slice 5 — `/proc/<pid>/fdinfo/<n>` and
// `/proc/<pid>/limits` (contract `docs/procfs.md` §12).
//
// Both files are parsed positionally by userland (`ls -l /proc/self/fdinfo`,
// `ulimit -a`, debuggers). The kernel-facing node layer supplies the real
// per-fd/per-process numbers; this property file pins the pure renderers to
// Linux's byte layout and proves that a sentinel (`unlimited`, or the
// documented 0 "no VFS identity") is never turned into a plausible-looking
// invented number.

use crate::procfs_format::{
    format_fdinfo, format_limits, pid_dir_ino, pid_fd_ino, pid_fdinfo_ino, FdInfo, LimitRow,
    LIMIT_ROWS, RLIMIT_COUNT, RLIM_INFINITY,
};
use proptest::prelude::*;

fn lines(out: &[u8]) -> Vec<&str> {
    core::str::from_utf8(out)
        .expect("procfs fdinfo/limits are ASCII")
        .trim_end_matches('\n')
        .split('\n')
        .collect()
}

fn limits_strategy() -> impl Strategy<Value = [(u64, u64); RLIMIT_COUNT]> {
    prop::collection::vec((any::<u64>(), any::<u64>()), RLIMIT_COUNT..=RLIMIT_COUNT).prop_map(
        |v| {
            let mut out = [(0u64, 0u64); RLIMIT_COUNT];
            out.copy_from_slice(&v);
            out
        },
    )
}

proptest! {
    /// `fdinfo` is exactly Linux's four lines, in Linux's order, and every
    /// value round-trips. `flags` is octal with the mandatory leading `0`.
    #[test]
    fn fdinfo_is_linux_shaped(
        pos in any::<u64>(),
        flags in any::<u64>(),
        mnt_id in any::<u64>(),
        ino in any::<u64>(),
    ) {
        let out = format_fdinfo(&FdInfo { pos, flags, mnt_id, ino });
        let ls = lines(&out);
        prop_assert_eq!(ls.len(), 4);
        prop_assert_eq!(ls[0], format!("pos:\t{pos}"));
        prop_assert_eq!(ls[1], format!("flags:\t0{flags:o}"));
        prop_assert_eq!(ls[2], format!("mnt_id:\t{mnt_id}"));
        prop_assert_eq!(ls[3], format!("ino:\t{ino}"));
        prop_assert!(out.ends_with(b"\n"));
        prop_assert!(!out.contains(&0));

        // Parse it back the way a userland tool would.
        let pos_v: u64 = ls[0].strip_prefix("pos:\t").unwrap().parse().unwrap();
        let flags_oct = ls[1].strip_prefix("flags:\t0").unwrap();
        let flags_v = u64::from_str_radix(flags_oct, 8).unwrap();
        let mnt_v: u64 = ls[2].strip_prefix("mnt_id:\t").unwrap().parse().unwrap();
        let ino_v: u64 = ls[3].strip_prefix("ino:\t").unwrap().parse().unwrap();
        prop_assert_eq!(pos_v, pos);
        prop_assert_eq!(flags_v, flags);
        prop_assert_eq!(mnt_v, mnt_id);
        prop_assert_eq!(ino_v, ino);
    }

    /// `limits` is the exact Linux header plus exactly `RLIMIT_COUNT` rows;
    /// columns are the Linux widths, `unlimited` substitutes only for
    /// `RLIM_INFINITY`, and every other value is the decimal input.
    #[test]
    fn limits_header_and_rows_are_linux_shaped(limits in limits_strategy()) {
        let out = format_limits(&limits);
        let text = core::str::from_utf8(&out).unwrap();
        prop_assert!(text.ends_with('\n'));
        prop_assert!(!text.contains('\0'));
        let ls: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
        prop_assert_eq!(ls.len(), RLIMIT_COUNT + 1);
        prop_assert_eq!(
            ls[0],
            "Limit                     Soft Limit           Hard Limit           Units     "
        );
        prop_assert_eq!(ls[0].len(), 78);

        for (i, (line, row)) in ls[1..].iter().zip(LIMIT_ROWS.iter()).enumerate() {
            let name = line[..25].trim_end();
            prop_assert_eq!(name, row.name);
            let soft = line[26..46].trim_end();
            let hard = line[47..67].trim_end();
            let want_soft = if limits[i].0 == RLIM_INFINITY {
                "unlimited".to_string()
            } else {
                format!("{}", limits[i].0)
            };
            let want_hard = if limits[i].1 == RLIM_INFINITY {
                "unlimited".to_string()
            } else {
                format!("{}", limits[i].1)
            };
            prop_assert_eq!(soft, want_soft.as_str());
            prop_assert_eq!(hard, want_hard.as_str());
            match row.units {
                Some(unit) => {
                    prop_assert_eq!(line.len(), 78);
                    prop_assert_eq!(&line[68..78], format!("{unit:<10}"));
                }
                None => prop_assert_eq!(line.len(), 68),
            }
        }

        // A fabricated value would show up as a digit where the contract says
        // "unlimited"; assert the sentinel path directly for every row.
        let all_inf = [(RLIM_INFINITY, RLIM_INFINITY); RLIMIT_COUNT];
        let all_inf_out = format_limits(&all_inf);
        let all_inf_text = core::str::from_utf8(&all_inf_out).unwrap();
        prop_assert_eq!(all_inf_text.matches("unlimited").count(), 32);
    }
}

#[test]
fn limits_table_is_the_linux_rlimit_table() {
    assert_eq!(RLIMIT_COUNT, 16);
    assert_eq!(RLIMIT_COUNT, LIMIT_ROWS.len());
    let names: Vec<&str> = LIMIT_ROWS.iter().map(|r| r.name).collect();
    assert_eq!(
        names,
        [
            "Max cpu time",
            "Max file size",
            "Max data size",
            "Max stack size",
            "Max core file size",
            "Max resident set",
            "Max processes",
            "Max open files",
            "Max locked memory",
            "Max address space",
            "Max file locks",
            "Max pending signals",
            "Max msgqueue size",
            "Max nice priority",
            "Max realtime priority",
            "Max realtime timeout",
        ]
    );
    assert_eq!(LIMIT_ROWS[13].units, None);
    assert_eq!(LIMIT_ROWS[14].units, None);
    assert_eq!(
        LIMIT_ROWS[15],
        LimitRow {
            name: "Max realtime timeout",
            units: Some("us")
        }
    );

    let zero = [(0u64, 0u64); RLIMIT_COUNT];
    let zero_out = format_limits(&zero);
    let text = core::str::from_utf8(&zero_out).unwrap();
    assert!(text.contains("Max open files            0                    0                    files     "));
}

#[test]
fn fdinfo_and_fd_link_inodes_are_distinct() {
    for pid in [1u64, 2, 42, 999_999, i32::MAX as u64] {
        let dir = crate::procfs_format::pid_dir_ino(pid);
        for fd in [0u32, 1, 2, 3, 1023] {
            let link = pid_fd_ino(pid, fd);
            let info = pid_fdinfo_ino(pid, fd);
            assert_ne!(link, info, "pid {pid} fd {fd}: fd and fdinfo must not share an inode");
            assert!(link > dir && info > dir);
            assert_eq!(link & (1 << 63), 0);
            assert_eq!(info & (1 << 63), 0);
        }
    }
    assert_ne!(pid_dir_ino(7), pid_dir_ino(8));
}
