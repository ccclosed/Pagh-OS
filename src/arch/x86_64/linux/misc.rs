//! Effectful misc Linux syscall handlers (task 12.5):
//! `getpid`/`uname`/`arch_prctl`/`set_tid_address`/`clock_gettime`/`getrandom`/
//! `exit`/`exit_group`.
//!
//! This is a **kernel-only** module (it is not `#[path]`-included by `host-tests`),
//! so it freely uses the scheduler, the per-process [`CompatState`], the FS-base
//! MSR, and the kernel logging facade. It reuses the pure planners in
//! [`super::rand_clock`] (`getrandom_plan`/`ticks_to_timespec`) and the diagnostics
//! helpers in [`super::diag`] (`exit_code_byte`).
//!
//! Every user pointer is validated through the single [`super::check_user_ptr`]
//! choke point before being dereferenced.
#![allow(dead_code)]

use x86_64::registers::model_specific::FsBase;
use x86_64::VirtAddr;

use crate::task::compat;
use crate::task::scheduler;

use super::check_user_ptr;
use super::diag::exit_code_byte;
use super::errno::Errno;
use super::rand_clock::{getrandom_plan, ticks_to_timespec, Timespec, CLOCK_REALTIME};
use super::rtc;
use super::timeconv::{encode_timeval, Timeval};

/// LAPIC periodic-timer tick rate (see `arch::x86_64::apic`). Supplied to
/// the pure [`ticks_to_timespec`] so `clock_gettime` reports wall-ish time from the
/// scheduler tick counter.
use crate::arch::x86_64::apic::TICK_HZ;
use crate::sync::spinlock::Spinlock;

/// `arch_prctl` subfunction: set the `FS.base` register.
const ARCH_SET_FS: u64 = 0x1002;
/// `arch_prctl` subfunction: read the `FS.base` register into a user `u64`.
const ARCH_GET_FS: u64 = 0x1003;

/// `getpid` (39): return the calling process's pid.
pub fn sys_getpid() -> Result<u64, Errno> {
    Ok(compat::current_tgid())
}

/// The x86_64 Linux `struct utsname`: six fixed-size NUL-terminated fields.
#[repr(C)]
struct Utsname {
    sysname: [u8; 65],
    nodename: [u8; 65],
    release: [u8; 65],
    version: [u8; 65],
    machine: [u8; 65],
    domainname: [u8; 65],
}

/// Copy `s` into a 65-byte field, NUL-padded (truncated if longer than 64).
fn field(s: &str) -> [u8; 65] {
    let mut out = [0u8; 65];
    let bytes = s.as_bytes();
    let n = core::cmp::min(bytes.len(), 64);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

/// `uname` (63): populate the user `struct utsname` with fixed identifying
/// strings and return 0 (R2.11).
pub fn sys_uname(buf: u64) -> Result<u64, Errno> {
    check_user_ptr(buf, core::mem::size_of::<Utsname>() as u64)?;
    let uts = Utsname {
        sysname: field("Linux"),
        nodename: field("pagh"),
        release: field("6.1.0-pagh"),
        version: field("#1 pagh compat"),
        machine: field("x86_64"),
        domainname: field("(none)"),
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &uts as *const Utsname as *const u8,
            core::mem::size_of::<Utsname>(),
        )
    };
    // SAFETY: `buf` validated for the full struct length above; active CR3 is the
    // calling process's user PML4.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, bytes.len());
    }
    Ok(0)
}

