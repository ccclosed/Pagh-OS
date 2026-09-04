//! Linux errno model for the x86_64 binary-compatibility layer.
//!
//! Pure, allocation-free, `core`-only logic shared with the `host-tests` crate
//! (R11.6). The kernel folds a handler's `Err(e)` into `rax` as the negated errno
//! value Linux expects, i.e. a value in the range `-4095..=-1` (R1.3).
#![allow(dead_code)]

/// The subset of Linux `errno` values the compatibility layer can report.
///
/// Discriminants match the Linux x86_64 ABI exactly so the encoded `rax` value is
/// the negated number a Linux binary expects.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Errno {
    /// Operation not permitted.
    EPERM = 1,
    /// No such file or directory.
    ENOENT = 2,
    ESRCH = 3,
    /// Interrupted system call (a deliverable signal poked a blocking wait).
    EINTR = 4,
    /// Input/output error (a real VFS/device read or write failure).
    EIO = 5,
    /// Argument list too long.
    E2BIG = 7,
    /// Exec format error.
    ENOEXEC = 8,
    /// No child processes.
    ECHILD = 10,
    /// Resource temporarily unavailable (including unavailable secure entropy).
    EAGAIN = 11,
    /// Socket operation on non-socket.
    ENOTSOCK = 88,
    /// Address family not supported by protocol (AF_INET6 etc.).
    EAFNOSUPPORT = 97,
    /// Network is unreachable.
    ENETUNREACH = 101,
    /// Connection already in progress (non-blocking connect).
    EINPROGRESS = 115,
    /// Transport endpoint is not connected.
    ENOTCONN = 107,
    /// Connection refused.
    ECONNREFUSED = 111,
    /// Socket is already connected.
    EISCONN = 106,
    /// Bad file descriptor.
    EBADF = 9,
    /// Cannot allocate memory.
    ENOMEM = 12,
    /// Permission denied (directory rename refusal).
    EACCES = 13,
    /// Bad address.
    EFAULT = 14,
    /// File exists.
    EEXIST = 17,
    /// Not a directory.
    ENOTDIR = 20,
    /// Is a directory.
    EISDIR = 21,
    /// Invalid argument.
    EINVAL = 22,
    /// Inappropriate ioctl for device (not a tty).
    ENOTTY = 25,
    /// Illegal seek (pipe/non-seekable).
    ESPIPE = 29,
    /// Broken pipe.
    EPIPE = 32,
    /// Result too large / buffer too small.
    ERANGE = 34,
    /// Function not implemented.
    ENOSYS = 38,
    /// Operation timed out.
    ETIMEDOUT = 110,
}

/// Encode an [`Errno`] for return in `rax` as Linux's negated-errno convention.
///
/// The result reinterpreted as an `i64` lies in `[-4095, -1]` (R1.3).
pub fn encode_errno(e: Errno) -> u64 {
    (-(e as i64)) as u64
}
