//! File I/O syscall planning and effectful handlers.
//!
//! This task (2.1) implements only the **pure** planning logic for `read` and
//! `lseek`: allocation-free, `core`-only functions that compute outcomes without
//! touching the VFS, the per-process `FdTable`, hardware, or global mutable state
//! (R11.6). The effectful handlers that wire these plans to real descriptors land
//! in task 12.1.
//!
//! The `Errno` type is shared from the sibling `errno` module via `super::errno` so
//! the same source compiles both in the kernel
//! (`crate::arch::x86_64::linux::errno`) and when included into the `host-tests`
//! crate as a crate-root sibling module (R11.6).
#![allow(dead_code)]

use super::errno::Errno;

/// `lseek` whence: set the offset to `delta` relative to the start of the file.
pub const SEEK_SET: u32 = 0;
/// `lseek` whence: set the offset to `delta` relative to the current offset.
pub const SEEK_CUR: u32 = 1;
/// `lseek` whence: set the offset to `delta` relative to the end of the file.
pub const SEEK_END: u32 = 2;

/// Plan a `read` of `count` bytes from offset `off` of a file of length `size`.
///
/// Returns `(copied, new_off)` where the byte count is clamped to what remains
/// before EOF and the descriptor offset is advanced by exactly that many bytes
/// (R2.3, Property 5):
///
/// * `copied = min(count, size.saturating_sub(off))`
/// * `new_off = off + copied`
///
/// `saturating_sub` makes an offset at or beyond EOF yield `copied == 0` (and thus
/// `new_off == off`) rather than underflowing. `off + copied` cannot overflow
/// because `copied <= size - off` whenever `off <= size`, and `copied == 0`
/// otherwise.
pub fn plan_read(size: u64, off: u64, count: u64) -> (u64, u64) {
    let copied = core::cmp::min(count, size.saturating_sub(off));
    let new_off = off + copied;
    (copied, new_off)
}

/// Plan an `lseek` to a new absolute offset.
///
/// `whence` selects the base the signed `delta` is applied to:
///
/// * [`SEEK_SET`] → base `0`
/// * [`SEEK_CUR`] → base `cur` (the descriptor's current offset)
/// * [`SEEK_END`] → base `size` (the file length)
///
/// The absolute offset is computed in `i128` to avoid overflow at the `u64`
/// boundary, then validated: a non-negative result that fits in a `u64` is returned
/// as `Ok`; a negative result, a result exceeding `u64::MAX`, or an unrecognized
/// `whence` yields `Err(Errno::EINVAL)`, leaving the caller's offset unchanged
/// (R2.7, R2.15, Property 6).
pub fn plan_lseek(whence: u32, cur: u64, size: u64, delta: i64) -> Result<u64, Errno> {
    let base: u64 = match whence {
        SEEK_SET => 0,
        SEEK_CUR => cur,
        SEEK_END => size,
        _ => return Err(Errno::EINVAL),
    };

    let absolute = base as i128 + delta as i128;
    if absolute < 0 || absolute > u64::MAX as i128 {
        return Err(Errno::EINVAL);
    }
    Ok(absolute as u64)
}
