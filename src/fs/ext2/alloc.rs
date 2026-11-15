//! Аллокация ext2 по битмапам: выделение и освобождение блоков/inode по
//! младшему свободному биту с синхронизацией счётчиков в групповом дескрипторе
//! и суперблоке.
//!
//! Номера inode — с единицы (бит `k` битмапа inode == inode `k + 1`). Номера
//! блоков отсчитываются от `s_first_data_block` (для блоков 4 KiB он равен 0,
//! поэтому бит `k` битмапа блоков == блок `k`).

#![allow(dead_code)]

use super::structs::{Ext2GroupDesc, Ext2SuperBlock, BS};

/// Находит и ставит младший свободный бит в `bitmap`, возвращая его индекс.
///
/// `None`, когда в адресуемом диапазоне выставлены все биты
/// (`FsError::OutOfSpace` на стороне вызова).
pub fn alloc_bit(bitmap: &mut [u8]) -> Option<u32> {
    let scan = core::cmp::min(bitmap.len(), BS);
    for byte_idx in 0..scan {
        let byte = bitmap[byte_idx];
        if byte != 0xFF {
            // Младший свободный бит в этом байте.
            let bit = (!byte).trailing_zeros(); // 0..=7
            bitmap[byte_idx] = byte | (1u8 << bit);
            return Some((byte_idx as u32) * 8 + bit);
        }
    }
    None
}

/// Ставит бит `idx` (помечает занятым). `true`, если он был свободен.
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

/// Сбрасывает бит `idx` (помечает свободным). `true`, если он был занят.
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

/// Проверяет, выставлен ли бит `idx` (занято).
pub fn test_bit(bitmap: &[u8], idx: u32) -> bool {
    let byte = (idx / 8) as usize;
    let bit = (idx % 8) as u8;
    byte < bitmap.len() && (bitmap[byte] & (1 << bit)) != 0
}

/// Считает выставленные биты в адресуемом диапазоне битмапа `[0, span)`.
pub fn count_set_bits(bitmap: &[u8], span: u32) -> u32 {
    let mut count = 0;
    for idx in 0..span {
        if test_bit(bitmap, idx) {
            count += 1;
        }
    }
    count
}

/// Выделяет блок данных из `block_bitmap`, уменьшая счётчики свободного.
///
/// Возвращает **номер блока** (== индексу бита, так как `s_first_data_block == 0`).
pub fn alloc_block(
    block_bitmap: &mut [u8],
    sb: &mut Ext2SuperBlock,
    gd: &mut Ext2GroupDesc,
) -> Option<u32> {
    let bit = alloc_bit(block_bitmap)?;
    let block = sb.s_first_data_block + bit;
    if block >= sb.s_blocks_count {
        // Аллокация вышла за объявленный регион ext2: откат и «места нет».
        clear_bit(block_bitmap, bit);
        return None;
    }
    sb.s_free_blocks_count = sb.s_free_blocks_count.saturating_sub(1);
    gd.bg_free_blocks_count = gd.bg_free_blocks_count.saturating_sub(1);
    Some(block)
}

/// Освобождает ранее выделенный блок данных, увеличивая счётчики свободного.
pub fn free_block(
    block_bitmap: &mut [u8],
    sb: &mut Ext2SuperBlock,
    gd: &mut Ext2GroupDesc,
    block: u32,
) {
    // Блок ниже s_first_data_block завернулся бы и освободил мусорный бит
    // (тихая порча на мусорных указателях) — игнорируем его.
    let Some(bit) = block.checked_sub(sb.s_first_data_block) else {
        return;
    };
    if clear_bit(block_bitmap, bit) {
        sb.s_free_blocks_count += 1;
        gd.bg_free_blocks_count += 1;
    }
}

/// Выделяет inode из `inode_bitmap`, уменьшая счётчики свободного.
///
/// Возвращает **номер inode с единицы** (`bit + 1`).
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

/// Освобождает ранее выделенный inode, увеличивая счётчики свободного.
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
