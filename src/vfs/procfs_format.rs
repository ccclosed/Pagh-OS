//! Pure, `core`+`alloc`-only rendering of the synthetic `/proc` texts (issue #11).
//!
//! Everything a `/proc` file needs to *say* lives here as plain data in, bytes out:
//! no VFS, no kernel state, no allocation-visible side effects. The kernel-facing
//! node layer ([`super::procfs`]) only collects the inputs (PMM counters, tick
//! clock, `CompatState`, CPUID) and hands them to these formatters; the exact
//! byte formats and the path/inode table are here so `host-tests` can
//! `#[path]`-include this file verbatim and property-test it (contract
//! `docs/procfs.md` §7).
//!
//! Formats are Linux-compatible on purpose, and the load-bearing details were
//! taken from the consumers' own parsers (see `docs/procfs.md` §4):
//!   * `meminfo` — libuv `strstr("MemTotal:")` + `sscanf("%llu kB")`;
//!   * `cpuinfo` — libuv `fscanf("processor\t: %u\n")` + literal `"model name\t: "`;
//!   * `uptime`  — libuv/vmstat `sscanf("%lf")`;
//!   * `maps`    — readelf/sanitizers address-range + perms parsing;
//!   * `status`  — key/tab/value lines as in Linux `fs/proc/array.c`.

#![allow(dead_code)]

use alloc::fmt;
use alloc::string::String;
use alloc::vec::Vec;

/// Append a formatted line to `out`.
///
/// `Vec<u8>` does not implement `core::fmt::Write` in this configuration, so the
/// formatted text is produced by `alloc::fmt::format` and copied in — one small
/// allocation per line, which keeps the byte layout obvious and identical in the
/// kernel and under `host-tests`.
fn push(out: &mut Vec<u8>, args: fmt::Arguments<'_>) {
    let s = fmt::format(args);
    out.extend_from_slice(s.as_bytes());
}

// ─── Path / inode table ─────────────────────────────────────────────────────
//
// Inode numbers are constants so a path keeps one identity across every
// `lookup()`: `d_ino` from `getdents64`, `st_ino` from `stat`, and re-opens all
// agree. The base sits above the ext2 range (the ramfs counter starts at
// 0x0054_0000 for exactly that reason, `src/vfs/ramfs.rs`) and below bit 63,
// which `synth_ino` reserves for the FNV identity of nodes with no real inode.

/// Base inode number for every procfs node.
pub const PROC_INO_BASE: u64 = 0x00F0_0000_0000;
/// `/proc` itself (the inner node; `MountNode` reports the FNV identity of the
/// `proc` mount name, see `docs/procfs.md` §2.3).
pub const PROC_ROOT_DIR_INO: u64 = PROC_INO_BASE;
/// `/proc/self`.
pub const PROC_SELF_DIR_INO: u64 = PROC_INO_BASE + 1;
/// `/proc/sys` (`docs/procfs.md` §2.3).
pub const PROC_SYS_DIR_INO: u64 = PROC_INO_BASE + 10;
/// `/proc/sys/kernel`.
pub const PROC_SYS_KERNEL_DIR_INO: u64 = PROC_INO_BASE + 11;

/// Which `/proc` file a node presents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcKind {
    /// `/proc/self/exe` — a symlink to the running image.
    SelfExe,
    /// `/proc/self/cmdline`.
    SelfCmdline,
    /// `/proc/self/status`.
    SelfStatus,
    /// `/proc/self/maps`.
    SelfMaps,
    /// `/proc/self/stat` — the Linux per-task `stat` line of the reader.
    SelfStat,
    /// `/proc/self/statm` — the reader's memory summary in pages.
    SelfStatm,
    /// `/proc/cpuinfo`.
    CpuInfo,
    /// `/proc/meminfo`.
    MemInfo,
    /// `/proc/uptime`.
    Uptime,
    /// `/proc/stat` — the kernel tick/event counters.
    Stat,
    /// `/proc/version` — this kernel's identity string.
    Version,
    /// `/proc/filesystems` — the filesystem drivers this build implements.
    FileSystems,
    /// `/proc/loadavg` — load averages plus live task counts.
    LoadAvg,
    /// `/proc/mounts` — the VFS mount table (Linux field order).
    Mounts,
    /// `/proc/self/mounts` — same table, per-process path.
    SelfMounts,
    /// `/proc/self/cgroup` — root cgroup (no controllers).
    SelfCgroup,
    /// `/proc/<pid>/cgroup`.
    PidCgroup,
    /// `/proc/self/mountinfo`.
    SelfMountInfo,
    /// `/proc/<pid>/mountinfo`.
    PidMountInfo,
    /// `/proc/sys/kernel/ostype`.
    SysOstype,
    /// `/proc/sys/kernel/osrelease`.
    SysOsrelease,
    /// `/proc/sys/kernel/hostname`.
    SysHostname,
    /// `/proc/<pid>/cmdline`.
    PidCmdline,
    /// `/proc/<pid>/stat`.
    PidStat,
    /// `/proc/<pid>/status`.
    PidStatus,
    /// `/proc/<pid>/maps`.
    PidMaps,
}

/// One named entry: its `readdir` name, stable inode, and content kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcEntry {
    pub name: &'static str,
    pub ino: u64,
    pub kind: ProcKind,
}

/// The regular files directly under `/proc` (in `readdir` order).
pub const PROC_FILES: [ProcEntry; 8] = [
    ProcEntry {
        name: "cpuinfo",
        ino: PROC_INO_BASE + 6,
        kind: ProcKind::CpuInfo,
    },
    ProcEntry {
        name: "meminfo",
        ino: PROC_INO_BASE + 7,
        kind: ProcKind::MemInfo,
    },
    ProcEntry {
        name: "uptime",
        ino: PROC_INO_BASE + 8,
        kind: ProcKind::Uptime,
    },
    ProcEntry {
        name: "stat",
        ino: PROC_INO_BASE + 9,
        kind: ProcKind::Stat,
    },
    ProcEntry {
        name: "version",
        ino: PROC_INO_BASE + 15,
        kind: ProcKind::Version,
    },
    ProcEntry {
        name: "filesystems",
        ino: PROC_INO_BASE + 16,
        kind: ProcKind::FileSystems,
    },
    ProcEntry {
        name: "loadavg",
        ino: PROC_INO_BASE + 17,
        kind: ProcKind::LoadAvg,
    },
    ProcEntry {
        name: "mounts",
        ino: PROC_INO_BASE + 18,
        kind: ProcKind::Mounts,
    },
];

/// The regular files under `/proc/sys/kernel` (in `readdir` order).
pub const PROC_SYS_FILES: [ProcEntry; 3] = [
    ProcEntry {
        name: "ostype",
        ino: PROC_INO_BASE + 12,
        kind: ProcKind::SysOstype,
    },
    ProcEntry {
        name: "osrelease",
        ino: PROC_INO_BASE + 13,
        kind: ProcKind::SysOsrelease,
    },
    ProcEntry {
        name: "hostname",
        ino: PROC_INO_BASE + 14,
        kind: ProcKind::SysHostname,
    },
];