/// `arch_prctl` (158): `ARCH_SET_FS` sets the calling process's `FS.base` (and
/// records it in the [`CompatState`]) and returns 0 (R2.9); `ARCH_GET_FS` writes
/// the recorded base to the user pointer. Other subfunctions are `EINVAL`.
pub fn sys_arch_prctl(code: u64, addr: u64) -> Result<u64, Errno> {
    match code {
        ARCH_SET_FS => {
            // Set the architectural FS.base for the running thread...
            FsBase::write(VirtAddr::new(addr));
            // ...and record it in the process's compat state so it survives in
            // the per-process model (R2.9).
            compat::with_current_compat(|cs| cs.fs_base = addr);
            Ok(0)
        }
        ARCH_GET_FS => {
            check_user_ptr(addr, 8)?;
            let base = compat::with_current_compat(|cs| cs.fs_base).unwrap_or(0);
            // SAFETY: `addr` validated for 8 bytes above.
            unsafe {
                *(addr as *mut u64) = base;
            }
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// `set_tid_address` (218): return the calling thread's tid (R2.10). The supplied
/// `clear_child_tid` pointer is accepted but unused (no thread teardown here).
pub fn sys_set_tid_address(tidptr: u64) -> Result<u64, Errno> {
    if tidptr != 0 {
        check_user_ptr(tidptr, 4)?;
    }
    let tid = compat::with_current_compat(|cs| {
        cs.clear_child_tid = tidptr;
        cs.tid
    })
    .unwrap_or_else(scheduler::current_pid);
    Ok(tid)
}

/// `clock_gettime` (228): populate the user `struct timespec` from the kernel tick
/// clock for `CLOCK_MONOTONIC`/`CLOCK_REALTIME` and return 0 (R2.13); `EINVAL` for
/// an unsupported clock id, leaving the buffer unmodified (R2.16).
pub fn sys_clock_gettime(clock_id: u64, tsptr: u64) -> Result<u64, Errno> {
    // Validate the clock id BEFORE touching the user buffer so an unsupported id
    // leaves it unmodified (R2.16).
    let mut ts = ticks_to_timespec(scheduler::ticks(), clock_id as u32, TICK_HZ)?;
    // CLOCK_REALTIME is wall-clock: take whole seconds from the CMOS RTC and keep
    // the tick-derived sub-second nanoseconds. CLOCK_MONOTONIC stays tick-based.
    if clock_id as u32 == CLOCK_REALTIME || clock_id as u32 == 5 {
        // 5 = CLOCK_REALTIME_COARSE — same wall clock, lower precision.
        ts.tv_sec = rtc::now_unix() as i64;
    }
    check_user_ptr(tsptr, core::mem::size_of::<Timespec>() as u64)?;
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &ts as *const Timespec as *const u8,
            core::mem::size_of::<Timespec>(),
        )
    };
    // SAFETY: `tsptr` validated for the timespec length above.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), tsptr as *mut u8, bytes.len());
    }
    Ok(0)
}

/// `getrandom` (318): fill a user buffer from the hardware-backed entropy API.
/// Fails with `EAGAIN` instead of silently returning predictable timestamp-based
/// bytes when RDSEED/RDRAND is unavailable.
pub fn sys_getrandom(buf: u64, count: u64, _flags: u64) -> Result<u64, Errno> {
    let n = getrandom_plan(count, count)?;
    if n == 0 {
        return Ok(0);
    }
    check_user_ptr(buf, n)?;
    let out = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, n as usize) };
    crate::security::entropy::fill(out).map_err(|_| Errno::EAGAIN)?;
    Ok(n)
}

/// Produce the ELF `AT_RANDOM` block. Hardware entropy (RDSEED/RDRAND) is
/// preferred. When the platform exposes none, the bytes are mixed from every
/// cheap entropy source the kernel has — the tick clock, the RTC wall clock,
/// the current pid, and a free-running process-lifetime counter — through a
/// xorshift generator, so the block NEVER degrades to all-zero bytes
/// (glibc consumes AT_RANDOM for stack-canary / pointer-mangling keys; a
/// constant value defeats both).
pub fn random_bytes_16() -> [u8; 16] {
    let mut out = [0u8; 16];
    if crate::security::entropy::fill(&mut out).is_ok() {
        return out;
    }
    static FALLBACK_STATE: Spinlock<u64> = Spinlock::new(0x9E37_79B9_7F4A_7C15);
    let mut g = FALLBACK_STATE.lock();
    let mut x = *g
        ^ scheduler::ticks().wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (rtc::now_unix() as u64).rotate_left(32)
        ^ scheduler::current_pid() << 48
        ^ crate::security::entropy::secure_u64().unwrap_or(0);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    let second = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    *g = x;
    out[..8].copy_from_slice(&x.to_le_bytes());
    out[8..].copy_from_slice(&second.to_le_bytes());
    out
}

