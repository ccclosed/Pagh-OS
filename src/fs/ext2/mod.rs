//! ext2-compatible filesystem (read + write) with a write-ahead-log journal.
//!
//! `format` writes a host-mountable ext2 image (superblock @ byte 1024,
//! `s_magic = 0xEF53`, `s_log_block_size = 2`, a single block group with block
//! and inode bitmaps + inode table, root inode 2 carrying `.`/`..`, all
//! `feature_*` cleared so a Linux host mounts it as plain ext2), plus an empty
//! WAL journal region in the reserved space after the ext2 region.
//!
//! `mount` validates the superblock, runs `journal.recover()` to replay any
//! committed-but-uncheckpointed transactions, then builds the root `VfsNode`.
//! Every mutating operation (data + inode + bitmap + dirent block writes) is
//! batched into a single journal transaction so the host-visible ext2 state
//! only ever advances atomically.
//!
//! This module is pure logic exercised over a RAM-mock `BlockDevice`
//! (`crate::test`); it is not wired into boot or the VFS mount table here.

#![allow(dead_code)]

pub mod alloc;
pub mod dir;
pub mod inode;
pub mod structs;

use ::alloc::collections::BTreeMap;
use ::alloc::string::String;
use ::alloc::sync::Arc;
use ::alloc::vec;
use ::alloc::vec::Vec;

use crate::drivers::BlockDevice;
use crate::fs::journal::{Journal, JournalArea};
use crate::fs::FsError;
use crate::sync::spinlock::Spinlock;
use crate::vfs::{VfsError, VfsNode, VfsResult};

use structs::{
    read_struct, read_u32, write_struct, write_u32, Ext2GroupDesc, Ext2Inode, Ext2SuperBlock, BS,
    EXT2_FIRST_INO, EXT2_MAGIC, EXT2_ROOT_INO, INODE_SIZE, PTRS_PER_BLOCK, S_IFDIR, S_IFREG,
    SECTORS_PER_BLOCK,
};

// ─── format layout constants (single block group) ───────────────────────────

/// Circular WAL log blocks (excludes the journal superblock).
const FMT_LOG_BLOCKS: u64 = 64;

/// FS blocks reserved at the device tail for the WAL journal: the journal
/// superblock (1 block) plus the circular log.
const JOURNAL_RESERVE_BLOCKS: u64 = FMT_LOG_BLOCKS + 1;

/// Maximum blocks (and inodes) a single block group can describe with one
/// 4096-byte bitmap block (`BS * 8 = 32768` bits). Clamping the derived counts
/// to this bound keeps the single-group layout valid AND keeps every per-group
/// on-disk `u16` count (`bg_free_blocks_count` / `bg_free_inodes_count`) within
/// range, since `32768 < u16::MAX`.
const MAX_GROUP_BLOCKS: u32 = (BS * 8) as u32;
const MAX_GROUP_INODES: u32 = (BS * 8) as u32;

/// Inode density: provision roughly one inode per this many bytes of capacity.
const BYTES_PER_INODE: u64 = 16 * 1024;

/// Floor on the inode count so even a tiny freshly-formatted FS keeps a few
/// usable inodes beyond the reserved set (inodes `1..=EXT2_FIRST_INO-1`).
const MIN_INODES: u32 = 32;

const SUPERBLOCK_OFFSET: usize = 1024;

// ─── Ext2Fs ──────────────────────────────────────────────────────────────────

struct Ext2Inner {
    sb: Ext2SuperBlock,
    // STAGE-13.8: the full group-descriptor table (one entry per block group).
    gds: Vec<Ext2GroupDesc>,
}

/// A mounted ext2 filesystem over a `BlockDevice`, with a WAL journal.
pub struct Ext2Fs {
    dev: Arc<dyn BlockDevice>,
    inner: Spinlock<Ext2Inner>,
    journal: Spinlock<Journal>,
}

fn inode_location(sb: &Ext2SuperBlock, gds: &[Ext2GroupDesc], ino: u32) -> (u64, usize) {
    let index = (ino - 1) as u64;
    let ipg = sb.s_inodes_per_group.max(1) as u64;
    let group = core::cmp::min((index / ipg) as usize, gds.len().saturating_sub(1));
    let within = index % ipg;
    let ipb = (BS / INODE_SIZE) as u64; // inodes per block (32)
    let block = gds[group].bg_inode_table as u64 + within / ipb;
    let off = (within % ipb) as usize * INODE_SIZE;
    (block, off)
}

impl Ext2Fs {
    // ── low-level device IO (4096-byte FS blocks over 512-byte sectors) ──

    pub fn read_fs_block(&self, block: u64) -> Result<Vec<u8>, FsError> {
        let mut buf = vec![0u8; BS];
        self.dev
            .read_block(block * SECTORS_PER_BLOCK, &mut buf)
            .map(|_| ())
            .map_err(|_| FsError::IoError)?;
        Ok(buf)
    }

    fn write_fs_block_direct(dev: &dyn BlockDevice, block: u64, data: &[u8]) -> Result<(), FsError> {
        debug_assert!(data.len() == BS);
        dev.write_block(block * SECTORS_PER_BLOCK, data)
            .map(|_| ())
            .map_err(|_| FsError::IoError)
    }

    pub fn read_inode(&self, ino: u32) -> Result<Ext2Inode, FsError> {
        if ino == 0 {
            return Err(FsError::Corrupt);
        }
        let (block, off) = {
            let inner = self.inner.lock();
            if ino > inner.sb.s_inodes_count {
                return Err(FsError::Corrupt);
            }
            inode_location(&inner.sb, &inner.gds, ino)
        };
        let buf = self.read_fs_block(block)?;
        Ok(unsafe { read_struct::<Ext2Inode>(&buf[off..]) })
    }

    pub fn superblock(&self) -> Ext2SuperBlock {
        self.inner.lock().sb
    }

    pub fn group_desc(&self) -> Ext2GroupDesc {
        self.inner.lock().gds[0]
    }

    fn journal_area(sb: &Ext2SuperBlock) -> JournalArea {
        JournalArea {
            super_block: sb.s_blocks_count as u64,
            log_blocks: FMT_LOG_BLOCKS,
            fs_blocks: sb.s_blocks_count as u64,
        }
    }
}

// ─── transaction context ──────────────────────────────────────────────────────

