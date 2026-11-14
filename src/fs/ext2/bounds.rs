//! Pure bounds checks for attacker-controlled ext2 on-disk size fields.
//!
//! Every `i_size` an ext2 image supplies is untrusted input: a mounted volume
//! may come from an archive, a `.deb`, a USB stick or an attacker, and the
//! kernel must neither allocate nor iterate on a size it never validated. Two
//! sites needed that bound in the VFS audit (F-x86v-1/F-x86v-5):
//!
//!   * a symlink target is at most one block (`symlink::SLOW_MAX_TARGET`,
//!     classified in [`super::symlink::target_shape`]);
//!   * a **directory** occupies whole filesystem blocks and can never be
//!     larger than the filesystem that holds it, so deriving its block count
//!     from a crafted `i_size = 0xFFFFF000` used to make one `lookup`/`readdir`
//!     issue up to ~1M block reads (a kernel-side stall, F-x86v-5).
//!
//! The group/superblock counts are the same class of untrusted input and got
//! the same treatment in round 3 (F-r3-vfs-2): `s_inodes_per_group` is the
//! span `reconcile_free_counts` scans bit by bit, so a crafted
//! `s_inodes_count = s_inodes_per_group = 0xFFFFFFFF` turned every mount into
//! a 2^32-bit scan (28 s of boot CPU) and wrote a bogus free-inode total back
//! to the superblock. One bitmap block describes at most `BLOCK_SIZE * 8`
//! entries, so [`validate_group_counts`] rejects the impossible shapes before
//! any scan starts; [`clamp_group_span`] is the belt-and-braces bound the scan
//! itself applies.
//!
//! This module is `core`-only, has no dependencies and duplicates the 4 KiB
//! block size instead of naming `structs::BS`, exactly like `symlink.rs`, so
//! `host-tests` includes the *same* source with `#[path]` and property-tests
//! the rules the kernel actually runs (R11.6).
#![allow(dead_code)]

/// Size of one ext2 filesystem block in this implementation (mirrors
/// `structs::BS`; duplicated to keep this file self-contained for the host
/// include, and pinned by the host property below).
pub const BLOCK_SIZE: usize = 4096;

/// Bits described by one ext2 bitmap block (one bit per block or per inode).
/// A group cannot be larger than its bitmap, so this is a hard structural
/// limit, not a policy cap.
pub const BITMAP_BITS: u32 = (BLOCK_SIZE as u32) * 8;

/// Why a superblock's group/inode counts cannot describe a real filesystem.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum GroupCountError {
    /// A group with no blocks is not mountable (`s_blocks_per_group == 0`).
    ZeroBlocksPerGroup,
    /// A group with no inode slots cannot hold even the root inode.
    ZeroInodesPerGroup,
    /// `s_blocks_per_group` exceeds what one block bitmap can describe.
    BlocksPerGroupExceedsBitmap,
    /// `s_inodes_per_group` exceeds what one inode bitmap can describe.
    InodesPerGroupExceedsBitmap,
    /// A filesystem with zero inodes cannot contain the root inode (2).
    ZeroInodeCount,
    /// `s_inodes_count > s_inodes_per_group * groups`: the inode table is
    /// shorter than the count claims, so the extra ids address nothing.
    InodeCountExceedsGroups,
}