/// The regular files under `/proc/self` (in `readdir` order).
///
/// `stat` and `statm` (slice 3) describe the *calling* process, so their inodes
/// are fixed here like the older `self` entries. `/proc/<pid>/stat` uses the
/// pid-derived identity below instead; the two paths describe the same state but
/// are distinct VFS nodes (this `self` is a directory, not a symlink to the pid
/// directory).
pub const PROC_SELF_FILES: [ProcEntry; 9] = [
    ProcEntry {
        name: "exe",
        ino: PROC_INO_BASE + 2,
        kind: ProcKind::SelfExe,
    },
    ProcEntry {
        name: "cmdline",
        ino: PROC_INO_BASE + 3,
        kind: ProcKind::SelfCmdline,
    },
    ProcEntry {
        name: "status",
        ino: PROC_INO_BASE + 4,
        kind: ProcKind::SelfStatus,
    },
    ProcEntry {
        name: "maps",
        ino: PROC_INO_BASE + 5,
        kind: ProcKind::SelfMaps,
    },
    ProcEntry {
        name: "stat",
        ino: PROC_INO_BASE + 10,
        kind: ProcKind::SelfStat,
    },
    ProcEntry {
        name: "statm",
        ino: PROC_INO_BASE + 11,
        kind: ProcKind::SelfStatm,
    },
    ProcEntry {
        name: "mounts",
        ino: PROC_INO_BASE + 19,
        kind: ProcKind::SelfMounts,
    },
    ProcEntry {
        name: "cgroup",
        ino: PROC_INO_BASE + 20,
        kind: ProcKind::SelfCgroup,
    },
    ProcEntry {
        name: "mountinfo",
        ino: PROC_INO_BASE + 21,
        kind: ProcKind::SelfMountInfo,
    },
];

/// `readdir` order of `/proc`'s **static** entries: `self`/`sys` first, then the
/// regular files. Live numeric pid directories are inserted between `self` and
/// `cpuinfo` by the kernel-facing `ProcDir::readdir` (ascending pid, snapshot of
/// the compat registry); they are dynamic and therefore not listed here.
pub const PROC_ROOT_ORDER: [&str; 10] = [
    "self",
    "sys",
    "cpuinfo",
    "meminfo",
    "uptime",
    "stat",
    "version",
    "filesystems",
    "loadavg",
    "mounts",
];

// ─── /proc/<pid> path / inode identity ──────────────────────────────────────
//
// A pid directory has no fixed table row: its identity is derived from the pid,
// so `getdents64` (`d_ino`), `stat` and repeated `lookup`s agree for the whole
// life of the directory. The base sits above the static range (`PROC_INO_BASE +
// 11`) and the stride leaves room for the four files. The pid is masked to its
// low 32 bits: pagh's scheduler hands out small sequential pids, and a 32-bit
// mask keeps the result below bit 63 for any input.

/// First inode of the `/proc/<pid>` directory range.
pub const PROC_PID_DIR_INO_BASE: u64 = PROC_INO_BASE + 0x0000_0000_0010_0000;
/// Inodes reserved per pid directory (`+0` dir, `+1..=4` files).
pub const PROC_PID_INO_STRIDE: u64 = 8;
/// Pids are masked to this many bits before deriving an inode.
pub const PROC_PID_INO_MASK: u64 = 0xFFFF_FFFF;

/// Stable inode of the `/proc/<pid>` directory.
pub fn pid_dir_ino(pid: u64) -> u64 {
    PROC_PID_DIR_INO_BASE + (pid & PROC_PID_INO_MASK) * PROC_PID_INO_STRIDE
}

/// Stable inode of one file under `/proc/<pid>` (`slot` is its table entry).
pub fn pid_file_ino(pid: u64, slot: u64) -> u64 {
    pid_dir_ino(pid) + slot
}

/// One file under `/proc/<pid>`: a static name, its inode slot, and its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcPidEntry {
    pub name: &'static str,
    pub slot: u64,
    pub kind: ProcKind,
}

/// The files under every `/proc/<pid>` directory, in `readdir` order.
pub const PROC_PID_FILES: [ProcPidEntry; 6] = [
    ProcPidEntry {
        name: "cmdline",
        slot: 1,
        kind: ProcKind::PidCmdline,
    },
    ProcPidEntry {
        name: "stat",
        slot: 2,
        kind: ProcKind::PidStat,
    },
    ProcPidEntry {
        name: "status",
        slot: 3,
        kind: ProcKind::PidStatus,
    },
    ProcPidEntry {
        name: "maps",
        slot: 4,
        kind: ProcKind::PidMaps,
    },
    ProcPidEntry {
        name: "cgroup",
        slot: 5,
        kind: ProcKind::PidCgroup,
    },
    ProcPidEntry {
        name: "mountinfo",
        slot: 6,
        kind: ProcKind::PidMountInfo,
    },
];

/// `readdir` order of `/proc/<pid>`.
pub const PROC_PID_ORDER: [&str; 6] = ["cmdline", "stat", "status", "maps", "cgroup", "mountinfo"];

/// Look up a `/proc/<pid>` file by name.
pub fn proc_pid_file(name: &str) -> Option<ProcPidEntry> {
    PROC_PID_FILES.iter().copied().find(|e| e.name == name)
}

/// Parse a `/proc` path component as a Linux pid.
pub fn parse_pid_component(name: &str) -> Option<u64> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || !bytes.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let pid: u64 = name.parse().ok()?;
    if pid == 0 || pid > i32::MAX as u64 {
        return None;
    }
    Some(pid)
}

/// Look up a regular `/proc` file by name (`self`/`sys` are directories).
pub fn proc_file(name: &str) -> Option<ProcEntry> {
    PROC_FILES.iter().copied().find(|e| e.name == name)
}

/// Look up a `/proc/self` file by name.
pub fn proc_self_file(name: &str) -> Option<ProcEntry> {
    PROC_SELF_FILES.iter().copied().find(|e| e.name == name)
}

/// Look up a `/proc/sys/kernel` file by name.
pub fn proc_sys_file(name: &str) -> Option<ProcEntry> {
    PROC_SYS_FILES.iter().copied().find(|e| e.name == name)
}

// ─── /proc/meminfo ──────────────────────────────────────────────────────────

/// Kernel-memory counters for `/proc/meminfo`, already in KiB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemInfoKb {
    pub total_kb: u64,
    pub free_kb: u64,
}

