//! Linux x86_64 binary-compatibility layer.
//!
//! This module tree houses the pure, host-testable core (errno encoding, ABI
//! marshalling, pointer-range validation, syscall planning, diagnostics) and the
//! effectful kernel shells that drive it. Pure modules are shared into the
//! `host-tests` crate via `#[path]` includes (R11.6); the effectful handlers and
//! the `linux_dispatch` entry land in later tasks.

pub mod errno;
pub mod regs;

// The following submodules are populated by later tasks. They are declared here so
// the module tree exists up front; each starts as a placeholder that compiles
// cleanly and gains its real content in its own task.
pub mod abi;
pub mod validate;
pub mod io;
pub mod stat;
pub mod rand_clock;
pub mod mem;
pub mod misc;
pub mod diag;
pub mod dirent;
pub mod timeconv;

// Kernel-only effectful handler shells. These are NOT `#[path]`-included by the
// `host-tests` crate (only the pure modules above are), so they may freely use the
// VMM/PMM, VFS, scheduler, and per-process compat state. Keeping the effectful
// handlers here — rather than in the host-included `io.rs`/`mem.rs` — is what lets
// those pure planners stay host-testable (R11.6).
pub mod io_sys;
pub mod mem_sys;
pub mod rtc;

use abi::nr as sysno;
use errno::{encode_errno, Errno};
use regs::SavedRegs;

/// Single user-pointer validation choke point (R1.5, R1.6).
///
/// Every handler that takes a user pointer calls this before dereferencing it. It
/// runs the pure range check [`validate::check_user_range`] (rejecting buffers that
/// start at/above `User_Addr_Max`, overflow, or end above it — R1.5) and then walks
/// every 4 KiB page the buffer spans, confirming each is mapped in the CURRENT
/// address space via [`crate::memory::vmm::virt_to_phys`] (R1.6). Any failure yields
/// `EFAULT` without the buffer ever being read or written. A zero-length buffer is
/// always accepted (it dereferences nothing).
pub(crate) fn check_user_ptr(start: u64, len: u64) -> Result<(), Errno> {
    use validate::PtrCheck;
    if validate::check_user_range(start, len) == PtrCheck::Efault {
        return Err(Errno::EFAULT);
    }
    for page in validate::spanned_pages(start, len) {
        if crate::memory::vmm::virt_to_phys(page).is_none() {
            return Err(Errno::EFAULT);
        }
    }
    Ok(())
}

/// Emit the at-most-once-per-number `-ENOSYS` diagnostic for `nr` (R12.2).
///
/// For a Compat_Process the per-process `nosys_logged` set de-duplicates the entry
/// so each distinct unsupported number is logged once. For a native task (no compat
/// state) there is no per-process set, so the entry is logged each time — native
/// tasks never legitimately reach the unsupported path.
fn log_nosys(nr: u64) {
    match compat_log_nosys(nr) {
        Some(true) | None => {
            crate::warn!("[linux] unsupported syscall nr={} -> ENOSYS", nr);
        }
        Some(false) => { /* already logged once for this process */ }
    }
}

/// Record `nr` in the running process's `nosys_logged` set, returning whether this
/// was its first occurrence (`Some(true)`), a repeat (`Some(false)`), or there is no
/// compat state (`None`).
fn compat_log_nosys(nr: u64) -> Option<bool> {
    crate::task::compat::with_current_compat(|cs| diag::should_log_nosys(&mut cs.nosys_logged, nr))
}

