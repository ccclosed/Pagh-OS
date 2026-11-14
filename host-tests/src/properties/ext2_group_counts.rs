// Round-3 audit F-r3-vfs-2 — host properties for the ext2 superblock
// group/inode-count rules (`src/fs/ext2/bounds.rs`).
//
// The crafted image `attack/r3/vfs/img/ext2-bad-inodes.img` sets
// `s_inodes_count = s_inodes_per_group = 0xFFFFFFFF` on a single-group 64 MiB
// volume. The 2.22.5 mount path used `s_inodes_per_group` as the bitmap scan
// span, so every boot scanned 2^32 bits (~28 s of CPU), computed a bogus
// `s_free_inodes_count = 0xFFFF8E5B` and wrote it back. The kernel now
// validates the counts against one bitmap block's capacity (4096 bytes = 32768
// bits) and the group count before reading a single bit, and the scan itself
// clamps its span as a second line of defence.
//
// These properties pin the three things the kernel relies on:
//   * the acceptance rule is EXACTLY "a group fits in one bitmap and the
//     declared inode count fits in the declared groups" (no accidental
//     truncation, no impossible acceptance);
//   * an accepted superblock's group scans cover exactly `s_inodes_count`
//     entries in total, so free-inode accounting is not silently short;
//   * the scan span can never exceed one bitmap block, whatever garbage
//     reaches `clamp_group_span`.

use crate::ext2_bounds::{
    clamp_group_span, validate_group_counts, GroupCountError, BITMAP_BITS,
};
use proptest::prelude::*;

/// Independent restatement of the acceptance rule, so the property compares the
/// implementation against the specification rather than against itself.
fn spec_accepts(blocks_per_group: u32, inodes_per_group: u32, inodes_count: u32, groups: u32) -> bool {
    blocks_per_group != 0
        && inodes_per_group != 0
        && blocks_per_group <= BITMAP_BITS
        && inodes_per_group <= BITMAP_BITS
        && inodes_count != 0
        && (inodes_count as u64) <= (inodes_per_group as u64) * (groups as u64)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// Acceptance is exactly the structural rule; every rejection names the
    /// first broken clause.
    #[test]
    fn acceptance_is_the_structural_rule(
        blocks_per_group in prop::sample::select(vec![0u32, 1, 2, 8, 4096, 32767, 32768, 32769, 65536, 0xFFFFFFFF]),
        inodes_per_group in prop::sample::select(vec![0u32, 1, 2, 4096, 32767, 32768, 32769, 65536, 0xFFFFFFFF]),
        inodes_count in any::<u32>(),
        groups in 0u32..=4096,
    ) {
        let got = validate_group_counts(blocks_per_group, inodes_per_group, inodes_count, groups);
        let want = spec_accepts(blocks_per_group, inodes_per_group, inodes_count, groups);
        prop_assert_eq!(got.is_ok(), want);
        match (got, want) {
            (Ok(()), true) => {}
            (Err(GroupCountError::ZeroBlocksPerGroup), false) => {
                prop_assert_eq!(blocks_per_group, 0)
            }
            (Err(GroupCountError::ZeroInodesPerGroup), false) => {
                prop_assert_eq!(inodes_per_group, 0)
            }
            (Err(GroupCountError::BlocksPerGroupExceedsBitmap), false) => {
                prop_assert!(blocks_per_group > BITMAP_BITS)
            }
            (Err(GroupCountError::InodesPerGroupExceedsBitmap), false) => {
                prop_assert!(inodes_per_group > BITMAP_BITS)
            }
            (Err(GroupCountError::ZeroInodeCount), false) => {
                prop_assert_eq!(inodes_count, 0)
            }
            (Err(GroupCountError::InodeCountExceedsGroups), false) => {
                prop_assert!((inodes_count as u64) > (inodes_per_group as u64) * (groups as u64))
            }
            other => prop_assert!(false, "unexpected {:?}", other),
        }
    }

    /// For an accepted superblock the per-group scans add up to exactly
    /// `s_inodes_count`: the reconciler counts every declared inode once, so
    /// the rewritten free-inode total is trustworthy.
    #[test]
    fn accepted_scans_cover_the_declared_count(
        inodes_per_group in 1u32..=BITMAP_BITS,
        extra_groups in 0u32..=64,
        remainder in 0u32..=BITMAP_BITS,
    ) {
        let groups = extra_groups + 1;
        let inodes_count = (inodes_per_group as u64 * groups as u64
            - (inodes_per_group as u64 - remainder.min(inodes_per_group) as u64)) as u32;
        prop_assume!(inodes_count != 0);
        prop_assert_eq!(
            validate_group_counts(BITMAP_BITS, inodes_per_group, inodes_count, groups),
            Ok(())
        );
        let covered: u64 = (0..groups)
            .map(|g| clamp_group_span(inodes_count, inodes_per_group, g) as u64)
            .sum();
        prop_assert_eq!(covered, inodes_count as u64);
    }

    /// The scan span never exceeds one bitmap block, even for garbage counts
    /// that bypass mount validation — this is what bounds the historical 2^32
    /// scan.
    #[test]
    fn scan_span_never_exceeds_the_bitmap(
        total in any::<u32>(),
        per_group in any::<u32>(),
        index in any::<u32>(),
    ) {
        let span = clamp_group_span(total, per_group, index);
        prop_assert!(span <= BITMAP_BITS, "span {} > bitmap {}", span, BITMAP_BITS);
        prop_assert!(span <= per_group);
    }
}

/// The exact crafted superblock from `ext2-bad-inodes.img`: refused as corrupt
/// before any bitmap scan, and even the defensive clamp caps the span at one
/// bitmap block (not the 2^32 the 2.22.5 mount scanned).
#[test]
fn crafted_bad_inodes_image_is_refused_and_bounded() {
    const INODES: u32 = 0xFFFF_FFFF;
    // blocks_count = 0x3FBF (16319), blocks_per_group = 0x8000 -> one group.
    assert_eq!(
        validate_group_counts(0x8000, INODES, INODES, 1),
        Err(GroupCountError::InodesPerGroupExceedsBitmap)
    );
    assert_eq!(clamp_group_span(INODES, INODES, 0), BITMAP_BITS);
    // The real 64 MiB / 16384-block image shape (the one the audit booted).
    assert_eq!(
        validate_group_counts(0x8000, INODES, 0x7FF, 1),
        Err(GroupCountError::InodesPerGroupExceedsBitmap)
    );
}

/// A normal single-group volume shaped like this kernel's own `format` output
/// is accepted, and its groups describe its inodes exactly.
#[test]
fn kernel_format_shape_is_accepted() {
    // format(): 16319 blocks, one group, 22 inodes (see ext2/mod.rs).
    assert_eq!(validate_group_counts(0x8000, 0x1000, 22, 1), Ok(()));
    assert_eq!(validate_group_counts(4096, 1024, 1024, 1), Ok(()));
    // A short last group is legal (total < per_group * groups).
    assert_eq!(validate_group_counts(4096, 1024, 1500, 2), Ok(()));
    // One inode more than the groups can hold is not.
    assert_eq!(
        validate_group_counts(4096, 1024, 2049, 2),
        Err(GroupCountError::InodeCountExceedsGroups)
    );
    assert_eq!(clamp_group_span(1500, 1024, 0), 1024);
    assert_eq!(clamp_group_span(1500, 1024, 1), 476);
}