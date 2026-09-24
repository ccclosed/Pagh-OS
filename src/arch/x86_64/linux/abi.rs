//! Pure Linux x86_64 syscall ABI marshalling and supported-set membership.
//!
//! Pure, allocation-free, `core`-only logic shared with the `host-tests` crate
//! (R11.6). Two responsibilities live here:
//!
//!   * [`marshal_args`] — decode the Linux x86_64 calling convention from the saved
//!     general-purpose registers into a `(nr, args)` pair the dispatcher consumes
//!     (R1.1, R1.8).
//!   * [`is_supported`] — exact membership test for the fixed `Supported_Syscall_Set`
//!     (R2.1), used to gate dispatch and return `-ENOSYS` for everything else
//!     (R1.4, R11.4, R11.5).
//!
//! The numeric syscall constants are exposed as `pub const`s so later tasks (the
//! effectful io/mem/misc handlers and `linux_dispatch`) match against shared names
//! rather than bare literals.
#![allow(dead_code)]

/// Linux x86_64 syscall numbers in the `Supported_Syscall_Set` (R2.1).
pub mod nr {
    /// `read` — read from a file descriptor.
    pub const READ: u64 = 0;
    /// `write` — write to a file descriptor.
    pub const WRITE: u64 = 1;
    /// `open` — open/create a file.
    pub const OPEN: u64 = 2;
    /// `close` — close a file descriptor.
    pub const CLOSE: u64 = 3;
    /// `fstat` — file status by descriptor.
    pub const FSTAT: u64 = 5;
    pub const POLL: u64 = 7;
    /// `select` — synchronous fd multiplexing (timeval timeout).
    pub const SELECT: u64 = 23;
    /// `pselect6` — select with timespec + sigmask (readline waits on this).
    pub const EPOLL_WAIT: u64 = 232;
    pub const EPOLL_CTL: u64 = 233;
    pub const PSELECT6: u64 = 270;
    /// `ppoll` — poll with timespec + sigmask.
    pub const PPOLL: u64 = 271;
    pub const EVENTFD2: u64 = 290;
    pub const EPOLL_CREATE1: u64 = 291;
    /// `epoll_pwait` — epoll_wait with a sigmask (mask accepted and ignored).
    pub const EPOLL_PWAIT: u64 = 281;
    /// `socketpair` — pair of connected AF_UNIX stream sockets.
    pub const SOCKETPAIR: u64 = 53;
    /// `statx` — extended file status.
    pub const STATX: u64 = 332;
    /// AF_UNIX stream sockets (nvim's msgpack-rpc server).
    pub const SOCKET: u64 = 41;
    pub const CONNECT: u64 = 42;
    pub const ACCEPT: u64 = 43;
    pub const BIND: u64 = 49;
    pub const LISTEN: u64 = 50;
    pub const GETSOCKNAME: u64 = 51;
    pub const ACCEPT4: u64 = 288;
    pub const SETSOCKOPT: u64 = 54;
    pub const GETSOCKOPT: u64 = 55;
    /// Session/permission/locking stubs.
    pub const SETSID: u64 = 112;
    /// Job-control family bash probes at startup.
    pub const SETPGID: u64 = 109;
    pub const GETPGRP: u64 = 111;
    pub const GETPGID: u64 = 121;
    pub const UMASK: u64 = 95;
    pub const FLOCK: u64 = 73;
    /// Vectored / positional-vectored I/O + file sync. nvim's
    /// ShaDa reader hit ENOSYS here (E886 "function not implemented").
    pub const READV: u64 = 19;
    pub const PREADV: u64 = 295;
    pub const PWRITEV: u64 = 296;
    pub const FSYNC: u64 = 74;
    pub const FDATASYNC: u64 = 75;
    /// Rename family. nvim writes ShaDa to a tmp file and
    /// renames it into place; missing rename left "E136: Can't rename ShaDa"
    /// + a Press-ENTER prompt wedged on every exit.
    pub const RENAME: u64 = 82;
    pub const RENAMEAT: u64 = 264;
    pub const RENAMEAT2: u64 = 316;
    /// Real x86_64 numbers (asm/unistd_64.h): settime=227, gettime=228,
    /// getres=229, nanosleep=230, exit_group=231. Verified against
    /// /usr/include/asm/unistd_64.h after a wrong "off by one" fix collided
    /// nanosleep with exit_group and made process exit impossible.
    pub const CLOCK_SETTIME: u64 = 227;
    pub const CLOCK_GETTIME: u64 = 228;
    pub const CLOCK_GETRES: u64 = 229;
    /// `lseek` — reposition a descriptor's offset.
    pub const LSEEK: u64 = 8;
    /// `mmap` — map memory.
    pub const MMAP: u64 = 9;
    /// `mprotect` — change memory protection.
    pub const MPROTECT: u64 = 10;
    /// `munmap` — unmap memory.
    pub const MUNMAP: u64 = 11;
    /// `mremap` — resize a mapping (glibc realloc fast path).
    pub const MREMAP: u64 = 25;
    /// `brk` — change the program break.
    pub const BRK: u64 = 12;
    /// `rt_sigaction` — examine/change a signal action.
    pub const RT_SIGACTION: u64 = 13;
    /// `rt_sigprocmask` — examine/change the blocked-signal mask.
    pub const RT_SIGPROCMASK: u64 = 14;
    /// `rt_sigreturn` — return from a signal handler (restores the context
    /// saved in the `rt_sigframe` the restorer stands on).
    pub const RT_SIGRETURN: u64 = 15;
    /// `ioctl` — device control.
    pub const IOCTL: u64 = 16;
    /// `pread64` — positional read (offset not advanced).
    pub const PREAD64: u64 = 17;
    /// `pwrite64` — positional write (offset not advanced).
    pub const PWRITE64: u64 = 18;
    /// `writev` — gathered write.
    pub const WRITEV: u64 = 20;
    /// `access` — check file accessibility.
    pub const ACCESS: u64 = 21;
    pub const PIPE: u64 = 22;
    /// `sched_yield` — yield the CPU.
    pub const SCHED_YIELD: u64 = 24;
    /// `dup` — duplicate a file descriptor (lowest free).
    pub const DUP: u64 = 32;
    /// `dup2` — duplicate a file descriptor to an explicit target.
    pub const DUP2: u64 = 33;
    /// `nanosleep` — high-resolution sleep.
    pub const NANOSLEEP: u64 = 35;
    /// `getpid` — get process id.
    pub const GETPID: u64 = 39;
    /// `clone` — create a thread sharing the address space.
    pub const CLONE: u64 = 56;
    /// `execve` — replace the current process image.
    pub const EXECVE: u64 = 59;
    /// `exit` — terminate the calling task.
    pub const EXIT: u64 = 60;
    /// wait4 — wait for a child process.
    pub const WAIT4: u64 = 61;
    /// `kill` — send a signal to a process, process group, or every process.
    /// Argument decoding + target classification live in [`super::kill`]
    /// (host-tested by `p51`); the effectful handler is
    /// [`super::signal::sys_kill`].
    pub const KILL: u64 = 62;
    /// `uname` — get system identification.
    pub const UNAME: u64 = 63;
    /// `fcntl` — file descriptor control.
    pub const FCNTL: u64 = 72;
    /// `getcwd` — get the current working directory.
    pub const GETCWD: u64 = 79;
    /// `chdir` — change the current working directory.
    pub const CHDIR: u64 = 80;
    /// `fchdir` — change the cwd to a directory fd's path.
    pub const FCHDIR: u64 = 81;
    /// `readlink` — read the target of a symbolic link.
    /// `mkdir` — create a directory.
    pub const MKDIR: u64 = 83;
    /// `rmdir` (84) — git removes empty temp dirs during cleanup.
    pub const RMDIR: u64 = 84;
    /// `sendto` (44) — glibc resolver DNS queries over UDP.
    pub const SENDTO: u64 = 44;
    /// `recvfrom` (45) — DNS replies.
    pub const RECVFROM: u64 = 45;
    /// `unlink` (87) — git removes its `*.lock` files.
    pub const UNLINK: u64 = 87;
    /// `chmod` (90) — git chmods `.git/config.lock` during init.
    pub const CHMOD: u64 = 90;
    pub const READLINK: u64 = 89;
    /// `gettimeofday` — get wall-clock time as a `timeval`.
    pub const GETTIMEOFDAY: u64 = 96;
    /// `getrlimit` — get a resource limit.
    pub const GETRLIMIT: u64 = 97;
    /// `sysinfo` — get system statistics.
    pub const SYSINFO: u64 = 99;
    /// `getuid` — get real user id.
    pub const GETUID: u64 = 102;
    /// `getgid` — get real group id.
    pub const GETGID: u64 = 104;
    /// `geteuid` — get effective user id.
    pub const GETEUID: u64 = 107;
    /// `getegid` — get effective group id.
    pub const GETEGID: u64 = 108;
    /// `getppid` — get parent process id.
    pub const GETPPID: u64 = 110;
    /// `statfs` — filesystem statistics by path.
    pub const STATFS: u64 = 137;
    /// `fstatfs` — filesystem statistics by fd.
    pub const FSTATFS: u64 = 138;
    /// `arch_prctl` — set/get architecture-specific thread state.
    pub const ARCH_PRCTL: u64 = 158;
    /// `gettid` — get thread id.
    pub const GETTID: u64 = 186;
    /// `time` — get wall-clock seconds.
    pub const TIME: u64 = 201;
    /// `futex` — userspace synchronization primitive.
    pub const FUTEX: u64 = 202;
    /// `getdents64` — read directory entries.
    pub const GETDENTS64: u64 = 217;
    /// `set_tid_address` — set pointer to thread id.
    pub const SET_TID_ADDRESS: u64 = 218;
    /// `clock_gettime` — read a POSIX clock.
    /// `clock_nanosleep` — high-resolution sleep against a clock.
    pub const CLOCK_NANOSLEEP: u64 = 230;
    /// `exit_group` — terminate the calling task (thread-group form).
    pub const EXIT_GROUP: u64 = 231;
    /// `openat` — open relative to a directory fd.
    pub const OPENAT: u64 = 257;
    /// `newfstatat` — file status relative to a directory fd.
    pub const NEWFSTATAT: u64 = 262;
    /// `readlinkat` — read a symlink target relative to a directory fd.
    pub const READLINKAT: u64 = 267;
    /// `set_robust_list` / `get_robust_list` — robust futex ownership.
    pub const SET_ROBUST_LIST: u64 = 273;
    pub const GET_ROBUST_LIST: u64 = 274;
    /// `dup3` — duplicate a file descriptor to an explicit target with flags.
    pub const DUP3: u64 = 292;
    pub const PIPE2: u64 = 293;
    /// `madvise` — advisory memory-usage hints (accepted and ignored).
    pub const MADVISE: u64 = 28;
    /// `tgkill` — send a signal to a specific thread (single-thread model: self).
    pub const TGKILL: u64 = 234;
    /// `prlimit64` — get/set a resource limit.
    pub const PRLIMIT64: u64 = 302;
    /// `getrandom` — fill a buffer with random bytes.
    pub const GETRANDOM: u64 = 318;
    /// `rseq` — restartable sequences registration (stub).
    pub const RSEQ: u64 = 334;
    /// `sigaltstack` — set/get the signal stack (stub).
    pub const SIGALTSTACK: u64 = 131;
}