/// In-memory working set of dirty blocks plus working `sb`/`gd`. On `commit`
/// every dirty block (including the patched superblock and group descriptor) is
/// handed to the journal as a single atomic transaction.
struct Tx<'a> {
    fs: &'a Ext2Fs,
    sb: Ext2SuperBlock,
    gds: Vec<Ext2GroupDesc>,
    dirty: BTreeMap<u64, Vec<u8>>,
    /// STAGE-16.8: blocks in `dirty` that are FILE DATA. Ordered mode: on
    /// commit they are written straight to their final location and are NOT
    /// copied through the WAL (metadata-only journaling).
    data: alloc::collections::BTreeSet<u64>,
}

impl<'a> Tx<'a> {
    fn new(fs: &'a Ext2Fs) -> Self {
        let inner = fs.inner.lock();
        Tx {
            fs,
            sb: inner.sb,
            gds: inner.gds.clone(),
            dirty: BTreeMap::new(),
            data: alloc::collections::BTreeSet::new(),
        }
    }

    fn block(&mut self, blk: u64) -> Result<&mut Vec<u8>, FsError> {
        if !self.dirty.contains_key(&blk) {
            let data = self.fs.read_fs_block(blk)?;
            self.dirty.insert(blk, data);
        }
        Ok(self.dirty.get_mut(&blk).unwrap())
    }

    /// STAGE-16.8: fetch a FILE DATA block into the dirty set. When the whole
    /// block is about to be overwritten the disk read is skipped —
    /// read-modify-write halved sequential write throughput for nothing.
    fn data_block(&mut self, blk: u64, full_overwrite: bool) -> Result<&mut Vec<u8>, FsError> {
        if !self.dirty.contains_key(&blk) {
            let data = if full_overwrite {
                vec![0u8; BS]
            } else {
                self.fs.read_fs_block(blk)?
            };
            self.dirty.insert(blk, data);
        }
        self.data.insert(blk);
        Ok(self.dirty.get_mut(&blk).unwrap())
    }

    /// STAGE-13.8: allocate a zeroed data block from the first group with a
    /// free block (previously only group 0 was ever consulted, capping every
    /// filesystem at 128 MiB regardless of the device size).
    fn alloc_zeroed_block(&mut self) -> Result<u32, FsError> {
        let bpg = self.sb.s_blocks_per_group;
        let first = self.sb.s_first_data_block;
        for g in 0..self.gds.len() {
            if self.gds[g].bg_free_blocks_count == 0 {
                continue;
            }
            let bbm = self.gds[g].bg_block_bitmap as u64;
            self.block(bbm)?;
            let bit = {
                let bm = self.dirty.get_mut(&bbm).unwrap();
                alloc::alloc_bit(bm)
            };
            let Some(bit) = bit else {
                // Bitmap disagrees with the cached free count: mark the group
                // exhausted and move on.
                self.gds[g].bg_free_blocks_count = 0;
                continue;
            };
            let blk = first + g as u32 * bpg + bit;
            if blk >= self.sb.s_blocks_count {
                // Past the declared region (padding bits normally prevent
                // this): undo and treat the group as full.
                let bm = self.dirty.get_mut(&bbm).unwrap();
                alloc::clear_bit(bm, bit);
                self.gds[g].bg_free_blocks_count = 0;
                continue;
            }
            self.sb.s_free_blocks_count = self.sb.s_free_blocks_count.saturating_sub(1);
            self.gds[g].bg_free_blocks_count = self.gds[g].bg_free_blocks_count.saturating_sub(1);
            let b = self.block(blk as u64)?;
            for x in b.iter_mut() {
                *x = 0;
            }
            return Ok(blk);
        }
        Err(FsError::OutOfSpace)
    }

    fn free_data_block(&mut self, blk: u32) -> Result<(), FsError> {
        let bpg = self.sb.s_blocks_per_group.max(1);
        let rel = blk.saturating_sub(self.sb.s_first_data_block);
        let g = (rel / bpg) as usize;
        let bit = rel % bpg;
        if g >= self.gds.len() {
            return Err(FsError::Corrupt);
        }
        let bbm = self.gds[g].bg_block_bitmap as u64;
        self.block(bbm)?;
        let bm = self.dirty.get_mut(&bbm).unwrap();
        if alloc::clear_bit(bm, bit) {
            self.sb.s_free_blocks_count += 1;
            self.gds[g].bg_free_blocks_count += 1;
        }
        Ok(())
    }

    /// STAGE-13.8: allocate an inode from the first group with a free slot.
    fn alloc_new_inode(&mut self) -> Result<u32, FsError> {
        let ipg = self.sb.s_inodes_per_group;
        for g in 0..self.gds.len() {
            if self.gds[g].bg_free_inodes_count == 0 {
                continue;
            }
            let ibm = self.gds[g].bg_inode_bitmap as u64;
            self.block(ibm)?;
            let bit = {
                let bm = self.dirty.get_mut(&ibm).unwrap();
                alloc::alloc_bit(bm)
            };
            let Some(bit) = bit else {
                self.gds[g].bg_free_inodes_count = 0;
                continue;
            };
            let ino = g as u32 * ipg + bit + 1;
            if ino > self.sb.s_inodes_count {
                let bm = self.dirty.get_mut(&ibm).unwrap();
                alloc::clear_bit(bm, bit);
                self.gds[g].bg_free_inodes_count = 0;
                continue;
            }
            self.sb.s_free_inodes_count = self.sb.s_free_inodes_count.saturating_sub(1);
            self.gds[g].bg_free_inodes_count = self.gds[g].bg_free_inodes_count.saturating_sub(1);
            return Ok(ino);
        }
        Err(FsError::OutOfSpace)
    }

    fn free_inode_bit(&mut self, ino: u32) -> Result<(), FsError> {
        if ino == 0 {
            return Ok(());
        }
        let ipg = self.sb.s_inodes_per_group.max(1);
        let index = ino - 1;
        let g = (index / ipg) as usize;
        let bit = index % ipg;
        if g >= self.gds.len() {
            return Err(FsError::Corrupt);
        }
        let ibm = self.gds[g].bg_inode_bitmap as u64;
        self.block(ibm)?;
        let bm = self.dirty.get_mut(&ibm).unwrap();
        if alloc::clear_bit(bm, bit) {
            self.sb.s_free_inodes_count += 1;
            self.gds[g].bg_free_inodes_count += 1;
        }
        Ok(())
    }

