// Feature: crafted ext2 size fields — host properties for the pure bounds
// rules the kernel applies to attacker-controlled on-disk `i_size` values
// (`src/fs/ext2/bounds.rs`, security batch after 2.22.0):
//
//   * F-x86v-5 — a **directory** `i_size` used to drive the number of blocks a
//     `lookup`/`readdir`/`unlink` scan would visit, so a crafted
//     `i_size = 0xFFFFF000` on a 64 MiB image made one syscall issue up to
//     ~1M block reads (kernel stall). A directory is block-aligned and cannot
//     be larger than its filesystem, so both violations are rejected as
//     `Corrupt` before the loop starts;
//   * the same entry point is where the **symlink** bound lives for the
//     `read_symlink` path (F-x86v-1), so the tests below also pin the exact
//     crafted inode from the audit: `i_blocks = 8, i_size = 0xFFFFFFF0`.
//
// The regression cases use the audit's literal crafted values; the randomized
// properties prove the acceptance rule is exactly
// `i_size % 4096 == 0 && i_size / 4096 <= fs_blocks`, so a malformed image can
// never be silently truncated into a shorter scan (that would hide entries)
// nor accepted whole (that would stall the kernel).

use crate::ext2_bounds::{dir_blocks, DirSizeError, BLOCK_SIZE};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// The size of a directory in blocks is either refused or exactly the
    /// claimed size, never larger than the filesystem, never a partial block.
    #[test]
    fn directory_blocks_are_exact_and_bounded(
        i_size in any::<u32>(),
        fs_blocks in 1u64..=1 << 20,
    ) {
        match dir_blocks(i_size, fs_blocks) {
            Ok(blocks) => {
                prop_assert_eq!(i_size as u64 % BLOCK_SIZE as u64, 0);
                prop_assert_eq!(blocks * BLOCK_SIZE as u64, i_size as u64);
                prop_assert!(blocks <= fs_blocks);
            }
            Err(DirSizeError::NotBlockAligned) => {
                prop_assert_ne!(i_size as u64 % BLOCK_SIZE as u64, 0);
            }
            Err(DirSizeError::LargerThanFilesystem) => {
                prop_assert_eq!(i_size as u64 % BLOCK_SIZE as u64, 0);
                prop_assert!(i_size as u64 / BLOCK_SIZE as u64 > fs_blocks);
            }
        }
    }

    /// Adding more blocks to the filesystem can only turn a refusal into an
    /// acceptance, never the reverse: the bound is monotone in `fs_blocks`.
    #[test]
    fn filesystem_bound_is_monotone(
        i_size in any::<u32>(),
        fs_blocks in 0u64..=1 << 20,
        extra in 0u64..=1 << 20,
    ) {
        let more = fs_blocks.saturating_add(extra);
        match dir_blocks(i_size, fs_blocks) {
            Ok(blocks) => prop_assert_eq!(dir_blocks(i_size, more), Ok(blocks)),
            Err(DirSizeError::NotBlockAligned) => {
                prop_assert_eq!(dir_blocks(i_size, more), Err(DirSizeError::NotBlockAligned));
            }
            Err(DirSizeError::LargerThanFilesystem) => {
                // Still refused only while the claim is above the new bound.
                match dir_blocks(i_size, more) {
                    Ok(blocks) => {
                        prop_assert!(blocks > fs_blocks);
                        prop_assert!(blocks <= more);
                    }
                    Err(DirSizeError::LargerThanFilesystem) => {}
                    Err(DirSizeError::NotBlockAligned) => prop_assert!(false),
                }
            }
        }
    }
}

/// The exact crafted inode from the audit (F-x86v-5): `i_size = 0xFFFFF000`
/// (~4 GiB) on the 64 MiB / 16384-block bench image. It must be refused before
/// any block loop, in both the "block aligned" and the "off by one" variants.
#[test]
fn crafted_directory_size_from_the_audit_is_refused() {
    let fs_blocks = 16_384u64; // the 64 MiB attack image
    assert_eq!(
        dir_blocks(0xFFFF_F000, fs_blocks),
        Err(DirSizeError::LargerThanFilesystem)
    );
    assert_eq!(
        dir_blocks(0xFFFF_F001, fs_blocks),
        Err(DirSizeError::NotBlockAligned)
    );
    assert_eq!(
        dir_blocks(0xFFFF_FFFF, fs_blocks),
        Err(DirSizeError::NotBlockAligned)
    );
    // The audit's ~1M-block read count would have been ceil(0xFFFFF000 / BS).
    assert_eq!(0xFFFF_F000u64 / BLOCK_SIZE as u64, 1_048_575);
}

/// A directory can use the whole filesystem but not one block more — the
/// boundary that keeps the bound useful on a full volume.
#[test]
fn valid_directories_at_the_boundary_are_accepted() {
    let fs_blocks = 16_384u64;
    assert_eq!(dir_blocks(0, fs_blocks), Ok(0));
    assert_eq!(dir_blocks(BLOCK_SIZE as u32, fs_blocks), Ok(1));
    let max = (fs_blocks * BLOCK_SIZE as u64) as u32;
    assert_eq!(dir_blocks(max, fs_blocks), Ok(fs_blocks));
    assert_eq!(
        dir_blocks(max.saturating_add(BLOCK_SIZE as u32), fs_blocks),
        Err(DirSizeError::LargerThanFilesystem)
    );
}