/// Marshal the saved Linux x86_64 syscall registers into `(nr, args)`.
///
/// Per the Linux x86_64 convention the syscall number is in `rax` and the six
/// arguments are in `rdi, rsi, rdx, r10, r8, r9` — note `r10`, not `rcx`, holds the
/// fourth argument. This function applies exactly that permutation and always copies
/// all six argument registers regardless of how many the named syscall actually
/// consumes (R1.1, R1.8).
///
/// Returns `(nr, [a1, a2, a3, a4, a5, a6])` where
/// `[a1..a6] == [rdi, rsi, rdx, r10, r8, r9]`.
#[inline]
pub fn marshal_args(
    rax: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    r10: u64,
    r8: u64,
    r9: u64,
) -> (u64, [u64; 6]) {
    (rax, [rdi, rsi, rdx, r10, r8, r9])
}

/// The complete `Supported_Syscall_Set` (R2.1) — the single source of truth for
/// both the dispatch gate and the exactness test.
///
/// Written as `nr::` NAMES, not bare numbers, so the list is self-documenting and
/// cannot drift from the constants the dispatcher matches on. Every entry has a
/// real arm in `linux::dispatch_supported` (the two entries routed specially by
/// `linux_dispatch` itself — `execve`, `clone`, `rt_sigreturn` — are members
/// too, since they pass the gate on the way there); a number NOT here returns
/// `-ENOSYS` before any argument pointer is inspected (R1.4, R11.4, R11.5).
/// The list is strictly ascending, so [`is_supported`] can binary-search it.
///
/// ## Maintenance contract (three edits, same commit)
///
/// Adding a syscall means: an `nr::` constant, a `dispatch_supported` arm, and
/// this list. The two runtime-visible halves are then checked from both ends:
///
///   * the `supported_set_is_exact` test compares this list against an
///     INDEPENDENT literal enumeration of the numbers (so a typo in a constant
///     or a missing/extra entry fails on the host), asserts strict ascending
///     order (so a duplicated or misplaced entry fails), and walks `0..=600`
///     in BOTH directions (accepted here ⇒ gated in; not here ⇒ `-ENOSYS`);
///   * the `dispatch_supported` fallback arm logs
///     `nr has NO dispatch arm` and returns `ENOSYS` — the reverse drift
///     (gated in, no arm) therefore announces itself instead of silently
///     turning a syscall into a permanent `ENOSYS`.
///
/// Match arms cannot be introspected from a test, so this list is the manual
/// enumeration of the dispatcher's coverage; the test locks the numbers and the
/// fallback tripwire locks the arms.
///
/// History note: the previous test asserted membership for only 69 of the 109
/// numbers the gate accepted and never asserted that anything was *unsupported*,
/// so it could not catch either class of drift.
pub const SUPPORTED_SYSCALLS: [u64; 110] = [
    nr::READ,
    nr::WRITE,
    nr::OPEN,
    nr::CLOSE,
    nr::FSTAT,
    nr::POLL,
    nr::LSEEK,
    nr::MMAP,
    nr::MPROTECT,
    nr::MUNMAP,
    nr::BRK,
    nr::RT_SIGACTION,
    nr::RT_SIGPROCMASK,
    nr::RT_SIGRETURN,
    nr::IOCTL,
    nr::PREAD64,
    nr::PWRITE64,
    nr::READV,
    nr::WRITEV,
    nr::ACCESS,
    nr::PIPE,
    nr::SELECT,
    nr::SCHED_YIELD,
    nr::MREMAP,
    nr::MADVISE,
    nr::DUP,
    nr::DUP2,
    nr::NANOSLEEP,
    nr::GETPID,
    nr::SOCKET,
    nr::CONNECT,
    nr::ACCEPT,
    nr::SENDTO,
    nr::RECVFROM,
    nr::BIND,
    nr::LISTEN,
    nr::GETSOCKNAME,
    nr::SOCKETPAIR,
    nr::SETSOCKOPT,
    nr::GETSOCKOPT,
    nr::CLONE,
    nr::EXECVE,
    nr::EXIT,
    nr::WAIT4,
    nr::KILL,
    nr::UNAME,
    nr::FCNTL,
    nr::FLOCK,
    nr::FSYNC,
    nr::FDATASYNC,
    nr::GETCWD,
    nr::CHDIR,
    nr::FCHDIR,
    nr::RENAME,
    nr::MKDIR,
    nr::RMDIR,
    nr::UNLINK,
    nr::READLINK,
    nr::CHMOD,
    nr::UMASK,
    nr::GETTIMEOFDAY,
    nr::GETRLIMIT,
    nr::SYSINFO,
    nr::GETUID,
    nr::GETGID,
    nr::GETEUID,
    nr::GETEGID,
    nr::SETPGID,
    nr::GETPPID,
    nr::GETPGRP,
    nr::SETSID,
    nr::GETPGID,
    nr::SIGALTSTACK,
    nr::STATFS,
    nr::FSTATFS,
    nr::ARCH_PRCTL,
    nr::GETTID,
    nr::TIME,
    nr::FUTEX,
    nr::GETDENTS64,
    nr::SET_TID_ADDRESS,
    nr::CLOCK_SETTIME,
    nr::CLOCK_GETTIME,
    nr::CLOCK_GETRES,
    nr::CLOCK_NANOSLEEP,
    nr::EXIT_GROUP,
    nr::EPOLL_WAIT,
    nr::EPOLL_CTL,
    nr::TGKILL,
    nr::OPENAT,
    nr::NEWFSTATAT,
    nr::RENAMEAT,
    nr::READLINKAT,
    nr::PSELECT6,
    nr::PPOLL,
    nr::SET_ROBUST_LIST,
    nr::GET_ROBUST_LIST,
    nr::EPOLL_PWAIT,
    nr::ACCEPT4,
    nr::EVENTFD2,
    nr::EPOLL_CREATE1,
    nr::DUP3,
    nr::PIPE2,
    nr::PREADV,
    nr::PWRITEV,
    nr::PRLIMIT64,
    nr::RENAMEAT2,
    nr::GETRANDOM,
    nr::STATX,
    nr::RSEQ,
];