    fn read_inode(&mut self, ino: u32) -> Result<Ext2Inode, FsError> {
        let (block, off) = inode_location(&self.sb, &self.gds, ino);
        self.block(block)?;
        let buf = self.dirty.get(&block).unwrap();
        Ok(unsafe { read_struct::<Ext2Inode>(&buf[off..]) })
    }

    fn write_inode(&mut self, ino: u32, inode: &Ext2Inode) -> Result<(), FsError> {
        let (block, off) = inode_location(&self.sb, &self.gds, ino);
        let b = self.block(block)?;
        unsafe { write_struct(&mut b[off..], inode) };
        Ok(())
    }

    /// Map (allocating as needed) logical block `lbn` of `inode` to an ext2
    /// block, walking 12 direct + single/double/triple indirect pointers.
    fn map_or_alloc(&mut self, inode: &mut Ext2Inode, lbn: u64) -> Result<u32, FsError> {
        let ppb = PTRS_PER_BLOCK as u64;
        let sectors = (BS / 512) as u32;

        if lbn < 12 {
            let i = lbn as usize;
            if inode.i_block[i] == 0 {
                let nb = self.alloc_zeroed_block()?;
                inode.i_block[i] = nb;
                inode.i_blocks += sectors;
            }
            return Ok(inode.i_block[i]);
        }
        let mut l = lbn - 12;

        if l < ppb {
            let root = self.ensure_indirect_root(inode, 12)?;
            return self.map_indirect(inode, root, l as u32);
        }
        l -= ppb;

        if l < ppb * ppb {
            let root = self.ensure_indirect_root(inode, 13)?;
            let l1 = self.map_indirect(inode, root, (l / ppb) as u32)?;
            return self.map_indirect(inode, l1, (l % ppb) as u32);
        }
        l -= ppb * ppb;

        if l < ppb * ppb * ppb {
            let root = self.ensure_indirect_root(inode, 14)?;
            let l2 = self.map_indirect(inode, root, (l / (ppb * ppb)) as u32)?;
            let l1 = self.map_indirect(inode, l2, ((l / ppb) % ppb) as u32)?;
            return self.map_indirect(inode, l1, (l % ppb) as u32);
        }
        Err(FsError::OutOfSpace)
    }

    fn ensure_indirect_root(&mut self, inode: &mut Ext2Inode, which: usize) -> Result<u32, FsError> {
        if inode.i_block[which] == 0 {
            let nb = self.alloc_zeroed_block()?;
            inode.i_block[which] = nb;
            inode.i_blocks += (BS / 512) as u32;
        }
        Ok(inode.i_block[which])
    }

    fn map_indirect(
        &mut self,
        inode: &mut Ext2Inode,
        ind_block: u32,
        slot: u32,
    ) -> Result<u32, FsError> {
        self.block(ind_block as u64)?;
        let cur = read_u32(self.dirty.get(&(ind_block as u64)).unwrap(), slot as usize * 4);
        if cur != 0 {
            return Ok(cur);
        }
        let nb = self.alloc_zeroed_block()?;
        inode.i_blocks += (BS / 512) as u32;
        let b = self.block(ind_block as u64)?;
        write_u32(b, slot as usize * 4, nb);
        Ok(nb)
    }

    /// Free every data + indirect block referenced by `inode`.
    fn free_all_blocks(&mut self, inode: &Ext2Inode) -> Result<(), FsError> {
        for i in 0..12 {
            if inode.i_block[i] != 0 {
                self.free_data_block(inode.i_block[i])?;
            }
        }
        self.free_indirect(inode.i_block[12], 1)?;
        self.free_indirect(inode.i_block[13], 2)?;
        self.free_indirect(inode.i_block[14], 3)?;
        Ok(())
    }

    fn free_indirect(&mut self, blk: u32, level: u32) -> Result<(), FsError> {
        if blk == 0 {
            return Ok(());
        }
        let buf = self.block(blk as u64)?.clone();
        let ppb = PTRS_PER_BLOCK as usize;
        for slot in 0..ppb {
            let p = read_u32(&buf, slot * 4);
            if p != 0 {
                if level > 1 {
                    self.free_indirect(p, level - 1)?;
                } else {
                    self.free_data_block(p)?;
                }
            }
        }
        self.free_data_block(blk)?;
        Ok(())
    }

    /// Commit: patch the superblock + every group descriptor into their
    /// blocks, then hand every dirty block to the journal as one atomic
    /// transaction. On success the in-memory `sb`/`gds` are published.
    fn commit(mut self) -> Result<(), FsError> {
        {
            let sb = self.sb;
            let b0 = self.block(0)?;
            unsafe { write_struct(&mut b0[SUPERBLOCK_OFFSET..], &sb) };
        }
        {
            let gds = self.gds.clone();
            let gd_size = core::mem::size_of::<Ext2GroupDesc>();
            for (g, gd) in gds.iter().enumerate() {
                let blk = 1 + (g * gd_size / BS) as u64;
                let off = (g * gd_size) % BS;
                let b = self.block(blk)?;
                unsafe { write_struct(&mut b[off..], gd) };
            }
        }

        // STAGE-16.8 ORDERED MODE: file data goes straight to its final
        // location BEFORE the metadata transaction commits (so committed
        // metadata never points at unwritten data), and only metadata rides
        // the WAL. This removes the double write of every data block, which
        // dominated apt/dpkg unpack time.
        for (blk, data) in self.dirty.iter() {
            if self.data.contains(blk) {
                Ext2Fs::write_fs_block_direct(&*self.fs.dev, *blk, data)?;
            }
        }
        // Hand the remaining (metadata) dirty blocks to the journal as one
        // atomic transaction.
        {
            let mut j = self.fs.journal.lock();
            let mut txn = j.begin();
            for (blk, data) in self.dirty.iter() {
                if !self.data.contains(blk) {
                    j.log_block(&mut txn, *blk, data);
                }
            }
            j.commit(txn)?;
        }

        // Publish the new superblock / group descriptors.
        let mut inner = self.fs.inner.lock();
        inner.sb = self.sb;
        inner.gds = self.gds;
        Ok(())
    }
}


// ─── format ───────────────────────────────────────────────────────────────

