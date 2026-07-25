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
    /// `socketpair` — pair of connected AF_UNIX stream sockets (STAGE-15).
    pub const SOCKETPAIR: u64 = 53;
    /// `statx` — extended file status (STAGE-15).
    pub const STATX: u64 = 332;
    /// STAGE-16: AF_UNIX stream sockets (nvim's msgpack-rpc server).
    pub const SOCKET: u64 = 41;
    pub const CONNECT: u64 = 42;
    pub const ACCEPT: u64 = 43;
    pub const BIND: u64 = 49;
    pub const LISTEN: u64 = 50;
    pub const GETSOCKNAME: u64 = 51;
    pub const ACCEPT4: u64 = 288;
pub const SETSOCKOPT: u64 = 54;
pub const GETSOCKOPT: u64 = 55;
    /// STAGE-16: session/permission/locking stubs.
    pub const SETSID: u64 = 112;
    pub const UMASK: u64 = 95;
    pub const FLOCK: u64 = 73;
    /// STAGE-16.11: vectored / positional-vectored I/O + file sync. nvim's
    /// ShaDa reader hit ENOSYS here (E886 "function not implemented").
    pub const READV: u64 = 19;
    pub const PREADV: u64 = 295;
    pub const PWRITEV: u64 = 296;
    pub const FSYNC: u64 = 74;
    pub const FDATASYNC: u64 = 75;
    /// STAGE-16.13: rename family. nvim writes ShaDa to a tmp file and
    /// renames it into place; missing rename left "E136: Can't rename ShaDa"
    /// + a Press-ENTER prompt wedged on every exit.
    pub const RENAME: u64 = 82;
    pub const RENAMEAT: u64 = 264;
    pub const RENAMEAT2: u64 = 316;
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
    /// `rt_sigaction` — examine/change a signal action (stub).
    pub const RT_SIGACTION: u64 = 13;
    /// `rt_sigprocmask` — examine/change the blocked-signal mask (stub).
    pub const RT_SIGPROCMASK: u64 = 14;
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
    pub const CLOCK_GETTIME: u64 = 228;
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

/// Test whether `nr` is a member of the `Supported_Syscall_Set` (R2.1).
///
/// Returns `true` for exactly the enumerated numbers and `false` for everything
/// else — including the explicitly out-of-scope `clone` (56), `fork` (57),
/// `vfork` (58), and `futex` (202), as well as any graphical/windowing syscall
/// number (R1.4, R11.4, R11.5). The dispatcher uses this gate to return `-ENOSYS`
/// before inspecting any argument pointer.
#[inline]
pub fn is_supported(nr: u64) -> bool {
    matches!(
        nr,
        nr::READ
            | nr::WRITE
            | nr::OPEN
            | nr::CLOSE
            | nr::FSTAT
            | nr::POLL
            | nr::SELECT
            | nr::PSELECT6
            | nr::PPOLL
            | nr::LSEEK
            | nr::MMAP
            | nr::MPROTECT
            | nr::MUNMAP
            | nr::MREMAP
            | nr::MADVISE
            | nr::TGKILL
            | nr::BRK
            | nr::RT_SIGACTION
            | nr::RT_SIGPROCMASK
            | nr::IOCTL
            | nr::PREAD64
            | nr::PWRITE64
            | nr::WRITEV
            | nr::READV
            | nr::PREADV
            | nr::PWRITEV
            | nr::FSYNC
            | nr::FDATASYNC
            | nr::RENAME
            | nr::RENAMEAT
            | nr::RENAMEAT2
            | nr::ACCESS
            | nr::PIPE
            | nr::SCHED_YIELD
            | nr::DUP
            | nr::DUP2
            | nr::NANOSLEEP
            | nr::GETPID
            | nr::CLONE
            | nr::EXECVE
            | nr::EXIT
            | nr::WAIT4
            | nr::UNAME
            | nr::FCNTL
            | nr::GETCWD
            | nr::CHDIR
            | nr::FCHDIR
            | nr::READLINK
            | nr::MKDIR
            | nr::GETTIMEOFDAY
            | nr::GETRLIMIT
            | nr::SYSINFO
            | nr::GETUID
            | nr::GETGID
            | nr::GETEUID
            | nr::GETEGID
            | nr::GETPPID
            | nr::STATFS
            | nr::FSTATFS
            | nr::ARCH_PRCTL
            | nr::GETTID
            | nr::TIME
            | nr::FUTEX
            | nr::GETDENTS64
            | nr::SET_TID_ADDRESS
            | nr::CLOCK_GETRES
            | nr::CLOCK_GETTIME
            | nr::CLOCK_NANOSLEEP
            | nr::EPOLL_WAIT
            | nr::EPOLL_CTL
            | nr::EPOLL_CREATE1
            | nr::EVENTFD2
            | nr::EXIT_GROUP
            | nr::OPENAT
            | nr::NEWFSTATAT
            | nr::READLINKAT
            | nr::SET_ROBUST_LIST
            | nr::GET_ROBUST_LIST
            | nr::DUP3
            | nr::PIPE2
            | nr::PRLIMIT64
            | nr::GETRANDOM
            | nr::RSEQ
            | nr::SIGALTSTACK
            | nr::SOCKETPAIR
            | nr::EPOLL_PWAIT
            | nr::STATX
            | nr::SOCKET
            | nr::CONNECT
            | nr::ACCEPT
            | nr::BIND
            | nr::LISTEN
            | nr::GETSOCKNAME
            | nr::ACCEPT4
            | nr::SETSOCKOPT
            | nr::GETSOCKOPT
            | nr::SETSID
            | nr::UMASK
            | nr::FLOCK
    )
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

    #[test]
    fn supported_set_is_exact() {
        let supported = [
            0, 1, 2, 3, 5, 7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 18, 20, 21, 22, 24, 28, 32, 33, 35, 39,
            60, 63, 72, 79, 80, 81, 89, 96, 97, 99, 102, 104, 107, 108, 110, 131, 137, 138,
            158, 186, 201, 202, 217, 218, 228, 230, 231, 257, 262, 267, 273, 292, 293, 302, 318, 334,
        ];
        for nr in supported {
            assert!(is_supported(nr), "expected {nr} to be supported");
        }
        // Out-of-scope numbers: clone/fork/vfork/futex stay unsupported.
        for nr in [4, 57, 58, 1000, u64::MAX] {
            if supported.contains(&nr) {
                continue;
            }
            assert!(!is_supported(nr), "expected {nr} to be unsupported");
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