/// `exit` (60) / `exit_group` (231): record the normalized exit code (low byte,
/// R12.3), log the pid + code (R12.3/R12.5), then terminate only the calling
/// process and yield to the scheduler forever (R7.2). Never returns.
pub fn sys_exit(code: u64) -> ! {
    super::process_sys::cleanup_current_thread_exit();
    let byte = exit_code_byte(code);
    compat::with_current_compat(|cs| cs.exit_code = Some(byte));
    let pid = scheduler::current_pid();
    crate::info!(
        "[linux] Compat_Process pid={} exited with code {}",
        pid,
        byte
    );
    // Terminates only this task; the scheduler keeps running others (R7.2).
    scheduler::exit_current()
}

pub fn sys_exit_group(code: u64) -> ! {
    let byte = exit_code_byte(code);
    let current = scheduler::current_pid();
    let tgid = compat::current_tgid();
    let others = compat::group_member_pids(tgid, current);
    for pid in others.iter().copied() {
        super::process_sys::cleanup_thread_exit(pid);
        compat::remove_compat(pid);
    }
    scheduler::remove_ready_pids(&others);
    compat::with_current_compat(|cs| cs.exit_code = Some(byte));
    super::process_sys::cleanup_current_thread_exit();
    crate::debug!("[linux] thread-group {} exited with code {}", tgid, byte);
    scheduler::exit_current()
}

/// `tgkill` (234): only the truly fatal signals terminate the thread group
/// (glibc `abort()`/`raise()`). Benign signals (`SIGCHLD`, `SIGCONT`, `SIGSTOP`,
/// `SIGWINCH`, `SIGURG`, …) are accepted and ignored — a process using tgkill
/// for ordinary signaling must not self-destruct. Returning `ENOSYS` for the
/// fatal ones made glibc's `abort()` fall through to a ring-3 `hlt`, which
/// surfaced as a spurious #GP after every "Fatal Python error".
pub fn sys_tgkill(_tgid: u64, _tid: u64, sig: u64) -> Result<u64, Errno> {
    const SIGKILL: u64 = 9;
    const SIGSEGV: u64 = 11;
    const SIGABRT: u64 = 6;
    const SIGFPE: u64 = 8;
    const SIGILL: u64 = 4;
    const SIGBUS: u64 = 7;
    const SIGSYS: u64 = 31;
    match sig & 0x3f {
        SIGKILL | SIGSEGV | SIGABRT | SIGFPE | SIGILL | SIGBUS | SIGSYS => {
            crate::info!(
                "[linux] tgkill: fatal signal {} to self - exiting thread group",
                sig
            );
            sys_exit_group(128 + (sig & 0x3f))
        }
        _ => Ok(0),
    }
}

// ─────────────── identity / info / time / sleep / sched / signals ───────────────
// (Feature: linux-binary-compat) We run as a single root-ish process, so the
// identity calls return constant ids; signal-related calls accept and return 0
// because signals are never delivered.

/// Microseconds per second.
const USEC_PER_SEC: u64 = 1_000_000;
/// One scheduler tick is `1/TICK_HZ` seconds (1 ms at 1000 Hz).
const NS_PER_TICK: u64 = 1_000_000_000 / TICK_HZ;

/// `getuid`/`geteuid`/`getgid`/`getegid` (102/107/104/108): we run root-ish, so
/// every id is 0.
pub fn sys_getid() -> Result<u64, Errno> {
    Ok(0)
}

