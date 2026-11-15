//! Чистые проверки границ для управляемых атакующим on-disk полей размера ext2.
//!
//! Любой `i_size` из образа ext2 — недоверенный ввод: том может прийти из
//! архива, `.deb`, USB-флешки или от атакующего, и ядро не должно ни
//! аллоцировать, ни итерировать по размеру, который не проверило. Границы нужны
//! двум местам:
//!
//!   * цель симлинка — не больше одного блока (`symlink::SLOW_MAX_TARGET`,
//!     классификация в [`super::symlink::target_shape`]);
//!   * **каталог** занимает целые блоки ФС и не может быть больше ФС, в которой
//!     лежит: число блоков из подделанного `i_size = 0xFFFFF000` заставляло один
//!     `lookup`/`readdir` делать до ~1M чтений блоков.
//!
//! Счётчики групп/суперблока — тот же класс недоверенного ввода:
//! `s_inodes_per_group` задаёт диапазон, который `reconcile_free_counts`
//! сканирует бит за битом, поэтому подделанные
//! `s_inodes_count = s_inodes_per_group = 0xFFFFFFFF` превращали каждый mount в
//! скан 2^32 бит и записывали мусорный итог свободных inode обратно в
//! суперблок. Один блок битмапа описывает не больше `BLOCK_SIZE * 8` записей,
//! поэтому [`validate_group_counts`] отвергает невозможные формы до начала
//! скана; [`clamp_group_span`] — страховочная граница для самого скана.
//!
//! Модуль `core`-only, без зависимостей; размер блока 4 KiB дублируется вместо
//! `structs::BS` (как в `symlink.rs`), чтобы `host-tests` включали *тот же*
//! исходник через `#[path]` и проверяли правилами свойства того, что реально
//! исполняет ядро (R11.6).
#![allow(dead_code)]

/// Размер одного блока ext2 в этой реализации (зеркалит `structs::BS`;
/// продублирован, чтобы файл оставался самодостаточным для host-включения;
/// сверяется host-свойством ниже).
pub const BLOCK_SIZE: usize = 4096;

/// Биты, описываемые одним блоком битмапа ext2 (бит на блок или на inode).
/// Группа не может быть больше своего битмапа — это жёсткий структурный предел,
/// а не политика.
pub const BITMAP_BITS: u32 = (BLOCK_SIZE as u32) * 8;

/// Почему счётчики групп/inode суперблока не описывают настоящую ФС.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum GroupCountError {
    /// Группа без блоков не монтируется (`s_blocks_per_group == 0`).
    ZeroBlocksPerGroup,
    /// Группа без слотов inode не вместит даже корневой inode.
    ZeroInodesPerGroup,
    /// `s_blocks_per_group` больше того, что описывает один блок битмапа.
    BlocksPerGroupExceedsBitmap,
    /// `s_inodes_per_group` больше того, что описывает один битмап inode.
    InodesPerGroupExceedsBitmap,
    /// ФС с нулём inode не может содержать корневой inode (2).
    ZeroInodeCount,
    /// `s_inodes_count > s_inodes_per_group * groups`: таблица inode короче,
    /// чем заявлено, и лишние id никуда не адресуют.
    InodeCountExceedsGroups,
}

/// Проверяет счётчики суперблока на группу против структурной ёмкости
/// битмапов и числа групп блоков.
///
/// `groups` выведено из `s_blocks_count`, то есть вызывающий уже его ограничил
/// (драйвер ext2 отвергает больше 65536 групп). `Ok(())` для любой формы,
/// которую даёт настоящий `mke2fs` или `format` этого драйвера, включая короткую
/// последнюю группу (`s_inodes_count` может быть меньше
/// `s_inodes_per_group * groups`, но никогда больше).
pub fn validate_group_counts(
    blocks_per_group: u32,
    inodes_per_group: u32,
    inodes_count: u32,
    groups: u32,
) -> Result<(), GroupCountError> {
    if blocks_per_group == 0 {
        return Err(GroupCountError::ZeroBlocksPerGroup);
    }
    if inodes_per_group == 0 {
        return Err(GroupCountError::ZeroInodesPerGroup);
    }
    if blocks_per_group > BITMAP_BITS {
        return Err(GroupCountError::BlocksPerGroupExceedsBitmap);
    }
    if inodes_per_group > BITMAP_BITS {
        return Err(GroupCountError::InodesPerGroupExceedsBitmap);
    }
    if inodes_count == 0 {
        return Err(GroupCountError::ZeroInodeCount);
    }
    let capacity = (inodes_per_group as u64) * (groups as u64);
    if (inodes_count as u64) > capacity {
        return Err(GroupCountError::InodeCountExceedsGroups);
    }
    Ok(())
}

/// Ограничивает диапазон `count_set_bits` одной группы: `total` записей,
/// `per_group` на группу, `index` — номер группы с нуля.
///
/// Клампится и к остатку записей в последней группе, и к ёмкости самого блока
/// битмапа, поэтому испорченное значение, проскочившее mount-валидацию (или
/// перечитанный суперблок, изменившийся под нами), не заставит mount сканировать
/// диапазон 2^32 бит или читать за 4-КиБ буфер битмапа.
pub fn clamp_group_span(total: u32, per_group: u32, index: u32) -> u32 {
    let consumed = (per_group as u64) * (index as u64);
    let remaining = (total as u64).saturating_sub(consumed);
    let span = core::cmp::min(per_group as u64, remaining);
    core::cmp::min(span, BITMAP_BITS as u64) as u32
}

/// Почему `i_size` inode каталога нельзя доверять.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum DirSizeError {
    /// Каталоги ext2 выровнены по блокам; `i_size % BLOCK_SIZE != 0` писатель
    /// никогда не создаёт, и эта форма этому читателю не подходит.
    NotBlockAligned,
    /// Каталог заявляет больше блоков, чем есть в ФС.
    LargerThanFilesystem,
}

/// Число блоков, которое законно занимает inode каталога — по его on-disk
/// `i_size` и общему числу блоков ФС.
///
/// `Ok(0)` для каталога нулевого размера (inode, чьи данные так и не выделены,
/// перечисляется как пустой, а не портит что-либо). Вызывающие обязаны
/// использовать это везде, где выводят `nblocks` из `i_size` каталога;
/// умножение не переполняется, потому что `fs_blocks` — on-disk `u32`-счётчик,
/// расширенный до `u64`.
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

    /// Точная подделанная форма (см. SECURITY.md): каталог, чей `i_size`
    /// заявляет ~4 ГиБ на 64-МиБ образе, отвергается до цикла.
    #[test]
    fn crafted_huge_directory_size_is_refused() {
        assert_eq!(
            dir_blocks(0xFFFF_F000, 16_384),
            Err(DirSizeError::LargerThanFilesystem)
        );
        // Тот же порядок величины, но не выровнено по блоку: всё равно отказ.
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
        // Ровно размер ФС в блоках — задокументированный предел.
        assert_eq!(dir_blocks(16_384 * 4096, 16_384), Ok(16_384));
        // На один блок больше ёмкости ФС — порча.
        assert_eq!(
            dir_blocks((16_385 * 4096) as u32, 16_384),
            Err(DirSizeError::LargerThanFilesystem)
        );
    }
}
