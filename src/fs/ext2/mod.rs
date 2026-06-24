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
    gd: Ext2GroupDesc,
}

/// A mounted ext2 filesystem over a `BlockDevice`, with a WAL journal.
pub struct Ext2Fs {
    dev: Arc<dyn BlockDevice>,
    inner: Spinlock<Ext2Inner>,
    journal: Spinlock<Journal>,
}

fn inode_location(gd: &Ext2GroupDesc, ino: u32) -> (u64, usize) {
    let index = (ino - 1) as u64;
    let ipb = (BS / INODE_SIZE) as u64; // inodes per block (32)
    let block = gd.bg_inode_table as u64 + index / ipb;
    let off = (index % ipb) as usize * INODE_SIZE;
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
        let gd = self.inner.lock().gd;
        let (block, off) = inode_location(&gd, ino);
        let buf = self.read_fs_block(block)?;
        Ok(unsafe { read_struct::<Ext2Inode>(&buf[off..]) })
    }

    pub fn superblock(&self) -> Ext2SuperBlock {
        self.inner.lock().sb
    }

    pub fn group_desc(&self) -> Ext2GroupDesc {
        self.inner.lock().gd
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
    gd: Ext2GroupDesc,
    dirty: BTreeMap<u64, Vec<u8>>,
}

impl<'a> Tx<'a> {
    fn new(fs: &'a Ext2Fs) -> Self {
        let inner = fs.inner.lock();
        Tx {
            fs,
            sb: inner.sb,
            gd: inner.gd,
            dirty: BTreeMap::new(),
        }
    }

    fn block(&mut self, blk: u64) -> Result<&mut Vec<u8>, FsError> {
        if !self.dirty.contains_key(&blk) {
            let data = self.fs.read_fs_block(blk)?;
            self.dirty.insert(blk, data);
        }
        Ok(self.dirty.get_mut(&blk).unwrap())
    }

    fn alloc_zeroed_block(&mut self) -> Result<u32, FsError> {
        let bbm = self.gd.bg_block_bitmap as u64;
        self.block(bbm)?;
        let mut sb = self.sb;
        let mut gd = self.gd;
        let blk = {
            let bm = self.dirty.get_mut(&bbm).unwrap();
            alloc::alloc_block(bm, &mut sb, &mut gd)
        };
        self.sb = sb;
        self.gd = gd;
        let blk = blk.ok_or(FsError::OutOfSpace)?;
        let b = self.block(blk as u64)?;
        for x in b.iter_mut() {
            *x = 0;
        }
        Ok(blk)
    }

    fn free_data_block(&mut self, blk: u32) -> Result<(), FsError> {
        let bbm = self.gd.bg_block_bitmap as u64;
        self.block(bbm)?;
        let mut sb = self.sb;
        let mut gd = self.gd;
        {
            let bm = self.dirty.get_mut(&bbm).unwrap();
            alloc::free_block(bm, &mut sb, &mut gd, blk);
        }
        self.sb = sb;
        self.gd = gd;
        Ok(())
    }

    fn alloc_new_inode(&mut self) -> Result<u32, FsError> {
        let ibm = self.gd.bg_inode_bitmap as u64;
        self.block(ibm)?;
        let mut sb = self.sb;
        let mut gd = self.gd;
        let ino = {
            let bm = self.dirty.get_mut(&ibm).unwrap();
            alloc::alloc_inode(bm, &mut sb, &mut gd)
        };
        self.sb = sb;
        self.gd = gd;
        ino.ok_or(FsError::OutOfSpace)
    }

    fn free_inode_bit(&mut self, ino: u32) -> Result<(), FsError> {
        let ibm = self.gd.bg_inode_bitmap as u64;
        self.block(ibm)?;
        let mut sb = self.sb;
        let mut gd = self.gd;
        {
            let bm = self.dirty.get_mut(&ibm).unwrap();
            alloc::free_inode(bm, &mut sb, &mut gd, ino);
        }
        self.sb = sb;
        self.gd = gd;
        Ok(())
    }

