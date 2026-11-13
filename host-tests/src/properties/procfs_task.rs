// Feature: procfs (issue #11), slice 6 — `/proc/<pid>/task` and
// `/proc/<pid>/task/<tid>` (contract `docs/procfs.md` §13).
//
// The kernel-facing node layer resolves `<tid>` to that task's pid directory, so
// the only new pure identity is the `task` directory's own inode. This file
// pins that identity: it must be distinct from every other entry in the pid's
// stride (the static file slots, the `fd`/`fdinfo` directories and the dynamic
// `fd/<n>`/`fdinfo/<n>` inodes), stable for a path, and never collide with the
// synthetic FNV identity that reserves bit 63 (`synth_ino`). `getdents64`
// reports it as `d_ino` and `stat` reports it as `st_ino`, so a collision would
// make two different paths look like one object.

use crate::procfs_format::{
    parse_pid_component, pid_dir_ino, pid_fd_ino, pid_fdinfo_ino, pid_task_dir_ino, PROC_INO_BASE,
    PROC_PID_TASK_DIR_OFFSET,
};
use proptest::prelude::*;

/// The fd numbers the dynamic inode ranges have to stay clear of (0 is the
/// lowest open descriptor, 1023 the last slot `NOFILE_MAX` admits).
const SAMPLE_FDS: [u32; 4] = [0, 1, 2, 1023];
/// The static file slots under `/proc/<pid>` (cmdline..limits).
const STATIC_SLOTS: [u64; 7] = [1, 2, 3, 4, 5, 6, 7];

proptest! {
    /// The `task` directory's inode is unique within the pid's stride: distinct
    /// from the pid directory, every static file slot, both directory entries
    /// and every dynamic fd-derived inode this kernel can create.
    #[test]
    fn task_dir_inode_is_unique_within_the_pid_stride(
        pid in 1u64..=i32::MAX as u64,
    ) {
        let task = pid_task_dir_ino(pid);
        prop_assert_ne!(task, pid_dir_ino(pid));
        for slot in STATIC_SLOTS {
            prop_assert_ne!(task, pid_dir_ino(pid) + slot);
        }
        // The fd / fdinfo directory offsets (`+0x10`, `+0x11`).
        prop_assert_ne!(task, pid_dir_ino(pid) + 0x10);
        prop_assert_ne!(task, pid_dir_ino(pid) + 0x11);
        for fd in SAMPLE_FDS {
            prop_assert_ne!(task, pid_fd_ino(pid, fd));
            prop_assert_ne!(task, pid_fdinfo_ino(pid, fd));
        }
        // The FNV fallback identity lives above bit 63; a real inode never does.
        prop_assert_eq!(task & (1 << 63), 0);
        prop_assert!(task > PROC_INO_BASE);
    }

    /// Distinct pids get distinct `task` inodes, and the offset is the
    /// documented constant (a path's identity must not depend on the caller).
    #[test]
    fn task_dir_inode_is_path_stable(
        a in 1u64..=i32::MAX as u64,
        b in 1u64..=i32::MAX as u64,
    ) {
        prop_assert_eq!(pid_task_dir_ino(a), pid_dir_ino(a) + PROC_PID_TASK_DIR_OFFSET);
        if a != b {
            prop_assert_ne!(pid_task_dir_ino(a), pid_task_dir_ino(b));
        }
    }

    /// Every decimal thread id a `/proc/<pid>/task` directory can list is parsed
    /// by the same pid parser `/proc` already uses (a task id IS a pid in
    /// pagh), and distinct threads resolve to distinct pid directories.
    #[test]
    fn thread_components_are_pids(tid in 1u64..=i32::MAX as u64, other in 1u64..=i32::MAX as u64) {
        let name = alloc::format!("{tid}");
        prop_assert_eq!(parse_pid_component(&name), Some(tid));
        if tid != other {
            prop_assert_ne!(pid_dir_ino(tid), pid_dir_ino(other));
        }
    }
}

/// The offset is part of the contract table, so it is pinned by value: the
/// kernel/node layer and this file must not drift.
#[test]
fn task_offset_matches_the_contract() {
    assert_eq!(PROC_PID_TASK_DIR_OFFSET, 0x30);
    assert_eq!(pid_task_dir_ino(1), pid_dir_ino(1) + 0x30);
}

/// The fd-derived ranges are the only other dynamic identities in the stride,
/// so their extent is included in the uniqueness argument.
#[test]
fn dynamic_fd_inode_ranges_stay_below_the_stride() {
    // `fd/<n>` lives at `+0x100+n` and `fdinfo/<n>` at `+0x1000+n`; with the
    // 1024-descriptor limit the highest inode is `+0x13FF`, far inside the
    // 0x1_0000 stride — so neighbouring pids can never share an entry.
    assert_eq!(pid_fd_ino(1, 1023), pid_dir_ino(1) + 0x100 + 1023);
    assert_eq!(pid_fdinfo_ino(1, 1023), pid_dir_ino(1) + 0x1000 + 1023);
    assert!(pid_fdinfo_ino(1, 1023) < pid_dir_ino(2));
}