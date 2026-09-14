// Feature: boot-time device safety (issue #33), Property 50: pagh formats a
// block device ONLY when the device is genuinely blank.
//
// `boot::init_fs` used to format whenever `Ext2Fs::mount` failed and the ext2
// magic was missing at byte 1024 of the RAW namespace. On a real machine that is
// the machine's own disk (the NVMe path takes the first active namespace, not a
// partition) and byte 1024 is the GPT partition-entry array — so a partitioned,
// non-ext2 disk was classified "blank" and erased: protective MBR + GPT header,
// per-group metadata across the whole device, and the backup GPT at the tail.
//
// No test in CI ever boots the kernel, and every QEMU path attaches a freshly
// created (blank) `disk.img`, where formatting is the INTENDED behaviour — which
// is exactly why the destructive branch looked correct for so long. The decision
// is therefore exercised here, over the same `src/fs/format_policy.rs` the kernel
// compiles, against the boot areas that actually occur in the field:
//
//   * a GPT disk (protective MBR at LBA 0 + `EFI PART` at LBA 1);
//   * an MBR disk (boot signature at byte 510, partition entries above 446);
//   * an ext2/ext4 superblock (magic `0xEF53` at byte 1024) — mountable or not;
//   * arbitrary non-zero data anywhere in the probed window;
//   * a genuinely blank device, one of the only two cases that may be formatted;
//   * the deliberate `PAGH-FORMAT` opt-in marker in sector 0 — the other one.
//
// The properties below are the anti-regression net for the bug itself: no probe
// carrying a single non-zero byte outside a valid superblock may ever come back
// `Blank`, and `format_allowed` must refuse every layout except `Blank` unless the
// caller passes the explicit `allow_destructive` opt-in, which only the marker sets.

use crate::format_policy::{
    format_allowed, Layout, Probe, Refusal, EXT2_MAGIC_LE, GPT_SIGNATURE, MBR_SIGNATURE, MIN_PROBE,
    OPT_IN_MARKER, SUPERBLOCK_OFFSET,
};
use proptest::prelude::*;

/// Probe chunk size used by `boot::probe_boot_area` (one ext2 filesystem block).
const CHUNK: usize = 4096;

/// A realistic GPT disk start: protective MBR (single 0xEE entry + boot
/// signature) followed by the GPT header signature at LBA 1.
fn gpt_boot_area() -> Vec<u8> {
    let mut b = vec![0u8; CHUNK];
    b[446] = 0x00; // status: not bootable
    b[450] = 0xEE; // partition type: GPT protective
    b[454] = 0x01; // first LBA = 1
    b[458] = 0xFF; // last LBA (0xFFFFFFFF, "rest of the disk")
    b[459] = 0xFF;
    b[460] = 0xFF;
    b[461] = 0xFF;
    b[510] = 0x55;
    b[511] = 0xAA;
    b[512..520].copy_from_slice(GPT_SIGNATURE);
    b
}

/// An MBR-only disk start: boot signature present, one non-empty partition entry.
fn mbr_boot_area() -> Vec<u8> {
    let mut b = vec![0u8; CHUNK];
    b[446] = 0x80; // status: bootable
    b[450] = 0x83; // type: Linux
    b[455] = 0x08; // first LBA 2048 (little-endian)
    b[510] = 0x55;
    b[511] = 0xAA;
    b
}

