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
pub mod diag;
pub mod dirent;
pub mod io;
pub mod mem;
pub mod misc;
pub mod rand_clock;
pub mod stat;
pub mod timeconv;
pub mod validate;

// Kernel-only effectful handler shells. These are NOT `#[path]`-included by the
// `host-tests` crate (only the pure modules above are), so they may freely use the
// VMM/PMM, VFS, scheduler, and per-process compat state. Keeping the effectful
// handlers here — rather than in the host-included `io.rs`/`mem.rs` — is what lets
// those pure planners stay host-testable (R11.6).
pub mod inet_sock;
pub mod io_sys;
pub mod epoll_sys;
pub mod mem_sys;
pub mod process_sys;
pub mod unix_sock;
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
    use x86_64::structures::paging::PageTableFlags;
    for page in validate::spanned_pages(start, len) {
        let flags = crate::memory::vmm::page_flags(page).ok_or(Errno::EFAULT)?;
        if !flags.contains(PageTableFlags::USER_ACCESSIBLE) {
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
        sysno::READV => io_sys::sys_readv(a[0], a[1], a[2]),
        sysno::PREADV => io_sys::sys_preadv(a[0], a[1], a[2], a[3]),
        sysno::PWRITEV => io_sys::sys_pwritev(a[0], a[1], a[2], a[3]),
        sysno::FSYNC | sysno::FDATASYNC => io_sys::sys_fsync(a[0]),
        sysno::RENAME => io_sys::sys_rename(a[0], a[1]),
        sysno::RENAMEAT => io_sys::sys_renameat(a[0], a[1], a[2], a[3]),
        sysno::RENAMEAT2 => io_sys::sys_renameat2(a[0], a[1], a[2], a[3], a[4]),
        sysno::FSTAT => io_sys::sys_fstat(a[0], a[1]),
        sysno::NEWFSTATAT => io_sys::sys_newfstatat(a[0], a[1], a[2], a[3]),
        sysno::IOCTL => io_sys::sys_ioctl(a[0], a[1], a[2]),
        sysno::ACCESS => io_sys::sys_access(a[0], a[1]),
        sysno::MKDIR => io_sys::sys_mkdir(a[0], a[1]),
        sysno::RMDIR => io_sys::sys_rmdir(a[0]),
        sysno::UNLINK => io_sys::sys_unlink(a[0]),
        sysno::CHMOD => io_sys::sys_chmod(a[0], a[1]),
        sysno::POLL => io_sys::sys_poll(a[0], a[1], a[2]),
        sysno::SELECT => io_sys::sys_select(a[0], a[1], a[2], a[3], a[4]),
        sysno::PSELECT6 => io_sys::sys_pselect6(a[0], a[1], a[2], a[3], a[4], a[5]),
        sysno::PPOLL => io_sys::sys_ppoll(a[0], a[1], a[2], a[3]),
        sysno::CLOCK_GETRES => epoll_sys::sys_clock_getres(a[0], a[1]),
        sysno::EVENTFD2 => epoll_sys::sys_eventfd2(a[0], a[1]),
        sysno::EPOLL_CREATE1 => epoll_sys::sys_epoll_create1(a[0]),
        sysno::EPOLL_CTL => epoll_sys::sys_epoll_ctl(a[0], a[1], a[2], a[3]),
        sysno::EPOLL_WAIT => epoll_sys::sys_epoll_wait(a[0], a[1], a[2], a[3]),
        sysno::PIPE => io_sys::sys_pipe(a[0]),
        sysno::PIPE2 => io_sys::sys_pipe2(a[0], a[1]),
        // ── nvim uv_spawn support ──
        sysno::SOCKETPAIR => io_sys::sys_socketpair(a[0], a[1], a[2], a[3]),
        sysno::STATX => io_sys::sys_statx(a[0], a[1], a[2], a[3], a[4]),
        sysno::EPOLL_PWAIT => epoll_sys::sys_epoll_pwait(a[0], a[1], a[2], a[3], a[4]),
        sysno::SOCKET => {
            if a[0] == crate::arch::x86_64::linux::inet_sock::AF_INET {
                crate::arch::x86_64::linux::inet_sock::sys_socket_in(a[0], a[1])
            } else {
                unix_sock::sys_socket(a[0], a[1], a[2])
            }
        }
        sysno::CONNECT => {
            // Peek the sockaddr family to pick the stack (AF_UNIX registry vs AF_INET smoltcp).
            let fam = if a[1] != 0 && a[2] >= 2 {
                (unsafe { core::ptr::read_unaligned(a[1] as *const u16) }) as u64
            } else { 1 };
            if fam == crate::arch::x86_64::linux::inet_sock::AF_INET {
                crate::arch::x86_64::linux::inet_sock::sys_connect_tcp(a[0], a[1], a[2])
            } else {
                unix_sock::sys_connect(a[0], a[1], a[2])
            }
        }
        sysno::SENDTO => {
            let fam = if a[4] != 0 && a[5] >= 8 {
                (unsafe { core::ptr::read_unaligned(a[4] as *const u16) }) as u64
            } else { 1 };
            if fam == crate::arch::x86_64::linux::inet_sock::AF_INET {
                check_user_ptr(a[1], a[2].min(1 << 20))?;
                let data = crate::arch::x86_64::linux::io_sys::copy_in_pub(a[1], a[2]);
                let n = crate::arch::x86_64::linux::inet_sock::udp_sendto_fd(a[0], &data, a[4], a[5])?;
                Ok(n as u64)
            } else {
                Err(Errno::EINVAL)
            }
        }
        sysno::RECVFROM => {
            // Route by fd type: AF_INET UDP sockets come here (glibc resolver).
            let is_inet_udp = crate::task::compat::with_current_compat(|cs| {
                matches!(cs.fds.get(a[0] as u32),
                    Some(crate::task::fd::OpenObject::InetUdp(_)))
            }).unwrap_or(false);
            if !is_inet_udp {
                return Err(Errno::EINVAL);
            }
            check_user_ptr(a[1], a[2].min(1 << 20))?;
            let mut buf = alloc::vec![0u8; a[2] as usize];
            let (n, port, octets) =
                crate::arch::x86_64::linux::inet_sock::udp_recvfrom_fd(a[0], &mut buf)?;
            crate::arch::x86_64::linux::io_sys::copy_out_pub(a[1], &buf[..n]);
            if a[4] != 0 && a[5] != 0 {
                // struct sockaddr_in { family=AF_INET(2), port(be16), addr, zero }
                let mut sa = [0u8; 16];
                sa[0..2].copy_from_slice(&2u16.to_ne_bytes());
                sa[2..4].copy_from_slice(&port.to_be_bytes());
                sa[4..8].copy_from_slice(&octets);
                unsafe {
                    core::ptr::write_unaligned(a[4] as *mut [u8; 16], sa);
                    core::ptr::write_unaligned(a[5] as *mut u32, 16);
                }
            }
            Ok(n as u64)
        }
        sysno::ACCEPT => unix_sock::sys_accept(a[0], a[1], a[2]),
        sysno::BIND => unix_sock::sys_bind(a[0], a[1], a[2]),
        sysno::LISTEN => unix_sock::sys_listen(a[0], a[1]),
        sysno::GETSOCKNAME => unix_sock::sys_getsockname(a[0], a[1], a[2]),
        sysno::ACCEPT4 => unix_sock::sys_accept4(a[0], a[1], a[2], a[3]),
        sysno::SETSOCKOPT => unix_sock::sys_setsockopt(a[0], a[1], a[2], a[3], a[4]),
        sysno::GETSOCKOPT => {
            let is_inet = crate::task::compat::with_current_compat(|cs| {
                matches!(cs.fds.get(a[0] as u32),
                    Some(crate::task::fd::OpenObject::InetTcp(_)))
            }).unwrap_or(false);
            if is_inet {
                crate::arch::x86_64::linux::inet_sock::getsockopt_in(None, a[1], a[2], a[3], a[4])
            } else {
                unix_sock::sys_getsockopt(a[0], a[1], a[2], a[3], a[4])
            }
        }
        sysno::SETSID => misc::sys_setsid(),
        sysno::SETPGID => misc::sys_setpgid(a[0], a[1]),
        sysno::GETPGRP | sysno::GETPGID => misc::sys_getpgid(),
        sysno::UMASK => misc::sys_umask(a[0]),
        sysno::FLOCK => misc::sys_flock(a[0], a[1]),
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
        sysno::MREMAP => mem_sys::sys_mremap(a[0], a[1], a[2], a[3], a[4]),
        sysno::MPROTECT => mem_sys::sys_mprotect(a[0], a[1], a[2]),
        // `madvise` is purely advisory: accept every hint and do nothing.
        sysno::TGKILL => misc::sys_tgkill(a[0], a[1], a[2]),
        sysno::MADVISE => Ok(0),
        // ── Misc + process (task 12.5) ──
        sysno::GETPID => misc::sys_getpid(),
        sysno::WAIT4 => process_sys::sys_wait4(a[0], a[1], a[2], a[3]),
        sysno::UNAME => misc::sys_uname(a[0]),
        sysno::ARCH_PRCTL => misc::sys_arch_prctl(a[0], a[1]),
        sysno::SET_TID_ADDRESS => misc::sys_set_tid_address(a[0]),
        sysno::CLOCK_GETTIME => misc::sys_clock_gettime(a[0], a[1]),
        sysno::GETRANDOM => misc::sys_getrandom(a[0], a[1], a[2]),
        sysno::FUTEX => process_sys::sys_futex(a[0], a[1], a[2], a[3], a[4], a[5]),
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
        sysno::GET_ROBUST_LIST => misc::sys_get_robust_list(a[0], a[1], a[2]),
        sysno::RSEQ => misc::sys_rseq(),
        sysno::PRLIMIT64 => misc::sys_prlimit64(a[0], a[1], a[2], a[3]),
        sysno::GETRLIMIT => misc::sys_getrlimit(a[0], a[1]),
        // `exit`/`exit_group` diverge (never return); the `!` coerces to the
        // `Result` arm type.
        sysno::EXIT => misc::sys_exit(a[0]),
        sysno::EXIT_GROUP => misc::sys_exit_group(a[0]),
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
    // just pushed on the current task's own kernel stack; it outlives this call
    // and is uniquely owned for the duration (each task owns a private kernel
    // stack, so preemption cannot alias the frame).
    let r = unsafe { &mut *regs };

    // The stubs enter with IF masked (SFMASK / interrupt gate). The frame and
    // the per-task user-RSP slot are now safely parked on this task's kernel
    // stack, so the window is preemption-safe: re-enable interrupts so the
    // timer keeps ticking while handlers block (futex/poll/pipe waits,
    // nanosleep). With IF masked, `ticks()` never advances — timeouts can
    // never fire — and `hlt` inside `sleep_ticks` would sleep forever. The
    // syscall exit stub re-masks IF (`cli`) before unwinding the frame.
    crate::arch::cpu::enable_interrupts();

    let (nr, args) = abi::marshal_args(r.rax, r.rdi, r.rsi, r.rdx, r.r10, r.r8, r.r9);

    // #GP post-mortem context: remember which syscall this task is running
    // (see `gp_fault_handler` in idt.rs).
    crate::arch::x86_64::idt::note_syscall(crate::task::scheduler::current_pid(), nr);

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
    // Bracket the routed handler with the stuck-syscall watchdog
    // table, so a silent in-kernel block names its pid + syscall in the log.
    let wd_pid = crate::task::scheduler::current_pid();
    inflight_enter(wd_pid, nr, args[0]);
    let result = if nr == sysno::EXECVE {
        process_sys::sys_execve(r, args[0], args[1], args[2])
    } else if nr == sysno::CLONE {
        process_sys::sys_clone(r, args[0], args[1], args[2], args[3], args[4])
    } else {
        dispatch_supported(nr, &args)
    };
    inflight_exit(wd_pid);
    match result {
        Ok(v) => v,
        Err(e) => {
            // EINVAL returns are logged with their syscall
            // number and raw args. Userspace surfaces a bare OSError(EINVAL)
            // (python importlib get_data) with no way to tell which syscall
            // produced it; this pinpoints the culprit on the next run.
            // readlink's EINVAL means "not a symlink" — a legitimate answer
            // for a regular file, not a misbehaving syscall.
            if matches!(e, Errno::EINVAL) && nr != sysno::READLINK {
                crate::warn!(
                    "[linux] EINVAL diag: nr={} args=[{:#x}, {:#x}, {:#x}, {:#x}]",
                    nr,
                    args[0],
                    args[1],
                    args[2],
                    args[3]
                );
            }
            encode_errno(e)
        }
    }
}


// ─── stuck-syscall watchdog ────────────────────────────────────────
// A silent hang (nvim's black screen) means some Compat_Process is parked
// inside one blocking syscall forever, with nothing in the log to say WHICH
// pid in WHICH syscall. linux_dispatch brackets every routed syscall with
// inflight_enter/inflight_exit; watchdog_tick (driven by yield_current, i.e.
// by the very tasks spinning in blocking loops) scans the in-flight table
// about once a second and reports anyone stuck longer than 5 seconds,
// re-reporting every 30 seconds. Entries of exited pids are swept lazily.
// Values: pid -> (nr, arg0, start_tick, last_warn_tick, last_dump_tick).
static SYSCALL_INFLIGHT: crate::sync::spinlock::Spinlock<
    alloc::collections::BTreeMap<u64, (u64, u64, u64, u64, u64)>,
> = crate::sync::spinlock::Spinlock::new(alloc::collections::BTreeMap::new());
static WD_LAST_SCAN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn inflight_enter(pid: u64, nr: u64, arg0: u64) {
    let now = crate::task::scheduler::ticks();
    SYSCALL_INFLIGHT.lock().insert(pid, (nr, arg0, now, 0, 0));
}

fn inflight_exit(pid: u64) { SYSCALL_INFLIGHT.lock().remove(&pid); }

/// Human name for the syscalls a task can realistically block in.
fn wd_sys_name(nr: u64) -> &'static str {
    match nr {
        0 => "read", 1 => "write", 7 => "poll", 20 => "writev", 23 => "select",
        35 => "nanosleep", 43 => "accept", 61 => "wait4", 202 => "futex",
        232 => "epoll_wait", 270 => "pselect6", 271 => "ppoll", 281 => "epoll_pwait",
        _ => "?",
    }
}

