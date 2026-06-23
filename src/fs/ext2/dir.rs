//! ext2 directory entries: `rec_len`-walked iteration, insertion (split slack),
//! and removal (merge `rec_len` into the previous entry) within a single 4 KiB
//! directory block.
//!
//! Invariant maintained over every block: the `rec_len` chain exactly tiles
//! `[0, BS)` — no gaps, no overlap, no entry crossing the block boundary, and
//! the last entry's `rec_len` reaches `BS`.
//!
//! With `s_feature_incompat == 0` (no FILETYPE feature) the `file_type` byte is
//! written as 0; entry types are determined from the target inode's `i_mode`.

#![allow(dead_code)]

use alloc::string::String;
use alloc::vec::Vec;

use super::structs::{align4, Ext2DirEntryHeader, BS};
use crate::fs::FsError;

/// Minimum record length for a name of `name_len` bytes.
#[inline]
pub fn min_rec_len(name_len: usize) -> usize {
    align4(8 + name_len)
}

/// A decoded directory entry within a block.
#[derive(Clone)]
pub struct DirEntry {
    pub pos: usize,
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub name: String,
}

fn read_header(block: &[u8], pos: usize) -> Ext2DirEntryHeader {
    Ext2DirEntryHeader {
        inode: u32::from_le_bytes([block[pos], block[pos + 1], block[pos + 2], block[pos + 3]]),
        rec_len: u16::from_le_bytes([block[pos + 4], block[pos + 5]]),
        name_len: block[pos + 6],
        file_type: block[pos + 7],
    }
}

fn write_header(block: &mut [u8], pos: usize, inode: u32, rec_len: u16, name_len: u8) {
    block[pos..pos + 4].copy_from_slice(&inode.to_le_bytes());
    block[pos + 4..pos + 6].copy_from_slice(&rec_len.to_le_bytes());
    block[pos + 6] = name_len;
    block[pos + 7] = 0; // file_type unused (feature_incompat == 0)
}

/// Walk the entries of a directory block, returning all decoded entries.
///
/// Returns `Corrupt` if the `rec_len` chain does not tile `[0, BS)` cleanly.
pub fn iter_entries(block: &[u8]) -> Result<Vec<DirEntry>, FsError> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < BS {
        if pos + 8 > BS {
            return Err(FsError::Corrupt);
        }
        let h = read_header(block, pos);
        let rec_len = h.rec_len as usize;
        // rec_len must be a positive multiple of 4 and stay within the block.
        if rec_len < 8 || rec_len % 4 != 0 || pos + rec_len > BS {
            return Err(FsError::Corrupt);
        }
        let name = if h.inode != 0 {
            let nl = h.name_len as usize;
            if 8 + nl > rec_len {
                return Err(FsError::Corrupt);
            }
            String::from_utf8_lossy(&block[pos + 8..pos + 8 + nl]).into_owned()
        } else {
            String::new()
        };
        out.push(DirEntry {
            pos,
            inode: h.inode,
            rec_len: h.rec_len,
            name_len: h.name_len,
            name,
        });
        pos += rec_len;
    }
    if pos != BS {
        return Err(FsError::Corrupt);
    }
    Ok(out)
}

/// Find an entry by name; returns `(inode, file_type)` on a match.
pub fn find(block: &[u8], name: &str) -> Result<Option<(u32, u8)>, FsError> {
    for e in iter_entries(block)? {
        if e.inode != 0 && e.name == name {
            return Ok(Some((e.inode, 0)));
        }
    }
    Ok(None)
}

/// Initialize a freshly allocated directory block as a single free record that
/// spans the whole block.
pub fn init_empty_block(block: &mut [u8]) {
    for b in block.iter_mut() {
        *b = 0;
    }
    write_header(block, 0, 0, BS as u16, 0);
}

/// Write a `.`/`..` pair into a freshly zeroed directory block (used by mkfs and
/// `mkdir`). `self_ino` is this directory, `parent_ino` its parent.
pub fn init_dot_entries(block: &mut [u8], self_ino: u32, parent_ino: u32) {
    for b in block.iter_mut() {
        *b = 0;
    }
    // "."  — rec_len = align4(8 + 1) = 12
    let dot_rec = min_rec_len(1) as u16;
    write_header(block, 0, self_ino, dot_rec, 1);
    block[8] = b'.';
    // ".." — rec_len extends to end of block
    let ddot_pos = dot_rec as usize;
    let ddot_rec = (BS - ddot_pos) as u16;
    write_header(block, ddot_pos, parent_ino, ddot_rec, 2);
    block[ddot_pos + 8] = b'.';
    block[ddot_pos + 9] = b'.';
}

/// Attempt to insert `(name, ino)` into the directory block by splitting the
/// slack of an existing record. Returns `Ok(true)` on success, `Ok(false)` if
/// the block has no room (caller should grow a new block), or `NameTooLong`.
pub fn insert_into_block(
    block: &mut [u8],
    name: &str,
    ino: u32,
) -> Result<bool, FsError> {
    let name_bytes = name.as_bytes();
    if name_bytes.len() > 255 {
        return Err(FsError::NameTooLong);
    }
    if name_bytes.is_empty() {
        return Err(FsError::Corrupt);
    }
    let needed = min_rec_len(name_bytes.len());

    let entries = iter_entries(block)?;
    for e in entries {
        let ideal = if e.inode == 0 {
            0
        } else {
            min_rec_len(e.name_len as usize)
        };
        let slack = e.rec_len as usize - ideal;
        if slack >= needed {
            let new_pos = e.pos + ideal;
            let new_rec_len = (e.rec_len as usize - ideal) as u16;
            write_header(block, new_pos, ino, new_rec_len, name_bytes.len() as u8);
            block[new_pos + 8..new_pos + 8 + name_bytes.len()].copy_from_slice(name_bytes);
            if e.inode != 0 {
                // Shrink the donor record to its ideal size.
                let cur = read_header(block, e.pos);
                write_header(block, e.pos, cur.inode, ideal as u16, cur.name_len);
            }
            return Ok(true);
        }
    }
    Ok(false)
}

/// Remove the entry named `name` from the block, merging its `rec_len` into the
/// previous entry (or zeroing its inode if it is the first record).
///
/// Returns `Ok(Some(inode))` with the removed entry's inode, or `Ok(None)` if
/// no such entry exists.
pub fn remove_from_block(block: &mut [u8], name: &str) -> Result<Option<u32>, FsError> {
    let entries = iter_entries(block)?;
    let mut prev_pos: Option<usize> = None;
    for e in &entries {
        if e.inode != 0 && e.name == name {
            let removed = e.inode;
            match prev_pos {
                Some(pp) => {
                    // Merge this record's rec_len into the previous record.
                    let prev = read_header(block, pp);
                    let merged = prev.rec_len + e.rec_len;
                    write_header(block, pp, prev.inode, merged, prev.name_len);
                }
                None => {
                    // First record: just free it in place (keep its rec_len).
                    write_header(block, e.pos, 0, e.rec_len, 0);
                }
            }
            return Ok(Some(removed));
        }
        prev_pos = Some(e.pos);
    }
    Ok(None)
}

/// Count the live (non-free, non-dot) entries in a directory block.
pub fn live_entry_count(block: &[u8]) -> Result<usize, FsError> {
    let mut n = 0;
    for e in iter_entries(block)? {
        if e.inode != 0 && e.name != "." && e.name != ".." {
            n += 1;
        }
    }
    Ok(n)
}
