//! On-disk `#[repr(C)]`-структуры ext2 и журнала WAL, помощники байты↔структура
//! и встроенный CRC32.
//!
//! Все структуры используют канонические имена/смещения полей ext2, поэтому
//! свежеотформатированный образ маунтится на хосте как обычный ext2. Цель ядра —
//! x86_64 (little-endian), ext2 тоже little-endian, так что байтовая раскладка
//! `#[repr(C)]` в памяти совпадает с on-disk: целые структуры читаются и
//! пишутся в байтовые буферы невыровненными копиями (`read_struct`/
//! `write_struct`) — без создания невыровненных ссылок и с сохранением байтов.
//!
//! Проверки `const _: () = assert!(...)` фиксируют размеры структур (inode
//! 128 байт, заголовок dirent 8 байт, групповой дескриптор 32 байта).

#![allow(dead_code)]

/// Размер блока ФС в байтах. `s_log_block_size = 2` -> `1024 << 2 = 4096`.
pub const BS: usize = 4096;

/// 512-байтных секторов устройства в 4096-байтном блоке ФС.
pub const SECTORS_PER_BLOCK: u64 = (BS / 512) as u64;

/// Классический размер inode ext2 для `s_rev_level == 1` (байт).
pub const INODE_SIZE: usize = 128;

/// Действующий on-disk размер inode для смонтированного суперблока.
///
/// Образы с `s_rev_level >= 1` могут объявлять в `s_inode_size` больший
/// (степень двойки) размер — например, 256; арифметика адресов обязана его
/// учитывать, иначе любой доступ к таблице inode попадает не в тот слот. У
/// образов rev-0 всегда inode по 128 байт, а испорченному `s_inode_size` нельзя
/// доверять — откат к 128.
pub fn inode_size(sb: &Ext2SuperBlock) -> usize {
    let v = sb.s_inode_size as usize;
    if sb.s_rev_level == 0 || v < INODE_SIZE || v > BS || !v.is_power_of_two() {
        return INODE_SIZE;
    }
    v
}

/// Магия ext2 по смещению 0x38 в суперблоке.
pub const EXT2_MAGIC: u16 = 0xEF53;

/// Номер inode корневого каталога.
pub const EXT2_ROOT_INO: u32 = 2;

/// Первый нерезервированный inode.
pub const EXT2_FIRST_INO: u32 = 11;

/// Число `u32`-указателей в одном indirect-блоке (`BS / 4 == 1024`).
pub const PTRS_PER_BLOCK: u32 = (BS / 4) as u32;

/// Биты режима inode.
pub const S_IFREG: u16 = 0x8000;
pub const S_IFDIR: u16 = 0x4000;
pub const S_IFLNK: u16 = 0xA000;

/// Магия суперблока журнала ("PAGHJNL\1") — **свой** формат, не jbd2.
pub const JNL_MAGIC: u64 = 0x5041_4748_4A4E_4C01;
/// Магия дескриптора журнала ("JDES").
pub const JDES_MAGIC: u32 = 0x4A44_4553;
/// Магия коммита журнала ("JCMT").
pub const JCMT_MAGIC: u32 = 0x4A43_4D54;

// ─── суперблок ext2 (по смещению 1024 байт) ─────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Ext2SuperBlock {
    pub s_inodes_count: u32,      // 0x00
    pub s_blocks_count: u32,      // 0x04
    pub s_r_blocks_count: u32,    // 0x08
    pub s_free_blocks_count: u32, // 0x0C
    pub s_free_inodes_count: u32, // 0x10
    pub s_first_data_block: u32,  // 0x14
    pub s_log_block_size: u32,    // 0x18
    pub s_log_frag_size: u32,     // 0x1C
    pub s_blocks_per_group: u32,  // 0x20
    pub s_frags_per_group: u32,   // 0x24
    pub s_inodes_per_group: u32,  // 0x28
    pub s_mtime: u32,             // 0x2C
    pub s_wtime: u32,             // 0x30
    pub s_mnt_count: u16,         // 0x34
    pub s_max_mnt_count: u16,     // 0x36
    pub s_magic: u16,             // 0x38 = 0xEF53
    pub s_state: u16,             // 0x3A
    pub s_errors: u16,            // 0x3C
    pub s_minor_rev_level: u16,   // 0x3E
    pub s_lastcheck: u32,         // 0x40
    pub s_checkinterval: u32,     // 0x44
    pub s_creator_os: u32,        // 0x48
    pub s_rev_level: u32,         // 0x4C
    pub s_def_resuid: u16,        // 0x50
    pub s_def_resgid: u16,        // 0x52
    pub s_first_ino: u32,         // 0x54
    pub s_inode_size: u16,        // 0x58
    pub s_block_group_nr: u16,    // 0x5A
    pub s_feature_compat: u32,    // 0x5C
    pub s_feature_incompat: u32,  // 0x60
    pub s_feature_ro_compat: u32, // 0x64
    pub s_uuid: [u8; 16],         // 0x68
    pub s_volume_name: [u8; 16],  // 0x78
}

