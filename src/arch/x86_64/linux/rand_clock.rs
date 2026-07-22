//! `getrandom` and `clock_gettime` syscall planning.
//!
//! This task (2.6) implements only the **pure** planning logic: allocation-free,
//! `core`-only functions that compute outcomes without touching hardware, the RNG
//! source, the kernel tick clock, or global mutable state (R11.6). The effectful
//! handlers that wire these plans to the real RNG and tick clock land in task 12.5,
//! which supplies the actual tick rate to [`ticks_to_timespec`].
//!
//! The `Errno` type is shared from the sibling `errno` module via `super::errno` so
//! the same source compiles both in the kernel
//! (`crate::arch::x86_64::linux::errno`) and when included into the `host-tests`
//! crate as a crate-root sibling module (R11.6).
#![allow(dead_code)]

use super::errno::Errno;

/// Number of nanoseconds in one second.
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// POSIX `clock_gettime` clock id for the realtime (wall-clock) clock.
pub const CLOCK_REALTIME: u32 = 0;
/// POSIX `clock_gettime` clock id for the monotonic clock.
pub const CLOCK_MONOTONIC: u32 = 1;

/// Linux `struct timespec` as laid out by the x86_64 ABI.
///
/// Both fields are signed 64-bit. A normalized timespec keeps
/// `tv_nsec ∈ [0, 1_000_000_000)` (R2.13).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timespec {
    /// Whole seconds.
    pub tv_sec: i64,
    /// Sub-second remainder in nanoseconds, always in `[0, 1_000_000_000)`.
    pub tv_nsec: i64,
}

/// Plan a `getrandom` fill of `n` bytes into a buffer of length `buflen`.
///
/// Returns `Ok(n)` — the number of bytes the handler must fill — when the request
/// fits the buffer (`n <= buflen`), and `Err(Errno::EINVAL)` otherwise, in which
/// case the handler leaves the user buffer unmodified (R2.12, R2.16, Property 9).
pub fn getrandom_plan(buflen: u64, n: u64) -> Result<u64, Errno> {
    if n <= buflen {
        Ok(n)
    } else {
        Err(Errno::EINVAL)
    }
}

/// Convert a kernel tick count into a normalized [`Timespec`] for `clock_gettime`.
///
/// `clock_id` must be one of the supported POSIX clocks, [`CLOCK_REALTIME`] or
/// [`CLOCK_MONOTONIC`]; any other id yields `Err(Errno::EINVAL)` and the handler
/// leaves the user buffer unmodified (R2.13, R2.16, Property 10).
///
/// `tick_hz` is the tick rate (ticks per second) the effectful caller (task 12.5)
/// supplies; keeping it a parameter makes this function pure and host-testable. The
/// total elapsed nanoseconds are computed in 128-bit arithmetic to avoid overflow
/// before splitting into seconds and the sub-second remainder:
///
/// * `total_ns = ticks * 1_000_000_000 / tick_hz`
/// * `tv_sec   = total_ns / 1_000_000_000`
/// * `tv_nsec  = total_ns % 1_000_000_000`  (guaranteed in `[0, 1e9)`)
///
/// A `tick_hz` of `0` is rejected with `Err(Errno::EINVAL)` rather than dividing by
/// zero, so the function is total for every input.
pub fn ticks_to_timespec(ticks: u64, clock_id: u32, tick_hz: u64) -> Result<Timespec, Errno> {
    match clock_id {
        CLOCK_REALTIME | CLOCK_MONOTONIC => {}
        _ => return Err(Errno::EINVAL),
    }

    if tick_hz == 0 {
        return Err(Errno::EINVAL);
    }

    let total_ns: u128 = (ticks as u128 * NANOS_PER_SEC as u128) / tick_hz as u128;
    let tv_sec = (total_ns / NANOS_PER_SEC as u128) as i64;
    let tv_nsec = (total_ns % NANOS_PER_SEC as u128) as i64;

    Ok(Timespec { tv_sec, tv_nsec })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn getrandom_accepts_when_fits() {
        assert_eq!(getrandom_plan(16, 0), Ok(0));
        assert_eq!(getrandom_plan(16, 16), Ok(16));
        assert_eq!(getrandom_plan(16, 8), Ok(8));
    }

    #[test]
    fn getrandom_rejects_overrun() {
        assert_eq!(getrandom_plan(16, 17), Err(Errno::EINVAL));
        assert_eq!(getrandom_plan(0, 1), Err(Errno::EINVAL));
    }

    #[test]
    fn timespec_rejects_unsupported_clock() {
        assert_eq!(ticks_to_timespec(100, 2, 1000), Err(Errno::EINVAL));
        assert_eq!(ticks_to_timespec(100, 99, 1000), Err(Errno::EINVAL));
    }

    #[test]
    fn timespec_rejects_zero_hz() {
        assert_eq!(ticks_to_timespec(100, CLOCK_MONOTONIC, 0), Err(Errno::EINVAL));
    }

    #[test]
    fn timespec_normalizes() {
        // 1500 ticks at 1000 Hz = 1.5 s = 1 s + 500_000_000 ns.
        let ts = ticks_to_timespec(1500, CLOCK_REALTIME, 1000).unwrap();
        assert_eq!(ts, Timespec { tv_sec: 1, tv_nsec: 500_000_000 });
        // tv_nsec is always in range.
        assert!(ts.tv_nsec >= 0 && ts.tv_nsec < NANOS_PER_SEC as i64);
    }

    #[test]
    fn timespec_total_ns_invariant() {
        let ts = ticks_to_timespec(7, CLOCK_MONOTONIC, 3).unwrap();
        let total = ts.tv_sec as i128 * NANOS_PER_SEC as i128 + ts.tv_nsec as i128;
        assert_eq!(total, (7u128 * NANOS_PER_SEC as u128 / 3) as i128);
    }
}
