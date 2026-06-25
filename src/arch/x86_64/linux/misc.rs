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

use core::sync::atomic::{AtomicU64, Ordering};

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

/// LAPIC periodic-timer tick rate (see `arch::x86_64::apic`: ~100 Hz). Supplied to
/// the pure [`ticks_to_timespec`] so `clock_gettime` reports wall-ish time from the
/// scheduler tick counter.
const TICK_HZ: u64 = 100;

/// `arch_prctl` subfunction: set the `FS.base` register.
const ARCH_SET_FS: u64 = 0x1002;
/// `arch_prctl` subfunction: read the `FS.base` register into a user `u64`.
const ARCH_GET_FS: u64 = 0x1003;

/// `getpid` (39): return the calling process's pid.
pub fn sys_getpid() -> Result<u64, Errno> {
    Ok(scheduler::current_pid())
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
pub fn sys_set_tid_address(_tidptr: u64) -> Result<u64, Errno> {
    let tid = compat::with_current_compat(|cs| cs.tid).unwrap_or_else(scheduler::current_pid);
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
    if clock_id as u32 == CLOCK_REALTIME {
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

/// Kernel entropy state for `getrandom`. Seeded lazily from the timestamp counter
/// and the tick clock; advanced with a fast xorshift64. This is a non-cryptographic
/// source adequate for the static CLI binaries this layer targets.
static RNG_STATE: AtomicU64 = AtomicU64::new(0);

/// Produce the next 64-bit pseudo-random word.
fn next_rand() -> u64 {
    let mut x = RNG_STATE.load(Ordering::Relaxed);
    if x == 0 {
        // SAFETY: `_rdtsc` is an unprivileged timestamp read, always valid.
        let tsc = unsafe { core::arch::x86_64::_rdtsc() };
        x = tsc ^ 0x9E37_79B9_7F4A_7C15 ^ scheduler::ticks().wrapping_mul(0x2545_F491_4F6C_DD1D);
        if x == 0 {
            x = 0xDEAD_BEEF_CAFE_F00D;
        }
    }
    // xorshift64
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    RNG_STATE.store(x, Ordering::Relaxed);
    x
}

/// `getrandom` (318): fill the user buffer with `count` random bytes and return
/// `count` (R2.12). The buffer length and the requested count are the same syscall
/// argument, so the [`getrandom_plan`] check (`n <= buflen`) always holds here; the
/// pure planner is still consulted to keep the policy in one place.
pub fn sys_getrandom(buf: u64, count: u64, _flags: u64) -> Result<u64, Errno> {
    let n = getrandom_plan(count, count)?;
    if n == 0 {
        return Ok(0);
    }
    check_user_ptr(buf, n)?;
    fill_random(buf, n);
    Ok(n)
}

/// Produce 16 bytes of pseudo-random data from the kernel entropy source.
/// Produce 16 bytes of pseudo-random data from the kernel entropy source.
///
/// Used by `run_linux_binary` (task 13.3) to seed the `AT_RANDOM` block of the
/// initial process stack (R6.6). Draws two 64-bit xorshift words from the same
/// non-cryptographic source as `getrandom`, which is adequate for the static CLI
/// binaries this layer targets.
pub fn random_bytes_16() -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&next_rand().to_le_bytes());
    out[8..].copy_from_slice(&next_rand().to_le_bytes());
    out
}

/// Draw the next 64-bit word from the kernel entropy source.
///
/// **WEAK / NON-CRYPTOGRAPHIC.** This is the same fast xorshift64 (seeded once
/// from `RDTSC` ^ the tick counter) that backs `getrandom`/`AT_RANDOM`. It is
/// adequate for ASLR-ish seeding and the static CLI binaries this layer targets,
/// but it is **not** a cryptographically secure RNG. `src/net/tls.rs` wraps it as
/// the TLS session RNG, which is why the resulting TLS session is not secure
/// against a capable attacker (it emits a one-time runtime warning saying so).
pub fn next_rand_u64() -> u64 {
    next_rand()
}

/// Fill `len` validated user bytes at `ptr` with pseudo-random data.
fn fill_random(ptr: u64, len: u64) {
    let mut remaining = len;
    let mut addr = ptr;
    while remaining > 0 {
        let word = next_rand().to_le_bytes();
        let chunk = core::cmp::min(remaining, 8) as usize;
        // SAFETY: `[ptr, ptr+len)` validated by the caller; each write stays in it.
        unsafe {
            core::ptr::copy_nonoverlapping(word.as_ptr(), addr as *mut u8, chunk);
        }
        addr += chunk as u64;
        remaining -= chunk as u64;
    }
}

/// `exit` (60) / `exit_group` (231): record the normalized exit code (low byte,
/// R12.3), log the pid + code (R12.3/R12.5), then terminate only the calling
/// process and yield to the scheduler forever (R7.2). Never returns.
pub fn sys_exit(code: u64) -> ! {
    let byte = exit_code_byte(code);
    compat::with_current_compat(|cs| cs.exit_code = Some(byte));
    let pid = scheduler::current_pid();
    crate::info!("[linux] Compat_Process pid={} exited with code {}", pid, byte);
    // Terminates only this task; the scheduler keeps running others (R7.2).
    scheduler::exit_current()
}

// ─────────────── identity / info / time / sleep / sched / signals ───────────────
// (Feature: linux-binary-compat) We run as a single root-ish process, so the
// identity calls return constant ids; signal-related calls accept and return 0
// because signals are never delivered.

/// Microseconds per second.
const USEC_PER_SEC: u64 = 1_000_000;
/// One scheduler tick is `1/TICK_HZ` seconds = 10 ms at 100 Hz.
const NS_PER_TICK: u64 = 1_000_000_000 / TICK_HZ;

/// `getuid`/`geteuid`/`getgid`/`getegid` (102/107/104/108): we run root-ish, so
/// every id is 0.
pub fn sys_getid() -> Result<u64, Errno> {
    Ok(0)
}

/// `getppid` (110): no real parent process model; report init (1).
pub fn sys_getppid() -> Result<u64, Errno> {
    Ok(1)
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
fn duration_to_ticks(sec: i64, nsec: i64) -> u64 {
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
pub fn sys_clock_nanosleep(
    _clock_id: u64,
    _flags: u64,
    req: u64,
    _rem: u64,
) -> Result<u64, Errno> {
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
/// without installing any handler. The act/oldact pointers are not dereferenced.
pub fn sys_rt_sigaction(_sig: u64, _act: u64, _oldact: u64, _sigsetsize: u64) -> Result<u64, Errno> {
    Ok(0)
}

/// `rt_sigprocmask` (14): no signals to mask; accept and return 0. The set/oldset
/// pointers are not dereferenced.
pub fn sys_rt_sigprocmask(
    _how: u64,
    _set: u64,
    _oldset: u64,
    _sigsetsize: u64,
) -> Result<u64, Errno> {
    Ok(0)
}

/// `sigaltstack` (131): accept and return 0 (no alternate signal stack is needed
/// since signals are never delivered).
pub fn sys_sigaltstack(_ss: u64, _old_ss: u64) -> Result<u64, Errno> {
    Ok(0)
}

/// `set_robust_list` (273): accept and return 0 (no futex/robust-list support).
pub fn sys_set_robust_list(_head: u64, _len: u64) -> Result<u64, Errno> {
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

/// Return a sane `(cur, max)` rlimit pair for `resource`.
fn rlimit_for(resource: u64) -> (u64, u64) {
    match resource {
        RLIMIT_STACK => (8 * 1024 * 1024, RLIM_INFINITY), // 8 MiB soft stack
        RLIMIT_NOFILE => (1024, 4096),                    // open-file limits
        _ => (RLIM_INFINITY, RLIM_INFINITY),
    }
}

/// Write a built `Rlimit` to a validated user pointer.
fn write_rlimit(ptr: u64, resource: u64) -> Result<(), Errno> {
    check_user_ptr(ptr, core::mem::size_of::<Rlimit>() as u64)?;
    let (cur, max) = rlimit_for(resource);
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

/// `prlimit64` (302): report a sane limit for the resource via the `old_limit`
/// pointer (if non-null); any `new_limit` is accepted but not enforced. Returns 0.
pub fn sys_prlimit64(
    _pid: u64,
    resource: u64,
    _new_limit: u64,
    old_limit: u64,
) -> Result<u64, Errno> {
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