/// `getppid` (110): return the tracked parent pid.
pub fn sys_getppid() -> Result<u64, Errno> {
    Ok(compat::current_ppid())
}

/// `gettid` (186): return the calling thread's tid (R2.10), falling back to the
/// pid for a context with no compat state.
pub fn sys_gettid() -> Result<u64, Errno> {
    let tid = compat::with_current_compat(|cs| cs.tid).unwrap_or_else(scheduler::current_pid);
    Ok(tid)
}

/// `gettimeofday` (96): fill the user `struct timeval` with the wall-clock time
/// (whole seconds from the CMOS RTC, sub-second microseconds from the tick clock)
/// and return 0. The timezone pointer is accepted but ignored.
pub fn sys_gettimeofday(tvptr: u64, _tzptr: u64) -> Result<u64, Errno> {
    if tvptr == 0 {
        return Ok(0);
    }
    check_user_ptr(tvptr, core::mem::size_of::<Timeval>() as u64)?;
    let secs = rtc::now_unix() as i64;
    let usecs = ((scheduler::ticks() % TICK_HZ) * (NS_PER_TICK / 1000)) as i64;
    let tv = encode_timeval(secs, usecs);
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &tv as *const Timeval as *const u8,
            core::mem::size_of::<Timeval>(),
        )
    };
    // SAFETY: `tvptr` validated for the timeval length above.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), tvptr as *mut u8, bytes.len());
    }
    Ok(0)
}

/// `time` (201): return the wall-clock seconds, also writing them to the optional
/// user `time_t` pointer when it is non-null.
pub fn sys_time(tptr: u64) -> Result<u64, Errno> {
    let secs = rtc::now_unix();
    if tptr != 0 {
        check_user_ptr(tptr, 8)?;
        // SAFETY: `tptr` validated for 8 bytes above.
        unsafe {
            *(tptr as *mut i64) = secs as i64;
        }
    }
    Ok(secs)
}