impl Ext2Fs {
    /// Produce a fresh, host-mountable ext2 image plus an empty WAL journal.
    ///
    /// STAGE-13.8: multi-group layout. The filesystem now spans the whole
    /// device (minus the WAL reserve) with as many 32768-block (128 MiB)
    /// groups as fit, instead of clamping everything to a single group.
    /// Every group starts with a superblock + GD-table backup (no
    /// sparse_super), then its block bitmap, inode bitmap, inode table, and
    /// data blocks.
    pub fn format(dev: Arc<dyn BlockDevice>) -> Result<(), FsError> {
        // Device capacity in 4 KiB FS blocks.
        let device_blocks = dev.sector_count() / SECTORS_PER_BLOCK;

        // Minimum-capacity guard (R7.4), sized for one small group.
        let min_inode_table_blocks =
            ((MIN_INODES as usize * INODE_SIZE) + BS - 1) / BS; // ceil
        let min_ext2_blocks = 4u64
            + min_inode_table_blocks as u64
            + 1
            + 1;
        let min_layout_blocks = min_ext2_blocks + JOURNAL_RESERVE_BLOCKS;
        if device_blocks < min_layout_blocks {
            return Err(FsError::OutOfSpace);
        }

        // ext2 region = device minus the WAL journal reserve at the tail.
        let data_blocks = device_blocks.saturating_sub(JOURNAL_RESERVE_BLOCKS);

        let bpg = MAX_GROUP_BLOCKS; // 32768 blocks (128 MiB) per group
        let mut total_blocks = data_blocks.min(u32::MAX as u64) as u32;
        let mut groups =
            ((total_blocks as u64 + bpg as u64 - 1) / bpg as u64) as u32;

        let gd_size = core::mem::size_of::<Ext2GroupDesc>();
        let gd_blocks = |groups: u32| -> u32 {
            (((groups as usize * gd_size) + BS - 1) / BS) as u32
        };

        // Inode density: one inode per BYTES_PER_INODE across the region,
        // spread uniformly (s_inodes_per_group must be identical in every
        // group), rounded to whole inode-table blocks, one bitmap per group.
        let inodes_per_block = (BS / INODE_SIZE) as u64;
        let scaled = (total_blocks as u64 * BS as u64) / BYTES_PER_INODE;
        let per_group = (scaled.max(MIN_INODES as u64) / groups as u64).max(inodes_per_block);
        let per_group = per_group
            .saturating_add(inodes_per_block - 1)
            / inodes_per_block
            * inodes_per_block;
        let ipg = per_group.min(MAX_GROUP_INODES as u64) as u32;
        let itb = ipg / (BS / INODE_SIZE) as u32; // inode-table blocks/group

        // Drop a tail group too small to hold its own metadata + some data.
        loop {
            let base = (groups - 1) * bpg;
            let span = total_blocks - base;
            let meta_guess = 1 + gd_blocks(groups) + 2 + itb;
            if groups > 1 && span < meta_guess + 8 {
                groups -= 1;
                total_blocks = groups * bpg;
            } else {
                break;
            }
        }
        let gdb = gd_blocks(groups);
        let meta = 1 + gdb + 2 + itb; // per-group metadata block count
        let total_inodes = groups * ipg;
        let reserved_inodes = EXT2_FIRST_INO - 1; // inodes 1..=10 marked used

        // Root directory: first data block of group 0.
        let root_dir_block = meta;
        if total_blocks <= root_dir_block + 1 {
            return Err(FsError::OutOfSpace);
        }

        // Used blocks: per-group metadata everywhere + root dir in group 0.
        let used_total = meta * groups + 1;
        if total_blocks <= used_total {
            return Err(FsError::OutOfSpace);
        }

        let sb = Ext2SuperBlock {
            s_inodes_count: total_inodes,
            s_blocks_count: total_blocks,
            s_r_blocks_count: 0,
            s_free_blocks_count: total_blocks - used_total,
            s_free_inodes_count: total_inodes - reserved_inodes,
            s_first_data_block: 0,
            s_log_block_size: 2,
            s_log_frag_size: 2,
            s_blocks_per_group: bpg,
            s_frags_per_group: bpg,
            s_inodes_per_group: ipg,
            s_mtime: 0,
            s_wtime: 0,
            s_mnt_count: 0,
            s_max_mnt_count: 0xFFFF,
            s_magic: EXT2_MAGIC,
            s_state: 1,
            s_errors: 1,
            s_minor_rev_level: 0,
            s_lastcheck: 0,
            s_checkinterval: 0,
            s_creator_os: 0,
            s_rev_level: 1,
            s_def_resuid: 0,
            s_def_resgid: 0,
            s_first_ino: EXT2_FIRST_INO,
            s_inode_size: INODE_SIZE as u16,
            s_block_group_nr: 0,
            s_feature_compat: 0,
            s_feature_incompat: 0,
            s_feature_ro_compat: 0,
            s_uuid: [0; 16],
            s_volume_name: [0; 16],
        };

        // Build every group descriptor (the GD table is replicated).
        let mut gds: Vec<Ext2GroupDesc> = Vec::with_capacity(groups as usize);
        for g in 0..groups {
            let base = g * bpg;
            let span = core::cmp::min(bpg, total_blocks - base);
            let used = if g == 0 { meta + 1 } else { meta }; // + root dir
            gds.push(Ext2GroupDesc {
                bg_block_bitmap: base + 1 + gdb,
                bg_inode_bitmap: base + 1 + gdb + 1,
                bg_inode_table: base + 1 + gdb + 2,
                bg_free_blocks_count: (span - used) as u16,
                bg_free_inodes_count: if g == 0 {
                    (ipg - reserved_inodes) as u16
                } else {
                    ipg as u16
                },
                bg_used_dirs_count: if g == 0 { 1 } else { 0 },
                bg_pad: 0,
                bg_reserved: [0; 12],
            });
        }

        // Superblock block image (SB @ byte 1024 of the group's first block).
        let mut sb_block = vec![0u8; BS];
        unsafe { write_struct(&mut sb_block[SUPERBLOCK_OFFSET..], &sb) };

        // GD table image (gdb blocks), replicated into every group.
        let mut gd_table = vec![0u8; gdb as usize * BS];
        for (g, gd) in gds.iter().enumerate() {
            unsafe { write_struct(&mut gd_table[g * gd_size..], gd) };
        }

        let zero = vec![0u8; BS];
        for g in 0..groups {
            let base = g * bpg;
            let span = core::cmp::min(bpg, total_blocks - base);
            // SB backup + GD table copy at the group start (group 0: primary).
            Self::write_fs_block_direct(&*dev, base as u64, &sb_block)?;
            for i in 0..gdb {
                let src = &gd_table[i as usize * BS..(i as usize + 1) * BS];
                Self::write_fs_block_direct(&*dev, (base + 1 + i) as u64, src)?;
            }
            // Block bitmap: metadata (+ root dir in group 0) used; padding
            // bits beyond the declared span are set so allocation can never
            // run past the filesystem end.
            let mut bbm = vec![0u8; BS];
            let used = if g == 0 { meta + 1 } else { meta };
            for b in 0..used {
                alloc::set_bit(&mut bbm, b);
            }
            for b in span..MAX_GROUP_BLOCKS {
                alloc::set_bit(&mut bbm, b);
            }
            Self::write_fs_block_direct(&*dev, gds[g as usize].bg_block_bitmap as u64, &bbm)?;
            // Inode bitmap: group 0 reserves the classic system inodes; every
            // group marks the padding bits beyond s_inodes_per_group.
            let mut ibm = vec![0u8; BS];
            if g == 0 {
                for i in 0..reserved_inodes {
                    alloc::set_bit(&mut ibm, i);
                }
            }
            for i in ipg..MAX_GROUP_INODES {
                alloc::set_bit(&mut ibm, i);
            }
            Self::write_fs_block_direct(&*dev, gds[g as usize].bg_inode_bitmap as u64, &ibm)?;
            // Zero the inode table (data blocks need no wipe: the bitmaps
            // declare them free, and the block allocator zeroes on alloc).
            for b in 0..itb {
                Self::write_fs_block_direct(
                    &*dev,
                    (gds[g as usize].bg_inode_table + b) as u64,
                    &zero,
                )?;
            }
        }

        // Root directory data block with "." and "..".
        let mut root_block = vec![0u8; BS];
        dir::init_dot_entries(&mut root_block, EXT2_ROOT_INO, EXT2_ROOT_INO);
        Self::write_fs_block_direct(&*dev, root_dir_block as u64, &root_block)?;

        // Root inode (inode 2): directory, size = one block.
        let mut root_inode = Ext2Inode::zeroed();
        root_inode.i_mode = S_IFDIR | 0o755;
        root_inode.i_links_count = 2; // "." and ".."
        root_inode.i_size = BS as u32;
        root_inode.i_blocks = (BS / 512) as u32;
        root_inode.i_block[0] = root_dir_block;

        let (rblock, roff) = inode_location(&sb, &gds, EXT2_ROOT_INO);
        let mut itbuf = vec![0u8; BS];
        unsafe { write_struct(&mut itbuf[roff..], &root_inode) };
        Self::write_fs_block_direct(&*dev, rblock, &itbuf)?;

        // Empty WAL journal in the reserved region after the ext2 area.
        let area = Self::journal_area(&sb);
        Journal::format(&*dev, area)?;
        Ok(())
    }
}