    fn read_inode(&mut self, ino: u32) -> Result<Ext2Inode, FsError> {
        let (block, off) = inode_location(&self.gd, ino);
        self.block(block)?;
        let buf = self.dirty.get(&block).unwrap();
        Ok(unsafe { read_struct::<Ext2Inode>(&buf[off..]) })
    }

    fn write_inode(&mut self, ino: u32, inode: &Ext2Inode) -> Result<(), FsError> {
        let (block, off) = inode_location(&self.gd, ino);
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

    /// Commit: patch the superblock + group descriptor into their blocks, then
    /// hand every dirty block to the journal as one atomic transaction. On
    /// success the in-memory `sb`/`gd` are published.
    fn commit(mut self) -> Result<(), FsError> {
        // Patch superblock (block 0 @ offset 1024) and group descriptor (block 1).
        {
            let sb = self.sb;
            let b0 = self.block(0)?;
            unsafe { write_struct(&mut b0[SUPERBLOCK_OFFSET..], &sb) };
        }
        {
            let gd = self.gd;
            let b1 = self.block(1)?;
            unsafe { write_struct(&mut b1[0..], &gd) };
        }

        // Hand all dirty blocks to the journal as one transaction.
        {
            let mut j = self.fs.journal.lock();
            let mut txn = j.begin();
            for (blk, data) in self.dirty.iter() {
                j.log_block(&mut txn, *blk, data);
            }
            j.commit(txn)?;
        }

        // Publish the new superblock / group descriptor.
        let mut inner = self.fs.inner.lock();
        inner.sb = self.sb;
        inner.gd = self.gd;
        Ok(())
    }
}

// ─── format ───────────────────────────────────────────────────────────────────

impl Ext2Fs {
    /// Produce a fresh, host-mountable ext2 image plus an empty WAL journal.
    pub fn format(dev: Arc<dyn BlockDevice>) -> Result<(), FsError> {
        // ── Derive sizing from the real device capacity (R7.1) ──
        // Device capacity in 4 KiB FS blocks.
        let device_blocks = dev.sector_count() / SECTORS_PER_BLOCK;

        // ── Minimum-capacity guard (R7.4) ──
        // Reject a device that cannot hold the smallest valid ext2 layout plus
        // the WAL journal BEFORE deriving sizing or writing anything, so a
        // partially-written corrupt image is never produced. The minimum
        // layout is the fixed metadata blocks (block 0 superblock, block 1
        // group descriptor, block 2 block bitmap, block 3 inode bitmap), an
        // inode table sized for MIN_INODES, the root directory block, at least
        // one allocatable data block, plus the journal reserve at the tail.
        let min_inode_table_blocks =
            ((MIN_INODES as usize * INODE_SIZE) + BS - 1) / BS; // ceil
        let min_ext2_blocks = 4u64                       // superblock + group desc + 2 bitmaps
            + min_inode_table_blocks as u64              // inode table for MIN_INODES
            + 1                                          // root directory block
            + 1; // at least one allocatable data block
        let min_layout_blocks = min_ext2_blocks + JOURNAL_RESERVE_BLOCKS;
        if device_blocks < min_layout_blocks {
            return Err(FsError::OutOfSpace);
        }

        // Reserve the WAL journal region (journal superblock + circular log) at
        // the device tail; the ext2 region occupies the blocks before it.
        let data_blocks = device_blocks.saturating_sub(JOURNAL_RESERVE_BLOCKS);

        // Clamp the ext2 region to one block group (a single bitmap block) so
        // every per-group on-disk count stays within its field range (R7.2).
        let total_blocks_u64 = core::cmp::min(data_blocks, MAX_GROUP_BLOCKS as u64);
        let total_blocks = total_blocks_u64 as u32;

        // Scale the inode count from the data area (one inode per
        // BYTES_PER_INODE), floored at MIN_INODES and clamped to the
        // single-group bitmap capacity so `bg_free_inodes_count` (u16) fits.
        let scaled_inodes = (total_blocks_u64 * BS as u64) / BYTES_PER_INODE;
        let total_inodes = scaled_inodes
            .max(MIN_INODES as u64)
            .min(MAX_GROUP_INODES as u64) as u32;

        let inode_table_blocks =
            ((total_inodes as usize * INODE_SIZE) + BS - 1) / BS; // ceil
        let block_bitmap_block = 2u32;
        let inode_bitmap_block = 3u32;
        let inode_table_start = 4u32;
        let root_dir_block = inode_table_start + inode_table_blocks as u32; // first data block

        // Blocks used by metadata + root dir: [0 .. root_dir_block].
        let used_blocks = root_dir_block + 1;

        // Underflow safety net for the `total_blocks - used_blocks` free-count
        // computation below. The up-front minimum-capacity guard (R7.4)
        // already rejects devices too small to hold the layout; this retains a
        // defensive check in case clamping leaves no allocatable data block.
        if total_blocks <= used_blocks {
            return Err(FsError::OutOfSpace);
        }

        let reserved_inodes = EXT2_FIRST_INO - 1; // inodes 1..=10 marked used

        let sb = Ext2SuperBlock {
            s_inodes_count: total_inodes,
            s_blocks_count: total_blocks,
            s_r_blocks_count: 0,
            s_free_blocks_count: total_blocks - used_blocks,
            s_free_inodes_count: total_inodes - reserved_inodes,
            s_first_data_block: 0,
            s_log_block_size: 2,
            s_log_frag_size: 2,
            s_blocks_per_group: total_blocks,
            s_frags_per_group: total_blocks,
            s_inodes_per_group: total_inodes,
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

        let gd = Ext2GroupDesc {
            bg_block_bitmap: block_bitmap_block,
            bg_inode_bitmap: inode_bitmap_block,
            bg_inode_table: inode_table_start,
            // Single group: per-group totals equal the superblock totals. The
            // MAX_GROUP_* clamp guarantees these `u16` casts never truncate.
            bg_free_blocks_count: (total_blocks - used_blocks) as u16,
            bg_free_inodes_count: (total_inodes - reserved_inodes) as u16,
            bg_used_dirs_count: 1, // root
            bg_pad: 0,
            bg_reserved: [0; 12],
        };

        // Zero the whole ext2 region first (clean bitmaps, inode table, data).
        let zero = vec![0u8; BS];
        for b in 0..total_blocks as u64 {
            Self::write_fs_block_direct(&*dev, b, &zero)?;
        }

        // Block bitmap: mark blocks [0, used_blocks) used.
        let mut block_bitmap = vec![0u8; BS];
        for b in 0..used_blocks {
            alloc::set_bit(&mut block_bitmap, b);
        }
        Self::write_fs_block_direct(&*dev, block_bitmap_block as u64, &block_bitmap)?;

        // Inode bitmap: mark inodes 1..=reserved_inodes used.
        let mut inode_bitmap = vec![0u8; BS];
        for i in 0..reserved_inodes {
            alloc::set_bit(&mut inode_bitmap, i);
        }
        Self::write_fs_block_direct(&*dev, inode_bitmap_block as u64, &inode_bitmap)?;

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

        let (rblock, roff) = inode_location(&gd, EXT2_ROOT_INO);
        let mut itbuf = vec![0u8; BS];
        // The inode-table block was just zeroed on disk; read it (all zero) then
        // patch the root inode in.
        unsafe { write_struct(&mut itbuf[roff..], &root_inode) };
        Self::write_fs_block_direct(&*dev, rblock, &itbuf)?;

        // Superblock @ offset 1024 in block 0.
        let mut block0 = vec![0u8; BS];
        unsafe { write_struct(&mut block0[SUPERBLOCK_OFFSET..], &sb) };
        Self::write_fs_block_direct(&*dev, 0, &block0)?;

        // Group descriptor table @ block 1.
        let mut block1 = vec![0u8; BS];
        unsafe { write_struct(&mut block1[0..], &gd) };
        Self::write_fs_block_direct(&*dev, 1, &block1)?;

        // Empty WAL journal in the reserved region after the ext2 area.
        let area = Self::journal_area(&sb);
        Journal::format(&*dev, area)?;
        Ok(())
    }
}

// ─── mount ────────────────────────────────────────────────────────────────────

impl Ext2Fs {
    fn read_sb_gd(dev: &dyn BlockDevice) -> Result<(Ext2SuperBlock, Ext2GroupDesc), FsError> {
        let mut b0 = vec![0u8; BS];
        dev.read_block(0, &mut b0).map_err(|_| FsError::IoError)?;
        let sb: Ext2SuperBlock = unsafe { read_struct(&b0[SUPERBLOCK_OFFSET..]) };
        if sb.s_magic != EXT2_MAGIC {
            return Err(FsError::BadSuperBlock);
        }
        if (1024usize << sb.s_log_block_size) != BS {
            return Err(FsError::BadSuperBlock);
        }
        let mut b1 = vec![0u8; BS];
        dev.read_block(1 * SECTORS_PER_BLOCK, &mut b1)
            .map_err(|_| FsError::IoError)?;
        let gd: Ext2GroupDesc = unsafe { read_struct(&b1[0..]) };
        Ok((sb, gd))
    }