/// Required `meminfo` keys, in order: `(label, value)`.
///
/// `MemAvailable` equals `MemFree` because this kernel has no page cache and no
/// reclaimable slab: that is the honest number, not a placeholder
/// (`docs/procfs.md` DEVIATION-3). Every other counter is a real zero.
fn meminfo_rows(m: &MemInfoKb) -> [(&'static str, u64); 18] {
    [
        ("MemTotal:", m.total_kb),
        ("MemFree:", m.free_kb),
        ("MemAvailable:", m.free_kb),
        ("Buffers:", 0),
        ("Cached:", 0),
        ("SwapCached:", 0),
        ("Active:", 0),
        ("Inactive:", 0),
        ("SwapTotal:", 0),
        ("SwapFree:", 0),
        ("Dirty:", 0),
        ("Writeback:", 0),
        ("AnonPages:", 0),
        ("Mapped:", 0),
        ("Shmem:", 0),
        ("Slab:", 0),
        ("SReclaimable:", 0),
        ("SUnreclaim:", 0),
    ]
}

/// Render `/proc/meminfo`: `label` padded to 16 columns, value right-aligned in
/// 8, then `" kB\n"` — byte-identical in shape to Linux `show_val_kb`.
pub fn format_meminfo(m: &MemInfoKb) -> Vec<u8> {
    let mut out = Vec::with_capacity(1024);
    for (label, kb) in meminfo_rows(m) {
        push(&mut out, format_args!("{:<16}{:>8} kB\n", label, kb));
    }
    out
}

// ─── /proc/uptime ───────────────────────────────────────────────────────────

/// Render `/proc/uptime`: `"<sec>.<centi> <idle_sec>.<idle_centi>\n"`.
///
/// The idle field is `0.00`: this kernel does not account for idle time
/// (`docs/procfs.md` DEVIATION-4) and printing a fabricated non-zero idle would
/// be worse than printing the truth. Consumers (`libuv`, `uptime(1)`, `htop`)
/// read the first field only.
pub fn format_uptime(ticks: u64, tick_hz: u64) -> Vec<u8> {
    let hz = if tick_hz == 0 { 1 } else { tick_hz };
    let secs = ticks / hz;
    let centis = (ticks % hz) * 100 / hz;
    let mut out = Vec::with_capacity(32);
    push(&mut out, format_args!("{}.{:02} 0.00\n", secs, centis));
    out
}

// ─── /proc/version ──────────────────────────────────────────────────────────

/// Render `/proc/version`.
///
/// pagh is not Linux, so this is **not** a fake Linux release string: the first
/// word is the kernel name, the second is the real crate version
/// (`env!("CARGO_PKG_VERSION")`, the same source as `/mnt/etc/pagh-release`),
/// then the machine and the fixed build tag.
///
/// The trailing `#1 SMP` is the shape consumer scripts grep for; `#1` is this
/// kernel's constant build number and `SMP` is part of the requested fixed tag —
/// pagh is a **single-CPU** kernel and this line is not a claim of SMP support.
/// The version is passed in rather than read here so the formatter stays pure and
/// host-testable; the kernel-side node supplies the crate version.
/// Format `/proc/mounts` / `/proc/self/mounts`.
///
/// Linux field order: `device mount_point fstype options dump pass`. pagh has no
/// per-mount options and no separate block devices, so the device field is the
/// mount point itself, options are `rw`, and dump/pass are `0 0`.
/// Format `/proc/*/mountinfo` (Linux's 36-field mountinfo subset).
///
/// pagh has no separate block devices and no per-mount option strings, so the
/// optional-fields section is empty (`-`), major:minor is `0:0`, root is `/`,
/// and both options and super-options are `rw`. IDs are assigned in sorted
/// mount-point order; the root is its own parent, everything else has parent 1.
pub fn format_mountinfo(mounts: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, (point, fs)) in mounts.iter().enumerate() {
        let id = index + 1;
        let parent = if index == 0 { id } else { 1 };
        push(
            &mut out,
            format_args!("{id} {parent} 0:0 / {} rw - {} {} rw\n", point, fs, fs),
        );
    }
    out
}

pub fn format_mounts(mounts: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (point, fs) in mounts {
        push(
            &mut out,
            format_args!("{} {} {} rw 0 0\n", point, point, fs),
        );
    }
    out
}

/// `/proc/*/cgroup` — pagh runs every process in one root cgroup and implements
/// no controllers. Linux's v2 format has one line per hierarchy: `0::/`.
pub fn format_cgroup() -> Vec<u8> {
    Vec::from(&b"0::/\n"[..])
}

pub fn format_version(version: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + version.len());
    push(&mut out, format_args!("pagh {} (x86_64) #1 SMP\n", version));
    out
}

// ─── /proc/filesystems ──────────────────────────────────────────────────────

/// One `/proc/filesystems` row: a filesystem this build actually implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileSystemEntry {
    /// Linux's `nodev` marker: the filesystem needs no block device.
    pub nodev: bool,
    /// The name Linux uses for the filesystem (not the Rust module name).
    pub name: &'static str,
}

/// The filesystem drivers present in this build, in the documented order.
///
/// "Present" means **implemented and registered in the VFS**, not "mounted": Linux
/// lists drivers here regardless of whether an instance is mounted. There is no
/// runtime filesystem registry in pagh, so this table *is* the registry — adding or
/// removing a driver must update it in the same commit (`docs/procfs.md` §4.10).
///
/// * `proc` — `src/vfs/procfs.rs`, synthetic, no block device;
/// * `ramfs` — `src/vfs/ramfs.rs`, in-memory `/tmp`, no block device;
/// * `ext2` — `src/fs/ext2/`, the on-disk root mounted at `/mnt` when present.
pub const FILESYSTEMS: [FileSystemEntry; 3] = [
    FileSystemEntry {
        nodev: true,
        name: "proc",
    },
    FileSystemEntry {
        nodev: true,
        name: "ramfs",
    },
    FileSystemEntry {
        nodev: false,
        name: "ext2",
    },
];

/// Render `/proc/filesystems`: `nodev\t<name>\n` for a device-less filesystem,
/// `\t<name>\n` for one backed by a block device.
///
/// The leading tab on a block-device row is Linux's exact layout (`seq_puts(m,
/// "\text4\n")` without a `nodev` word); the parser everyone uses splits on
/// whitespace, but the byte shape is fixed here so a future `mount -t` reader can
/// rely on it.
pub fn format_filesystems(entries: &[FileSystemEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 * entries.len());
    for e in entries {
        if e.nodev {
            out.extend_from_slice(b"nodev\t");
        } else {
            out.push(b'\t');
        }
        out.extend_from_slice(e.name.as_bytes());
        out.push(b'\n');
    }
    out
}

// ─── /proc/loadavg ──────────────────────────────────────────────────────────