// ─── mount ────────────────────────────────────────────────────────────────

impl Ext2Fs {
    /// Read the superblock and the full group-descriptor table (STAGE-13.8:
    /// one descriptor per block group; the table starts at block 1). Old
    /// single-group images load unchanged (they simply yield one descriptor).
    fn read_sb_gds(dev: &dyn BlockDevice) -> Result<(Ext2SuperBlock, Vec<Ext2GroupDesc>), FsError> {
        let mut b0 = vec![0u8; BS];
        dev.read_block(0, &mut b0).map_err(|_| FsError::IoError)?;
        let sb: Ext2SuperBlock = unsafe { read_struct(&b0[SUPERBLOCK_OFFSET..]) };
        if sb.s_magic != EXT2_MAGIC {
            return Err(FsError::BadSuperBlock);
        }
        if (1024usize << sb.s_log_block_size) != BS {
            return Err(FsError::BadSuperBlock);
        }
        if sb.s_blocks_per_group == 0 || sb.s_inodes_per_group == 0 {
            return Err(FsError::BadSuperBlock);
        }
        let span = sb.s_blocks_count.saturating_sub(sb.s_first_data_block);
        let groups = ((span as u64 + sb.s_blocks_per_group as u64 - 1)
            / sb.s_blocks_per_group as u64) as usize;
        if groups == 0 || groups > 65536 {
            return Err(FsError::BadSuperBlock);
        }
        let gd_size = core::mem::size_of::<Ext2GroupDesc>();
        let mut gds = Vec::with_capacity(groups);
        let mut buf = vec![0u8; BS];
        let mut cached = u64::MAX;
        for g in 0..groups {
            let blk = 1 + (g * gd_size / BS) as u64;
            let off = (g * gd_size) % BS;
            if blk != cached {
                dev.read_block(blk * SECTORS_PER_BLOCK, &mut buf)
                    .map_err(|_| FsError::IoError)?;
                cached = blk;
            }
            gds.push(unsafe { read_struct::<Ext2GroupDesc>(&buf[off..]) });
        }
        Ok((sb, gds))
    }

    /// True when the device contains a structurally valid ext2 superblock/group.
    /// Boot uses this to distinguish an empty disk from an existing filesystem
    /// whose private WAL is damaged; only the former may be auto-formatted.
    pub fn has_valid_superblock(dev: &dyn BlockDevice) -> bool {
        Self::read_sb_gds(dev).is_ok()
    }