    /// Mount the filesystem: validate the superblock, recover the journal, then
    /// build the `Ext2Fs`. Returns the live filesystem handle.
    pub fn mount_fs(dev: Arc<dyn BlockDevice>) -> Result<Arc<Ext2Fs>, FsError> {
        // 1. Validate superblock + block size.
        let (sb, _gd0) = Self::read_sb_gd(&*dev)?;

        // 2. Recover the journal BEFORE building the root (replay committed txns).
        let area = Self::journal_area(&sb);
        let mut journal = Journal::open(dev.clone(), area)?;
        journal.recover()?;

        // 3. Re-read sb + gd now that the journal has been applied.
        let (sb, gd) = Self::read_sb_gd(&*dev)?;

        Ok(Arc::new(Ext2Fs {
            dev,
            inner: Spinlock::new(Ext2Inner { sb, gd }),
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
            tx.gd.bg_used_dirs_count += 1;
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
            tx.gd.bg_used_dirs_count = tx.gd.bg_used_dirs_count.saturating_sub(1);
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

    /// Write `data` to file `ino` at `offset`, allocating blocks and growing
    /// `i_size` as needed. Atomic via the journal.
    pub fn write_file(&self, ino: u32, offset: u64, data: &[u8]) -> Result<usize, FsError> {
        if data.is_empty() {
            return Ok(0);
        }
        let mut tx = Tx::new(self);
        let mut inode = tx.read_inode(ino)?;
        if inode.is_dir() {
            return Err(FsError::Corrupt);
        }

        let mut written = 0usize;
        let mut pos = offset;
        while written < data.len() {
            let lbn = pos / BS as u64;
            let within = (pos % BS as u64) as usize;
            let blk = tx.map_or_alloc(&mut inode, lbn)?;
            let chunk = core::cmp::min(BS - within, data.len() - written);
            {
                let b = tx.block(blk as u64)?;
                b[within..within + chunk].copy_from_slice(&data[written..written + chunk]);
            }
            written += chunk;
            pos += chunk as u64;
        }
        if pos > inode.i_size as u64 {
            inode.i_size = pos as u32;
        }
        tx.write_inode(ino, &inode)?;
        tx.commit()?;
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
    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.fs.read_file(self.ino, offset, buf).map_err(fs_to_vfs)
    }
    fn write(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.fs.write_file(self.ino, offset, buf).map_err(fs_to_vfs)
    }
    fn size(&self) -> u64 {
        self.fs.read_inode(self.ino).map(|i| i.i_size as u64).unwrap_or(0)
    }
}
