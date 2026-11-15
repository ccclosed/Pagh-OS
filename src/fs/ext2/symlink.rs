//! Чистые правила раскладки симлинков ext2 и учёта хардлинков.
//!
//! Цель симлинка хранится в одной из двух форм: **fast** («inline») — цель с
//! завершающим NUL помещается в 60-байтный `i_block` inode (`[u32; 15]`), блока
//! данных нет (`i_blocks == 0`); **slow** — цель лежит в одном блоке данных
//! (`i_blocks == BS/512`), в начале блока цель, хвост нулевой.
//!
//! Граница как у `e2fsprogs` 1.47.3 и Linux: 59-байтная цель ещё fast,
//! 60-байтная уже slow (`strlen(target) + 1` против `sizeof(i_block) == 60`);
//! ошибка на единицу теряет последний символ цели на Linux-маунте.
//!
//! Файл `core`-only и самодостаточен, поэтому `host-tests` включают его дословно
//! через `#[path]`, и тот же исходник собирает `#![no_std]`-ядро (R11.6).
#![allow(dead_code)]

/// Байт inline-хранилища цели: `i_block` — это `[u32; 15]`.
pub const INLINE_CAPACITY: usize = 60;

/// `u32`-слов в `i_block`.
pub const INLINE_WORDS: usize = 15;

/// Самая длинная цель, которая ещё помещается inline *вместе* с NUL.
pub const FAST_MAX_TARGET: usize = INLINE_CAPACITY - 1;

/// Наибольшая цель **slow**-симлинка: один блок ФС ext2.
///
/// ext2 хранит slow-цель в одном блоке данных (e2fsprogs отказывается создавать
/// более длинную ссылку), поэтому больший `i_size` у inode `S_IFLNK` —
/// испорченный или подделанный образ, а не легитимная цель. Читатель использует
/// это как жёсткую границу аллокации: подделанный `i_size = 0xFFFFFFF0`
/// приводил к `vec![0u8; 0xFFFFFFF0]` в `Ext2Fs::read_symlink` и аборту ядра.
/// Значение дублирует `structs::BS`, чтобы модуль остался самодостаточным для
/// host-`#[path]`-включения.
pub const SLOW_MAX_TARGET: usize = 4096;

/// Что означает управляемая атакующим пара `(i_blocks, i_size)` для симлинка.
///
/// Одно решение для всех потребителей (чтение и удаление), чтобы испорченный
/// inode не оказался «fast» для одного и «slow» для другого: иначе `unlink`
/// брал бы slow-ветку для `i_blocks == 0, i_size > 60` и освобождал `i_block[0]`
/// — первые четыре байта *текста цели* — как номер блока.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TargetShape {
    /// Inline-цель из стольких байт (`i_blocks == 0`, `i_size <= 60`).
    Fast(usize),
    /// Цель в одном блоке данных, столько байт (`i_blocks != 0`).
    Slow(usize),
    /// Несогласованная форма: [`FsError::Corrupt`], ничего не аллоцировать.
    Corrupt,
}

/// Классифицирует on-disk цель симлинка строго по полям inode.
///
/// Fast требует `i_blocks == 0` *и* вписывания в inline-ёмкость (`is_fast`);
/// inode с `i_blocks == 0`, заявляющий более длинную цель, несогласован — его
/// байты `i_block` это текст, а не карта блоков, — и никогда не должен читаться
/// через slow-путь. Slow требует правдоподобной цели в один блок; всё длиннее
/// [`SLOW_MAX_TARGET`] — порча *до* любой аллокации.
pub fn target_shape(i_blocks: u32, i_size: u32) -> TargetShape {
    let size = i_size as usize;
    if is_fast(i_blocks, i_size) {
        TargetShape::Fast(size)
    } else if i_blocks == 0 {
        // Текст в `i_block`, но `i_size` > inline-ёмкости: ни та, ни другая форма.
        TargetShape::Corrupt
    } else if size <= SLOW_MAX_TARGET {
        TargetShape::Slow(size)
    } else {
        TargetShape::Corrupt
    }
}

/// Владеет ли inode симлинка блоком данных?
///
/// Удаление решает **только** по `i_blocks`, как `ext2_inode_is_fast_symlink` в
/// Linux (`i_blocks - ea_blocks == 0`): inline-цель — это текст в `i_block` при
/// любом `i_size`, поэтому ни один блок нельзя освобождать и нельзя выбирать
/// slow-ветку по `i_size`.
pub fn owns_data_block(i_blocks: u32) -> bool {
    i_blocks != 0
}