    /// Repair stale free-space counters from the authoritative bitmaps of
    /// every block group before exposing the mount.
    fn reconcile_free_counts(
        dev: &dyn BlockDevice,
        mut sb: Ext2SuperBlock,
        mut gds: Vec<Ext2GroupDesc>,
    ) -> Result<(Ext2SuperBlock, Vec<Ext2GroupDesc>), FsError> {
        let bpg = sb.s_blocks_per_group;
        let ipg = sb.s_inodes_per_group;
        let mut free_blocks_total = 0u32;
        let mut free_inodes_total = 0u32;
        let mut changed = false;
        for (g, gd) in gds.iter_mut().enumerate() {
            let base = sb.s_first_data_block + g as u32 * bpg;
            let span = core::cmp::min(bpg, sb.s_blocks_count.saturating_sub(base));
            let mut bbm = vec![0u8; BS];
            dev.read_block(gd.bg_block_bitmap as u64 * SECTORS_PER_BLOCK, &mut bbm)
                .map_err(|_| FsError::IoError)?;
            let mut ibm = vec![0u8; BS];
            dev.read_block(gd.bg_inode_bitmap as u64 * SECTORS_PER_BLOCK, &mut ibm)
                .map_err(|_| FsError::IoError)?;
            let ispan = core::cmp::min(ipg, sb.s_inodes_count.saturating_sub(g as u32 * ipg));
            let free_b = span.saturating_sub(alloc::count_set_bits(&bbm, span));
            let free_i = ispan.saturating_sub(alloc::count_set_bits(&ibm, ispan));
            if gd.bg_free_blocks_count as u32 != free_b
                || gd.bg_free_inodes_count as u32 != free_i
            {
                gd.bg_free_blocks_count = free_b.min(u16::MAX as u32) as u16;
                gd.bg_free_inodes_count = free_i.min(u16::MAX as u32) as u16;
                changed = true;
            }
            free_blocks_total += free_b;
            free_inodes_total += free_i;
        }
        if sb.s_free_blocks_count != free_blocks_total
            || sb.s_free_inodes_count != free_inodes_total
        {
            sb.s_free_blocks_count = free_blocks_total;
            sb.s_free_inodes_count = free_inodes_total;
            changed = true;
        }
        if changed {
            let mut b0 = vec![0u8; BS];
            dev.read_block(0, &mut b0).map_err(|_| FsError::IoError)?;
            unsafe { write_struct(&mut b0[SUPERBLOCK_OFFSET..], &sb) };
            dev.write_block(0, &b0).map_err(|_| FsError::IoError)?;
            let gd_size = core::mem::size_of::<Ext2GroupDesc>();
            let table_blocks = ((gds.len() * gd_size) + BS - 1) / BS;
            for tb in 0..table_blocks {
                let blk = (1 + tb) as u64;
                let mut b = vec![0u8; BS];
                dev.read_block(blk * SECTORS_PER_BLOCK, &mut b)
                    .map_err(|_| FsError::IoError)?;
                for (g, gd) in gds.iter().enumerate() {
                    if g * gd_size / BS == tb {
                        unsafe { write_struct(&mut b[(g * gd_size) % BS..], gd) };
                    }
                }
                dev.write_block(blk * SECTORS_PER_BLOCK, &b)
                    .map_err(|_| FsError::IoError)?;
            }
        }
        Ok((sb, gds))
    }

    /// Mount the filesystem: validate the superblock, recover the journal, then
    /// build the `Ext2Fs`. Returns the live filesystem handle.
    pub fn mount_fs(dev: Arc<dyn BlockDevice>) -> Result<Arc<Ext2Fs>, FsError> {
        // 1. Validate superblock + block size.
        let (sb, _gds) = Self::read_sb_gds(&*dev)?;

        // 2. Recover the journal BEFORE building the root (replay committed txns).
        let area = Self::journal_area(&sb);
        let mut journal = Journal::open(dev.clone(), area)?;
        journal.recover()?;

        // 3. Re-read metadata after replay, then repair stale free-space
        // counters from the authoritative bitmaps before exposing the mount.
        let (sb, gds) = Self::read_sb_gds(&*dev)?;
        let (sb, gds) = Self::reconcile_free_counts(&*dev, sb, gds)?;

        Ok(Arc::new(Ext2Fs {
            dev,
            inner: Spinlock::new(Ext2Inner { sb, gds }),
            journal: Spinlock::new(journal),
        }))
    }

    /// Mount and return the root directory `VfsNode` (design entry point).
    pub fn mount(dev: Arc<dyn BlockDevice>) -> Result<Arc<dyn VfsNode>, FsError> {
        let fs = Self::mount_fs(dev)?;
        Ok(fs.root_node())
    }

    /// Build the root directory node (inode 2).
    pub fn root_node(self: &Arc<Self>) -> Arc<dyn VfsNode> {
        Arc::new(Ext2Dir {
            fs: self.clone(),
            ino: EXT2_ROOT_INO,
            name: String::from("/"),
        })
    }

    /// Build a child node by inode/name, choosing dir vs file from `i_mode`.
    fn node_for(fs: &Arc<Ext2Fs>, ino: u32, name: &str) -> Result<Arc<dyn VfsNode>, FsError> {
        let inode = fs.read_inode(ino)?;
        if inode.is_dir() {
            Ok(Arc::new(Ext2Dir {
                fs: fs.clone(),
                ino,
                name: String::from(name),
            }))
        } else {
            Ok(Arc::new(Ext2File {
                fs: fs.clone(),
                ino,
                name: String::from(name),
            }))
        }
    }

    /// Flush is a no-op: every mutation is already durably journaled+checkpointed
    /// at `commit` time. Provided for API completeness.
    pub fn sync(&self) {}
}

// ─── directory + file operations ──────────────────────────────────────────────