/// `/proc/loadavg` inputs.
///
/// The three load averages are not inputs because pagh has no load accounting:
/// the formatter always emits `0.00 0.00 0.00`, the honest value. libuv's
/// `uv_loadavg` falls back to `sysinfo(2)`, which is already implemented with
/// `loads = [0; 3]` (`docs/procfs.md` §4.11), so nothing downstream is misled.
/// The remaining fields are real: runnable and total are live scheduler counts,
/// `last_pid` is the most recently allocated pid (`0` before the first
/// allocation).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadAvg {
    /// 1-minute average in hundredths (`123` = `1.23`).
    pub one: u32,
    /// 5-minute average in hundredths.
    pub five: u32,
    /// 15-minute average in hundredths.
    pub fifteen: u32,
    /// Ready queue + the running task (idle excluded).
    pub runnable: u64,
    /// Live tasks: runnable + parked/stopped (blocking waits stay runnable).
    pub total: u64,
    /// Last pid handed out by the scheduler, or `0` if none was ever allocated.
    pub last_pid: u64,
}

/// Render `/proc/loadavg`:
/// `"<1m> <5m> <15m> <runnable>/<total> <last_pid>\n"`, averages in hundredths.
///
/// Linux's fourth field is `runnable/total` where `total` is the number of
/// threads. pagh has no blocked (uninterruptible) state and no sleeping-task
/// list, so `total` is every live task in the scheduler's rotation states —
/// runnable plus stopped. Task creation counters are deliberately not used: that
/// would grow forever and is not a live count.
pub fn format_loadavg(l: &LoadAvg) -> Vec<u8> {
    let mut out = Vec::with_capacity(48);
    push(
        &mut out,
        format_args!(
            "{}.{:02} {}.{:02} {}.{:02} {}/{} {}\n",
            l.one / 100,
            l.one % 100,
            l.five / 100,
            l.five % 100,
            l.fifteen / 100,
            l.fifteen % 100,
            l.runnable,
            l.total,
            l.last_pid
        ),
    );
    out
}

// ─── /proc/stat ─────────────────────────────────────────────────────────────

/// Kernel tick and event counters behind `/proc/stat`, already in `USER_HZ`
/// units (one tick = one unit; `USER_HZ == TICK_HZ == 1000`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatInputs {
    /// Ring-3 ticks.
    pub user: u64,
    /// Ring-0 non-idle ticks.
    pub system: u64,
    /// Idle-task / `hlt` ticks.
    pub idle: u64,
    /// Context switches since boot.
    pub ctxt: u64,
    /// Boot wall-clock time in Unix seconds, `0` when the RTC had no valid time.
    pub btime: u64,
    /// Tasks created since boot (Linux `total_forks` analogue).
    pub processes: u64,
    /// Ready + running tasks (idle excluded).
    pub procs_running: u64,
    /// Blocked tasks (this kernel has no blocked state: always `0`).
    pub procs_blocked: u64,
}

/// Render `/proc/stat` for the single-CPU machine.
///
/// Linux field order, exactly 10 numeric columns per CPU line:
///
/// ```text
/// cpu  user nice system idle iowait irq softirq steal guest guest_nice
/// cpu0 …
/// ctxt …
/// btime …
/// processes …
/// procs_running …
/// procs_blocked …
/// ```
///
/// `nice`, `iowait`, `irq`, `softirq`, `steal`, `guest` and `guest_nice` are
/// real zeros: this kernel does not account for them, and inventing values
/// would be worse than saying nothing. For the same reason the `intr` and
/// `softirq` lines are omitted — there is no per-IRQ counter to fill them
/// truthfully (`docs/procfs.md` §4.8, DEVIATION-15).
///
/// The aggregate `cpu` line has **two** spaces after the label: Linux writes
/// `"cpu "` and then each value prefixed by one space (`show_val`), while the
/// per-CPU `cpu0` line writes the label without a trailing space. libuv's
/// `uv_cpu_info` discards the first line, then parses `cpu%u` + six numbers,
/// so the `cpu0` line is what makes Node's `os.cpus()` work on a one-CPU box.
pub fn format_stat(s: &StatInputs) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    push(
        &mut out,
        format_args!("cpu  {} 0 {} {} 0 0 0 0 0 0\n", s.user, s.system, s.idle),
    );
    push(
        &mut out,
        format_args!("cpu0 {} 0 {} {} 0 0 0 0 0 0\n", s.user, s.system, s.idle),
    );
    push(&mut out, format_args!("ctxt {}\n", s.ctxt));
    push(&mut out, format_args!("btime {}\n", s.btime));
    push(&mut out, format_args!("processes {}\n", s.processes));
    push(
        &mut out,
        format_args!("procs_running {}\n", s.procs_running),
    );
    push(
        &mut out,
        format_args!("procs_blocked {}\n", s.procs_blocked),
    );
    out
}

// ─── /proc/sys/kernel ───────────────────────────────────────────────────────

/// `/proc/sys/kernel/ostype`: the kernel's own name, not Linux's.
///
/// Linux answers `Linux\n` here because it reads `init_uts_ns.name.sysname`.
/// pagh's `uname(2)` reports `Linux` for compatibility, but the procfs contract
/// asks for this kernel's identity, so the value is `Pagh` — the same name the
/// project uses (`docs/procfs.md` §4.12).
pub const KERNEL_OSTYPE: &str = "Pagh";

/// `/proc/sys/kernel/hostname`: the nodename `uname(2)` reports.
///
/// There is no hostname variable and no `sethostname(2)` in pagh;
/// `sys_uname` hard-codes `pagh`, so this file mirrors that constant and is
/// therefore always writable-looking but static. A future hostname facility must
/// update both call sites (`src/arch/x86_64/linux/misc.rs` and here).
pub const KERNEL_HOSTNAME: &str = "pagh";

/// Render `/proc/sys/kernel/ostype`: `"Pagh\n"`.
pub fn format_sys_ostype() -> Vec<u8> {
    let mut out = Vec::with_capacity(KERNEL_OSTYPE.len() + 1);
    out.extend_from_slice(KERNEL_OSTYPE.as_bytes());
    out.push(b'\n');
    out
}

/// Render `/proc/sys/kernel/osrelease`: the real crate version plus a newline.
///
/// This is the same `env!("CARGO_PKG_VERSION")` that `/proc/version` embeds and
/// that `provision.rs` writes to `/mnt/etc/pagh-release`; it is passed in so the
/// formatter stays host-testable.
pub fn format_sys_osrelease(version: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(version.len() + 1);
    out.extend_from_slice(version.as_bytes());
    out.push(b'\n');
    out
}

/// Render `/proc/sys/kernel/hostname`: `KERNEL_HOSTNAME` plus a newline.
pub fn format_sys_hostname() -> Vec<u8> {
    let mut out = Vec::with_capacity(KERNEL_HOSTNAME.len() + 1);
    out.extend_from_slice(KERNEL_HOSTNAME.as_bytes());
    out.push(b'\n');
    out
}

// ─── /proc/cpuinfo ──────────────────────────────────────────────────────────