/// Route a supported Linux syscall to its effectful handler, returning the
/// handler's `Result<u64, Errno>` (the caller folds it into `rax`). The supported
/// gate has already been checked, so the final arm is unreachable in practice.
fn dispatch_supported(nr: u64, a: &[u64; 6]) -> Result<u64, Errno> {
    match nr {
        // ── I/O (task 12.1) ──
        sysno::READ => io_sys::sys_read(a[0], a[1], a[2]),
        sysno::WRITE => io_sys::sys_write(a[0], a[1], a[2]),
        sysno::WRITEV => io_sys::sys_writev(a[0], a[1], a[2]),
        sysno::OPEN => io_sys::sys_open(a[0], a[1], a[2]),
        sysno::OPENAT => io_sys::sys_openat(a[0], a[1], a[2], a[3]),
        sysno::CLOSE => io_sys::sys_close(a[0]),
        sysno::LSEEK => io_sys::sys_lseek(a[0], a[1], a[2]),
        sysno::PREAD64 => io_sys::sys_pread64(a[0], a[1], a[2], a[3]),
        sysno::PWRITE64 => io_sys::sys_pwrite64(a[0], a[1], a[2], a[3]),
        sysno::FSTAT => io_sys::sys_fstat(a[0], a[1]),
        sysno::NEWFSTATAT => io_sys::sys_newfstatat(a[0], a[1], a[2], a[3]),
        sysno::IOCTL => io_sys::sys_ioctl(a[0], a[1], a[2]),
        sysno::ACCESS => io_sys::sys_access(a[0], a[1]),
        // ── Directory / path / fd (linux-binary-compat) ──
        sysno::GETDENTS64 => io_sys::sys_getdents64(a[0], a[1], a[2]),
        sysno::GETCWD => io_sys::sys_getcwd(a[0], a[1]),
        sysno::CHDIR => io_sys::sys_chdir(a[0]),
        sysno::FCHDIR => io_sys::sys_fchdir(a[0]),
        sysno::DUP => io_sys::sys_dup(a[0]),
        sysno::DUP2 => io_sys::sys_dup2(a[0], a[1]),
        sysno::DUP3 => io_sys::sys_dup3(a[0], a[1], a[2]),
        sysno::FCNTL => io_sys::sys_fcntl(a[0], a[1], a[2]),
        sysno::READLINK => io_sys::sys_readlink(a[0], a[1], a[2]),
        sysno::READLINKAT => io_sys::sys_readlinkat(a[0], a[1], a[2], a[3]),
        sysno::STATFS => io_sys::sys_statfs(a[0], a[1]),
        sysno::FSTATFS => io_sys::sys_fstatfs(a[0], a[1]),
        // ── Memory (task 12.3) ──
        sysno::BRK => mem_sys::sys_brk(a[0]),
        sysno::MMAP => mem_sys::sys_mmap(a[0], a[1], a[2], a[3], a[4], a[5]),
        sysno::MUNMAP => mem_sys::sys_munmap(a[0], a[1]),
        sysno::MPROTECT => mem_sys::sys_mprotect(a[0], a[1], a[2]),
        // ── Misc + process (task 12.5) ──
        sysno::GETPID => misc::sys_getpid(),
        sysno::UNAME => misc::sys_uname(a[0]),
        sysno::ARCH_PRCTL => misc::sys_arch_prctl(a[0], a[1]),
        sysno::SET_TID_ADDRESS => misc::sys_set_tid_address(a[0]),
        sysno::CLOCK_GETTIME => misc::sys_clock_gettime(a[0], a[1]),
        sysno::GETRANDOM => misc::sys_getrandom(a[0], a[1], a[2]),
        // ── Identity / info / time / sleep / sched / signals (linux-binary-compat) ──
        sysno::GETUID | sysno::GETEUID | sysno::GETGID | sysno::GETEGID => misc::sys_getid(),
        sysno::GETPPID => misc::sys_getppid(),
        sysno::GETTID => misc::sys_gettid(),
        sysno::GETTIMEOFDAY => misc::sys_gettimeofday(a[0], a[1]),
        sysno::TIME => misc::sys_time(a[0]),
        sysno::NANOSLEEP => misc::sys_nanosleep(a[0], a[1]),
        sysno::CLOCK_NANOSLEEP => misc::sys_clock_nanosleep(a[0], a[1], a[2], a[3]),
        sysno::SYSINFO => misc::sys_sysinfo(a[0]),
        sysno::SCHED_YIELD => misc::sys_sched_yield(),
        sysno::RT_SIGACTION => misc::sys_rt_sigaction(a[0], a[1], a[2], a[3]),
        sysno::RT_SIGPROCMASK => misc::sys_rt_sigprocmask(a[0], a[1], a[2], a[3]),
        sysno::SIGALTSTACK => misc::sys_sigaltstack(a[0], a[1]),
        sysno::SET_ROBUST_LIST => misc::sys_set_robust_list(a[0], a[1]),
        sysno::RSEQ => misc::sys_rseq(),
        sysno::PRLIMIT64 => misc::sys_prlimit64(a[0], a[1], a[2], a[3]),
        sysno::GETRLIMIT => misc::sys_getrlimit(a[0], a[1]),
        // `exit`/`exit_group` diverge (never return); the `!` coerces to the
        // `Result` arm type.
        sysno::EXIT | sysno::EXIT_GROUP => misc::sys_exit(a[0]),
        // Unreachable: `is_supported` gated everything else to ENOSYS already.
        _ => Err(Errno::ENOSYS),
    }
}

