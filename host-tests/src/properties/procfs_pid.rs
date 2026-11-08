// Feature: procfs (issue #11), contract `docs/procfs.md` §4.9/§7.2. Slice 3 adds
// the per-pid family and `/proc/self/{stat,statm}`. `ps`/`procps` parse the
// `/proc/<pid>/stat` line positionally (field 1 pid, 2 comm in parentheses,
// 3 state, 4 ppid, 18 priority, 20 num_threads, 23 vsize, 24 rss, 39 processor,
// …), so the field COUNT and the parenthesised comm are contract, not cosmetics.

use crate::procfs_format::{
    format_pid_stat, format_statm, format_status, parse_pid_component, state_word, stat_comm,
    PidStatInputs, StatusInputs, VmKb, TASK_NAME_MAX,
};
use proptest::prelude::*;

fn vm(size: u64, rss: u64) -> VmKb {
    let rss = core::cmp::min(rss, size);
    VmKb {
        size,
        rss,
        data: size / 4,
        stk: 0,
        exe: size / 2,
        lib: 0,
    }
}

/// Parse the fixed prefix + the parenthesised comm of a stat line.
fn parse_stat(line: &str) -> (u64, &str, Vec<&str>) {
    let open = line.find('(').expect("comm opens");
    let close = line.rfind(')').expect("comm closes");
    let pid: u64 = line[..open].trim_end().parse().expect("pid");
    let comm = &line[open + 1..close];
    let suffix: Vec<&str> = line[close + 1..].split_whitespace().collect();
    (pid, comm, suffix)
}

proptest! {
    /// 52 Linux fields: pid, comm, state, ppid, and the memory/thread fields all
    /// round-trip; the comm is a single parenthesised token that cannot contain
    /// a newline or a stray `)`.
    #[test]
    fn pid_stat_has_linux_shape(
        name in prop::collection::vec(0x20u8..0x7f, 0..64),
        pid in 1u64..i32::MAX as u64,
        ppid in 0u64..u32::MAX as u64,
        threads in 1u64..1024,
        vm_size in 0u64..(1u64 << 36),
        vm_rss in 0u64..(1u64 << 36),
    ) {
        let vm_rss = core::cmp::min(vm_rss, vm_size);
        let v = vm(vm_size, vm_rss);
        let out = format_pid_stat(&PidStatInputs {
            name: &name,
            pid,
            state: b'R',
            ppid,
            pgrp: 3,
            session: 3,
            threads,
            sig_blocked: 0,
            sig_ign: 0,
            sig_cgt: 0,
            vm: v,
            start_brk: 0x40_0000,
            startstack: 0x7fff_0000,
            exit_code: 0,
        });
        prop_assert!(!out.contains(&0));
        let text = core::str::from_utf8(&out).expect("ASCII line");
        prop_assert!(text.ends_with('\n'));
        prop_assert_eq!(text.matches('\n').count(), 1);

        let (got_pid, comm, suffix) = parse_stat(text);
        let want_comm = stat_comm(&name);
        prop_assert_eq!(got_pid, pid);
        prop_assert_eq!(comm.as_bytes(), want_comm.as_slice());
        prop_assert!(comm.len() <= TASK_NAME_MAX);
        prop_assert!(!comm.contains('\n'));
        prop_assert!(!comm.contains(')'));

        // Fields 3..52 => 50 whitespace-separated tokens.
        prop_assert_eq!(suffix.len(), 50);
        prop_assert_eq!(suffix[0], "R");
        prop_assert_eq!(suffix[1].parse::<u64>().unwrap(), ppid);
        prop_assert_eq!(suffix[15], "20"); // priority
        prop_assert_eq!(suffix[17].parse::<u64>().unwrap(), threads);
        prop_assert_eq!(suffix[20].parse::<u64>().unwrap(), vm_size.saturating_mul(1024));
        prop_assert_eq!(suffix[21].parse::<u64>().unwrap(), vm_rss / 4);
        // The tail is present, not truncated at field 20: procps reads these.
        prop_assert_eq!(suffix[35], "17"); // exit_signal = SIGCHLD
        prop_assert_eq!(suffix[36], "0"); // processor
        prop_assert_eq!(suffix.len(), 50);
    }

    /// `/proc/self/statm` is exactly seven decimal page counts, and they derive
    /// from the same KiB snapshot as status/stat.
    #[test]
    fn statm_is_seven_page_counts(
        vm_size in 0u64..(1u64 << 36),
        vm_rss in 0u64..(1u64 << 36),
    ) {
        let vm_rss = core::cmp::min(vm_rss, vm_size);
        let out = format_statm(&vm(vm_size, vm_rss));
        let text = core::str::from_utf8(&out).unwrap();
        prop_assert!(text.ends_with('\n'));
        let fields: Vec<&str> = text.trim_end_matches('\n').split(' ').collect();
        prop_assert_eq!(fields.len(), 7);
        for f in &fields {
            prop_assert!(f.bytes().all(|b| b.is_ascii_digit()), "field {f:?}");
        }
        prop_assert_eq!(fields[0].parse::<u64>().unwrap(), vm_size / 4);
        prop_assert_eq!(fields[1].parse::<u64>().unwrap(), vm_rss / 4);
        prop_assert_eq!(fields[2], "0");
        prop_assert_eq!(fields[6], "0");
    }

    /// A stopped target is visible in both files (`ps` renders `T`), and status
    /// keeps its one-line-per-key shape with the state letter substituted.
    #[test]
    fn stopped_state_is_reported(pid in 1u64..i32::MAX as u64) {
        let v = vm(8192, 8192);
        let status = format_status(&StatusInputs {
            name: b"child",
            state: b'T',
            umask: 0o022,
            pid,
            tgid: pid,
            ppid: 1,
            threads: 1,
            fd_size: 3,
            sig_pending: 0,
            sig_blocked: 0,
            sig_ign: 0,
            sig_cgt: 0,
            uids: [0, 0, 0, 0],
            gids: [0, 0, 0, 0],
            vm: v,
        });
        let text = core::str::from_utf8(&status).unwrap();
        prop_assert!(text.contains("State:\tT (stopped)\n"));
        let want_pid = format!("Pid:\t{}\n", pid);
        prop_assert!(text.contains(&want_pid));
    }
}

/// The state vocabulary is total, and the comm fallback is never empty (a Linux
/// `comm` is never empty either).
#[test]
fn state_and_comm_helpers_are_total() {
    assert_eq!(state_word(b'R'), "running");
    assert_eq!(state_word(b'T'), "stopped");
    assert_eq!(state_word(b'Z'), "zombie");
    assert_eq!(state_word(0), "unknown");
    assert_eq!(stat_comm(b""), b"pagh");
    assert_eq!(stat_comm(b"a)b\nc"), b"a]b c");
    assert!(stat_comm(&[b'x'; 64]).len() <= TASK_NAME_MAX);
}

/// Pid formatting is decimal and never accepts a non-pid component; this is the
/// pure half of "no wildcard fallback" (the kernel side additionally proves the
/// pid is LIVE before a directory is created).
#[test]
fn pid_formatting_is_decimal_only() {
    for pid in [1u64, 2, 99, 1000, i32::MAX as u64] {
        assert_eq!(parse_pid_component(&format!("{pid}")), Some(pid));
    }
    for bad in ["", "0", "self", "1.0", "0x10", "1\n", " 7", "7 ", "✀"] {
        assert_eq!(parse_pid_component(bad), None);
    }
}