/// Test whether `nr` is a member of the `Supported_Syscall_Set` (R2.1).
///
/// A binary search of [`SUPPORTED_SYSCALLS`], so the gate and the enumerated
/// table can never disagree. Returns `false` for every number not in the table,
/// including the still-out-of-scope `fork` (57) and `vfork` (58) and any
/// graphical/windowing syscall number (R1.4, R11.4, R11.5). The dispatcher uses
/// this gate to return `-ENOSYS` before inspecting any argument pointer.
#[inline]
pub fn is_supported(nr: u64) -> bool {
    SUPPORTED_SYSCALLS.binary_search(&nr).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marshal_applies_linux_permutation() {
        let (nr, args) = marshal_args(0xAA, 1, 2, 3, 4, 5, 6);
        assert_eq!(nr, 0xAA);
        assert_eq!(args, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn marshal_r10_is_fourth_arg() {
        // r10 (not rcx) is a4; confirm positions are exactly rdi,rsi,rdx,r10,r8,r9.
        let (_, args) = marshal_args(0, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60);
        assert_eq!(args, [0x10, 0x20, 0x30, 0x40, 0x50, 0x60]);
    }

    /// The gate and the enumerated `Supported_Syscall_Set` are the same set —
    /// checked in BOTH directions against an independent literal enumeration of
    /// the Linux x86_64 numbers the dispatcher implements.
    ///
    /// `EXPECTED` is deliberately NOT derived from [`SUPPORTED_SYSCALLS`] or from
    /// [`is_supported`]: it is the independent half of a double-entry check
    /// (`nr::` names vs bare numbers), so a wrong constant value, a missing
    /// entry, or a duplicated one fails here instead of shipping. The reverse
    /// drift (a number gated in with no `dispatch_supported` arm) is caught by
    /// that function's fallback tripwire at runtime.
    ///
    /// The old version of this test asserted membership for only 69 of the 109
    /// numbers the gate accepted, never asserted that anything was unsupported,
    /// and compared against a subset instead of the whole `0..=600` domain.
    #[test]
    fn supported_set_is_exact() {
        // Independent enumeration: the Linux x86_64 syscall numbers with a real
        // dispatcher arm, in ascending order (keep it sorted!).
        const EXPECTED: [u64; 110] = [
            0, 1, 2, 3, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
            28, 32, 33, 35, 39, 41, 42, 43, 44, 45, 49, 50, 51, 53, 54, 55, 56, 59, 60, 61, 62, 63,
            72, 73, 74, 75, 79, 80, 81, 82, 83, 84, 87, 89, 90, 95, 96, 97, 99, 102, 104, 107, 108,
            109, 110, 111, 112, 121, 131, 137, 138, 158, 186, 201, 202, 217, 218, 227, 228, 229,
            230, 231, 232, 233, 234, 257, 262, 264, 267, 270, 271, 273, 274, 281, 288, 290, 291,
            292, 293, 295, 296, 302, 316, 318, 332, 334,
        ];

        // 1. The two independent enumerations agree element-wise, and both are
        //    strictly ascending (the gate's binary search depends on it; a
        //    duplicated or misplaced entry cannot hide behind a passing check).
        assert_eq!(
            SUPPORTED_SYSCALLS, EXPECTED,
            "nr:: names and the literal number list disagree"
        );
        for w in EXPECTED.windows(2) {
            assert!(
                w[0] < w[1],
                "enumeration is not strictly ascending: {} then {}",
                w[0],
                w[1]
            );
        }

        // 2. Two-way exactness over a superset of every defined number: the gate
        //    accepts `n` iff the independent enumeration contains `n`.
        for n in 0..=600u64 {
            assert_eq!(
                is_supported(n),
                EXPECTED.binary_search(&n).is_ok(),
                "gate and enumeration disagree on nr={n}"
            );
        }
        for n in [1000u64, 1 << 20, u64::MAX] {
            assert!(!is_supported(n), "nr={n} must stay unsupported");
        }

        // 3. Spot checks for the numbers whose treatment the docs got wrong:
        //    clone(56) and futex(202) ARE supported (the previous doc comment
        //    claimed they were out of scope), kill(62) is the newest member,
        //    and fork/vfork/stat never are.
        for n in [nr::CLONE, nr::FUTEX, nr::KILL] {
            assert!(is_supported(n), "nr={n} must be supported");
        }
        for n in [
            4u64, /* stat */
            57,   /* fork */
            58,   /* vfork */
        ] {
            assert!(!is_supported(n), "nr={n} must stay unsupported");
        }
    }

    #[test]
    fn process_model_syscalls_stay_unsupported() {
        // The "later milestone" process-model syscalls must remain ENOSYS.
        for nr in [57u64 /* fork */, 58 /* vfork */] {
            assert!(!is_supported(nr), "{nr} must stay unsupported");
        }
    }
}
