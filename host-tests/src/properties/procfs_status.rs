// Feature: procfs (issue #11), contract `docs/procfs.md` §4.3/§4.5/§7.2 (property
// topic P53). Two files share this property because the same consumers read both as
// plain text — libuv for `/proc/uptime`, htop/procps/gdb and Go's runtime dumps for
// `/proc/self/status` — and both are pure formatting over scalars.

use crate::procfs_format::{
    format_status, format_uptime, task_name, StatusInputs, VmKb, TASK_NAME_MAX,
};
use proptest::prelude::*;

/// Keys `/proc/self/status` must carry, each exactly once (`docs/procfs.md` §4.5).
const REQUIRED_STATUS_KEYS: [&str; 41] = [
    "Name:",
    "Umask:",
    "State:",
    "Tgid:",
    "Pid:",
    "PPid:",
    "TracerPid:",
    "Uid:",
    "Gid:",
    "FDSize:",
    "Threads:",
    "SigPnd:",
    "ShdPnd:",
    "SigBlk:",
    "SigIgn:",
    "SigCgt:",
    "CapInh:",
    "CapPrm:",
    "CapEff:",
    "CapBnd:",
    "CapAmb:",
    "NoNewPrivs:",
    "Seccomp:",
    "Cpus_allowed:",
    "Cpus_allowed_list:",
    "Mems_allowed:",
    "Mems_allowed_list:",
    "VmPeak:",
    "VmSize:",
    "VmLck:",
    "VmPin:",
    "VmHWM:",
    "VmRSS:",
    "VmData:",
    "VmStk:",
    "VmExe:",
    "VmLib:",
    "VmPTE:",
    "VmSwap:",
    "voluntary_ctxt_switches:",
    "nonvoluntary_ctxt_switches:",
];

/// Render a status file with the given scalars and return it as text. The name is
/// ASCII in these properties; the raw-byte case has its own test below.
fn render(name: &[u8], pid: u64, tgid: u64, ppid: u64, threads: u64, vm: VmKb) -> String {
    let out = format_status(&StatusInputs {
        name,
        umask: 0o022,
        pid,
        tgid,
        ppid,
        threads,
        fd_size: 64,
        sig_pending: 0,
        sig_blocked: 0,
        sig_ign: 0,
        sig_cgt: 0,
        vm,
    });
    String::from_utf8(out).expect("ASCII name in this property")
}

