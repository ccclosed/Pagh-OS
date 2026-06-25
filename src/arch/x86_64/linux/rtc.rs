//! CMOS real-time-clock reader (Feature: linux-binary-compat).
//!
//! Kernel-only effectful half of the wall-clock support: it performs the port I/O
//! against the CMOS RTC (index port `0x70`, data port `0x71`) and defers all
//! arithmetic — BCD decoding and the civil-date→Unix-seconds conversion — to the
//! pure, host-tested [`super::timeconv`] module. This split keeps the math
//! verifiable on the host while the (unavoidably effectful) device reads live here.
//!
//! ## Read protocol
//!
//! The RTC updates its registers roughly once per second; reading mid-update can
//! return inconsistent fields. We therefore:
//!   1. wait until the "update in progress" flag (Status Register A, bit 7) is
//!      clear,
//!   2. read all the time/date registers,
//!   3. read them a second time and only accept the values once two consecutive
//!      reads agree (the standard "read twice" guard against an update slipping
//!      between register reads).
//!
//! Status Register B (`0x0B`) tells us the data format: bit 2 (`DM`) set means the
//! registers are already binary; clear (the common default) means BCD. Bit 1
//! (`24/12`) set means 24-hour mode; clear means 12-hour, where the high bit of
//! the hours register flags PM.
#![allow(dead_code)]

use x86_64::instructions::port::Port;

use super::timeconv::{bcd_to_bin, civil_to_unix};

/// CMOS address/index port.
const CMOS_ADDR: u16 = 0x70;
/// CMOS data port.
const CMOS_DATA: u16 = 0x71;

/// RTC register indices.
const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;

/// Status Register A bit 7: an RTC update is in progress.
const STATUS_A_UPDATE_IN_PROGRESS: u8 = 0x80;
/// Status Register B bit 1: hours are in 24-hour format.
const STATUS_B_24_HOUR: u8 = 0x02;
/// Status Register B bit 2: registers are already binary (not BCD).
const STATUS_B_BINARY: u8 = 0x04;
/// High bit of the hours register flagging PM in 12-hour mode.
const HOUR_PM_FLAG: u8 = 0x80;

/// The raw register values read in a single consistent snapshot.
#[derive(Clone, Copy, PartialEq, Eq)]
struct RtcRaw {
    sec: u8,
    min: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
}

/// Read one CMOS register by index. Disabling NMI is not required here.
fn read_cmos(reg: u8) -> u8 {
    // SAFETY: CMOS index/data ports are fixed legacy I/O ports; selecting a
    // register then reading the data port has no memory effect.
    unsafe {
        let mut addr = Port::<u8>::new(CMOS_ADDR);
        let mut data = Port::<u8>::new(CMOS_DATA);
        addr.write(reg);
        data.read()
    }
}

/// Whether an RTC update is currently in progress.
fn update_in_progress() -> bool {
    read_cmos(REG_STATUS_A) & STATUS_A_UPDATE_IN_PROGRESS != 0
}

/// Take one raw register snapshot (no update-in-progress guarding).
fn read_raw() -> RtcRaw {
    RtcRaw {
        sec: read_cmos(REG_SECONDS),
        min: read_cmos(REG_MINUTES),
        hour: read_cmos(REG_HOURS),
        day: read_cmos(REG_DAY),
        month: read_cmos(REG_MONTH),
        year: read_cmos(REG_YEAR),
    }
}

/// A decoded RTC date/time in UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtcDate {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Read a consistent date/time from the CMOS RTC, decoding BCD/binary and
/// 12/24-hour format, returning a [`RtcDate`] in UTC. Uses the read-twice guard so
/// a register update slipping between reads is rejected and retried (bounded so a
/// pathological RTC cannot hang the boot).
pub fn read_date() -> RtcDate {
    // Wait out any in-progress update, with a generous bound.
    let mut guard = 0u32;
    while update_in_progress() && guard < 1_000_000 {
        guard += 1;
    }

    // Read twice until two consecutive snapshots agree.
    let mut prev = read_raw();
    let mut attempts = 0u32;
    loop {
        while update_in_progress() && attempts < 1_000_000 {
            attempts += 1;
        }
        let cur = read_raw();
        if cur == prev || attempts >= 1_000_000 {
            prev = cur;
            break;
        }
        prev = cur;
        attempts += 1;
    }

    let status_b = read_cmos(REG_STATUS_B);
    let is_binary = status_b & STATUS_B_BINARY != 0;
    let is_24h = status_b & STATUS_B_24_HOUR != 0;

    let decode = |v: u8| if is_binary { v } else { bcd_to_bin(v) };

    let sec = decode(prev.sec) as u32;
    let min = decode(prev.min) as u32;

    // Hours need special care: in 12-hour BCD mode the PM flag rides the high bit
    // of the raw register, so it must be stripped before BCD decoding and folded
    // back in afterwards.
    let raw_hour = prev.hour;
    let pm = !is_24h && (raw_hour & HOUR_PM_FLAG != 0);
    let hour_val = decode(raw_hour & !HOUR_PM_FLAG) as u32;
    let hour = if is_24h {
        hour_val
    } else {
        // 12-hour: 12AM -> 0, 12PM -> 12, otherwise add 12 for PM.
        match (hour_val % 12, pm) {
            (h, false) => h,         // 1..=11 AM, and 12AM -> 0
            (h, true) => h + 12,     // 1..=11 PM -> 13..=23, and 12PM -> 12
        }
    };

    let day = decode(prev.day) as u32;
    let month = decode(prev.month) as u32;
    // Two-digit year: assume the 2000s (this kernel targets the present era).
    let year = 2000 + decode(prev.year) as i64;

    RtcDate {
        year,
        month,
        day,
        hour,
        minute: min,
        second: sec,
    }
}

/// Read the current wall-clock time from the CMOS RTC as Unix seconds (seconds
/// since 1970-01-01T00:00:00Z), assuming the RTC keeps UTC.
pub fn now_unix() -> u64 {
    let d = read_date();
    let secs = civil_to_unix(d.year, d.month, d.day, d.hour, d.minute, d.second);
    if secs < 0 {
        0
    } else {
        secs as u64
    }
}
