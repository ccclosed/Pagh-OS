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
//! This module is `core`-only, has no dependencies and duplicates the 4 KiB
//! block size instead of naming `structs::BS`, exactly like `symlink.rs`, so
//! `host-tests` includes the *same* source with `#[path]` and property-tests
//! the rules the kernel actually runs (R11.6).
#![allow(dead_code)]

/// Size of one ext2 filesystem block in this implementation (mirrors
/// `structs::BS`; duplicated to keep this file self-contained for the host
/// include, and pinned by the host property below).
pub const BLOCK_SIZE: usize = 4096;

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
