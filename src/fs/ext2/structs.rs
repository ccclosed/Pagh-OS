//! ext2 + WAL journal on-disk `#[repr(C)]` structures, byte<->struct helpers,
//! and an inline CRC32.
//!
//! All structures use the canonical ext2 field names/offsets so a freshly
//! formatted image is host-mountable as plain ext2. The kernel target is
//! x86_64 (little-endian) and ext2 is a little-endian format, so the in-memory
//! `#[repr(C)]` byte layout equals the on-disk layout: we read/write whole
//! structs to byte buffers with unaligned copies (`read_struct`/`write_struct`),
//! which avoids creating misaligned references while preserving exact bytes.
//!
//! `const _: () = assert!(...)` size checks lock the struct sizes (e.g. the
//! 128-byte inode, 8-byte dir-entry header, 32-byte group descriptor).

#![allow(dead_code)]

/// Filesystem block size in bytes. `s_log_block_size = 2` -> `1024 << 2 = 4096`.
pub const BS: usize = 4096;

/// 512-byte device sectors per 4096-byte FS block.
pub const SECTORS_PER_BLOCK: u64 = (BS / 512) as u64;

/// Classic ext2 inode size for `s_rev_level == 1` (bytes).
pub const INODE_SIZE: usize = 128;

/// ext2 magic, stored at superblock offset 0x38.
pub const EXT2_MAGIC: u16 = 0xEF53;

/// Root directory inode number.
pub const EXT2_ROOT_INO: u32 = 2;

/// First non-reserved inode.
pub const EXT2_FIRST_INO: u32 = 11;

/// Number of `u32` pointers in one indirect block (`BS / 4 == 1024`).
pub const PTRS_PER_BLOCK: u32 = (BS / 4) as u32;

/// Inode mode bits.
pub const S_IFREG: u16 = 0x8000;
pub const S_IFDIR: u16 = 0x4000;

/// Journal superblock magic ("PAGHJNL\1") — OUR format, not jbd2.
pub const JNL_MAGIC: u64 = 0x5041_4748_4A4E_4C01;
/// Journal descriptor magic ("JDES").
pub const JDES_MAGIC: u32 = 0x4A44_4553;
/// Journal commit magic ("JCMT").
pub const JCMT_MAGIC: u32 = 0x4A43_4D54;

// ─── ext2 superblock (lives at byte offset 1024) ────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Ext2SuperBlock {
    pub s_inodes_count: u32,        // 0x00
    pub s_blocks_count: u32,        // 0x04
    pub s_r_blocks_count: u32,      // 0x08
    pub s_free_blocks_count: u32,   // 0x0C
    pub s_free_inodes_count: u32,   // 0x10
    pub s_first_data_block: u32,    // 0x14
    pub s_log_block_size: u32,      // 0x18
    pub s_log_frag_size: u32,       // 0x1C
    pub s_blocks_per_group: u32,    // 0x20
    pub s_frags_per_group: u32,     // 0x24
    pub s_inodes_per_group: u32,    // 0x28
    pub s_mtime: u32,               // 0x2C
    pub s_wtime: u32,               // 0x30
    pub s_mnt_count: u16,           // 0x34
    pub s_max_mnt_count: u16,       // 0x36
    pub s_magic: u16,               // 0x38 = 0xEF53
    pub s_state: u16,               // 0x3A
    pub s_errors: u16,              // 0x3C
    pub s_minor_rev_level: u16,     // 0x3E
    pub s_lastcheck: u32,           // 0x40
    pub s_checkinterval: u32,       // 0x44
    pub s_creator_os: u32,          // 0x48
    pub s_rev_level: u32,           // 0x4C
    pub s_def_resuid: u16,          // 0x50
    pub s_def_resgid: u16,          // 0x52
    pub s_first_ino: u32,           // 0x54
    pub s_inode_size: u16,          // 0x58
    pub s_block_group_nr: u16,      // 0x5A
    pub s_feature_compat: u32,      // 0x5C
    pub s_feature_incompat: u32,    // 0x60
    pub s_feature_ro_compat: u32,   // 0x64
    pub s_uuid: [u8; 16],           // 0x68
    pub s_volume_name: [u8; 16],    // 0x78
}

const _: () = assert!(core::mem::size_of::<Ext2SuperBlock>() == 0x88);
// Field-offset locks for the host-critical fields.
const _: () = assert!(core::mem::offset_of!(Ext2SuperBlock, s_log_block_size) == 0x18);
const _: () = assert!(core::mem::offset_of!(Ext2SuperBlock, s_magic) == 0x38);
const _: () = assert!(core::mem::offset_of!(Ext2SuperBlock, s_inode_size) == 0x58);
const _: () = assert!(core::mem::offset_of!(Ext2SuperBlock, s_feature_incompat) == 0x60);

// ─── ext2 block group descriptor (32 bytes) ─────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Ext2GroupDesc {
    pub bg_block_bitmap: u32,
    pub bg_inode_bitmap: u32,
    pub bg_inode_table: u32,
    pub bg_free_blocks_count: u16,
    pub bg_free_inodes_count: u16,
    pub bg_used_dirs_count: u16,
    pub bg_pad: u16,
    pub bg_reserved: [u8; 12],
}

