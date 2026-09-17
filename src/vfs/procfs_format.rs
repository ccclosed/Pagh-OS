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
    /// `/proc/cpuinfo`.
    CpuInfo,
    /// `/proc/meminfo`.
    MemInfo,
    /// `/proc/uptime`.
    Uptime,
}

/// One named entry: its `readdir` name, stable inode, and content kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcEntry {
    pub name: &'static str,
    pub ino: u64,
    pub kind: ProcKind,
}

/// The regular files directly under `/proc` (in `readdir` order).
pub const PROC_FILES: [ProcEntry; 3] = [
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
];

/// The regular files under `/proc/self` (in `readdir` order).
pub const PROC_SELF_FILES: [ProcEntry; 4] = [
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
];

/// `readdir` order of `/proc`: the `self` directory first, then the files.
pub const PROC_ROOT_ORDER: [&str; 4] = ["self", "cpuinfo", "meminfo", "uptime"];

/// Look up a regular `/proc` file by name (`self` is a directory, not here).
pub fn proc_file(name: &str) -> Option<ProcEntry> {
    PROC_FILES.iter().copied().find(|e| e.name == name)
}

/// Look up a `/proc/self` file by name.
pub fn proc_self_file(name: &str) -> Option<ProcEntry> {
    PROC_SELF_FILES.iter().copied().find(|e| e.name == name)
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
    pub vm: VmKb,
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
    out.extend_from_slice(b"State:\tR (running)\n");
    push(&mut out, format_args!("Tgid:\t{}\n", s.tgid));
    push(&mut out, format_args!("Pid:\t{}\n", s.pid));
    push(&mut out, format_args!("PPid:\t{}\n", s.ppid));
    out.extend_from_slice(b"TracerPid:\t0\n");
    out.extend_from_slice(b"Uid:\t0\t0\t0\t0\n");
    out.extend_from_slice(b"Gid:\t0\t0\t0\t0\n");
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