/// Which CPUID word a feature flag lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagWord {
    /// `CPUID.01h:EDX`.
    Leaf1Edx,
    /// `CPUID.01h:ECX`.
    Leaf1Ecx,
    /// `CPUID.07h:0:EBX`.
    Leaf7Ebx,
    /// `CPUID.07h:0:ECX`.
    Leaf7Ecx,
    /// `CPUID.80000001h:EDX`.
    Leaf81Edx,
    /// `CPUID.80000001h:ECX`.
    Leaf81Ecx,
}

/// `(name, word, bit)` for every flag this kernel is willing to advertise.
///
/// The names are the Linux `flags` vocabulary; a name is emitted **iff** its
/// CPUID bit is set, so the file can never claim a feature the CPU lacks
/// (property P52). `rdrand`/`rdseed` are included deliberately: they are the
/// userland-visible basis of the entropy source (issue #16).
pub const CPU_FLAG_TABLE: &[(&str, FlagWord, u8)] = &[
    ("fpu", FlagWord::Leaf1Edx, 0),
    ("vme", FlagWord::Leaf1Edx, 1),
    ("de", FlagWord::Leaf1Edx, 2),
    ("pse", FlagWord::Leaf1Edx, 3),
    ("tsc", FlagWord::Leaf1Edx, 4),
    ("msr", FlagWord::Leaf1Edx, 5),
    ("pae", FlagWord::Leaf1Edx, 6),
    ("mce", FlagWord::Leaf1Edx, 7),
    ("cx8", FlagWord::Leaf1Edx, 8),
    ("apic", FlagWord::Leaf1Edx, 9),
    ("sep", FlagWord::Leaf1Edx, 11),
    ("mtrr", FlagWord::Leaf1Edx, 12),
    ("pge", FlagWord::Leaf1Edx, 13),
    ("mca", FlagWord::Leaf1Edx, 14),
    ("cmov", FlagWord::Leaf1Edx, 15),
    ("pat", FlagWord::Leaf1Edx, 16),
    ("pse36", FlagWord::Leaf1Edx, 17),
    ("psn", FlagWord::Leaf1Edx, 18),
    ("clflush", FlagWord::Leaf1Edx, 19),
    ("mmx", FlagWord::Leaf1Edx, 23),
    ("fxsr", FlagWord::Leaf1Edx, 24),
    ("sse", FlagWord::Leaf1Edx, 25),
    ("sse2", FlagWord::Leaf1Edx, 26),
    ("ht", FlagWord::Leaf1Edx, 28),
    ("pbe", FlagWord::Leaf1Edx, 31),
    ("pni", FlagWord::Leaf1Ecx, 0),
    ("pclmulqdq", FlagWord::Leaf1Ecx, 1),
    ("monitor", FlagWord::Leaf1Ecx, 3),
    ("vmx", FlagWord::Leaf1Ecx, 5),
    ("ssse3", FlagWord::Leaf1Ecx, 9),
    ("fma", FlagWord::Leaf1Ecx, 12),
    ("cx16", FlagWord::Leaf1Ecx, 13),
    ("pcid", FlagWord::Leaf1Ecx, 17),
    ("sse4_1", FlagWord::Leaf1Ecx, 19),
    ("sse4_2", FlagWord::Leaf1Ecx, 20),
    ("x2apic", FlagWord::Leaf1Ecx, 21),
    ("movbe", FlagWord::Leaf1Ecx, 22),
    ("popcnt", FlagWord::Leaf1Ecx, 23),
    ("tsc_deadline_timer", FlagWord::Leaf1Ecx, 24),
    ("aes", FlagWord::Leaf1Ecx, 25),
    ("xsave", FlagWord::Leaf1Ecx, 26),
    ("avx", FlagWord::Leaf1Ecx, 28),
    ("f16c", FlagWord::Leaf1Ecx, 29),
    ("rdrand", FlagWord::Leaf1Ecx, 30),
    ("hypervisor", FlagWord::Leaf1Ecx, 31),
    ("fsgsbase", FlagWord::Leaf7Ebx, 0),
    ("bmi1", FlagWord::Leaf7Ebx, 3),
    ("avx2", FlagWord::Leaf7Ebx, 5),
    ("smep", FlagWord::Leaf7Ebx, 7),
    ("bmi2", FlagWord::Leaf7Ebx, 8),
    ("erms", FlagWord::Leaf7Ebx, 9),
    ("invpcid", FlagWord::Leaf7Ebx, 10),
    ("rdseed", FlagWord::Leaf7Ebx, 18),
    ("adx", FlagWord::Leaf7Ebx, 19),
    ("smap", FlagWord::Leaf7Ebx, 20),
    ("clflushopt", FlagWord::Leaf7Ebx, 23),
    ("clwb", FlagWord::Leaf7Ebx, 24),
    ("sha_ni", FlagWord::Leaf7Ebx, 29),
    ("umip", FlagWord::Leaf7Ecx, 2),
    ("pku", FlagWord::Leaf7Ecx, 3),
    ("ospke", FlagWord::Leaf7Ecx, 4),
];

/// Flags for a decoded CPUID word set, in table order.
pub fn cpu_flags(c: &CpuInfoData) -> Vec<&'static str> {
    let mut out = Vec::new();
    for (name, word, bit) in CPU_FLAG_TABLE.iter().copied() {
        let w = match word {
            FlagWord::Leaf1Edx => c.leaf1_edx,
            FlagWord::Leaf1Ecx => c.leaf1_ecx,
            FlagWord::Leaf7Ebx => c.leaf7_ebx,
            FlagWord::Leaf7Ecx => c.leaf7_ecx,
            FlagWord::Leaf81Edx => c.leaf81_edx,
            FlagWord::Leaf81Ecx => c.leaf81_ecx,
        };
        if w & (1u32 << bit) != 0 {
            out.push(name);
        }
    }
    out
}

/// One CPU's decoded CPUID data (the input side of `/proc/cpuinfo`).
///
/// `Option` fields are the ones that only exist on some CPUs: the frequency leaf
/// (`CPUID.16h`) and the cache descriptor (`CPUID.80000006h`). Absent means the
/// line is omitted rather than filled with an invented number
/// (`docs/procfs.md` §4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuInfoData {
    /// `CPUID.00h` EBX/EDX/ECX, 12 ASCII bytes.
    pub vendor: [u8; 12],
    /// `CPUID.80000002h..80000004h`, 48 ASCII bytes (may be all-NUL).
    pub brand: [u8; 48],
    pub family: u32,
    pub model: u32,
    pub stepping: u32,
    /// `CPUID.00h` EAX — the highest basic leaf.
    pub max_basic: u32,
    /// `CPUID.80000006h` ECX[31:16], KiB.
    pub cache_kb: Option<u32>,
    /// `CPUID.16h` EAX, MHz.
    pub mhz: Option<u32>,
    pub phys_bits: u8,
    pub virt_bits: u8,
    pub leaf1_edx: u32,
    pub leaf1_ecx: u32,
    pub leaf7_ebx: u32,
    pub leaf7_ecx: u32,
    pub leaf81_edx: u32,
    pub leaf81_ecx: u32,
}