/// Single funnel point for both Linux syscall entry stubs (`int80_stub` and the
/// `syscall`-instruction `syscall_entry`).
///
/// ## Calling convention
///
/// Both stubs save all 15 general-purpose registers into an identical
/// [`SavedRegs`] frame on the kernel stack and pass a single pointer to that frame
/// (`rdi = &SavedRegs`). Routing everything through one `*mut SavedRegs` — instead
/// of spreading the six Linux argument registers across the SysV C ABI — sidesteps
/// the six-register argument limit cleanly and lets the dispatcher both read the
/// Linux number/arguments out of the frame and (in later tasks) modify saved
/// registers such as `FS.base`. The value returned here is written by the stub
/// into the saved `rax` slot, so it becomes the syscall's `rax` result on return
/// to ring 3 (R1.2, R1.3); every other GPR is restored unchanged (R1.7).
///
/// ## Status: full Linux routing (task 12.7)
///
/// Reads the Linux number and arguments via [`abi::marshal_args`] (R1.1, R1.8),
/// then:
///
///   1. **Precedence shim.** If the running process has NO registered
///      [`CompatState`](crate::task::compat::CompatState) (a pagh-native task) and
///      the number is one of the three legacy pagh-native calls, it is delegated to
///      the legacy dispatcher so the existing boot/test path keeps working. A
///      Compat_Process (compat state present) instead gets full Linux semantics —
///      so the numeric overlap with Linux `open`(2)/`close`(3) is resolved by
///      "native ⇒ legacy, Linux ⇒ Linux".
///   2. **Supported-set gate (R1.4, R11.4, R11.5).** Unsupported numbers (incl.
///      `clone`/`fork`/`vfork`/`futex` and graphical syscalls) log one nosys
///      diagnostic and return `-ENOSYS` **before any argument pointer is
///      inspected**.
///   3. **Routing.** Supported numbers go to the io/mem/misc handlers, each of
///      which runs the single [`check_user_ptr`] choke point on its pointer
///      arguments. The handler's `Result<u64, Errno>` is folded `Ok(v) -> v` /
///      `Err(e) -> -errno` (R1.3) into the value written back to `rax`; every other
///      GPR is preserved by the entry stub (R1.7).
///
/// # Safety
///
/// `regs` must point at a fully-initialized [`SavedRegs`] frame on the current
/// kernel stack, exactly as built by the entry stubs. The stubs guarantee this.
#[no_mangle]
pub extern "C" fn linux_dispatch(regs: *mut SavedRegs) -> u64 {
    // SAFETY: the entry stubs always pass a pointer to the 15-register frame they
    // just pushed on the kernel stack; it outlives this call and is uniquely owned
    // for the duration (interrupts are masked across the syscall window).
    let r = unsafe { &mut *regs };

    let (nr, args) = abi::marshal_args(r.rax, r.rdi, r.rsi, r.rdx, r.r10, r.r8, r.r9);

    // ── 1. Precedence shim: native tasks keep the legacy pagh-native routing ──
    // A process WITH compat state is a Linux Compat_Process and bypasses this,
    // taking full Linux semantics for every number (including 1/2/3).
    if !crate::task::compat::current_has_compat() {
        use crate::arch::x86_64::syscall::{legacy_dispatch, SYS_EXIT, SYS_WRITE, SYS_YIELD};
        if matches!(nr, SYS_WRITE | SYS_EXIT | SYS_YIELD) {
            // a1/a2/a3 == rdi/rsi/rdx, matching the legacy 3-argument convention.
            return legacy_dispatch(nr, args[0], args[1], args[2]);
        }
    }

    // ── 2. Supported-set gate BEFORE any pointer inspection (R1.4) ──
    if !abi::is_supported(nr) {
        log_nosys(nr);
        return encode_errno(Errno::ENOSYS);
    }

    // ── 3. Route to the handler and fold the result into rax (R1.3) ──
    match dispatch_supported(nr, &args) {
        Ok(v) => v,
        Err(e) => encode_errno(e),
    }
}