impl Ext2Fs {
    /// Enumerate a directory's live entries as `(name, inode)` (excludes free
    /// slots; includes `.`/`..`).
    pub fn read_dir_entries(&self, dir_ino: u32) -> Result<Vec<(String, u32)>, FsError> {
        let inode = self.read_inode(dir_ino)?;
        if !inode.is_dir() {
            return Err(FsError::NotFound);
        }
        let nblocks = (inode.i_size as usize + BS - 1) / BS;
        let mut out = Vec::new();
        for lbn in 0..nblocks as u64 {
            if let Some(blk) = inode::block_for_offset(self, &inode, lbn * BS as u64)? {
                let buf = self.read_fs_block(blk as u64)?;
                for e in dir::iter_entries(&buf)? {
                    if e.inode != 0 {
                        out.push((e.name, e.inode));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Look up `name` in directory `dir_ino`, returning the child inode number.
    pub fn lookup_entry(&self, dir_ino: u32, name: &str) -> Result<u32, FsError> {
        let inode = self.read_inode(dir_ino)?;
        if !inode.is_dir() {
            return Err(FsError::NotFound);
        }
        let nblocks = (inode.i_size as usize + BS - 1) / BS;
        for lbn in 0..nblocks as u64 {
            if let Some(blk) = inode::block_for_offset(self, &inode, lbn * BS as u64)? {
                let buf = self.read_fs_block(blk as u64)?;
                if let Some((ino, _)) = dir::find(&buf, name)? {
                    return Ok(ino);
                }
            }
        }
        Err(FsError::NotFound)
    }

    /// Insert `(name -> child_ino)` into directory `dir_ino`, growing a new dir
    /// block if no existing block has room.
    fn insert_dirent(
        tx: &mut Tx,
        dir_ino: u32,
        name: &str,
        child_ino: u32,
    ) -> Result<(), FsError> {
        if name.as_bytes().len() > 255 {
            return Err(FsError::NameTooLong);
        }
        let mut dinode = tx.read_inode(dir_ino)?;
        let nblocks = (dinode.i_size as usize + BS - 1) / BS;

        // Try existing blocks.
        for lbn in 0..nblocks as u64 {
            let blk = tx.map_or_alloc(&mut dinode, lbn)?;
            let inserted = {
                let buf = tx.block(blk as u64)?;
                dir::insert_into_block(buf, name, child_ino)?
            };
            if inserted {
                tx.write_inode(dir_ino, &dinode)?;
                return Ok(());
            }
        }

        // Grow a new directory block.
        let new_lbn = nblocks as u64;
        let blk = tx.map_or_alloc(&mut dinode, new_lbn)?;
        {
            let buf = tx.block(blk as u64)?;
            dir::init_empty_block(buf);
            let ok = dir::insert_into_block(buf, name, child_ino)?;
            if !ok {
                return Err(FsError::Corrupt);
            }
        }
        dinode.i_size += BS as u32;
        tx.write_inode(dir_ino, &dinode)?;
        Ok(())
    }

    /// Create a regular file or directory named `name` under `parent_ino`.
    /// Returns the new inode number.
    pub fn create(&self, parent_ino: u32, name: &str, is_dir: bool) -> Result<u32, FsError> {
        if name.is_empty() || name == "." || name == ".." {
            return Err(FsError::Corrupt);
        }
        if name.as_bytes().len() > 255 {
            return Err(FsError::NameTooLong);
        }
        // Reject duplicates.
        if self.lookup_entry(parent_ino, name).is_ok() {
            return Err(FsError::AlreadyExists);
        }

        let mut tx = Tx::new(self);
        let new_ino = tx.alloc_new_inode()?;

        let mut inode = Ext2Inode::zeroed();
        if is_dir {
            // Allocate and initialize the new directory's data block.
            let dblock = tx.alloc_zeroed_block()?;
            {
                let buf = tx.block(dblock as u64)?;
                dir::init_dot_entries(buf, new_ino, parent_ino);
            }
            inode.i_mode = S_IFDIR | 0o755;
            inode.i_links_count = 2; // "." + entry in parent
            inode.i_size = BS as u32;
            inode.i_blocks = (BS / 512) as u32;
            inode.i_block[0] = dblock;
        } else {
            inode.i_mode = S_IFREG | 0o644;
            inode.i_links_count = 1;
            inode.i_size = 0;
            inode.i_blocks = 0;
        }
        tx.write_inode(new_ino, &inode)?;

        // Link into the parent directory.
        Self::insert_dirent(&mut tx, parent_ino, name, new_ino)?;

        if is_dir {
            // The child's ".." adds a hard link to the parent; bump used_dirs.
            let mut pinode = tx.read_inode(parent_ino)?;
            pinode.i_links_count += 1;
            tx.write_inode(parent_ino, &pinode)?;
            let g = ((new_ino - 1) / tx.sb.s_inodes_per_group) as usize;
            tx.gds[g].bg_used_dirs_count += 1;
        }

        tx.commit()?;
        Ok(new_ino)
    }

    /// Remove `name` from `parent_ino`, freeing the child inode and its blocks.
    /// Directories must be empty.
    pub fn unlink(&self, parent_ino: u32, name: &str) -> Result<(), FsError> {
        if name == "." || name == ".." {
            return Err(FsError::Corrupt);
        }
        let mut tx = Tx::new(self);
        let mut pinode = tx.read_inode(parent_ino)?;
        if !pinode.is_dir() {
            return Err(FsError::NotFound);
        }

        // Locate the entry's directory block and the child inode.
        let nblocks = (pinode.i_size as usize + BS - 1) / BS;
        let mut found: Option<(u64, u32)> = None; // (dir block, child ino)
        for lbn in 0..nblocks as u64 {
            let blk = tx.map_or_alloc(&mut pinode, lbn)?;
            let hit = {
                let buf = tx.block(blk as u64)?;
                dir::find(buf, name)?
            };
            if let Some((ino, _)) = hit {
                found = Some((blk as u64, ino));
                break;
            }
        }
        let (dir_block, child_ino) = found.ok_or(FsError::NotFound)?;
        let child = tx.read_inode(child_ino)?;

        // Empty-directory check (read committed state; child is unmodified here).
        if child.is_dir() {
            let cblocks = (child.i_size as usize + BS - 1) / BS;
            for lbn in 0..cblocks as u64 {
                if let Some(cb) = inode::block_for_offset(self, &child, lbn * BS as u64)? {
                    let buf = self.read_fs_block(cb as u64)?;
                    if dir::live_entry_count(&buf)? != 0 {
                        return Err(FsError::AlreadyExists); // non-empty directory
                    }
                }
            }
        }

        // Remove the directory entry.
        {
            let buf = tx.block(dir_block as u64)?;
            dir::remove_from_block(buf, name)?;
        }

        // Free the child's blocks and inode.
        tx.free_all_blocks(&child)?;
        tx.free_inode_bit(child_ino)?;

        if child.is_dir() {
            pinode.i_links_count = pinode.i_links_count.saturating_sub(1);
            let g = ((child_ino - 1) / tx.sb.s_inodes_per_group) as usize;
            tx.gds[g].bg_used_dirs_count = tx.gds[g].bg_used_dirs_count.saturating_sub(1);
        }
        tx.write_inode(parent_ino, &pinode)?;

        tx.commit()
    }

    /// Read up to `buf.len()` bytes of file `ino` starting at `offset`,
    /// clamped to `i_size`.
    pub fn read_file(&self, ino: u32, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        let inode = self.read_inode(ino)?;
        let size = inode.i_size as u64;
        if offset >= size {
            return Ok(0);
        }
        let to_read = core::cmp::min(buf.len() as u64, size - offset) as usize;
        let mut done = 0usize;
        let mut pos = offset;
        while done < to_read {
            let within = (pos % BS as u64) as usize;
            let chunk = core::cmp::min(BS - within, to_read - done);
            match inode::block_for_offset(self, &inode, pos)? {
                Some(blk) => {
                    let b = self.read_fs_block(blk as u64)?;
                    buf[done..done + chunk].copy_from_slice(&b[within..within + chunk]);
                }
                None => {
                    for x in &mut buf[done..done + chunk] {
                        *x = 0;
                    }
                }
            }
            done += chunk;
            pos += chunk as u64;
        }
        Ok(to_read)
    }

    /// Truncate a regular file to zero bytes, freeing all direct and indirect
    /// blocks in one journal transaction.
    pub fn truncate_file(&self, ino: u32) -> Result<(), FsError> {
        let mut tx = Tx::new(self);
        let mut inode = tx.read_inode(ino)?;
        if inode.is_dir() { return Err(FsError::Corrupt); }
        tx.free_all_blocks(&inode)?;
        inode.i_size = 0;
        inode.i_blocks = 0;
        inode.i_block = [0; 15];
        tx.write_inode(ino, &inode)?;
        tx.commit()
    }

    /// Write `data` to file `ino` at `offset`, allocating blocks and growing
    /// `i_size` as needed. Each chunk is atomic via the journal.
    ///
    /// STAGE-13.8 WAL-CAP FIX: the journal log holds only `FMT_LOG_BLOCKS`
    /// (64) blocks, and `Journal::commit` rejects any transaction with more
    /// than `log_blocks - 2` dirty blocks (`FsError::OutOfSpace`). The old
    /// single-transaction write therefore made every file larger than
    /// ~248 KiB fail with a bogus "NoSpace" even on a nearly empty disk
    /// (first seen on libc6's gconv/BIG5HKSCS.so, ~450 KiB). Large writes
    /// are now split into multiple transactions of at most `TX_DATA_BLOCKS`
    /// data blocks each; dirty metadata (bitmaps, inode table, indirect
    /// blocks, superblock + group descriptors) stays well under the WAL cap.
    /// Whole-file atomicity is not required here: the installer already
    /// removes partially written files on error (R10.4).
    pub fn write_file(&self, ino: u32, offset: u64, data: &[u8]) -> Result<usize, FsError> {
        if data.is_empty() {
            return Ok(0);
        }
        // STAGE-16.8: 64 data blocks per Tx — data is no longer journaled,
        // so the WAL cap only has to fit the metadata blocks.
        const TX_DATA_BLOCKS: usize = 64;

        let mut written = 0usize;
        let mut pos = offset;
        while written < data.len() {
            let mut tx = Tx::new(self);
            let mut inode = tx.read_inode(ino)?;
            if inode.is_dir() {
                return Err(FsError::Corrupt);
            }

            let mut blocks_touched = 0usize;
            while written < data.len() && blocks_touched < TX_DATA_BLOCKS {
                let lbn = pos / BS as u64;
                let within = (pos % BS as u64) as usize;
                let blk = tx.map_or_alloc(&mut inode, lbn)?;
                let chunk = core::cmp::min(BS - within, data.len() - written);
                {
                    let full = within == 0 && chunk == BS;
                    let b = tx.data_block(blk as u64, full)?;
                    b[within..within + chunk].copy_from_slice(&data[written..written + chunk]);
                }
                written += chunk;
                pos += chunk as u64;
                blocks_touched += 1;
            }
            if pos > inode.i_size as u64 {
                inode.i_size = pos as u32;
            }
            tx.write_inode(ino, &inode)?;
            tx.commit()?;
        }
        Ok(written)
    }
}

// ─── VfsNode adapters ──────────────────────────────────────────────────────────

fn fs_to_vfs(e: FsError) -> VfsError {
    match e {
        FsError::NotFound => VfsError::NotFound,
        FsError::AlreadyExists => VfsError::AlreadyExists,
        FsError::NameTooLong => VfsError::InvalidArgument,
        FsError::OutOfSpace => VfsError::IoError,
        _ => VfsError::IoError,
    }
}

struct Ext2Dir {
    fs: Arc<Ext2Fs>,
    ino: u32,
    name: String,
}

impl VfsNode for Ext2Dir {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_directory(&self) -> bool {
        true
    }
    fn fs_ino(&self) -> u64 {
        self.ino as u64
    }
    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        let entries = self.fs.read_dir_entries(self.ino).map_err(fs_to_vfs)?;
        let mut out = Vec::new();
        for (name, ino) in entries {
            if name == "." || name == ".." {
                continue;
            }
            let node = Ext2Fs::node_for(&self.fs, ino, &name).map_err(fs_to_vfs)?;
            out.push(node);
        }
        Ok(out)
    }
    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        let ino = self.fs.lookup_entry(self.ino, name).map_err(fs_to_vfs)?;
        Ext2Fs::node_for(&self.fs, ino, name).map_err(fs_to_vfs)
    }
    fn create_dir(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        let ino = self.fs.create(self.ino, name, true).map_err(fs_to_vfs)?;
        Ext2Fs::node_for(&self.fs, ino, name).map_err(fs_to_vfs)
    }
    fn create_file(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        let ino = self.fs.create(self.ino, name, false).map_err(fs_to_vfs)?;
        Ext2Fs::node_for(&self.fs, ino, name).map_err(fs_to_vfs)
    }
    fn remove(&self, name: &str) -> VfsResult<()> {
        self.fs.unlink(self.ino, name).map_err(fs_to_vfs)
    }
    fn sync(&self) {
        self.fs.sync()
    }
    fn size(&self) -> u64 {
        self.fs.read_inode(self.ino).map(|i| i.i_size as u64).unwrap_or(0)
    }
}

struct Ext2File {
    fs: Arc<Ext2Fs>,
    ino: u32,
    name: String,
}

impl VfsNode for Ext2File {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn fs_ino(&self) -> u64 {
        self.ino as u64
    }
    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.fs.read_file(self.ino, offset, buf).map_err(fs_to_vfs)
    }
    fn write(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.fs.write_file(self.ino, offset, buf).map_err(fs_to_vfs)
    }
    fn truncate(&self, size: u64) -> VfsResult<()> {
        if size != 0 { return Err(VfsError::NotSupported); }
        self.fs.truncate_file(self.ino).map_err(fs_to_vfs)
    }
    fn size(&self) -> u64 {
        self.fs.read_inode(self.ino).map(|i| i.i_size as u64).unwrap_or(0)
    }
}