impl Default for CpuInfoData {
    fn default() -> Self {
        CpuInfoData {
            vendor: [0; 12],
            brand: [0; 48],
            family: 0,
            model: 0,
            stepping: 0,
            max_basic: 0,
            cache_kb: None,
            mhz: None,
            phys_bits: 0,
            virt_bits: 0,
            leaf1_edx: 0,
            leaf1_ecx: 0,
            leaf7_ebx: 0,
            leaf7_ecx: 0,
            leaf81_edx: 0,
            leaf81_ecx: 0,
        }
    }
}

/// The literal emitted when the brand string is empty or all-NUL: never invent a
/// model name, but keep the key present (`docs/procfs.md` §4.2).
pub const UNKNOWN_MODEL: &str = "Unknown x86_64 CPU";

/// Trim a NUL-padded ASCII field, collapsing inner whitespace runs to one space.
fn clean_ascii(raw: &[u8]) -> alloc::string::String {
    let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
    let mut out = String::new();
    let mut pending_space = false;
    for &b in &raw[..end] {
        let c = if (0x20..0x7f).contains(&b) {
            b as char
        } else {
            ' '
        };
        if c == ' ' {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    out
}

/// Render `/proc/cpuinfo`: one block per entry in `cpus`, each terminated by a
/// blank line, with the exact `\t: ` separators libuv's parser expects.
pub fn format_cpuinfo(cpus: &[CpuInfoData]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1024);
    for (i, c) in cpus.iter().enumerate() {
        let vendor = clean_ascii(&c.vendor);
        let brand_raw = clean_ascii(&c.brand);
        let model_name = if brand_raw.is_empty() {
            String::from(UNKNOWN_MODEL)
        } else {
            brand_raw
        };

        push(&mut out, format_args!("processor\t: {}\n", i));
        if !vendor.is_empty() {
            push(&mut out, format_args!("vendor_id\t: {}\n", vendor));
        }
        push(&mut out, format_args!("cpu family\t: {}\n", c.family));
        push(&mut out, format_args!("model\t\t: {}\n", c.model));
        push(&mut out, format_args!("model name\t: {}\n", model_name));
        push(&mut out, format_args!("stepping\t: {}\n", c.stepping));
        if let Some(mhz) = c.mhz {
            push(&mut out, format_args!("cpu MHz\t\t: {}.000\n", mhz));
        }
        if let Some(kb) = c.cache_kb {
            push(&mut out, format_args!("cache size\t: {} KB\n", kb));
        }
        out.extend_from_slice(b"physical id\t: 0\n");
        out.extend_from_slice(b"siblings\t: 1\n");
        out.extend_from_slice(b"core id\t\t: 0\n");
        out.extend_from_slice(b"cpu cores\t: 1\n");
        out.extend_from_slice(b"apicid\t\t: 0\n");
        out.extend_from_slice(b"initial apicid\t: 0\n");
        out.extend_from_slice(b"fpu\t\t: yes\n");
        out.extend_from_slice(b"fpu_exception\t: yes\n");
        push(&mut out, format_args!("cpuid level\t: {}\n", c.max_basic));
        out.extend_from_slice(b"wp\t\t: yes\n");
        out.extend_from_slice(b"flags\t\t: ");
        let flags = cpu_flags(c);
        for (n, f) in flags.iter().enumerate() {
            if n > 0 {
                out.push(b' ');
            }
            out.extend_from_slice(f.as_bytes());
        }
        out.push(b'\n');
        out.extend_from_slice(b"bugs\t\t:\n");
        if let Some(mhz) = c.mhz {
            push(
                &mut out,
                format_args!("bogomips\t: {}.00\n", mhz.saturating_mul(2)),
            );
        }
        out.extend_from_slice(b"clflush size\t: 64\n");
        out.extend_from_slice(b"cache_alignment\t: 64\n");
        if c.phys_bits > 0 && c.virt_bits > 0 {
            push(
                &mut out,
                format_args!(
                    "address sizes\t: {} bits physical, {} bits virtual\n",
                    c.phys_bits, c.virt_bits
                ),
            );
        }
        out.extend_from_slice(b"power management:\n\n");
    }
    out
}

// ─── /proc/self/cmdline ─────────────────────────────────────────────────────

/// Upper bound for the `/proc/self/cmdline` **payload** (the summed argument
/// bytes), matching the initial-stack argument gate
/// (`src/task/stack.rs::arg_gate`, 4096 combined argv bytes). The NUL
/// terminators are not charged against it, so the largest argv the gate admits is
/// still fully representable; the rendered file is at most `cap + argc` bytes.
pub const CMDLINE_CAP: usize = 4096;

/// Render `/proc/self/cmdline`: every argument followed by NUL, nothing else.
///
/// An argument is never cut in half: once `cap` would be exceeded, that argument
/// and all following ones are dropped (`docs/procfs.md` §4.4). An empty `argv`
/// therefore renders an empty file, exactly like Linux.
pub fn format_cmdline(argv: &[&[u8]], cap: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut payload = 0usize;
    for arg in argv {
        if payload + arg.len() > cap {
            break;
        }
        payload += arg.len();
        out.extend_from_slice(arg);
        out.push(0);
    }
    out
}

// ─── /proc/self/status ──────────────────────────────────────────────────────

/// `TASK_COMM_LEN - 1`, the Linux limit for the `Name:` field.
pub const TASK_NAME_MAX: usize = 15;

/// Linux task name: the basename of the loaded image (not `argv[0]`).
///
/// Falls back to the basename of `argv0` when no image path is known, then to
/// `pagh`. Truncation is by bytes (as in Linux), so a non-UTF-8 argument passes
/// through unchanged.
pub fn task_name(exe_path: &str, argv0: &[u8]) -> Vec<u8> {
    fn basename(bytes: &[u8]) -> &[u8] {
        match bytes.iter().rposition(|b| *b == b'/') {
            Some(i) => &bytes[i + 1..],
            None => bytes,
        }
    }
    let mut base: &[u8] = &[];
    if !exe_path.is_empty() {
        base = basename(exe_path.as_bytes());
    }
    if base.is_empty() {
        base = basename(argv0);
    }
    if base.is_empty() {
        return b"pagh".to_vec();
    }
    base[..core::cmp::min(base.len(), TASK_NAME_MAX)].to_vec()
}

/// `/proc/self/status` inputs, all plain scalars.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmKb {
    pub size: u64,
    pub rss: u64,
    pub data: u64,
    pub stk: u64,
    pub exe: u64,
    pub lib: u64,
}

