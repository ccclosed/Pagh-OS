//! ext2 inode block map (read path): resolve a file byte offset to the ext2
//! block that holds it, walking the classic 12 direct + single/double/triple
//! indirect pointers (`ptrs_per_block = BS / 4 = 1024`).
//!
//! The write/allocate-past-EOF path lives in `ext2::mod` (`Tx::map_or_alloc`)
//! because it must allocate blocks and update the bitmaps inside the same
//! journal transaction.

#![allow(dead_code)]

use super::structs::{read_u32, Ext2Inode, PTRS_PER_BLOCK};
use super::Ext2Fs;
use crate::fs::FsError;

/// Return `Some(p)` when `p != 0`, else `None` (an unallocated hole).
#[inline]
fn nonzero(p: u32) -> Option<u32> {
    if p == 0 {
        None
    } else {
        Some(p)
    }
}

/// Read pointer slot `index` from indirect block `block`.
fn read_ptr(fs: &Ext2Fs, block: u32, index: u32) -> Result<u32, FsError> {
    if block == 0 {
        return Ok(0);
    }
    let buf = fs.read_fs_block(block as u64)?;
    Ok(read_u32(&buf, (index as usize) * 4))
}

/// Resolve the ext2 block holding file byte offset `off`, or `None` if that
/// logical block has not been allocated (a hole or past the last block).
///
/// Returns `Err` only on an underlying device error.
pub fn block_for_offset(
    fs: &Ext2Fs,
    inode: &Ext2Inode,
    off: u64,
) -> Result<Option<u32>, FsError> {
    let mut idx = off / super::structs::BS as u64;
    let ppb = PTRS_PER_BLOCK as u64;

    // 12 direct pointers.
    if idx < 12 {
        return Ok(nonzero(inode.i_block[idx as usize]));
    }
    idx -= 12;

    // Single indirect.
    if idx < ppb {
        let l1 = inode.i_block[12];
        if l1 == 0 {
            return Ok(None);
        }
        let ptr = read_ptr(fs, l1, idx as u32)?;
        return Ok(nonzero(ptr));
    }
    idx -= ppb;

    // Double indirect.
    if idx < ppb * ppb {
        let l2 = inode.i_block[13];
        if l2 == 0 {
            return Ok(None);
        }
        let l1 = read_ptr(fs, l2, (idx / ppb) as u32)?;
        if l1 == 0 {
            return Ok(None);
        }
        let ptr = read_ptr(fs, l1, (idx % ppb) as u32)?;
        return Ok(nonzero(ptr));
    }
    idx -= ppb * ppb;

    // Triple indirect.
    if idx < ppb * ppb * ppb {
        let l3 = inode.i_block[14];
        if l3 == 0 {
            return Ok(None);
        }
        let l2 = read_ptr(fs, l3, (idx / (ppb * ppb)) as u32)?;
        if l2 == 0 {
            return Ok(None);
        }
        let l1 = read_ptr(fs, l2, ((idx / ppb) % ppb) as u32)?;
        if l1 == 0 {
            return Ok(None);
        }
        let ptr = read_ptr(fs, l1, (idx % ppb) as u32)?;
        return Ok(nonzero(ptr));
    }

    // Beyond the triple-indirect range: not representable.
    Ok(None)
}
