//! Pure time conversions for the Linux compatibility layer
//! (Feature: linux-binary-compat).
//!
//! Pure, allocation-free, `core`-only logic shared with the `host-tests` crate
//! (R11.6). It holds the byte/field decoding and calendar math that back the
//! real-wall-clock syscalls (`gettimeofday`/`time`/`CLOCK_REALTIME`) and the CMOS
//! RTC reader — keeping the actual port I/O effectful in `rtc.rs` while every
//! arithmetic step here is host-testable against known dates:
//!
//!   * [`bcd_to_bin`] — CMOS BCD byte → binary (the RTC reports BCD by default).
//!   * [`days_from_civil`] — Howard Hinnant's proleptic-Gregorian day count
//!     (days since 1970-01-01), valid for any civil date.
//!   * [`civil_to_unix`] — a full `(Y,M,D,h,m,s)` breakdown → Unix seconds.
//!   * [`encode_timeval`] — build the x86_64 `struct timeval`.
#![allow(dead_code)]

/// Seconds in one day.
const SECS_PER_DAY: i64 = 86_400;

/// Linux `struct timeval` as laid out by the x86_64 ABI: two signed 64-bit
/// fields. `gettimeofday` writes one of these to the user buffer.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeval {
    /// Whole seconds since the Unix epoch.
    pub tv_sec: i64,
    /// Sub-second remainder in microseconds, normally in `[0, 1_000_000)`.
    pub tv_usec: i64,
}

/// Decode a CMOS BCD byte (e.g. `0x59`) into its binary value (`59`).
///
/// The RTC stores each two-digit field as packed binary-coded decimal: the high
/// nibble is the tens digit, the low nibble the units. `((v >> 4) * 10) + (v & 0x0F)`.
#[inline]
pub fn bcd_to_bin(v: u8) -> u8 {
    ((v >> 4) * 10) + (v & 0x0F)
}

/// Days since the Unix epoch (1970-01-01) for the proleptic-Gregorian civil date
/// `(y, m, d)`, where `m ∈ [1, 12]` and `d ∈ [1, 31]` (Howard Hinnant's algorithm).
///
/// Returns a signed count so dates before 1970 yield negatives. The algorithm
/// shifts the year so March is the first month (making the leap day the last day
/// of the year), then composes the day-of-era and era to land exactly on the days
/// elapsed since the epoch.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64; // [0, 399]
    let m = m as i64;
    let d = d as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Convert a full civil `(year, month, day, hour, minute, second)` breakdown into
/// Unix seconds (seconds since 1970-01-01T00:00:00Z), assuming UTC.
///
/// Pure calendar math built on [`days_from_civil`]; total for every input.
pub fn civil_to_unix(year: i64, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> i64 {
    let days = days_from_civil(year, month, day);
    days * SECS_PER_DAY + (hour as i64) * 3600 + (minute as i64) * 60 + (second as i64)
}

/// Build a [`Timeval`] from whole `secs` and a sub-second `usecs` remainder.
#[inline]
pub fn encode_timeval(secs: i64, usecs: i64) -> Timeval {
    Timeval {
        tv_sec: secs,
        tv_usec: usecs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcd_round_trips_small_values() {
        assert_eq!(bcd_to_bin(0x00), 0);
        assert_eq!(bcd_to_bin(0x09), 9);
        assert_eq!(bcd_to_bin(0x10), 10);
        assert_eq!(bcd_to_bin(0x59), 59);
        assert_eq!(bcd_to_bin(0x23), 23);
    }

    #[test]
    fn epoch_is_zero() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_to_unix(1970, 1, 1, 0, 0, 0), 0);
    }

    #[test]
    fn known_dates() {
        // 2000-01-01 is 30 years after the epoch: 10957 days.
        assert_eq!(days_from_civil(2000, 1, 1), 10957);
        assert_eq!(civil_to_unix(2000, 1, 1, 0, 0, 0), 946_684_800);
        // 2024-02-29 leap day.
        assert_eq!(civil_to_unix(2024, 2, 29, 0, 0, 0), 1_709_164_800);
    }
}
