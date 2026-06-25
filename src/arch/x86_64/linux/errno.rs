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
    /// Bad file descriptor.
    EBADF = 9,
    /// Cannot allocate memory.
    ENOMEM = 12,
    /// Bad address.
    EFAULT = 14,
    /// Not a directory.
    ENOTDIR = 20,
    /// Is a directory.
    EISDIR = 21,
    /// Invalid argument.
    EINVAL = 22,
    /// Illegal seek (pipe/non-seekable).
    ESPIPE = 29,
    /// Result too large / buffer too small.
    ERANGE = 34,
    /// Function not implemented.
    ENOSYS = 38,
}

/// Encode an [`Errno`] for return in `rax` as Linux's negated-errno convention.
///
/// The result reinterpreted as an `i64` lies in `[-4095, -1]` (R1.3).
pub fn encode_errno(e: Errno) -> u64 {
    (-(e as i64)) as u64
}
