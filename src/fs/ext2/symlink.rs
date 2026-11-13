//! Pure ext2 symbolic-link layout and hard-link accounting rules.
//!
//! ext2 stores a symlink's target in one of two shapes:
//!
//!   * **fast** ("inline") — the target and its terminating NUL fit in the
//!     inode's 60-byte `i_block` array (`[u32; 15]`), so the link occupies no
//!     data block at all (`i_blocks == 0`);
//!   * **slow** — the target needs a real data block (`i_blocks == BS/512`),
//!     whose first bytes are the target (the block is zero-filled, so a NUL
//!     always follows it).
//!
//! `e2fsprogs` 1.47.3 and Linux agree on the boundary: a 59-byte target is
//! still fast, a 60-byte one is already slow (`ext2_symlink` compares
//! `strlen(target) + 1` against `sizeof(i_block) == 60`). Getting this wrong
//! loses the last character of a 60-byte target on Linux, so the boundary is
//! pinned here once and property-tested on the host.
//!
//! The file is `core`-only and self-contained (it never names a sibling
//! module), so `host-tests` includes it verbatim via `#[path]` and the same
//! source is compiled by the `#![no_std]` kernel (R11.6).
#![allow(dead_code)]

/// Bytes of inline target storage: `i_block` is `[u32; 15]`.
pub const INLINE_CAPACITY: usize = 60;

/// `u32` words in `i_block`.
pub const INLINE_WORDS: usize = 15;

/// Longest target that still fits inline *with* its terminating NUL.
pub const FAST_MAX_TARGET: usize = INLINE_CAPACITY - 1;

/// Largest target a **slow** symlink may have: one ext2 filesystem block.
///
/// ext2 stores a slow target in a single data block (e2fsprogs refuses to
/// create a longer link), so a larger `i_size` on an `S_IFLNK` inode is a
/// corrupt/crafted image, never a legitimate target. The reader uses this as a
/// hard allocation bound: a crafted `i_size = 0xFFFFFFF0` used to reach
/// `vec![0u8; 0xFFFFFFF0]` in `Ext2Fs::read_symlink` and abort the kernel
/// (F-x86v-1). Keep it in sync with `structs::BS` — the value is duplicated so
/// this module stays self-contained for the host `#[path]` include.
pub const SLOW_MAX_TARGET: usize = 4096;

/// What an attacker-controlled `(i_blocks, i_size)` pair means for a symlink.
///
/// One decision for every consumer (read and delete), so a corrupt inode cannot
/// be "fast" for one and "slow" for the other (F-x86v-4: `unlink` used to take
/// the slow branch for an `i_blocks == 0, i_size > 60` inode and free
/// `i_block[0]` — the first four bytes of the *target text* — as a block
/// number).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TargetShape {
    /// Inline target of this many bytes (`i_blocks == 0`, `i_size <= 60`).
    Fast(usize),
    /// Target in one data block, this many bytes (`i_blocks != 0`).
    Slow(usize),
    /// Inconsistent shape: report [`FsError::Corrupt`], allocate nothing.
    Corrupt,
}

/// Classify an on-disk symlink target strictly from its inode fields.
///
/// Fast requires `i_blocks == 0` *and* the inline capacity (`is_fast`); a
/// `i_blocks == 0` inode claiming a longer target is inconsistent — its
/// `i_block` bytes are text, not a block map — and must never be read through
/// the slow path. Slow requires a plausible one-block target; anything longer
/// than [`SLOW_MAX_TARGET`] is corrupt *before* any allocation.
pub fn target_shape(i_blocks: u32, i_size: u32) -> TargetShape {
    let size = i_size as usize;
    if is_fast(i_blocks, i_size) {
        TargetShape::Fast(size)
    } else if i_blocks == 0 {
        // Text in `i_block`, but `i_size` > inline capacity: neither layout.
        TargetShape::Corrupt
    } else if size <= SLOW_MAX_TARGET {
        TargetShape::Slow(size)
    } else {
        TargetShape::Corrupt
    }
}

/// Does a symlink inode own a data block?
///
/// Deletion decides **only** from `i_blocks`, exactly like Linux's
/// `ext2_inode_is_fast_symlink` (`i_blocks - ea_blocks == 0`): an inline target
/// is text in `i_block` whatever `i_size` claims, so no block may be freed for
/// it and no `i_size` may be used to pick the slow branch.
pub fn owns_data_block(i_blocks: u32) -> bool {
    i_blocks != 0
}

/// Which on-disk shape a symlink target must use.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SymlinkLayout {
    /// Target (plus NUL) lives in `i_block`; no data block, `i_blocks == 0`.
    Fast,
    /// Target lives in a freshly allocated data block; `i_blocks == BS/512`.
    Slow,
}

