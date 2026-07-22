//! ext2 bitmap allocation: lowest-clear-bit block/inode allocation and free,
//! keeping the group-descriptor and superblock free counts in sync.
//!
//! Inode numbers are 1-based (bit `k` of the inode bitmap == inode `k + 1`).
//! Block numbers follow `s_first_data_block` (which is 0 for 4 KiB blocks, so
//! bit `k` of the block bitmap == block `k`).

#![allow(dead_code)]

use super::structs::{Ext2GroupDesc, Ext2SuperBlock, BS};

/// Find and set the lowest clear bit in `bitmap`, returning its index.
///
/// Returns `None` when every bit in the addressable range is set
/// (`FsError::OutOfSpace` at the call site).
pub fn alloc_bit(bitmap: &mut [u8]) -> Option<u32> {
    let scan = core::cmp::min(bitmap.len(), BS);
    for byte_idx in 0..scan {
        let byte = bitmap[byte_idx];
        if byte != 0xFF {
            // Lowest clear bit within this byte.
            let bit = (!byte).trailing_zeros(); // 0..=7
            bitmap[byte_idx] = byte | (1u8 << bit);
            return Some((byte_idx as u32) * 8 + bit);
        }
    }
    None
}

/// Set bit `idx` (mark used). Returns `true` if it was previously clear.
pub fn set_bit(bitmap: &mut [u8], idx: u32) -> bool {
    let byte = (idx / 8) as usize;
    let bit = (idx % 8) as u8;
    if byte >= bitmap.len() {
        return false;
    }
    let was_clear = (bitmap[byte] & (1 << bit)) == 0;
    bitmap[byte] |= 1 << bit;
    was_clear
}

/// Clear bit `idx` (mark free). Returns `true` if it was previously set.
pub fn clear_bit(bitmap: &mut [u8], idx: u32) -> bool {
    let byte = (idx / 8) as usize;
    let bit = (idx % 8) as u8;
    if byte >= bitmap.len() {
        return false;
    }
    let was_set = (bitmap[byte] & (1 << bit)) != 0;
    bitmap[byte] &= !(1 << bit);
    was_set
}

/// Test whether bit `idx` is set (in use).
pub fn test_bit(bitmap: &[u8], idx: u32) -> bool {
    let byte = (idx / 8) as usize;
    let bit = (idx % 8) as u8;
    byte < bitmap.len() && (bitmap[byte] & (1 << bit)) != 0
}

/// Count the set bits across the addressable bitmap range `[0, span)`.
pub fn count_set_bits(bitmap: &[u8], span: u32) -> u32 {
    let mut count = 0;
    for idx in 0..span {
        if test_bit(bitmap, idx) {
            count += 1;
        }
    }
    count
}

/// Allocate a data block out of `block_bitmap`, decrementing the free counts.
///
/// Returns the **block number** (== bit index, since `s_first_data_block == 0`).
pub fn alloc_block(
    block_bitmap: &mut [u8],
    sb: &mut Ext2SuperBlock,
    gd: &mut Ext2GroupDesc,
) -> Option<u32> {
    let bit = alloc_bit(block_bitmap)?;
    let block = sb.s_first_data_block + bit;
    if block >= sb.s_blocks_count {
        // Allocation ran past the declared ext2 region: undo and report full.
        clear_bit(block_bitmap, bit);
        return None;
    }
    sb.s_free_blocks_count = sb.s_free_blocks_count.saturating_sub(1);
    gd.bg_free_blocks_count = gd.bg_free_blocks_count.saturating_sub(1);
    Some(block)
}

/// Free a previously-allocated data block, incrementing the free counts.
pub fn free_block(
    block_bitmap: &mut [u8],
    sb: &mut Ext2SuperBlock,
    gd: &mut Ext2GroupDesc,
    block: u32,
) {
    let bit = block - sb.s_first_data_block;
    if clear_bit(block_bitmap, bit) {
        sb.s_free_blocks_count += 1;
        gd.bg_free_blocks_count += 1;
    }
}

/// Allocate an inode out of `inode_bitmap`, decrementing the free counts.
///
/// Returns the **1-based inode number** (`bit + 1`).
pub fn alloc_inode(
    inode_bitmap: &mut [u8],
    sb: &mut Ext2SuperBlock,
    gd: &mut Ext2GroupDesc,
) -> Option<u32> {
    let bit = alloc_bit(inode_bitmap)?;
    let ino = bit + 1;
    if ino > sb.s_inodes_count {
        clear_bit(inode_bitmap, bit);
        return None;
    }
    sb.s_free_inodes_count = sb.s_free_inodes_count.saturating_sub(1);
    gd.bg_free_inodes_count = gd.bg_free_inodes_count.saturating_sub(1);
    Some(ino)
}

/// Free a previously-allocated inode, incrementing the free counts.
pub fn free_inode(
    inode_bitmap: &mut [u8],
    sb: &mut Ext2SuperBlock,
    gd: &mut Ext2GroupDesc,
    ino: u32,
) {
    if ino == 0 {
        return;
    }
    let bit = ino - 1;
    if clear_bit(inode_bitmap, bit) {
        sb.s_free_inodes_count += 1;
        gd.bg_free_inodes_count += 1;
    }
}