/// `/proc/self/status` inputs.
#[derive(Clone, Copy, Debug)]
pub struct StatusInputs<'a> {
    /// Task name (≤ 15 bytes, raw bytes on purpose).
    pub name: &'a [u8],
    /// Linux state letter: `R` running, `T` stopped (the two this kernel can
    /// truthfully distinguish).
    pub state: u8,
    pub umask: u32,
    pub pid: u64,
    pub tgid: u64,
    pub ppid: u64,
    pub threads: u64,
    pub fd_size: u64,
    pub sig_pending: u64,
    pub sig_blocked: u64,
    /// Signals with `SIG_IGN` disposition.
    pub sig_ign: u64,
    /// Signals with a user handler installed.
    pub sig_cgt: u64,
    /// `[real, effective, saved, filesystem]` user ids. pagh has no separate
    /// fsuid, so the fourth entry is the effective id.
    pub uids: [u32; 4],
    /// `[real, effective, saved, filesystem]` group ids.
    pub gids: [u32; 4],
    pub vm: VmKb,
}

/// Long-form description of a Linux task-state letter for the `State:` line.
///
/// Only `R` and `T` are ever produced by this kernel (task 4 of the procfs
/// slice: there is no sleeping/blocked task bookkeeping and zombies are removed
/// from the compat registry at exit). The remaining Linux letters are mapped
/// too so a future producer cannot silently emit "unknown".
pub fn state_word(state: u8) -> &'static str {
    match state {
        b'R' => "running",
        b'S' => "sleeping",
        b'D' => "disk sleep",
        b'T' => "stopped",
        b't' => "tracing stop",
        b'Z' => "zombie",
        b'X' | b'x' => "dead",
        b'I' => "idle",
        _ => "unknown",
    }
}

/// Render `/proc/self/status`.
///
/// Field order and separators follow Linux `fs/proc/array.c`. Capabilities are
/// reported as zero: this kernel has no capability model and claiming the root
/// set for a model that is not enforced would be a lie (`docs/procfs.md` §4.5).
pub fn format_status(s: &StatusInputs<'_>) -> Vec<u8> {
    let vm = s.vm;
    let mut out = Vec::with_capacity(1024);
    out.extend_from_slice(b"Name:\t");
    out.extend_from_slice(s.name);
    out.push(b'\n');
    push(&mut out, format_args!("Umask:\t{:04o}\n", s.umask));
    push(
        &mut out,
        format_args!("State:\t{} ({})\n", s.state as char, state_word(s.state)),
    );
    push(&mut out, format_args!("Tgid:\t{}\n", s.tgid));
    push(&mut out, format_args!("Pid:\t{}\n", s.pid));
    push(&mut out, format_args!("PPid:\t{}\n", s.ppid));
    out.extend_from_slice(b"TracerPid:\t0\n");
    push(
        &mut out,
        format_args!(
            "Uid:\t{}\t{}\t{}\t{}\n",
            s.uids[0], s.uids[1], s.uids[2], s.uids[3]
        ),
    );
    push(
        &mut out,
        format_args!(
            "Gid:\t{}\t{}\t{}\t{}\n",
            s.gids[0], s.gids[1], s.gids[2], s.gids[3]
        ),
    );
    push(&mut out, format_args!("FDSize:\t{}\n", s.fd_size));
    push(&mut out, format_args!("Threads:\t{}\n", s.threads));
    push(&mut out, format_args!("SigPnd:\t{:016x}\n", s.sig_pending));
    out.extend_from_slice(b"ShdPnd:\t0000000000000000\n");
    push(&mut out, format_args!("SigBlk:\t{:016x}\n", s.sig_blocked));
    push(&mut out, format_args!("SigIgn:\t{:016x}\n", s.sig_ign));
    push(&mut out, format_args!("SigCgt:\t{:016x}\n", s.sig_cgt));
    for key in ["CapInh:", "CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"] {
        push(&mut out, format_args!("{}\t0000000000000000\n", key));
    }
    out.extend_from_slice(b"NoNewPrivs:\t0\n");
    out.extend_from_slice(b"Seccomp:\t0\n");
    out.extend_from_slice(b"Cpus_allowed:\t1\n");
    out.extend_from_slice(b"Cpus_allowed_list:\t0\n");
    out.extend_from_slice(b"Mems_allowed:\t1\n");
    out.extend_from_slice(b"Mems_allowed_list:\t0\n");
    push(&mut out, format_args!("VmPeak:\t{:>8} kB\n", vm.size));
    push(&mut out, format_args!("VmSize:\t{:>8} kB\n", vm.size));
    out.extend_from_slice(b"VmLck:\t       0 kB\n");
    out.extend_from_slice(b"VmPin:\t       0 kB\n");
    push(&mut out, format_args!("VmHWM:\t{:>8} kB\n", vm.rss));
    push(&mut out, format_args!("VmRSS:\t{:>8} kB\n", vm.rss));
    push(&mut out, format_args!("VmData:\t{:>8} kB\n", vm.data));
    push(&mut out, format_args!("VmStk:\t{:>8} kB\n", vm.stk));
    push(&mut out, format_args!("VmExe:\t{:>8} kB\n", vm.exe));
    push(&mut out, format_args!("VmLib:\t{:>8} kB\n", vm.lib));
    out.extend_from_slice(b"VmPTE:\t       0 kB\n");
    out.extend_from_slice(b"VmSwap:\t       0 kB\n");
    out.extend_from_slice(b"voluntary_ctxt_switches:\t0\n");
    out.extend_from_slice(b"nonvoluntary_ctxt_switches:\t0\n");
    out
}

// ─── /proc/<pid>/stat and /proc/self/statm ──────────────────────────────────

/// `/proc/<pid>/stat` inputs.
///
/// The line has 52 Linux fields; the ones this kernel can answer truthfully come
/// from the target's `CompatState` snapshot, and the rest are the documented
/// "not tracked" zeros (see the renderer). Keeping the full Linux width means
/// `ps`/`procps` can index the fields it cares about (18 priority, 20 threads,
/// 22 starttime, 23 vsize, 24 rss, 39 processor, …) without falling off the end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PidStatInputs<'a> {
    /// Task name (raw bytes; capped and sanitized by [`stat_comm`]).
    pub name: &'a [u8],
    pub pid: u64,
    /// Linux state letter (`R` or `T` here).
    pub state: u8,
    pub ppid: u64,
    /// Process group id. pagh has one group per thread group, so this is `tgid`.
    pub pgrp: u64,
    /// Session id; pagh has no sessions, so this is `tgid` too.
    pub session: u64,
    pub threads: u64,
    pub sig_blocked: u64,
    pub sig_ign: u64,
    pub sig_cgt: u64,
    pub vm: VmKb,
    /// Current program break (field 47, `start_brk`).
    pub start_brk: u64,
    /// Lowest address of the mapped user stack (field 28, `startstack`).
    pub startstack: u64,
    /// Field 52, `exit_code`: 0 until the process exits (a zombie is already gone
    /// from the compat registry in this kernel, so it is always 0 in practice).
    pub exit_code: i64,
}

