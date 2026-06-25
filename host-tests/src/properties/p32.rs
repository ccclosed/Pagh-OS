// Feature: linux-binary-compat, Property 32: the pure encoders/decoders behind the
// new "no new process model" syscall surface —
//   * getdents64 record packing (struct linux_dirent64): sizes, 8-byte alignment,
//     name NUL-termination, and field round-trip;
//   * timeval encoding;
//   * civil-date -> Unix-seconds conversion (and BCD decode) against known dates.
//
// The packing/conversion logic is pure (`core` + `alloc`), so it is exercised here
// on the host against the SAME source the kernel compiles (R11.6).

use crate::dirent::{
    dirent_reclen, encode_dirent64, record_reclen, DIRENT_HEADER, DT_DIR, DT_REG,
};
use crate::timeconv::{bcd_to_bin, civil_to_unix, days_from_civil, encode_timeval};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// dirent64 packing — fixed examples
// ---------------------------------------------------------------------------

#[test]
fn dirent_header_is_19_bytes() {
    // d_ino(8) + d_off(8) + d_reclen(2) + d_type(1) = 19.
    assert_eq!(DIRENT_HEADER, 19);
}

#[test]
fn reclen_is_8_byte_aligned_and_holds_name_plus_nul() {
    for name_len in 0..64usize {
        let reclen = dirent_reclen(name_len);
        // 8-byte aligned.
        assert_eq!(reclen % 8, 0, "reclen for name_len={name_len} not aligned");
        // Holds the header, the whole name, and at least the NUL terminator.
        assert!(reclen >= DIRENT_HEADER + name_len + 1);
        // Minimal: subtracting 8 would no longer fit.
        assert!(reclen - 8 < DIRENT_HEADER + name_len + 1);
    }
}

#[test]
fn encode_known_record() {
    let rec = encode_dirent64(0x1122_3344_5566_7788, 0x0A0B_0C0D, DT_DIR, b"etc");
    // Length matches the planner.
    assert_eq!(rec.len(), dirent_reclen(3));
    // d_ino little-endian.
    assert_eq!(&rec[0..8], &0x1122_3344_5566_7788u64.to_le_bytes());
    // d_off little-endian.
    assert_eq!(&rec[8..16], &0x0A0B_0C0Di64.to_le_bytes());
    // d_reclen field equals the record length.
    assert_eq!(record_reclen(&rec) as usize, rec.len());
    // d_type.
    assert_eq!(rec[18], DT_DIR);
    // Name then NUL.
    assert_eq!(&rec[19..22], b"etc");
    assert_eq!(rec[22], 0);
    // Trailing padding up to reclen is zero.
    for &b in &rec[23..] {
        assert_eq!(b, 0);
    }
}

#[test]
fn empty_name_still_nul_terminated() {
    let rec = encode_dirent64(1, 1, DT_REG, b"");
    assert_eq!(record_reclen(&rec) as usize, rec.len());
    // The first name byte is the NUL terminator.
    assert_eq!(rec[DIRENT_HEADER], 0);
}

// ---------------------------------------------------------------------------
// timeval encoding
// ---------------------------------------------------------------------------

#[test]
fn timeval_fields_round_trip() {
    let tv = encode_timeval(1_700_000_000, 123_456);
    assert_eq!(tv.tv_sec, 1_700_000_000);
    assert_eq!(tv.tv_usec, 123_456);
    // Layout matches the x86_64 ABI: two contiguous i64.
    assert_eq!(core::mem::size_of_val(&tv), 16);
}

// ---------------------------------------------------------------------------
// civil-date -> Unix-seconds (and BCD decode) — known dates
// ---------------------------------------------------------------------------

#[test]
fn known_unix_seconds() {
    // 1970-01-01T00:00:00Z -> 0.
    assert_eq!(civil_to_unix(1970, 1, 1, 0, 0, 0), 0);
    assert_eq!(days_from_civil(1970, 1, 1), 0);

    // 2000-01-01T00:00:00Z -> 946684800.
    assert_eq!(civil_to_unix(2000, 1, 1, 0, 0, 0), 946_684_800);

    // 2024-02-29 (leap day) T00:00:00Z -> 1709164800.
    assert_eq!(civil_to_unix(2024, 2, 29, 0, 0, 0), 1_709_164_800);

    // A time-of-day component adds correctly: 2000-01-01T01:02:03.
    assert_eq!(
        civil_to_unix(2000, 1, 1, 1, 2, 3),
        946_684_800 + 3600 + 120 + 3
    );
}

#[test]
fn bcd_decodes_clock_fields() {
    assert_eq!(bcd_to_bin(0x00), 0);
    assert_eq!(bcd_to_bin(0x12), 12);
    assert_eq!(bcd_to_bin(0x23), 23); // 23:00 hours
    assert_eq!(bcd_to_bin(0x59), 59); // :59 minutes/seconds
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any valid two-digit BCD byte, decoding yields the expected 0..=99 value.
    #[test]
    fn bcd_matches_two_digit_decode(tens in 0u8..10, units in 0u8..10) {
        let byte = (tens << 4) | units;
        prop_assert_eq!(bcd_to_bin(byte), tens * 10 + units);
    }

    /// A packed dirent record always: has an 8-aligned length equal to its own
    /// d_reclen field, contains the verbatim name followed by a NUL, and zeroes any
    /// trailing pad. Field bytes round-trip.
    #[test]
    fn dirent_packing_is_consistent(
        ino in any::<u64>(),
        off in any::<i64>(),
        is_dir in any::<bool>(),
        name in prop::collection::vec(1u8..=255u8, 0..40),
    ) {
        let d_type = if is_dir { DT_DIR } else { DT_REG };
        let rec = encode_dirent64(ino, off, d_type, &name);

        prop_assert_eq!(rec.len() % 8, 0);
        prop_assert_eq!(record_reclen(&rec) as usize, rec.len());
        prop_assert_eq!(&rec[0..8], &ino.to_le_bytes());
        prop_assert_eq!(&rec[8..16], &off.to_le_bytes());
        prop_assert_eq!(rec[18], d_type);
        prop_assert_eq!(&rec[DIRENT_HEADER..DIRENT_HEADER + name.len()], &name[..]);
        // NUL terminator immediately follows the name (names contain no 0 byte).
        prop_assert_eq!(rec[DIRENT_HEADER + name.len()], 0u8);
    }

    /// days_from_civil is strictly monotonic across consecutive days, and a full
    /// day adds exactly 86400 seconds.
    #[test]
    fn civil_seconds_advance_by_a_day(
        year in 1971i64..2100,
        month in 1u32..=12,
        day in 1u32..=28,
    ) {
        let d0 = civil_to_unix(year, month, day, 0, 0, 0);
        // The next day (day in 1..=28 keeps month/day valid).
        let d1 = civil_to_unix(year, month, day + 1, 0, 0, 0);
        prop_assert_eq!(d1 - d0, 86_400);
        // days_from_civil agrees with the seconds/day relationship.
        prop_assert_eq!(
            days_from_civil(year, month, day + 1) - days_from_civil(year, month, day),
            1
        );
    }
}