/// Validate the superblock's per-group counts against the structural capacity
/// of the bitmaps and the number of block groups.
///
/// `groups` is the group count derived from `s_blocks_count`, so the caller
/// has already bounded it (the ext2 driver refuses more than 65536 groups).
/// Returns `Ok(())` for every shape real `mke2fs`/this driver's `format`
/// produces, including a short last group (`s_inodes_count` may be less than
/// `s_inodes_per_group * groups`, never more).
pub fn validate_group_counts(
    blocks_per_group: u32,
    inodes_per_group: u32,
    inodes_count: u32,
    groups: u32,
) -> Result<(), GroupCountError> {
    if blocks_per_group == 0 {
        return Err(GroupCountError::ZeroBlocksPerGroup);
    }
    if inodes_per_group == 0 {
        return Err(GroupCountError::ZeroInodesPerGroup);
    }
    if blocks_per_group > BITMAP_BITS {
        return Err(GroupCountError::BlocksPerGroupExceedsBitmap);
    }
    if inodes_per_group > BITMAP_BITS {
        return Err(GroupCountError::InodesPerGroupExceedsBitmap);
    }
    if inodes_count == 0 {
        return Err(GroupCountError::ZeroInodeCount);
    }
    let capacity = (inodes_per_group as u64) * (groups as u64);
    if (inodes_count as u64) > capacity {
        return Err(GroupCountError::InodeCountExceedsGroups);
    }
    Ok(())
}

/// Bound the `count_set_bits` span of one group for `total` entries at
/// `per_group` per group, page `index` (0-based).
///
/// Clamps to both the remaining entries in the last group and the bitmap
/// block's own capacity, so a corrupt value that slipped past mount validation
/// (or a re-read of a superblock that changed under us) can never make the
/// mount scan a 2^32-bit range or read past the 4 KiB bitmap buffer.
pub fn clamp_group_span(total: u32, per_group: u32, index: u32) -> u32 {
    let consumed = (per_group as u64) * (index as u64);
    let remaining = (total as u64).saturating_sub(consumed);
    let span = core::cmp::min(per_group as u64, remaining);
    core::cmp::min(span, BITMAP_BITS as u64) as u32
}

/// Why a directory inode's `i_size` cannot be trusted.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum DirSizeError {
    /// ext2 directories are block-aligned; `i_size % BLOCK_SIZE != 0` is never
    /// produced by the writer and is not a shape this reader supports.
    NotBlockAligned,
    /// The directory claims more blocks than the filesystem holds.
    LargerThanFilesystem,
}

/// Number of blocks a directory inode legitimately occupies, from its on-disk
/// `i_size` and the filesystem's total block count.
///
/// Returns `Ok(0)` for a zero-size directory (an inode whose data was never
/// allocated still enumerates as empty rather than corrupting anything).
/// Callers must use this everywhere they derive `nblocks` from a directory
/// `i_size`; the multiplication cannot overflow because `fs_blocks` is the
/// on-disk `u32` block count widened to `u64`.
pub fn dir_blocks(i_size: u32, fs_blocks: u64) -> Result<u64, DirSizeError> {
    let size = i_size as u64;
    if size % BLOCK_SIZE as u64 != 0 {
        return Err(DirSizeError::NotBlockAligned);
    }
    let blocks = size / BLOCK_SIZE as u64;
    if blocks > fs_blocks {
        return Err(DirSizeError::LargerThanFilesystem);
    }
    Ok(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact crafted shape from the audit (F-x86v-5): a directory whose
    /// `i_size` claims ~4 GiB on a 64 MiB image is refused before the loop.
    #[test]
    fn crafted_huge_directory_size_is_refused() {
        assert_eq!(
            dir_blocks(0xFFFF_F000, 16_384),
            Err(DirSizeError::LargerThanFilesystem)
        );
        // Same order of magnitude, not block aligned: still refused.
        assert_eq!(
            dir_blocks(0xFFFF_F001, 16_384),
            Err(DirSizeError::NotBlockAligned)
        );
    }

    #[test]
    fn ordinary_directory_sizes_are_accepted() {
        assert_eq!(dir_blocks(0, 16_384), Ok(0));
        assert_eq!(dir_blocks(4096, 16_384), Ok(1));
        assert_eq!(dir_blocks(4 * 4096, 16_384), Ok(4));
        // Exactly the filesystem's size in blocks is the documented limit.
        assert_eq!(dir_blocks(16_384 * 4096, 16_384), Ok(16_384));
        // One block more than the filesystem can hold is corrupt.
        assert_eq!(
            dir_blocks((16_385 * 4096) as u32, 16_384),
            Err(DirSizeError::LargerThanFilesystem)
        );
    }
}