/// Sanitize a task name for the parenthesised `comm` field of `/proc/<pid>/stat`.
///
/// Linux emits the raw 15-byte `comm`; this renderer additionally folds NUL,
/// CR/LF and `)` to visible ASCII (`)` becomes `]`) so a filename that a user
/// happened to create can neither split the line nor confuse the conventional
/// "find the LAST `)`" parse used by `ps`/`procps`. Truncation is by bytes at
/// Linux's `TASK_COMM_LEN - 1`.
pub fn stat_comm(name: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = name
        .iter()
        .take(TASK_NAME_MAX)
        .map(|b| match *b {
            0 | b'\n' | b'\r' => b' ',
            b')' => b']',
            other => other,
        })
        .collect();
    if out.is_empty() {
        out.extend_from_slice(b"pagh");
    }
    out
}

/// Render `/proc/<pid>/stat` (and `/proc/self/stat`): Linux's 52-field line.
///
/// Fields the kernel does not account for are emitted as 0 rather than invented
/// (`utime`/`stime`/`cutime`/`cstime`, page-fault counters, `starttime`, the
/// signal bitmap, code/data segment addresses and `arg/env` bounds). `vsize` is
/// KiB → bytes and `rss` is KiB → 4 KiB pages, both derived from the same
/// [`VmKb`] snapshot as `/proc/<pid>/status`, so the two files agree.
/// `rsslim` is Linux's `RLIM_INFINITY` (all ones), `exit_signal` is `SIGCHLD`.
pub fn format_pid_stat(s: &PidStatInputs<'_>) -> Vec<u8> {
    let comm = stat_comm(s.name);
    let mut out = Vec::with_capacity(384);

    // Fields 1–9: pid, comm, state, ppid, pgrp, session, tty_nr, tpgid, flags.
    push(&mut out, format_args!("{} (", s.pid));
    out.extend_from_slice(&comm);
    push(
        &mut out,
        format_args!(
            ") {} {} {} {} 0 -1 0",
            s.state as char, s.ppid, s.pgrp, s.session
        ),
    );

    // Fields 10–17: minflt, cminflt, majflt, cmajflt, utime, stime, cutime,
    // cstime — no per-process CPU/fault accounting exists.
    out.extend_from_slice(b" 0 0 0 0 0 0 0 0");

    // Fields 18–52: priority, nice, threads, itrealvalue, starttime, vsize,
    // rss, rsslim, startcode, endcode, startstack, kstkesp, kstkeip, signal,
    // blocked, sigignore, sigcatch, wchan, nswap, cnswap, exit_signal,
    // processor, rt_priority, policy, delayacct_blkio_ticks, guest_time,
    // cguest_time, start_data, end_data, start_brk, arg_start, arg_end,
    // env_start, env_end, exit_code.
    push(
        &mut out,
        format_args!(
            " 20 0 {} 0 0 {} {} {} 0 0 {} 0 0 0 {} {} {} 0 0 0 17 0 0 0 0 0 0 0 0 {} 0 0 0 0 {}\n",
            s.threads,
            s.vm.size.saturating_mul(1024),
            s.vm.rss / 4,
            u64::MAX,
            s.startstack,
            s.sig_blocked,
            s.sig_ign,
            s.sig_cgt,
            s.start_brk,
            s.exit_code,
        ),
    );
    out
}

/// Render `/proc/self/statm`: `size resident shared text lib data dt` in pages.
///
/// `shared` and `dt` are real zeros (no shared-memory accounting). `lib` is 0 in
/// practice because `VmKb::lib` is 0 (interpreter segments are folded into
/// `exe`). `size`/`resident`/`text`/`data` are the same KiB values as
/// `/proc/<pid>/status`, divided by the 4 KiB page size.
pub fn format_statm(vm: &VmKb) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    push(
        &mut out,
        format_args!(
            "{} {} 0 {} {} {} 0\n",
            vm.size / 4,
            vm.rss / 4,
            vm.exe / 4,
            vm.lib / 4,
            vm.data / 4
        ),
    );
    out
}

// ─── /proc/self/maps ────────────────────────────────────────────────────────

/// `mmap`/`mprotect` protection bit: readable (same value as `linux::mem`).
pub const PROT_READ: u32 = 1;
/// `mmap`/`mprotect` protection bit: writable.
pub const PROT_WRITE: u32 = 2;
/// `mmap`/`mprotect` protection bit: executable.
pub const PROT_EXEC: u32 = 4;

/// One `/proc/self/maps` line's worth of region data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapsRegion<'a> {
    pub start: u64,
    pub end: u64,
    /// Linux `PROT_*` bits.
    pub prot: u32,
    pub file_offset: u64,
    pub dev_major: u8,
    pub dev_minor: u8,
    pub ino: u64,
    /// Pathname column: an absolute path, or a bracketed label like `[stack]`.
    pub path: Option<&'a str>,
}

/// Render `/proc/self/maps`.
///
/// Lines are sorted by start address and overlapping inputs are coalesced, so the
/// output is always parseable by tools that binary-search the range list
/// (sanitizers, `addr2line`). A region without a pathname gets **no** trailing
/// space, matching Linux's anonymous mappings.
pub fn format_maps(regions: &[MapsRegion<'_>]) -> Vec<u8> {
    let mut sorted: Vec<MapsRegion<'_>> = regions.to_vec();
    sorted.sort_by_key(|r| (r.start, r.end));

    // Coalesce overlapping ranges: keep the first (lowest-start) name and take
    // the union of protections. Addresses are non-overlapping in a real address
    // space, so this only guards malformed input.
    let mut merged: Vec<MapsRegion<'_>> = Vec::with_capacity(sorted.len());
    for r in sorted {
        if let Some(last) = merged.last_mut() {
            if r.start < last.end {
                last.end = core::cmp::max(last.end, r.end);
                last.prot |= r.prot;
                continue;
            }
        }
        merged.push(r);
    }

    let mut out = Vec::with_capacity(256 + 64 * merged.len());
    for r in merged {
        let r_bit = if r.prot & PROT_READ != 0 { 'r' } else { '-' };
        let w_bit = if r.prot & PROT_WRITE != 0 { 'w' } else { '-' };
        let x_bit = if r.prot & PROT_EXEC != 0 { 'x' } else { '-' };
        push(
            &mut out,
            format_args!(
                "{:08x}-{:08x} {}{}{}p {:08x} {:02x}:{:02x} {}",
                r.start, r.end, r_bit, w_bit, x_bit, r.file_offset, r.dev_major, r.dev_minor, r.ino
            ),
        );
        if let Some(path) = r.path {
            push(&mut out, format_args!(" {}", path));
        }
        out.push(b'\n');
    }
    out
}

/// Format `dev_t`-style `major:minor` bytes for a maps line (hex, 2 digits each).
pub fn dev_bytes(dev: u64) -> (u8, u8) {
    let major = ((dev >> 8) & 0xfff) as u8;
    let minor = ((dev & 0xff) | ((dev >> 12) & 0xfff00)) as u8;
    (major, minor)
}