/// An ext2/ext4 superblock magic at the offset `read_sb_gds` looks at.
fn ext2_boot_area() -> Vec<u8> {
    let mut b = vec![0u8; CHUNK];
    b[SUPERBLOCK_OFFSET..MIN_PROBE].copy_from_slice(&EXT2_MAGIC_LE);
    b
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// The bug's exact shape: one non-zero byte anywhere in the probed window is
    /// enough to make the device `Foreign` — never `Blank`, and therefore never
    /// eligible for formatting.
    #[test]
    fn any_non_zero_byte_prevents_a_blank_verdict(
        chunks in 1..8usize,
        pos in any::<prop::sample::Index>(),
    ) {
        let mut image = vec![vec![0u8; CHUNK]; chunks];
        let at = pos.index(chunks * CHUNK);
        image[at / CHUNK][at % CHUNK] = 1;

        let mut probe = Probe::new(&image[0]);
        for chunk in &image[1..] {
            probe.feed(chunk);
        }
        prop_assert!(probe.layout() != Layout::Blank);
        prop_assert!(format_allowed(probe.layout(), false, false).is_err());
    }

    /// A partition table is recognised whatever else the block contains, and is
    /// refused with the partition-table reason — GPT (protective MBR + header)
    /// and MBR-only alike.
    #[test]
    fn partition_tables_are_recognised_and_refused(
        gpt in any::<bool>(),
        filler in prop::collection::vec(any::<u8>(), 0..CHUNK),
    ) {
        let mut b = vec![0u8; CHUNK];
        for (i, v) in filler.iter().enumerate() {
            b[i] = *v;
        }
        // Signatures are written after the filler and the ext2 magic cleared, so
        // the partition table is the only recognised structure in the block.
        b[510] = 0x55;
        b[511] = 0xAA;
        if gpt {
            b[512..520].copy_from_slice(GPT_SIGNATURE);
        }
        b[SUPERBLOCK_OFFSET] = 0;
        b[SUPERBLOCK_OFFSET + 1] = 0;

        let probe = Probe::new(&b);
        prop_assert_eq!(probe.layout(), Layout::PartitionTable);
        prop_assert_eq!(
            format_allowed(probe.layout(), false, false),
            Err(Refusal::PartitionTable)
        );
    }

    /// A parsable ext2 superblock is never formatted, whatever the layout probe
    /// says and whatever the opt-in says.
    #[test]
    fn a_mountable_superblock_is_never_formatted(
        blank in any::<bool>(),
        partitioned in any::<bool>(),
        allow_destructive in any::<bool>(),
    ) {
        let layout = if partitioned {
            Layout::PartitionTable
        } else if blank {
            Layout::Blank
        } else {
            Layout::Foreign
        };
        prop_assert_eq!(
            format_allowed(layout, true, allow_destructive),
            Err(Refusal::ExistingFilesystem)
        );
    }

    /// Only `Blank` is formatted without the explicit opt-in.
    #[test]
    fn only_blank_is_formatted_without_opt_in(
        layout in prop::sample::select(vec![
            Layout::Ext2,
            Layout::PartitionTable,
            Layout::Foreign,
        ]),
    ) {
        prop_assert!(format_allowed(layout, false, false).is_err());
        prop_assert_eq!(format_allowed(layout, false, true), Ok(()));
    }

    /// A blank device stays blank only while every probed chunk is zero; the
    /// first non-zero chunk decides `Foreign` for good (later zeros cannot undo it).
    #[test]
    fn blankness_is_monotone(before in 0..6usize, after in 0..6usize) {
        let zero = vec![0u8; CHUNK];
        let mut probe = Probe::new(&zero);
        for _ in 0..before {
            prop_assert!(probe.layout() == Layout::Blank);
            probe.feed(&zero);
        }
        let mut dirty = zero.clone();
        dirty[CHUNK - 1] = 0x7F;
        probe.feed(&dirty);
        prop_assert_eq!(probe.layout(), Layout::Foreign);
        prop_assert!(probe.is_decided());
        for _ in 0..after {
            probe.feed(&zero);
            prop_assert_eq!(probe.layout(), Layout::Foreign);
        }
    }
}

#[test]
fn gpt_disk_is_never_formatted() {
    let probe = Probe::new(&gpt_boot_area());
    assert_eq!(probe.layout(), Layout::PartitionTable);
    assert!(
        probe.is_decided(),
        "no need to scan further: the layout is known"
    );
    assert_eq!(
        format_allowed(probe.layout(), false, false),
        Err(Refusal::PartitionTable)
    );
}

#[test]
fn mbr_disk_is_never_formatted() {
    let probe = Probe::new(&mbr_boot_area());
    assert_eq!(probe.layout(), Layout::PartitionTable);
    assert_eq!(
        format_allowed(probe.layout(), false, false),
        Err(Refusal::PartitionTable)
    );
}

#[test]
fn ext2_magic_without_a_mountable_superblock_is_refused() {
    let probe = Probe::new(&ext2_boot_area());
    assert_eq!(probe.layout(), Layout::Ext2);
    assert_eq!(
        format_allowed(probe.layout(), false, false),
        Err(Refusal::Ext2Like)
    );
}

#[test]
fn a_trimmed_probe_fails_closed() {
    // Fewer bytes than the superblock offset: cannot classify, must not format.
    let probe = Probe::new(&[0u8; SUPERBLOCK_OFFSET]);
    assert_eq!(probe.layout(), Layout::Foreign);
    assert!(format_allowed(probe.layout(), false, false).is_err());
    assert_eq!(Probe::unreadable().layout(), Layout::Foreign);
}

#[test]
fn a_truly_blank_device_is_formatted() {
    // The dev/QEMU path: a fresh `qemu-img create` disk image must keep working.
    // 256 chunks is exactly the 1 MiB window `boot::probe_boot_area` scans.
    let mut probe = Probe::new(&vec![0u8; CHUNK]);
    for _ in 0..255 {
        probe.feed(&vec![0u8; CHUNK]);
    }
    assert_eq!(probe.layout(), Layout::Blank);
    assert_eq!(format_allowed(probe.layout(), false, false), Ok(()));
}

#[test]
fn the_opt_in_marker_is_the_only_way_past_a_partition_table() {
    // A real GPT disk plus the operator's deliberate marker: the marker is what
    // authorises the erase, and it is found only in sector 0.
    let mut image = gpt_boot_area();
    assert!(!Probe::new(&image).is_marked());
    assert!(format_allowed(Probe::new(&image).layout(), false, false).is_err());

    image[..OPT_IN_MARKER.len()].copy_from_slice(OPT_IN_MARKER);
    let probe = Probe::new(&image);
    assert!(probe.is_marked());
    assert_eq!(probe.layout(), Layout::PartitionTable, "still reported as GPT");
    assert_eq!(
        format_allowed(probe.layout(), false, probe.is_marked()),
        Ok(())
    );

    // The marker must not be able to authorise erasing a real ext2 filesystem.
    let mut ext2 = ext2_boot_area();
    ext2[..OPT_IN_MARKER.len()].copy_from_slice(OPT_IN_MARKER);
    let probe = Probe::new(&ext2);
    assert!(probe.is_marked());
    assert_eq!(
        format_allowed(probe.layout(), true, probe.is_marked()),
        Err(Refusal::ExistingFilesystem)
    );
}