/// Called from `yield_current` (thread context, no scheduler locks held).
/// COMPAT_STATES is only taken AFTER the in-flight lock is released, so the
/// two locks never nest and no ordering hazard is introduced.
pub fn watchdog_tick() {
    // The stuck task itself spins through yield_current, so this
    // is the one place where its OWN fd table is the current one — dump its
    // epoll interest list from here.
    maybe_dump_self();
    let now = crate::task::scheduler::ticks();
    let last = WD_LAST_SCAN.load(core::sync::atomic::Ordering::Relaxed);
    if now.saturating_sub(last) < 100 { return; } // scan at most once a second
    if WD_LAST_SCAN.compare_exchange(last, now,
        core::sync::atomic::Ordering::Relaxed,
        core::sync::atomic::Ordering::Relaxed).is_err() { return; }
    let snapshot: alloc::vec::Vec<(u64, (u64, u64, u64, u64, u64))> =
        SYSCALL_INFLIGHT.lock().iter().map(|(&p, &e)| (p, e)).collect();
    for (pid, (nr, arg0, start, warned, _dumped)) in snapshot {
        if !crate::task::compat::compat_exists(pid) {
            SYSCALL_INFLIGHT.lock().remove(&pid);
            continue;
        }
        let age = now.saturating_sub(start);
        if age >= 500 && (warned == 0 || now.saturating_sub(warned) >= 3000) {
            let mut still_stuck = false;
            if let Some(e) = SYSCALL_INFLIGHT.lock().get_mut(&pid) {
                // Same syscall instance only: a fresh entry means it moved on.
                if e.2 == start { e.3 = now; still_stuck = true; }
            }
            if still_stuck {
                crate::warn!(
                    "[WATCHDOG] pid={} stuck in syscall {} (nr={} arg0=0x{:x} {}) for {}s",
                    pid,
                    wd_sys_name(nr),
                    nr,
                    arg0,
                    crate::arch::x86_64::linux::io_sys::fd_kind(arg0),
                    age / crate::arch::x86_64::apic::TICK_HZ
                );
            }
        }
    }
}