/// Why a target cannot become a symlink at all.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SymlinkError {
    /// A zero-length target: never created (a dangling-but-empty link is not
    /// something any archive or `ln -s ''` should produce silently).
    Empty,
    /// The target does not fit `i_size` (a 32-bit field).
    TooBig,
}

/// The on-disk plan for one symlink target.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct SymlinkPlan {
    /// Fast or slow layout.
    pub layout: SymlinkLayout,
    /// Value for `i_size` (the target length, without any NUL).
    pub size: usize,
    /// The 60 inline bytes for [`SymlinkLayout::Fast`] (target + NUL padding).
    pub inline: Option<[u8; INLINE_CAPACITY]>,
}

/// Decide the layout for `target` and render the inline image when it is fast.
pub fn plan(target: &[u8]) -> Result<SymlinkPlan, SymlinkError> {
    let len = target.len();
    if len == 0 {
        return Err(SymlinkError::Empty);
    }
    if len > u32::MAX as usize {
        return Err(SymlinkError::TooBig);
    }
    if len <= FAST_MAX_TARGET {
        let mut inline = [0u8; INLINE_CAPACITY];
        inline[..len].copy_from_slice(target);
        Ok(SymlinkPlan {
            layout: SymlinkLayout::Fast,
            size: len,
            inline: Some(inline),
        })
    } else {
        Ok(SymlinkPlan {
            layout: SymlinkLayout::Slow,
            size: len,
            inline: None,
        })
    }
}

/// True when the inode is a *fast* symlink on disk.
///
/// Mirrors Linux's `ext2_inode_is_fast_symlink` (`i_blocks - ea_blocks == 0`);
/// this filesystem never writes xattr blocks (`i_file_acl == 0`), so the test
/// reduces to `i_blocks == 0`. The `i_size <= 60` half keeps a corrupt image
/// from making the reader copy inode fields that are not part of the target.
pub fn is_fast(i_blocks: u32, i_size: u32) -> bool {
    i_blocks == 0 && i_size as usize <= INLINE_CAPACITY
}

/// The `i_size` leading bytes of an inline image — the target exactly as stored.
pub fn fast_target(inline: &[u8; INLINE_CAPACITY], i_size: u32) -> Option<&[u8]> {
    let n = i_size as usize;
    if n > INLINE_CAPACITY {
        return None;
    }
    Some(&inline[..n])
}

/// Reinterpret the 60 inline bytes as `i_block`'s 15 little-endian `u32` words.
///
/// The on-disk inode is written as raw bytes, so this is a pure byte/word
/// reshape — no pointer interpretation is ever applied to a symlink inode.
pub fn inline_words(inline: &[u8; INLINE_CAPACITY]) -> [u32; INLINE_WORDS] {
    let mut words = [0u32; INLINE_WORDS];
    for (i, w) in words.iter_mut().enumerate() {
        let mut b = [0u8; 4];
        b.copy_from_slice(&inline[i * 4..i * 4 + 4]);
        *w = u32::from_le_bytes(b);
    }
    words
}

/// Inverse of [`inline_words`]: the raw 60 bytes of an inode's `i_block`.
pub fn inline_bytes(words: &[u32; INLINE_WORDS]) -> [u8; INLINE_CAPACITY] {
    let mut inline = [0u8; INLINE_CAPACITY];
    for (i, w) in words.iter().enumerate() {
        inline[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    inline
}

/// What `unlink` must do with the child inode after removing its directory
/// entry.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum UnlinkAction {
    /// Other names still point at the inode: only decrement `i_links_count`.
    DropLink,
    /// Last name: free the data/indirect blocks and the inode.
    FreeInode,
}

/// Decide the unlink action for a **non-directory** inode with `links_count`
/// links. `None` means the on-disk state is already broken (free inode with a
/// live directory entry) and must be reported as corruption.
///
/// Directories never take this path: `i_links_count` on a directory counts
/// `.`/`..` links, not names, and hard links to directories are forbidden.
pub fn unlink_action(links_count: u16) -> Option<UnlinkAction> {
    match links_count {
        0 => None,
        1 => Some(UnlinkAction::FreeInode),
        _ => Some(UnlinkAction::DropLink),
    }
}

/// `i_links_count` after adding one hard link, or `None` when that would
/// overflow the 16-bit field or the inode is already free.
pub fn link_bump(links_count: u16) -> Option<u16> {
    if links_count == 0 {
        None
    } else {
        links_count.checked_add(1)
    }
}