/// Convert a `(sec, nsec)` duration into a tick count at [`TICK_HZ`], rounding up
/// so any non-zero duration sleeps at least one tick. Saturating throughout.
pub(super) fn duration_to_ticks(sec: i64, nsec: i64) -> u64 {
    if sec <= 0 && nsec <= 0 {
        return 0;
    }
    let total_ns = (sec.max(0) as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(nsec.max(0) as u64);
    if total_ns == 0 {
        return 0;
    }
    let ticks = total_ns / NS_PER_TICK;
    if ticks == 0 {
        1
    } else {
        ticks
    }
}

/// Read a `struct timespec` (two i64) from a validated user pointer.
fn read_timespec(ptr: u64) -> Result<(i64, i64), Errno> {
    check_user_ptr(ptr, 16)?;
    // SAFETY: 16 bytes at `ptr` validated above.
    let sec = unsafe { *(ptr as *const i64) };
    let nsec = unsafe { *((ptr + 8) as *const i64) };
    if !(0..1_000_000_000).contains(&nsec) || sec < 0 {
        return Err(Errno::EINVAL);
    }
    Ok((sec, nsec))
}

/// `nanosleep` (35): sleep for the requested `struct timespec` duration via the
/// scheduler tick clock, returning 0. The remainder pointer is ignored (no signals
/// interrupt the sleep in this model).
pub fn sys_nanosleep(req: u64, _rem: u64) -> Result<u64, Errno> {
    let (sec, nsec) = read_timespec(req)?;
    let ticks = duration_to_ticks(sec, nsec);
    if ticks > 0 {
        scheduler::sleep_ticks(ticks);
    }
    Ok(0)
}

/// `clock_nanosleep` (230): relative sleep against the requested clock. Absolute
/// (`TIMER_ABSTIME`) sleeps and the clock id are ignored in this minimal model; it
/// sleeps the requested relative duration like `nanosleep` and returns 0.
pub fn sys_clock_nanosleep(_clock_id: u64, _flags: u64, req: u64, _rem: u64) -> Result<u64, Errno> {
    let (sec, nsec) = read_timespec(req)?;
    let ticks = duration_to_ticks(sec, nsec);
    if ticks > 0 {
        scheduler::sleep_ticks(ticks);
    }
    Ok(0)
}

/// The x86_64 Linux `struct sysinfo` (fields populated with plausible values).
#[repr(C)]
struct Sysinfo {
    uptime: i64,
    loads: [u64; 3],
    totalram: u64,
    freeram: u64,
    sharedram: u64,
    bufferram: u64,
    totalswap: u64,
    freeswap: u64,
    procs: u16,
    pad: u16,
    totalhigh: u64,
    freehigh: u64,
    mem_unit: u32,
    // Trailing padding bytes (`_f`) so the struct matches the 64-bit layout; the
    // C definition pads to a fixed size, which `#[repr(C)]` alignment reproduces.
    _f: [u8; 0],
}

/// `sysinfo` (99): fill the user `struct sysinfo` with the uptime (from the tick
/// clock) and total/free RAM (from the PMM frame counts), returning 0.
pub fn sys_sysinfo(info: u64) -> Result<u64, Errno> {
    check_user_ptr(info, core::mem::size_of::<Sysinfo>() as u64)?;
    let uptime = (scheduler::ticks() / TICK_HZ) as i64;
    let totalram = crate::memory::pmm::total_frames() as u64 * 4096;
    let freeram = crate::memory::pmm::free_frames() as u64 * 4096;
    let si = Sysinfo {
        uptime,
        loads: [0; 3],
        totalram,
        freeram,
        sharedram: 0,
        bufferram: 0,
        totalswap: 0,
        freeswap: 0,
        procs: 1,
        pad: 0,
        totalhigh: 0,
        freehigh: 0,
        mem_unit: 1,
        _f: [],
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &si as *const Sysinfo as *const u8,
            core::mem::size_of::<Sysinfo>(),
        )
    };
    // SAFETY: `info` validated for the sysinfo length above.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), info as *mut u8, bytes.len());
    }
    Ok(0)
}

/// `sched_yield` (24): yield the CPU to the scheduler and return 0.
pub fn sys_sched_yield() -> Result<u64, Errno> {
    scheduler::yield_current();
    Ok(0)
}

/// `rt_sigaction` (13): signals are never delivered, so accept and return 0
/// without installing any handler. `oldact`, when requested, receives a zeroed
/// `struct kernel_sigaction` (handler = SIG_DFL) so save-and-restore users read
/// back defined data instead of uninitialized memory.
pub fn sys_rt_sigaction(_sig: u64, _act: u64, oldact: u64, sigsetsize: u64) -> Result<u64, Errno> {
    if oldact != 0 {
        let size = if sigsetsize != 0 { sigsetsize } else { 8 };
        // kernel_sigaction = { handler, flags, restorer, mask[sigsetsize] }
        let total = 24u64.saturating_add(size);
        check_user_ptr(oldact, total)?;
        // SAFETY: the oldact range was validated above.
        unsafe {
            core::ptr::write_bytes(oldact as *mut u8, 0, total as usize);
        }
    }
    Ok(0)
}

/// `rt_sigprocmask` (14): no signals to mask; `oldset`, when requested, receives
/// an empty sigset (all zeros) instead of leaving user memory untouched.
pub fn sys_rt_sigprocmask(
    _how: u64,
    _set: u64,
    oldset: u64,
    sigsetsize: u64,
) -> Result<u64, Errno> {
    if oldset != 0 {
        let size = if sigsetsize != 0 { sigsetsize } else { 8 };
        check_user_ptr(oldset, size)?;
        // SAFETY: the oldset range was validated above.
        unsafe {
            core::ptr::write_bytes(oldset as *mut u8, 0, size as usize);
        }
    }
    Ok(0)
}