proptest! {
    /// The `/proc/uptime` line keeps Linux's `<sec>.<centi> <idle>.<centi>` shape
    /// and agrees with the tick arithmetic exactly (no drift).
    #[test]
    fn uptime_matches_the_tick_clock(ticks in any::<u64>(), hz in 1u64..100_000) {
        let out = format_uptime(ticks, hz);
        let t = core::str::from_utf8(&out).unwrap();
        prop_assert!(t.ends_with('\n'));
        let body = t.trim_end_matches('\n');
        let (first, idle) = body.split_once(' ').expect("two fields");
        prop_assert_eq!(idle, "0.00");
        let (secs, centis) = first.split_once('.').expect("seconds.centiseconds");
        prop_assert_eq!(centis.len(), 2);
        prop_assert_eq!(secs.parse::<u64>().unwrap(), ticks / hz);
        prop_assert_eq!(centis.parse::<u64>().unwrap(), (ticks % hz) * 100 / hz);
        prop_assert!(centis.parse::<u64>().unwrap() < 100);
    }

    /// Monotone in the tick count, and a zero tick frequency cannot panic.
    #[test]
    fn uptime_is_monotone(ticks in any::<u64>()) {
        let a = format_uptime(ticks, 1000);
        let b = format_uptime(ticks.saturating_add(1), 1000);
        prop_assert!(a <= b);
        let _ = format_uptime(ticks, 0);
    }

    /// Every required status key appears exactly once (matched with its tab, so
    /// `Cpus_allowed:` does not also match `Cpus_allowed_list:`), values
    /// round-trip, and the signal/capability masks are 16 lowercase hex digits.
    #[test]
    fn status_keys_and_values(
        name_seed in prop::collection::vec(0x21u8..0x7fu8, 1..=15),
        pid in 1u64..u32::MAX as u64,
        ppid in 0u64..u32::MAX as u64,
        threads in 1u64..64,
        vm_size in 0u64..(1u64 << 32),
        vm_rss in 0u64..(1u64 << 32),
    ) {
        let vm_rss = core::cmp::min(vm_rss, vm_size);
        let vm = VmKb { size: vm_size, rss: vm_rss, data: 0, stk: 0, exe: 0, lib: 0 };
        let t = render(&name_seed, pid, pid, ppid, threads, vm);

        for key in REQUIRED_STATUS_KEYS {
            let needle = format!("{}\t", key);
            let count = t.lines().filter(|l| l.starts_with(&needle)).count();
            prop_assert_eq!(count, 1, "key {} appears {} times", key, count);
        }
        let want_pid = format!("Pid:\t{}\n", pid);
        let want_tgid = format!("Tgid:\t{}\n", pid);
        let want_ppid = format!("PPid:\t{}\n", ppid);
        let want_threads = format!("Threads:\t{}\n", threads);
        let want_vm_size = format!("VmSize:\t{:>8} kB\n", vm_size);
        let want_vm_rss = format!("VmRSS:\t{:>8} kB\n", vm_rss);
        prop_assert!(t.contains(&want_pid));
        prop_assert!(t.contains(&want_tgid));
        prop_assert!(t.contains(&want_ppid));
        prop_assert!(t.contains(&want_threads));
        prop_assert!(t.contains("Umask:\t0022\n"));
        prop_assert!(t.contains(&want_vm_size));
        prop_assert!(t.contains(&want_vm_rss));
        prop_assert_eq!(t.matches('\n').count(), t.lines().count());
        for key in ["SigPnd:", "SigBlk:", "SigIgn:", "SigCgt:", "CapEff:"] {
            let line = t.lines().find(|l| l.starts_with(key)).unwrap();
            let value = line.split('\t').nth(1).unwrap();
            prop_assert_eq!(value.len(), 16);
            prop_assert!(value.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        }
        prop_assert!(!t.contains('\0'));
        prop_assert!(t.ends_with('\n'));
    }

    /// Raw bytes in `Name` survive verbatim: the line is not re-encoded.
    #[test]
    fn status_name_is_raw_bytes(name in prop::collection::vec(any::<u8>(), 1..=15)) {
        let vm = VmKb { size: 0, rss: 0, data: 0, stk: 0, exe: 0, lib: 0 };
        let out = format_status(&StatusInputs {
            name: &name,
            umask: 0o022,
            pid: 7,
            tgid: 7,
            ppid: 1,
            threads: 1,
            fd_size: 3,
            sig_pending: 0,
            sig_blocked: 0,
            sig_ign: 0,
            sig_cgt: 0,
            vm,
        });
        prop_assert!(out.starts_with(b"Name:\t"));
        prop_assert_eq!(&out["Name:\t".len().."Name:\t".len() + name.len()], &name[..]);
        prop_assert_eq!(out["Name:\t".len() + name.len()], b'\n');
    }
}

/// `Name:` is the basename of the loaded image, capped at 15 bytes — never the
/// whole path, and never empty.
#[test]
fn task_name_is_the_image_basename() {
    assert_eq!(task_name("/mnt/usr/bin/nvim", b"ignored"), b"nvim");
    assert_eq!(task_name("/mnt/bin/busybox", b"busybox"), b"busybox");
    assert_eq!(task_name("", b"/usr/bin/python3"), b"python3");
    assert_eq!(task_name("", b"just-a-name"), b"just-a-name");
    assert_eq!(task_name("", b""), b"pagh");
    // Truncated by bytes, as in Linux, and non-UTF-8 passes through unchanged.
    assert_eq!(
        task_name("/bin/an-extremely-long-program-name", b""),
        b"an-extremely-lo"
    );
    // The image path wins when it is known; `argv[0]` is the fallback.
    assert_eq!(task_name("/bin/x", &[0xff, 0xfe, 0x41]), b"x");
    assert_eq!(task_name("", &[0xff, 0xfe, 0x41]), &[0xff, 0xfe, 0x41]);
    assert!(task_name("/bin/an-extremely-long-program-name", b"").len() <= TASK_NAME_MAX);
}