const _: () = assert!(core::mem::size_of::<Ext2SuperBlock>() == 0x88);
// Фиксация смещений полей, критичных для host-тестов.
const _: () = assert!(core::mem::offset_of!(Ext2SuperBlock, s_log_block_size) == 0x18);
const _: () = assert!(core::mem::offset_of!(Ext2SuperBlock, s_magic) == 0x38);
const _: () = assert!(core::mem::offset_of!(Ext2SuperBlock, s_inode_size) == 0x58);
const _: () = assert!(core::mem::offset_of!(Ext2SuperBlock, s_feature_incompat) == 0x60);

// ─── групповой дескриптор блоков ext2 (32 байта) ────────────────────────────

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

// ─── inode ext2 (128 байт) ──────────────────────────────────────────────────

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
    pub i_blocks: u32, // число 512-байтных секторов
    pub i_flags: u32,
    pub i_osd1: u32,
    pub i_block: [u32; 15], // 0..=11 direct, 12 single, 13 double, 14 triple
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
            i_mode: 0,
            i_uid: 0,
            i_size: 0,
            i_atime: 0,
            i_ctime: 0,
            i_mtime: 0,
            i_dtime: 0,
            i_gid: 0,
            i_links_count: 0,
            i_blocks: 0,
            i_flags: 0,
            i_osd1: 0,
            i_block: [0; 15],
            i_generation: 0,
            i_file_acl: 0,
            i_dir_acl: 0,
            i_faddr: 0,
            i_osd2: [0; 12],
        }
    }
    pub fn is_dir(&self) -> bool {
        (self.i_mode & 0xF000) == S_IFDIR
    }
    pub fn is_reg(&self) -> bool {
        (self.i_mode & 0xF000) == S_IFREG
    }
    /// Симлинк: цель лежит inline в `i_block` (fast, `i_blocks == 0`) либо в
    /// одном блоке данных (slow). Расчёт формы — в `super::symlink`.
    pub fn is_symlink(&self) -> bool {
        (self.i_mode & 0xF000) == S_IFLNK
    }
}

// ─── заголовок записи каталога ext2 (8 байт; далее имя) ──────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Ext2DirEntryHeader {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
}

const _: () = assert!(core::mem::size_of::<Ext2DirEntryHeader>() == 8);

// ─── структуры журнала ──────────────────────────────────────────────────────

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

/// Максимум data-блоков, которые описывает один дескриптор.
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

// ─── помощники байты <-> структура ──────────────────────────────────────────

/// Читает `Copy`-POD-структуру из байтового буфера невыровненной загрузкой.
///
/// # Safety
/// `bytes.len()` не меньше `size_of::<T>()`, а байты — валидный битовый образ
/// `T` (для наших целочисленных/массивных POD-структур это всегда так).
pub unsafe fn read_struct<T: Copy>(bytes: &[u8]) -> T {
    debug_assert!(bytes.len() >= core::mem::size_of::<T>());
    core::ptr::read_unaligned(bytes.as_ptr() as *const T)
}

/// Пишет `Copy`-POD-структуру в байтовый буфер невыровненным сохранением.
///
/// # Safety
/// `bytes.len()` не меньше `size_of::<T>()`.
pub unsafe fn write_struct<T: Copy>(bytes: &mut [u8], value: &T) {
    debug_assert!(bytes.len() >= core::mem::size_of::<T>());
    core::ptr::write_unaligned(bytes.as_mut_ptr() as *mut T, *value);
}

/// Читает little-endian `u32` по смещению `off`.
pub fn read_u32(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

/// Пишет little-endian `u32` по смещению `off`.
pub fn write_u32(bytes: &mut [u8], off: usize, v: u32) {
    bytes[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

// ─── встроенный CRC32 (IEEE 802.3, отражённый полином 0xEDB88320) ───────────

/// Считает стандартный CRC32 (полином 0xEDB88320) от `data`.
///
/// Известный ответ: `crc32(b"123456789") == 0xCBF4_3926`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        let mut k = 0;
        while k < 8 {
            // Без ветвлений: маска из младшего бита, AND с полиномом.
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            k += 1;
        }
    }
    !crc
}

/// CRC32 по последовательности байтовых срезов (для многоблочных данных журнала).
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

/// `align4(n)` округляет `n` вверх до кратного 4.
#[inline]
pub fn align4(n: usize) -> usize {
    (n + 3) & !3
}