/// `sigaltstack` (131): accept and return 0 (no alternate signal stack is needed
/// since signals are never delivered). `old_ss`, when requested, receives a zeroed
/// stack_t so callers reading it back see "altstack disabled" rather than garbage.
pub fn sys_sigaltstack(_ss: u64, old_ss: u64) -> Result<u64, Errno> {
    if old_ss != 0 {
        // struct stack_t { void *ss_sp; int ss_flags; size_t ss_size; } = 24 bytes
        check_user_ptr(old_ss, 24)?;
        // SAFETY: the old_ss range was validated above.
        unsafe {
            core::ptr::write_bytes(old_ss as *mut u8, 0, 24);
        }
    }
    Ok(0)
}

/// `set_robust_list` (273): accept and return 0 (no futex/robust-list support).
pub fn sys_set_robust_list(head: u64, len: u64) -> Result<u64, Errno> {
    const ROBUST_HEAD_SIZE: u64 = 24;
    if len != ROBUST_HEAD_SIZE {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(head, len)?;
    compat::with_current_compat(|cs| {
        cs.robust_head = head;
        cs.robust_len = len;
    })
    .ok_or(Errno::EINVAL)?;
    Ok(0)
}
pub fn sys_get_robust_list(pid: u64, head_ptr: u64, len_ptr: u64) -> Result<u64, Errno> {
    let current = scheduler::current_pid();
    if pid != 0 && pid != current {
        return Err(Errno::ESRCH);
    }
    check_user_ptr(head_ptr, 8)?;
    check_user_ptr(len_ptr, 8)?;
    let (head, len) = compat::current_robust_list();
    unsafe {
        core::ptr::write_unaligned(head_ptr as *mut u64, head);
        core::ptr::write_unaligned(len_ptr as *mut u64, len)
    }
    Ok(0)
}

/// `rseq` (334): restartable sequences are not supported. Returning 0 (rather than
/// `-ENOSYS`) lets glibc-ish init continue without taking its fallback path; rseq
/// is purely an optimization, so reporting success with no registration is benign.
pub fn sys_rseq() -> Result<u64, Errno> {
    Ok(0)
}

/// The Linux `struct rlimit` / `rlimit64`: current and maximum (hard) limit.
#[repr(C)]
struct Rlimit {
    rlim_cur: u64,
    rlim_max: u64,
}

/// `RLIM_INFINITY` — no limit.
const RLIM_INFINITY: u64 = u64::MAX;
/// `RLIMIT_STACK` resource id.
const RLIMIT_STACK: u64 = 3;
/// `RLIMIT_NOFILE` resource id.
const RLIMIT_NOFILE: u64 = 7;
/// Number of rlimit resources Linux defines (`RLIMIT_NLIMITS`); larger
/// resource ids are rejected with `EINVAL`.
const RLIMIT_NLIMITS: u64 = 16;

/// Kernel-default `(cur, max)` rlimit pair for `resource` when the process has
/// no `prlimit64`/`setrlimit` override. `RLIMIT_NOFILE`'s hard limit is the
/// descriptor-table capacity (`io_sys::NOFILE_MAX`).
fn default_rlimit(resource: u64) -> (u64, u64) {
    match resource {
        RLIMIT_STACK => (8 * 1024 * 1024, RLIM_INFINITY), // 8 MiB soft stack
        RLIMIT_NOFILE => (1024, super::io_sys::NOFILE_MAX), // open-file limits
        _ => (RLIM_INFINITY, RLIM_INFINITY),
    }
}

/// The current process's effective `(cur, max)` rlimit for `resource`: the
/// per-process override stored in its [`CompatState`](crate::task::compat::CompatState)
/// when present, else the kernel default.
fn current_rlimit(resource: u64) -> (u64, u64) {
    compat::with_current_compat(|cs| match cs.rlimits.get(&resource) {
        Some(lim) => *lim,
        None => default_rlimit(resource),
    })
    .unwrap_or_else(|| default_rlimit(resource))
}

/// Write the current process's effective `Rlimit` for `resource` to a validated
/// user pointer. Unknown resource ids are rejected (`EINVAL`).
fn write_rlimit(ptr: u64, resource: u64) -> Result<(), Errno> {
    if resource >= RLIMIT_NLIMITS {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(ptr, core::mem::size_of::<Rlimit>() as u64)?;
    let (cur, max) = current_rlimit(resource);
    let rl = Rlimit {
        rlim_cur: cur,
        rlim_max: max,
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &rl as *const Rlimit as *const u8,
            core::mem::size_of::<Rlimit>(),
        )
    };
    // SAFETY: `ptr` validated for the rlimit length above.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
    }
    Ok(())
}