const _: () = assert!(core::mem::size_of::<Ext2GroupDesc>() == 32);

// ─── ext2 inode (128 bytes) ──────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Ext2Inode {
    pub i_mode: u16,
    pub i_uid: u16,
    pub i_size: u32,
    pub i_atime: u32,
    pub i_ctime: u32,
    pub i_mtime: u32,
    pub i_dtime: u32,
    pub i_gid: u16,
    pub i_links_count: u16,
    pub i_blocks: u32,          // count of 512-byte sectors
    pub i_flags: u32,
    pub i_osd1: u32,
    pub i_block: [u32; 15],     // 0..=11 direct, 12 single, 13 double, 14 triple
    pub i_generation: u32,
    pub i_file_acl: u32,
    pub i_dir_acl: u32,
    pub i_faddr: u32,
    pub i_osd2: [u8; 12],
}

const _: () = assert!(core::mem::size_of::<Ext2Inode>() == 128);
const _: () = assert!(core::mem::offset_of!(Ext2Inode, i_block) == 40);

impl Ext2Inode {
    pub const fn zeroed() -> Self {
        Ext2Inode {
            i_mode: 0, i_uid: 0, i_size: 0, i_atime: 0, i_ctime: 0, i_mtime: 0,
            i_dtime: 0, i_gid: 0, i_links_count: 0, i_blocks: 0, i_flags: 0,
            i_osd1: 0, i_block: [0; 15], i_generation: 0, i_file_acl: 0,
            i_dir_acl: 0, i_faddr: 0, i_osd2: [0; 12],
        }
    }
    pub fn is_dir(&self) -> bool { (self.i_mode & 0xF000) == S_IFDIR }
    pub fn is_reg(&self) -> bool { (self.i_mode & 0xF000) == S_IFREG }
}

// ─── ext2 directory entry header (8 bytes; name follows) ─────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Ext2DirEntryHeader {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
}

const _: () = assert!(core::mem::size_of::<Ext2DirEntryHeader>() == 8);

// ─── journal structures ──────────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct JournalSuper {
    pub magic: u64,
    pub head: u64,
    pub tail: u64,
    pub next_seq: u64,
    pub log_blocks: u64,
    pub fs_blocks: u64,
    pub checksum: u32,
}

const _: () = assert!(core::mem::size_of::<JournalSuper>() == 56);

/// Maximum data blocks a single descriptor can describe.
pub const JDESC_MAX_TARGETS: usize = 254;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct JournalDescriptor {
    pub magic: u32,
    pub seq: u64,
    pub count: u32,
    pub targets: [u64; JDESC_MAX_TARGETS],
}

const _: () = assert!(core::mem::size_of::<JournalDescriptor>() <= BS);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct JournalCommit {
    pub magic: u32,
    pub seq: u64,
    pub data_checksum: u32,
    pub _pad: u32,
}

const _: () = assert!(core::mem::size_of::<JournalCommit>() == 24);

// ─── byte <-> struct helpers ──────────────────────────────────────────────────

/// Read a `Copy` POD struct out of a byte buffer with an unaligned load.
///
/// # Safety
/// `bytes.len()` must be at least `size_of::<T>()`, and the bytes must be a
/// valid bit-pattern for `T` (always true for our integer/array POD structs).
pub unsafe fn read_struct<T: Copy>(bytes: &[u8]) -> T {
    debug_assert!(bytes.len() >= core::mem::size_of::<T>());
    core::ptr::read_unaligned(bytes.as_ptr() as *const T)
}

/// Write a `Copy` POD struct into a byte buffer with an unaligned store.
///
/// # Safety
/// `bytes.len()` must be at least `size_of::<T>()`.
pub unsafe fn write_struct<T: Copy>(bytes: &mut [u8], value: &T) {
    debug_assert!(bytes.len() >= core::mem::size_of::<T>());
    core::ptr::write_unaligned(bytes.as_mut_ptr() as *mut T, *value);
}

/// Read a little-endian `u32` at `off`.
pub fn read_u32(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

/// Write a little-endian `u32` at `off`.
pub fn write_u32(bytes: &mut [u8], off: usize, v: u32) {
    bytes[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

// ─── inline CRC32 (IEEE 802.3, reflected poly 0xEDB88320) ────────────────────

/// Compute the standard CRC32 (poly 0xEDB88320) of `data`.
///
/// Known answer: `crc32(b"123456789") == 0xCBF4_3926`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        let mut k = 0;
        while k < 8 {
            // Branchless: subtract 1-bit mask, AND with the polynomial.
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            k += 1;
        }
    }
    !crc
}

/// CRC32 over a sequence of byte slices (used for multi-block journal data).
pub fn crc32_slices(slices: &[&[u8]]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for s in slices {
        for &b in *s {
            crc ^= b as u32;
            let mut k = 0;
            while k < 8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
                k += 1;
            }
        }
    }
    !crc
}

/// `align4(n)` rounds `n` up to a multiple of 4.
#[inline]
pub fn align4(n: usize) -> usize {
    (n + 3) & !3
}