/// When the CURRENT task is the one stuck in epoll_wait (232) or
/// epoll_pwait (281), dump its interest list with live readiness every ~10 s.
/// Runs in the stuck task's own context (it spins through yield_current), so
/// the ordinary current-task fd helpers resolve against the right table.
fn maybe_dump_self() {
    let pid = crate::task::scheduler::current_pid();
    let now = crate::task::scheduler::ticks();
    let epfd = {
        let mut map = SYSCALL_INFLIGHT.lock();
        match map.get_mut(&pid) {
            Some(e) if e.0 == 232 || e.0 == 281 => {
                let age = now.saturating_sub(e.2);
                if age >= 500 && now.saturating_sub(e.4) >= 1000 {
                    e.4 = now;
                    Some(e.1 as u32)
                } else {
                    None
                }
            }
            _ => None,
        }
    };
    if let Some(epfd) = epfd {
        // The spawn-time [DIAG] lines are wiped when the TUI
        // clears the screen, and the QEMU display cannot scroll back. Re-print
        // the decisive evidence with every dump: what stdio is NOW and what it
        // was AT execve time (after the close-on-exec sweep).
        let _ = crate::task::compat::with_current_compat(|cs| {
            crate::warn!("[WATCHDOG] pid={} stdio now: fd0={} fd1={} fd2={} | at exec: fd0={} fd1={} fd2={}",
                pid, cs.fds.describe_fd(0), cs.fds.describe_fd(1), cs.fds.describe_fd(2),
                cs.exec_stdio[0], cs.exec_stdio[1], cs.exec_stdio[2]);
        });
        epoll_sys::dump_epoll_self(epfd);
    }
}