/// Какую on-disk форму обязана использовать цель симлинка.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SymlinkLayout {
    /// Цель (с NUL) в `i_block`; блока данных нет, `i_blocks == 0`.
    Fast,
    /// Цель в свежевыделенном блоке данных; `i_blocks == BS/512`.
    Slow,
}

/// Почему цель вообще не может стать симлинком.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SymlinkError {
    /// Цель нулевой длины: не создаётся (пустая висячая ссылка не должна
    /// появляться молча ни из архива, ни из `ln -s ''`).
    Empty,
    /// Цель не помещается в `i_size` (32-битное поле).
    TooBig,
}

/// On-disk-план для одной цели симлинка.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct SymlinkPlan {
    /// Fast- или slow-раскладка.
    pub layout: SymlinkLayout,
    /// Значение для `i_size` (длина цели, без NUL).
    pub size: usize,
    /// 60 inline-байт для [`SymlinkLayout::Fast`] (цель + NUL-добивка).
    pub inline: Option<[u8; INLINE_CAPACITY]>,
}

/// Выбирает раскладку для `target` и готовит inline-образ, если она fast.
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

/// True, когда inode на диске — *fast*-симлинк.
///
/// Повторяет `ext2_inode_is_fast_symlink` из Linux (`i_blocks - ea_blocks == 0`);
/// эта ФС никогда не пишет xattr-блоки (`i_file_acl == 0`), поэтому проверка
/// сводится к `i_blocks == 0`. Половина `i_size <= 60` не даёт испорченному
/// образу заставить читателя скопировать поля inode, не являющиеся целью.
pub fn is_fast(i_blocks: u32, i_size: u32) -> bool {
    i_blocks == 0 && i_size as usize <= INLINE_CAPACITY
}

/// Первые `i_size` байт inline-образа — цель ровно в том виде, как хранится.
pub fn fast_target(inline: &[u8; INLINE_CAPACITY], i_size: u32) -> Option<&[u8]> {
    let n = i_size as usize;
    if n > INLINE_CAPACITY {
        return None;
    }
    Some(&inline[..n])
}

/// Переинтерпретирует 60 inline-байт как 15 little-endian `u32`-слов `i_block`.
///
/// On-disk inode пишется сырыми байтами, поэтому это чистая перекладка
/// байты↔слова: к inode симлинка никогда не применяется интерпретация указателей.
pub fn inline_words(inline: &[u8; INLINE_CAPACITY]) -> [u32; INLINE_WORDS] {
    let mut words = [0u32; INLINE_WORDS];
    for (i, w) in words.iter_mut().enumerate() {
        let mut b = [0u8; 4];
        b.copy_from_slice(&inline[i * 4..i * 4 + 4]);
        *w = u32::from_le_bytes(b);
    }
    words
}

/// Обратное к [`inline_words`]: сырые 60 байт `i_block` inode.
pub fn inline_bytes(words: &[u32; INLINE_WORDS]) -> [u8; INLINE_CAPACITY] {
    let mut inline = [0u8; INLINE_CAPACITY];
    for (i, w) in words.iter().enumerate() {
        inline[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    inline
}

/// Что `unlink` обязан сделать с дочерним inode после удаления его записи
/// каталога.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum UnlinkAction {
    /// На inode ещё указывают другие имена: только уменьшить `i_links_count`.
    DropLink,
    /// Последнее имя: освободить data/indirect-блоки и inode.
    FreeInode,
}

/// Выбирает действие `unlink` для **не-каталога** с `links_count` ссылками.
/// `None` означает, что on-disk состояние уже сломано (свободный inode с живой
/// записью каталога), и это надо сообщить как порчу.
///
/// Каталоги сюда не попадают: `i_links_count` каталога считает ссылки `.`/`..`,
/// а не имена, и хардлинки на каталоги запрещены.
pub fn unlink_action(links_count: u16) -> Option<UnlinkAction> {
    match links_count {
        0 => None,
        1 => Some(UnlinkAction::FreeInode),
        _ => Some(UnlinkAction::DropLink),
    }
}

/// `i_links_count` после добавления одного хардлинка, либо `None`, если это
/// переполнит 16-битное поле или inode уже свободен.
pub fn link_bump(links_count: u16) -> Option<u16> {
    if links_count == 0 {
        None
    } else {
        links_count.checked_add(1)
    }
}