/// `prlimit64` (302): report the current process's effective limit for
/// `resource` via the `old_limit` pointer (if non-null); a non-null `new_limit`
/// (user pointer to `struct rlimit64`) updates the per-process override for
/// future queries. Limits are not enforced. Unknown resource ids and
/// `rlim_cur > rlim_max` yield `EINVAL`.
pub fn sys_prlimit64(
    _pid: u64,
    resource: u64,
    new_limit: u64,
    old_limit: u64,
) -> Result<u64, Errno> {
    if resource >= RLIMIT_NLIMITS {
        return Err(Errno::EINVAL);
    }
    if new_limit != 0 {
        check_user_ptr(new_limit, core::mem::size_of::<Rlimit>() as u64)?;
        // SAFETY: `new_limit` validated for the rlimit length above.
        let rl = unsafe { core::ptr::read_unaligned(new_limit as *const Rlimit) };
        if rl.rlim_cur > rl.rlim_max {
            return Err(Errno::EINVAL);
        }
        compat::with_current_compat(|cs| {
            cs.rlimits.insert(resource, (rl.rlim_cur, rl.rlim_max));
        });
    }
    if old_limit != 0 {
        write_rlimit(old_limit, resource)?;
    }
    Ok(0)
}

/// `getrlimit` (97): report a sane limit for the resource and return 0.
pub fn sys_getrlimit(resource: u64, rlim: u64) -> Result<u64, Errno> {
    write_rlimit(rlim, resource)?;
    Ok(0)
}

/// `setsid` (112): report the caller as its own session leader.
/// There is no real session/job-control model yet; returning the tgid
/// satisfies libuv's uv_spawn detach path.
pub fn sys_setsid() -> Result<u64, Errno> {
    Ok(compat::current_tgid())
}

/// `setpgid` (109): accepted as a no-op. There is still no real
/// process-group model; bash only needs the call to not fail so it stops
/// printing "initialize_job_control: setpgid: Function not implemented".
pub fn sys_setpgid(_pid: u64, _pgid: u64) -> Result<u64, Errno> {
    Ok(0)
}

/// `getpgrp` (111) / `getpgid` (121): every process is modeled as
/// the leader of its own group, mirroring sys_setsid above.
pub fn sys_getpgid() -> Result<u64, Errno> {
    Ok(compat::current_tgid())
}

/// `umask` (95): set the process's file-mode creation mask to `mask & 0777`
/// and return the previous mask. The mask is applied to the permission bits of
/// freshly created files/directories (see the `open`/`mkdir` handlers).
pub fn sys_umask(mask: u64) -> Result<u64, Errno> {
    let old = compat::with_current_compat(|cs| {
        let old = cs.umask;
        cs.umask = (mask & 0o777) as u32;
        old
    })
    .unwrap_or(0o022);
    Ok(old as u64)
}

/// `flock` (73): advisory locks are meaningless with one user and an
/// in-process VFS; report success so nvim's swap-file locking proceeds.
pub fn sys_flock(_fd: u64, _op: u64) -> Result<u64, Errno> {
    Ok(0)
}
