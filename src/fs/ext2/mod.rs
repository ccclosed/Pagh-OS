//! ext2-совместимая ФС (чтение + запись) с журналом write-ahead-log.
//!
//! `format` пишет маунтабельный на хосте образ ext2 (суперблок @ байт 1024,
//! `s_magic = 0xEF53`, `s_log_block_size = 2`, группы блоков с битмапами и
//! inode-таблицей, корневой inode 2 с `.`/`..`, все `feature_*` сняты — Linux
//! маунтит как обычный ext2) плюс пустую область журнала WAL в зарезервированном
//! месте после региона ext2.
//!
//! `mount` валидирует суперблок, запускает `journal.recover()` для реплея
//! закоммиченных, но не зачекпойнченных транзакций, затем строит корневой
//! `VfsNode`. Каждая мутация (данные + inode + битмап + блок dirent) идёт одной
//! транзакцией журнала, поэтому видимое хосту состояние ext2 меняется только
//! атомарно.
//!
//! # Ссылки
//!
//! Писатель создаёт все три вида inode, которые несёт Debian `data.tar`:
//! обычные файлы, каталоги и **симлинки** (`S_IFLNK`), и поддерживает
//! **хардлинки** как ext2: несколько записей каталога делят один inode, учёт
//! ведёт `i_links_count`:
//!
//!   * [`Ext2Fs::create_symlink`] выбирает fast/slow-раскладку через
//!     [`symlink::plan`] (цель ≤ 59 байт inline в `i_block`, длиннее — в одном
//!     блоке данных через `Tx::data_block`, то есть ordered-mode);
//!   * [`Ext2Fs::link`] увеличивает `i_links_count` и добавляет запись каталога,
//!     но не блок;
//!   * [`Ext2Fs::unlink`] смотрит счётчик ссылок: освобождает блоки + inode
//!     только для **последнего** имени, иначе лишь уменьшает счётчик — удаление
//!     одного имени хардлинка не освобождает данные, которые читает другое имя.
//!
//! `i_links_count` не чинят ни `recover()`, ни `reconcile_free_counts`, а fsck
//! здесь нет, поэтому учёт авторитетен: правила зафиксированы в чистом модуле
//! `symlink` и покрыты host-свойствами. Inode симлинка никогда не отдаётся как
//! [`Ext2File`]: `read` fast-ссылки истолковал бы текст цели как указатели на
//! блоки; см. [`Ext2Fs::node_for`].
//!
//! Модуль — чистая логика над RAM-моком `BlockDevice` (`crate::test`); в boot
//! и таблицу монтирования VFS он не подключён.

#![allow(dead_code)]

pub mod alloc;
pub mod bounds;
pub mod dir;
pub mod inode;
pub mod structs;
/// Чистые правила раскладки симлинков и учёта хардлинков (host-тестируемо,
/// R11.6).
pub mod symlink;

use ::alloc::collections::BTreeMap;
use ::alloc::string::String;
use ::alloc::sync::Arc;
use ::alloc::vec;
use ::alloc::vec::Vec;

use crate::drivers::BlockDevice;
use crate::fs::journal::{Journal, JournalArea};
use crate::fs::FsError;
use crate::sync::spinlock::{Spinlock, SpinlockGuard};
use crate::vfs::{FsStat, VfsError, VfsNode, VfsResult};
use structs::JNL_MAGIC;

/// Features, которые реализует драйвер. Всё вне этих масок принадлежит
/// ext3/ext4 (или более новой ревизии ext2) и отвергается: см. `read_sb_gds`.
const FEATURE_INCOMPAT_SUPPORTED: u32 = 0x2; // FILETYPE
const FEATURE_RO_COMPAT_SUPPORTED: u32 = 0x1 | 0x2; // SPARSE_SUPER | LARGE_FILE
/// Compatible-фичи по определению игнорируемы, кроме одной: тому с
/// `HAS_JOURNAL` нужен реплей jbd2, прежде чем доверять метаданным, а этот
/// драйвер пишет свой журнал. Отказ держит ext3/ext4-раздел пользователя
/// недосягаемым.
const FEATURE_COMPAT_UNSUPPORTED: u32 = 0x4; // HAS_JOURNAL

use structs::{
    inode_size, read_struct, read_u32, write_struct, write_u32, Ext2GroupDesc, Ext2Inode,
    Ext2SuperBlock, BS, EXT2_FIRST_INO, EXT2_MAGIC, EXT2_ROOT_INO, INODE_SIZE, PTRS_PER_BLOCK,
    SECTORS_PER_BLOCK, S_IFDIR, S_IFLNK, S_IFREG,
};

use symlink::{SymlinkLayout, TargetShape, UnlinkAction};

// ─── константы раскладки формата ────────────────────────────────────────────

/// Блоки кольцевого лога WAL (без суперблока журнала).
const FMT_LOG_BLOCKS: u64 = 64;

/// Блоки ФС, зарезервированные в конце устройства под журнал WAL: суперблок
/// журнала (1 блок) плюс кольцевой лог.
const JOURNAL_RESERVE_BLOCKS: u64 = FMT_LOG_BLOCKS + 1;

/// Максимум блоков (и inode), которые описывает одна группа одним 4096-байтным
/// битмапом (`BS * 8 = 32768` бит). Кламп выведенных счётчиков к этой границе
/// сохраняет валидность раскладки и держит каждый on-disk `u16`-счётчик группы
/// (`bg_free_blocks_count` / `bg_free_inodes_count`) в диапазоне, так как
/// `32768 < u16::MAX`.
const MAX_GROUP_BLOCKS: u32 = (BS * 8) as u32;
const MAX_GROUP_INODES: u32 = (BS * 8) as u32;

/// Плотность inode: примерно один inode на столько байт ёмкости.
const BYTES_PER_INODE: u64 = 16 * 1024;

/// Нижняя граница числа inode, чтобы даже крошечная свежая ФС сохраняла
/// несколько пригодных inode сверх резерва (inode `1..=EXT2_FIRST_INO-1`).
const MIN_INODES: u32 = 32;

const SUPERBLOCK_OFFSET: usize = 1024;

// ─── Ext2Fs ──────────────────────────────────────────────────────────────────

struct Ext2Inner {
    sb: Ext2SuperBlock,
    // Полная таблица групповых дескрипторов (по записи на группу).
    gds: Vec<Ext2GroupDesc>,
}

/// Смонтированная ФС ext2 над `BlockDevice`, с журналом WAL.
pub struct Ext2Fs {
    dev: Arc<dyn BlockDevice>,
    inner: Spinlock<Ext2Inner>,
    journal: Spinlock<Journal>,
    /// Сериализует мутирующие транзакции: `Tx` снимает снимок `sb`/`gds` и
    /// записывает их обратно на `commit`, поэтому две конкурентные мутации
    /// гонялись бы за обновления битмапов/счётчиков (потерянные обновления,
    /// двойная аллокация).
    tx_lock: Spinlock<()>,
}

/// Находит inode `ino` в inode-таблице его группы.
///
/// Валидирует номер inode: мусор (0 / больше `s_inodes_count` / группа за
/// таблицей дескрипторов) — `Corrupt`. Молчаливый кламп группы заалиасил бы
/// чужой слот таблицы inode, и позже из испорченной записи каталога
/// освобождались бы произвольные блоки.
fn inode_location(
    sb: &Ext2SuperBlock,
    gds: &[Ext2GroupDesc],
    ino: u32,
) -> Result<(u64, usize), FsError> {
    if ino == 0 || ino > sb.s_inodes_count {
        return Err(FsError::Corrupt);
    }
    let index = (ino - 1) as u64;
    let ipg = sb.s_inodes_per_group.max(1) as u64;
    let group = (index / ipg) as usize;
    if group >= gds.len() {
        return Err(FsError::Corrupt);
    }
    let within = index % ipg;
    let isz = inode_size(sb);
    let ipb = (BS / isz) as u64;
    let block = gds[group].bg_inode_table as u64 + within / ipb;
    let off = (within % ipb) as usize * isz;
    Ok((block, off))
}

impl Ext2Fs {
    // ── низкоуровневый I/O (блоки ФС 4096 байт через секторы 512) ──

    pub fn read_fs_block(&self, block: u64) -> Result<Vec<u8>, FsError> {
        let mut buf = vec![0u8; BS];
        self.dev
            .read_block(block * SECTORS_PER_BLOCK, &mut buf)
            .map(|_| ())
            .map_err(|_| FsError::IoError)?;
        Ok(buf)
    }

    fn write_fs_block_direct(
        dev: &dyn BlockDevice,
        block: u64,
        data: &[u8],
    ) -> Result<(), FsError> {
        debug_assert!(data.len() == BS);
        dev.write_block(block * SECTORS_PER_BLOCK, data)
            .map(|_| ())
            .map_err(|_| FsError::IoError)
    }

    pub fn read_inode(&self, ino: u32) -> Result<Ext2Inode, FsError> {
        let (block, off) = {
            let inner = self.inner.lock();
            inode_location(&inner.sb, &inner.gds, ino)?
        };
        let buf = self.read_fs_block(block)?;
        Ok(unsafe { read_struct::<Ext2Inode>(&buf[off..]) })
    }

    pub fn superblock(&self) -> Ext2SuperBlock {
        self.inner.lock().sb
    }

    /// Объявленное число блоков ФС (`s_blocks_count`): жёсткая верхняя граница
    /// для карты блоков любого inode.
    fn total_blocks(&self) -> u64 {
        self.superblock().s_blocks_count as u64
    }

    /// Проверенное число блоков **каталога**.
    ///
    /// Каталог выровнен по блокам и не может быть больше ФС, поэтому
    /// подделанный `i_size` — [`FsError::Corrupt`] *до* того, как вызывающий
    /// начнёт цикл по блокам: иначе один `lookup`/`readdir` при
    /// `i_size = 0xFFFFF000` делает до ~1M чтений блоков.
    fn dir_blocks(&self, inode: &Ext2Inode) -> Result<u64, FsError> {
        bounds::dir_blocks(inode.i_size, self.total_blocks()).map_err(|_| FsError::Corrupt)
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

// ─── контекст транзакции ────────────────────────────────────────────────────

/// Рабочий набор грязных блоков в памяти плюс рабочие `sb`/`gd`. На `commit`
/// каждый грязный блок (включая пропатченные суперблок и групповой дескриптор)
/// передаётся журналу одной атомарной транзакцией.
struct Tx<'a> {
    fs: &'a Ext2Fs,
    /// Держится всё время жизни Tx: мутирующие транзакции сериализуются на
    /// ФС (см. `Ext2Fs::tx_lock`).
    _tx_guard: SpinlockGuard<'a, ()>,
    sb: Ext2SuperBlock,
    gds: Vec<Ext2GroupDesc>,
    dirty: BTreeMap<u64, Vec<u8>>,
    /// Блоки в `dirty`, являющиеся ДАННЫМИ ФАЙЛА. Ordered mode: на commit они
    /// пишутся сразу на финальное место и НЕ проходят через WAL (журналируются
    /// только метаданные).
    data: ::alloc::collections::BTreeSet<u64>,
}

impl<'a> Tx<'a> {
    fn new(fs: &'a Ext2Fs) -> Self {
        let _tx_guard = fs.tx_lock.lock();
        let inner = fs.inner.lock();
        Tx {
            fs,
            _tx_guard,
            sb: inner.sb,
            gds: inner.gds.clone(),
            dirty: BTreeMap::new(),
            data: ::alloc::collections::BTreeSet::new(),
        }
    }

    fn block(&mut self, blk: u64) -> Result<&mut Vec<u8>, FsError> {
        if !self.dirty.contains_key(&blk) {
            let data = self.fs.read_fs_block(blk)?;
            self.dirty.insert(blk, data);
        }
        Ok(self.dirty.get_mut(&blk).unwrap())
    }

    /// Подтягивает блок ДАННЫХ ФАЙЛА в грязный набор. Если блок будет
    /// перезаписан целиком, чтение с диска пропускается: read-modify-write
    /// вдвое резал последовательный throughput без пользы.
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

    /// Выделяет обнулённый data-блок из первой группы со свободным блоком.
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
                // Битмап расходится с кэшированным счётчиком: помечаем группу
                // исчерпанной и идём дальше.
                self.gds[g].bg_free_blocks_count = 0;
                continue;
            };
            let blk = first + g as u32 * bpg + bit;
            if blk >= self.sb.s_blocks_count {
                // За объявленным регионом (обычно не бывает из-за padding-
                // битов): откат и считаем группу полной.
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

    /// Выделяет inode из первой группы со свободным слотом.
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
        // inode_location отвергает 0 / out-of-range: мусорный inode из dirent
        // не должен резолвиться в клампленный слот таблицы (произвольные
        // освобождения блоков).
        let (block, off) = inode_location(&self.sb, &self.gds, ino)?;
        self.block(block)?;
        let buf = self.dirty.get(&block).unwrap();
        Ok(unsafe { read_struct::<Ext2Inode>(&buf[off..]) })
    }

    fn write_inode(&mut self, ino: u32, inode: &Ext2Inode) -> Result<(), FsError> {
        let (block, off) = inode_location(&self.sb, &self.gds, ino)?;
        let b = self.block(block)?;
        unsafe { write_struct(&mut b[off..], inode) };
        Ok(())
    }

    /// Снимает маппинг логического блока `lbn`, освобождая его data-блок и
    /// любой indirect-блок, ставший пустым. Возвращает число освобождённых
    /// блоков (data + indirect), чтобы вызывающий скорректировал `i_blocks`.
    fn clear_mapping(&mut self, inode: &mut Ext2Inode, lbn: u64) -> Result<u32, FsError> {
        if lbn < 12 {
            let i = lbn as usize;
            if inode.i_block[i] == 0 {
                return Ok(0);
            }
            self.free_data_block(inode.i_block[i])?;
            inode.i_block[i] = 0;
            return Ok(1);
        }
        let ppb = PTRS_PER_BLOCK as u64;
        let l = lbn - 12;
        let (root_slot, path) = if l < ppb {
            (12usize, vec![l as u32])
        } else if l - ppb < ppb * ppb {
            let l2 = l - ppb;
            (13, vec![(l2 / ppb) as u32, (l2 % ppb) as u32])
        } else {
            let t = l - ppb * ppb;
            if t >= ppb * ppb * ppb {
                return Ok(0); // за пределами triple-indirect
            }
            (
                14,
                vec![
                    (t / (ppb * ppb)) as u32,
                    ((t / ppb) % ppb) as u32,
                    (t % ppb) as u32,
                ],
            )
        };
        let root = inode.i_block[root_slot];
        if root == 0 {
            return Ok(0);
        }
        let (empty, freed) = self.clear_slot(root, &path, path.len() as u32)?;
        if empty {
            self.free_data_block(root)?;
            inode.i_block[root_slot] = 0;
            return Ok(freed + 1);
        }
        Ok(freed)
    }

    /// Освобождает лист (и опустевшие дочерние indirect-блоки) под `ind`,
    /// обнуляя пройденные слоты указателей. Возвращает `(ind стал полностью
    /// пустым, число блоков, освобождённых ниже ind)`.
    fn clear_slot(&mut self, ind: u32, path: &[u32], level: u32) -> Result<(bool, u32), FsError> {
        let slot = path[0] as usize;
        self.block(ind as u64)?;
        let child = read_u32(self.dirty.get(&(ind as u64)).unwrap(), slot * 4);
        let mut freed = 0u32;
        if child != 0 {
            let child_empty = if level > 1 {
                let (e, f) = self.clear_slot(child, &path[1..], level - 1)?;
                freed += f;
                e
            } else {
                self.free_data_block(child)?;
                freed += 1;
                true
            };
            if child_empty {
                let b = self.dirty.get_mut(&(ind as u64)).unwrap();
                write_u32(b, slot * 4, 0);
            }
        }
        let b = self.dirty.get(&(ind as u64)).unwrap();
        let empty = b.chunks_exact(4).all(|c| c == [0u8; 4]);
        Ok((empty, freed))
    }

    /// Отображает (аллоцируя по необходимости) логический блок `lbn` inode в
    /// блок ext2, проходя 12 direct + single/double/triple indirect.
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

    fn ensure_indirect_root(
        &mut self,
        inode: &mut Ext2Inode,
        which: usize,
    ) -> Result<u32, FsError> {
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
        let cur = read_u32(
            self.dirty.get(&(ind_block as u64)).unwrap(),
            slot as usize * 4,
        );
        if cur != 0 {
            return Ok(cur);
        }
        let nb = self.alloc_zeroed_block()?;
        inode.i_blocks += (BS / 512) as u32;
        let b = self.block(ind_block as u64)?;
        write_u32(b, slot as usize * 4, nb);
        Ok(nb)
    }

    /// Освобождает все data- и indirect-блоки, на которые ссылается `inode`.
    ///
    /// Годится только для inode, чей `i_block` действительно карта блоков, —
    /// обычные файлы и каталоги. Симлинки обязаны идти через
    /// [`Self::free_inode_blocks`].
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

    /// Аналог `free_all_blocks` для удаления, учитывающий вид inode.
    ///
    /// **Fast**-симлинк держит *текст* цели в `i_block`, поэтому обход его как
    /// карты блоков освободил бы блоки, чьи номера случайно совпадают с текстом
    /// цели, — произвольные освобождения из испорченного, но правдоподобного
    /// inode. **Slow**-симлинк владеет ровно одним блоком в `i_block[0]` и
    /// indirect-дерева не имеет. Всё остальное — обычный файл или каталог с
    /// обычной картой.
    fn free_inode_blocks(&mut self, inode: &Ext2Inode) -> Result<(), FsError> {
        if inode.is_symlink() {
            // Решаем ТОЛЬКО по `i_blocks`, никогда по `i_size`: inline-цель —
            // это текст в `i_block` при любом `i_size`, поэтому освобождать
            // нечего; иначе inode с `i_size > 60, i_blocks == 0` пошёл бы по
            // slow-ветке и освободил номер блока, составленный из текста цели.
            if !symlink::owns_data_block(inode.i_blocks) {
                return Ok(()); // inline-цель: блока данных не существует
            }
            if inode.i_block[0] != 0 {
                self.free_data_block(inode.i_block[0])?;
            }
            return Ok(());
        }
        self.free_all_blocks(inode)
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

    /// Commit: патчит суперблок и каждый групповой дескриптор в их блоки, затем
    /// отдаёт все грязные блоки журналу одной атомарной транзакцией. При успехе
    /// in-memory `sb`/`gds` публикуются.
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

        // Данные файла идут сразу на финальное место ДО коммита транзакции
        // метаданных (чтобы закоммиченные метаданные никогда не указывали на
        // незаписанные данные), а через WAL едут только метаданные. Это убирает
        // двойную запись каждого data-блока, которая доминировала во времени
        // распаковки apt/dpkg.
        for (blk, data) in self.dirty.iter() {
            if self.data.contains(blk) {
                Ext2Fs::write_fs_block_direct(&*self.fs.dev, *blk, data)?;
            }
        }
        // Остальные (метаданные) грязные блоки отдаём журналу одной атомарной
        // транзакцией.
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

        // Публикуем новый суперблок / групповые дескрипторы.
        let mut inner = self.fs.inner.lock();
        inner.sb = self.sb;
        inner.gds = self.gds;
        Ok(())
    }
}

// ─── формат ─────────────────────────────────────────────────────────────────

impl Ext2Fs {
    /// Создаёт свежий маунтабельный на хосте образ ext2 плюс пустой журнал WAL.
    ///
    /// Многогрупповая раскладка: ФС занимает всё устройство (минус резерв WAL)
    /// столькими группами по 32768 блоков (128 MiB), сколько помещается. Каждая
    /// группа начинается с суперблока + резервной GD-таблицы (без
    /// sparse_super), затем идут битмап блоков, битмап inode, inode-таблица и
    /// блоки данных.
    pub fn format(dev: Arc<dyn BlockDevice>) -> Result<(), FsError> {
        // Ёмкость устройства в 4-КиБ блоках ФС.
        let device_blocks = dev.sector_count() / SECTORS_PER_BLOCK;

        // Защита минимальной ёмкости (R7.4), рассчитана на одну малую группу.
        let min_inode_table_blocks = ((MIN_INODES as usize * INODE_SIZE) + BS - 1) / BS; // ceil
        let min_ext2_blocks = 4u64 + min_inode_table_blocks as u64 + 1 + 1;
        let min_layout_blocks = min_ext2_blocks + JOURNAL_RESERVE_BLOCKS;
        if device_blocks < min_layout_blocks {
            return Err(FsError::OutOfSpace);
        }

        // Регион ext2 = устройство минус резерв журнала WAL в хвосте.
        let data_blocks = device_blocks.saturating_sub(JOURNAL_RESERVE_BLOCKS);

        let bpg = MAX_GROUP_BLOCKS; // 32768 блоков (128 MiB) на группу
        let mut total_blocks = data_blocks.min(u32::MAX as u64) as u32;
        let mut groups = ((total_blocks as u64 + bpg as u64 - 1) / bpg as u64) as u32;

        let gd_size = core::mem::size_of::<Ext2GroupDesc>();
        let gd_blocks =
            |groups: u32| -> u32 { (((groups as usize * gd_size) + BS - 1) / BS) as u32 };

        // Плотность inode: один inode на BYTES_PER_INODE по региону, равномерно
        // (`s_inodes_per_group` одинаков во всех группах), с округлением до
        // целых блоков inode-таблицы, по одному битмапу на группу.
        let inodes_per_block = (BS / INODE_SIZE) as u64;
        let scaled = (total_blocks as u64 * BS as u64) / BYTES_PER_INODE;
        let per_group = (scaled.max(MIN_INODES as u64) / groups as u64).max(inodes_per_block);
        let per_group =
            per_group.saturating_add(inodes_per_block - 1) / inodes_per_block * inodes_per_block;
        let ipg = per_group.min(MAX_GROUP_INODES as u64) as u32;
        let itb = ipg / (BS / INODE_SIZE) as u32; // блоков inode-таблицы/группу

        // Отбрасываем хвостовую группу, слишком малую для своих метаданных и
        // хотя бы немного данных.
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
        let meta = 1 + gdb + 2 + itb; // число метаблоков на группу
        let total_inodes = groups * ipg;
        let reserved_inodes = EXT2_FIRST_INO - 1; // inode 1..=10 помечены занятыми

        // Корневой каталог: первый блок данных группы 0.
        let root_dir_block = meta;
        if total_blocks <= root_dir_block + 1 {
            return Err(FsError::OutOfSpace);
        }

        // Занятые блоки: метаданные каждой группы + корневой каталог в группе 0.
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

        // Строим все групповые дескрипторы (GD-таблица реплицируется).
        let mut gds: Vec<Ext2GroupDesc> = Vec::with_capacity(groups as usize);
        for g in 0..groups {
            let base = g * bpg;
            let span = core::cmp::min(bpg, total_blocks - base);
            let used = if g == 0 { meta + 1 } else { meta }; // + корневой каталог
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

        // Образ блока суперблока (SB @ байт 1024 первого блока группы).
        let mut sb_block = vec![0u8; BS];
        unsafe { write_struct(&mut sb_block[SUPERBLOCK_OFFSET..], &sb) };

        // Образ GD-таблицы (`gdb` блоков), реплицируется в каждую группу.
        let mut gd_table = vec![0u8; gdb as usize * BS];
        for (g, gd) in gds.iter().enumerate() {
            unsafe { write_struct(&mut gd_table[g * gd_size..], gd) };
        }

        let zero = vec![0u8; BS];
        for g in 0..groups {
            let base = g * bpg;
            let span = core::cmp::min(bpg, total_blocks - base);
            // Резерв SB + копия GD-таблицы в начале группы (группа 0: основная).
            Self::write_fs_block_direct(&*dev, base as u64, &sb_block)?;
            for i in 0..gdb {
                let src = &gd_table[i as usize * BS..(i as usize + 1) * BS];
                Self::write_fs_block_direct(&*dev, (base + 1 + i) as u64, src)?;
            }
            // Битмап блоков: заняты метаданные (+ корневой каталог в группе 0);
            // padding-биты за объявленным диапазоном выставлены, чтобы аллокация
            // никогда не вышла за конец ФС.
            let mut bbm = vec![0u8; BS];
            let used = if g == 0 { meta + 1 } else { meta };
            for b in 0..used {
                alloc::set_bit(&mut bbm, b);
            }
            for b in span..MAX_GROUP_BLOCKS {
                alloc::set_bit(&mut bbm, b);
            }
            Self::write_fs_block_direct(&*dev, gds[g as usize].bg_block_bitmap as u64, &bbm)?;
            // Битмап inode: группа 0 резервирует классические системные inode;
            // каждая группа помечает padding-биты за s_inodes_per_group.
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
            // Обнуляем inode-таблицу (data-блоки обнулять не нужно: битмапы
            // объявляют их свободными, а аллокатор обнуляет при выделении).
            for b in 0..itb {
                Self::write_fs_block_direct(
                    &*dev,
                    (gds[g as usize].bg_inode_table + b) as u64,
                    &zero,
                )?;
            }
        }

        // Блок данных корневого каталога с "." и "..".
        let mut root_block = vec![0u8; BS];
        dir::init_dot_entries(&mut root_block, EXT2_ROOT_INO, EXT2_ROOT_INO);
        Self::write_fs_block_direct(&*dev, root_dir_block as u64, &root_block)?;

        // Корневой inode (inode 2): каталог, размер = один блок.
        let mut root_inode = Ext2Inode::zeroed();
        root_inode.i_mode = S_IFDIR | 0o755;
        root_inode.i_links_count = 2; // "." и ".."
        root_inode.i_size = BS as u32;
        root_inode.i_blocks = (BS / 512) as u32;
        root_inode.i_block[0] = root_dir_block;

        let (rblock, roff) = inode_location(&sb, &gds, EXT2_ROOT_INO)?;
        let mut itbuf = vec![0u8; BS];
        unsafe { write_struct(&mut itbuf[roff..], &root_inode) };
        Self::write_fs_block_direct(&*dev, rblock, &itbuf)?;

        // Пустой журнал WAL в зарезервированной области после ext2.
        let area = Self::journal_area(&sb);
        Journal::format(&*dev, area)?;
        Ok(())
    }
}

// ─── монтирование ───────────────────────────────────────────────────────────

impl Ext2Fs {
    /// True, когда том несёт собственный суперблок журнала pagh.
    ///
    /// МАРКЕР ВЛАДЕНИЯ. Структурно валидный том ext2 — не доказательство, что
    /// pagh можно в него писать: Linux-раздел пользователя часто обычный ext2
    /// или ext4, а WAL pagh кладёт на устройство только `disk format`. Поэтому
    /// маркер — суперблок WAL: его пишет только `format`, он лежит в области,
    /// которую ext2 иначе не использует, и его магию (`PAGHJNL\1`) другая ФС
    /// случайно не создаст.
    ///
    /// Boot-путь по нему решает, что можно монтировать самому. Том без маркера
    /// автоматически не трогается никогда.
    pub fn is_pagh_volume(dev: &dyn BlockDevice) -> bool {
        let Ok((sb, _)) = Self::read_sb_gds(dev) else {
            return false;
        };
        let area = Self::journal_area(&sb);
        let mut buf = vec![0u8; BS];
        if dev
            .read_block(area.super_block * SECTORS_PER_BLOCK, &mut buf)
            .is_err()
        {
            return false;
        }
        match crate::fs::journal::load_journal_super(&buf) {
            Ok(js) => js.magic == JNL_MAGIC && js.log_blocks != 0,
            Err(_) => false,
        }
    }

    /// Идентичность тома для uevent: `(s_uuid, s_volume_name)`.
    ///
    /// `None`, если суперблок структурно невалиден или фичи вне того, что
    /// понимает драйвер: чужой ext4 читать «наполовину» нельзя. Значения
    /// возвращаются как есть — нулевой uuid означает «идентичности нет», и
    /// вызывающий обязан выбрать fallback (label/capacity), а не выдумывать id.
    pub fn volume_identity(dev: &dyn BlockDevice) -> Option<([u8; 16], [u8; 16])> {
        let (sb, _) = Self::read_sb_gds(dev).ok()?;
        Some((sb.s_uuid, sb.s_volume_name))
    }

    /// Читает суперблок и полную таблицу групповых дескрипторов (по одному
    /// дескриптору на группу; таблица начинается с блока 1). Старые
    /// одногрупповые образы загружаются без изменений — просто дают один
    /// дескриптор.
    fn read_sb_gds(dev: &dyn BlockDevice) -> Result<(Ext2SuperBlock, Vec<Ext2GroupDesc>), FsError> {
        let mut b0 = vec![0u8; BS];
        dev.read_block(0, &mut b0).map_err(|_| FsError::IoError)?;
        let sb: Ext2SuperBlock = unsafe { read_struct(&b0[SUPERBLOCK_OFFSET..]) };
        if sb.s_magic != EXT2_MAGIC {
            return Err(FsError::BadSuperBlock);
        }
        // `s_magic` равен 0xEF53 у ext2, ext3 И ext4: это одно семейство, и
        // том ext4 проходит все структурные проверки ext2. Смонтировать его
        // значило бы писать: `reconcile_free_counts` переписывает групповые
        // дескрипторы, а WAL резервирует блоки в конце тома. На реальной машине
        // это чей-то Linux-root.
        //
        // Поэтому проверяются слова features, а не только магия. Маски ниже —
        // ровно то, что реализует драйвер: `FILETYPE` (запись каталога несёт
        // свой тип) и read-only `SPARSE_SUPER` / `LARGE_FILE`. Всё остальное —
        // extents, 64-bit, flex_bg, jbd2-журнал (`HAS_JOURNAL`) — ФС, которую
        // этот код не удержит консистентной; отказ, а не чтение наполовину.
        if sb.s_feature_incompat & !FEATURE_INCOMPAT_SUPPORTED != 0
            || sb.s_feature_ro_compat & !FEATURE_RO_COMPAT_SUPPORTED != 0
            || sb.s_feature_compat & FEATURE_COMPAT_UNSUPPORTED != 0
        {
            crate::warn!(
                "ext2: refusing a foreign/ext4-style volume (incompat={:#x} ro={:#x} compat={:#x})",
                sb.s_feature_incompat,
                sb.s_feature_ro_compat,
                sb.s_feature_compat
            );
            return Err(FsError::Unsupported);
        }
        if (1024usize << sb.s_log_block_size) != BS {
            return Err(FsError::BadSuperBlock);
        }
        if sb.s_blocks_per_group == 0 || sb.s_inodes_per_group == 0 {
            return Err(FsError::BadSuperBlock);
        }
        // Rev >= 1 обязан объявлять разумный размер inode; иначе арифметика
        // адресов попадёт в произвольные байты таблицы. Образы rev 0 всегда
        // используют 128.
        if sb.s_rev_level >= 1 {
            let isz = sb.s_inode_size as usize;
            if isz < INODE_SIZE || isz > BS || !isz.is_power_of_two() {
                return Err(FsError::BadSuperBlock);
            }
        }
        let span = sb.s_blocks_count.saturating_sub(sb.s_first_data_block);
        let groups = ((span as u64 + sb.s_blocks_per_group as u64 - 1)
            / sb.s_blocks_per_group as u64) as usize;
        if groups == 0 || groups > 65536 {
            return Err(FsError::BadSuperBlock);
        }
        // Счётчики на группу задают длину сканов битмапов
        // (`reconcile_free_counts`). Блок битмапа описывает ровно `BS * 8`
        // записей, поэтому всё большее — не «большая ФС», а испорченный или
        // подделанный суперблок: отказ до чтения первого бита (см. SECURITY.md:
        // `s_inodes_count = s_inodes_per_group = 0xFFFFFFFF` превращал каждый
        // mount в скан 2^32 бит и записывал мусорный `s_free_inodes_count`
        // обратно в том).
        if let Err(e) = bounds::validate_group_counts(
            sb.s_blocks_per_group,
            sb.s_inodes_per_group,
            sb.s_inodes_count,
            groups as u32,
        ) {
            crate::warn!(
                "ext2: refusing impossible superblock counts inodes={} per_group={} blocks_per_group={} groups={} ({:?})",
                sb.s_inodes_count,
                sb.s_inodes_per_group,
                sb.s_blocks_per_group,
                groups,
                e
            );
            return Err(FsError::Corrupt);
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

    /// True, когда на устройстве структурно валидный суперблок ext2.
    /// Boot отличает так пустой диск от существующей ФС с повреждённым
    /// собственным WAL; автоматически форматировать можно только первый.
    pub fn has_valid_superblock(dev: &dyn BlockDevice) -> bool {
        Self::read_sb_gds(dev).is_ok()
    }

    /// Чинит устаревшие счётчики свободного места по авторитетным битмапам
    /// каждой группы блоков до публикации монтирования.
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
            // `span`/`ispan` ограничивают скан битмапа и клампятся к ёмкости
            // одного блока битмапа (`BS * 8` бит): `read_sb_gds` уже отверг
            // невозможные счётчики, а это страховка, чтобы испорченное значение
            // не превратило mount в скан 2^32 бит.
            let span = core::cmp::min(bpg, sb.s_blocks_count.saturating_sub(base))
                .min(bounds::BITMAP_BITS);
            let mut bbm = vec![0u8; BS];
            dev.read_block(gd.bg_block_bitmap as u64 * SECTORS_PER_BLOCK, &mut bbm)
                .map_err(|_| FsError::IoError)?;
            let mut ibm = vec![0u8; BS];
            dev.read_block(gd.bg_inode_bitmap as u64 * SECTORS_PER_BLOCK, &mut ibm)
                .map_err(|_| FsError::IoError)?;
            let ispan = bounds::clamp_group_span(sb.s_inodes_count, ipg, g as u32);
            let free_b = span.saturating_sub(alloc::count_set_bits(&bbm, span));
            let free_i = ispan.saturating_sub(alloc::count_set_bits(&ibm, ispan));
            if gd.bg_free_blocks_count as u32 != free_b || gd.bg_free_inodes_count as u32 != free_i
            {
                gd.bg_free_blocks_count = free_b.min(u16::MAX as u32) as u16;
                gd.bg_free_inodes_count = free_i.min(u16::MAX as u32) as u16;
                changed = true;
            }
            free_blocks_total = free_blocks_total.saturating_add(free_b);
            free_inodes_total = free_inodes_total.saturating_add(free_i);
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

    /// Монтирует ФС: валидирует суперблок, восстанавливает журнал, затем строит
    /// `Ext2Fs`. Возвращает живой дескриптор ФС.
    pub fn mount_fs(dev: Arc<dyn BlockDevice>) -> Result<Arc<Ext2Fs>, FsError> {
        // 1. Валидация суперблока + размера блока.
        let (sb, _gds) = Self::read_sb_gds(&*dev)?;

        // 2. Восстановление журнала ДО построения корня (реплей коммитов).
        let area = Self::journal_area(&sb);
        let mut journal = Journal::open(dev.clone(), area)?;
        journal.recover()?;

        // 3. Перечитываем метаданные после реплея, затем чиним устаревшие
        // счётчики свободного места по авторитетным битмапам до публикации
        // монтирования.
        let (sb, gds) = Self::read_sb_gds(&*dev)?;
        let (sb, gds) = Self::reconcile_free_counts(&*dev, sb, gds)?;

        Ok(Arc::new(Ext2Fs {
            dev,
            inner: Spinlock::new(Ext2Inner { sb, gds }),
            journal: Spinlock::new(journal),
            tx_lock: Spinlock::new(()),
        }))
    }

    /// Монтирует и возвращает корневой `VfsNode` каталога (точка входа).
    pub fn mount(dev: Arc<dyn BlockDevice>) -> Result<Arc<dyn VfsNode>, FsError> {
        let fs = Self::mount_fs(dev)?;
        Ok(fs.root_node())
    }

    /// Строит узел корневого каталога (inode 2).
    pub fn root_node(self: &Arc<Self>) -> Arc<dyn VfsNode> {
        let cached_size = self
            .read_inode(EXT2_ROOT_INO)
            .map(|i| i.i_size as u64)
            .unwrap_or(0);
        Arc::new(Ext2Dir {
            fs: self.clone(),
            ino: EXT2_ROOT_INO,
            name: String::from("/"),
            cached_size,
        })
    }

    /// Строит дочерний узел по inode/имени, диспетчеризуя по `i_mode` inode.
    ///
    /// Диспетчеризация явная и обязана такой остаться: отдай inode симлинка
    /// [`Ext2File`] — и `read` прогонит карту блоков по `i_block`, где
    /// *fast*-симлинк держит текст цели, то есть байты цели будут поняты как
    /// номера блоков и вернётся произвольный блок диска. Всё, что не каталог,
    /// не обычный файл и не симлинк (device-узел чужого образа, fifo, ...),
    /// становится узлом [`Ext2Opaque`] без читаемого содержимого.
    fn node_for(fs: &Arc<Ext2Fs>, ino: u32, name: &str) -> Result<Arc<dyn VfsNode>, FsError> {
        let inode = fs.read_inode(ino)?;
        let cached_size = inode.i_size as u64;
        if inode.is_dir() {
            Ok(Arc::new(Ext2Dir {
                fs: fs.clone(),
                ino,
                name: String::from(name),
                cached_size,
            }))
        } else if inode.is_symlink() {
            Ok(Arc::new(Ext2Symlink {
                fs: fs.clone(),
                ino,
                name: String::from(name),
                cached_size,
            }))
        } else if inode.is_reg() {
            Ok(Arc::new(Ext2File {
                fs: fs.clone(),
                ino,
                name: String::from(name),
                cached_size,
            }))
        } else {
            Ok(Arc::new(Ext2Opaque {
                fs: fs.clone(),
                ino,
                name: String::from(name),
                cached_size,
            }))
        }
    }

    /// Flush — no-op: каждая мутация уже прочно зажурналирована и
    /// зачекпойнчена на `commit`. Оставлен для полноты API.
    pub fn sync(&self) {}

    /// Биты прав `ino` (`i_mode & 0o7777`) либо `fallback`, если inode не
    /// читается (ошибка устройства не должна делать файл исполняемым).
    pub fn node_mode(&self, ino: u32, fallback: u32) -> u32 {
        match self.read_inode(ino) {
            Ok(inode) => (inode.i_mode as u32) & 0o7777,
            Err(_) => fallback,
        }
    }

    /// `(i_uid, i_gid)` для `ino`, `(0, 0)`, если inode не читается (писатель
    /// ставит root на каждый создаваемый inode).
    pub fn node_owner(&self, ino: u32) -> (u32, u32) {
        match self.read_inode(ino) {
            Ok(inode) => (inode.i_uid as u32, inode.i_gid as u32),
            Err(_) => (0, 0),
        }
    }

    /// Пишет биты прав `ino` через журнал, сохраняя типовые биты inode. Одна
    /// закоммиченная транзакция, как и любая другая мутация.
    pub fn set_inode_mode(&self, ino: u32, mode: u32) -> Result<(), FsError> {
        let mut tx = Tx::new(self);
        let mut inode = tx.read_inode(ino)?;
        inode.i_mode = (inode.i_mode & 0xF000) | ((mode as u16) & 0o7777);
        tx.write_inode(ino, &inode)?;
        tx.commit()
    }

    /// Пишет `i_uid`/`i_gid` для `ino` через журнал. ext2 хранит оба как `u16`;
    /// syscall-слой уже отверг не помещающиеся значения.
    pub fn set_inode_owner(&self, ino: u32, uid: u16, gid: u16) -> Result<(), FsError> {
        let mut tx = Tx::new(self);
        let mut inode = tx.read_inode(ino)?;
        inode.i_uid = uid;
        inode.i_gid = gid;
        tx.write_inode(ino, &inode)?;
        tx.commit()
    }
}

// ─── операции с каталогами и файлами ────────────────────────────────────────

impl Ext2Fs {
    /// Перечисляет живые записи каталога как `(имя, inode)` (без свободных
    /// слотов; включает `.`/`..`).
    pub fn read_dir_entries(&self, dir_ino: u32) -> Result<Vec<(String, u32)>, FsError> {
        let inode = self.read_inode(dir_ino)?;
        if !inode.is_dir() {
            return Err(FsError::NotFound);
        }
        let nblocks = self.dir_blocks(&inode)?;
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

    /// Ищет `name` в каталоге `dir_ino` и возвращает номер дочернего inode.
    pub fn lookup_entry(&self, dir_ino: u32, name: &str) -> Result<u32, FsError> {
        let inode = self.read_inode(dir_ino)?;
        if !inode.is_dir() {
            return Err(FsError::NotFound);
        }
        let nblocks = self.dir_blocks(&inode)?;
        for lbn in 0..nblocks {
            if let Some(blk) = inode::block_for_offset(self, &inode, lbn * BS as u64)? {
                let buf = self.read_fs_block(blk as u64)?;
                if let Some((ino, _)) = dir::find(&buf, name)? {
                    return Ok(ino);
                }
            }
        }
        Err(FsError::NotFound)
    }

    /// Вставляет `(name -> child_ino)` в каталог `dir_ino`, при нехватке места
    /// выделяя новый блок каталога.
    fn insert_dirent(tx: &mut Tx, dir_ino: u32, name: &str, child_ino: u32) -> Result<(), FsError> {
        if name.as_bytes().len() > 255 {
            return Err(FsError::NameTooLong);
        }
        let mut dinode = tx.read_inode(dir_ino)?;
        let nblocks = tx.fs.dir_blocks(&dinode)?;

        // Полный скан дублей внутри транзакции. Вызывающие сначала проверяют
        // `lookup_entry`, но эта проверка идёт вне tx-лока; вторая запись с тем
        // же именем оставила бы два dirent на один inode и тихо сломала бы учёт
        // ссылок. Прочитанные здесь блоки кэшируются в транзакции, поэтому цикл
        // вставки ниже ничего не перечитывает.
        for lbn in 0..nblocks {
            let Some(blk) = inode::block_for_offset(tx.fs, &dinode, lbn * BS as u64)? else {
                continue;
            };
            let exists = {
                let buf = tx.block(blk as u64)?;
                dir::find(buf, name)?.is_some()
            };
            if exists {
                return Err(FsError::AlreadyExists);
            }
        }

        // Пробуем существующие блоки. Скан read-only: дыры здесь НЕЛЬЗЯ
        // мапить через map_or_alloc — это выделило бы блок (и сдвинуло
        // i_blocks/битмапы) просто ради поиска имени, а при ранней ошибке скан
        // утёк бы аллокацией.
        for lbn in 0..nblocks {
            let Some(blk) = inode::block_for_offset(tx.fs, &dinode, lbn * BS as u64)? else {
                continue;
            };
            let inserted = {
                let buf = tx.block(blk as u64)?;
                dir::insert_into_block(buf, name, child_ino)?
            };
            if inserted {
                tx.write_inode(dir_ino, &dinode)?;
                return Ok(());
            }
        }

        // Выделяем новый блок каталога.
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

    /// Общая предпроверка новой записи каталога: отказ непригодным именам и
    /// имени, которое уже есть в `parent_ino`. Используется `create`,
    /// `create_symlink` и `link`, чтобы все три давали одинаковые ошибки.
    ///
    /// Проверка дубля — только оптимизация: `insert_dirent` перепроверяет её
    /// внутри транзакции.
    fn check_new_entry(&self, parent_ino: u32, name: &str) -> Result<(), FsError> {
        if name.is_empty() || name == "." || name == ".." {
            return Err(FsError::Corrupt);
        }
        // Имя записи каталога никогда не содержит разделителя: вызывающие уже
        // разбили путь, а запись с '/' в имени больше никогда не нашлась бы
        // (`lookup_entry` режет по '/') — недостижимый и неудаляемый файл.
        // Отказываем на стороне писателя.
        if name.contains('/') {
            return Err(FsError::Corrupt);
        }
        if name.as_bytes().len() > 255 {
            return Err(FsError::NameTooLong);
        }
        if self.lookup_entry(parent_ino, name).is_ok() {
            return Err(FsError::AlreadyExists);
        }
        Ok(())
    }

    /// Создаёт обычный файл или каталог `name` в `parent_ino`. Возвращает номер
    /// нового inode.
    pub fn create(&self, parent_ino: u32, name: &str, is_dir: bool) -> Result<u32, FsError> {
        self.check_new_entry(parent_ino, name)?;

        let mut tx = Tx::new(self);
        let new_ino = tx.alloc_new_inode()?;

        let mut inode = Ext2Inode::zeroed();
        if is_dir {
            // Выделяем и инициализируем блок данных нового каталога.
            let dblock = tx.alloc_zeroed_block()?;
            {
                let buf = tx.block(dblock as u64)?;
                dir::init_dot_entries(buf, new_ino, parent_ino);
            }
            inode.i_mode = S_IFDIR | 0o755;
            inode.i_links_count = 2; // "." + запись в родителе
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

        // Привязываем в родительский каталог.
        Self::insert_dirent(&mut tx, parent_ino, name, new_ino)?;

        if is_dir {
            // ".." ребёнка добавляет хардлинк на родителя; увеличиваем used_dirs.
            let mut pinode = tx.read_inode(parent_ino)?;
            pinode.i_links_count += 1;
            tx.write_inode(parent_ino, &pinode)?;
            let g = ((new_ino - 1) / tx.sb.s_inodes_per_group) as usize;
            tx.gds[g].bg_used_dirs_count += 1;
        }

        tx.commit()?;
        Ok(new_ino)
    }

    /// Создаёт симлинк `name` в `parent_ino` с целью из сырых байт `target`
    /// (цель хранится дословно — она никогда не резолвится и не нормализуется).
    /// Возвращает номер нового inode.
    ///
    /// On-disk форму выбирает [`symlink::plan`]:
    ///
    ///   * **fast** — `target.len() <= 59`: цель с завершающим NUL помещается в
    ///     `i_block`, `i_blocks == 0`, блок данных не выделяется;
    ///   * **slow** — цели длиннее: один обнулённый блок данных несёт цель (его
    ///     хвост по построению нулевой), `i_blocks == BS/512`.
    ///
    /// Медленный блок берётся через `Tx::data_block`, поэтому пишется на
    /// финальное место *до* коммита транзакции метаданных (ordered mode):
    /// закоммиченный inode симлинка не может указывать на незаписанный блок. Одна
    /// транзакция покрывает inode, его бит в битмапе, запись в родительском
    /// каталоге и счётчики суперблока/группы — ничего не пишется вне обычных
    /// транзакционных границ.
    ///
    /// Пустая цель отвергается ([`FsError::Corrupt`]): молча созданная ссылка
    /// без цели — ровно та порча, которую не должна вносить распаковка `tar`.
    pub fn create_symlink(
        &self,
        parent_ino: u32,
        name: &str,
        target: &[u8],
    ) -> Result<u32, FsError> {
        self.check_new_entry(parent_ino, name)?;
        let plan = match symlink::plan(target) {
            Ok(p) => p,
            Err(symlink::SymlinkError::Empty) => return Err(FsError::Corrupt),
            Err(symlink::SymlinkError::TooBig) => return Err(FsError::FileTooBig),
        };

        let mut tx = Tx::new(self);
        let new_ino = tx.alloc_new_inode()?;

        let mut inode = Ext2Inode::zeroed();
        inode.i_mode = S_IFLNK | 0o777; // у симлинков ext2 всегда rwxrwxrwx
        inode.i_size = plan.size as u32;
        inode.i_links_count = 1;
        match plan.layout {
            SymlinkLayout::Fast => {
                let inline = plan.inline.unwrap_or([0u8; symlink::INLINE_CAPACITY]);
                inode.i_block = symlink::inline_words(&inline);
                inode.i_blocks = 0;
            }
            SymlinkLayout::Slow => {
                let blk = tx.alloc_zeroed_block()?;
                {
                    let b = tx.data_block(blk as u64, true)?;
                    b[..target.len()].copy_from_slice(target);
                }
                inode.i_block[0] = blk;
                inode.i_blocks = (BS / 512) as u32;
            }
        }
        tx.write_inode(new_ino, &inode)?;

        // Привязываем в родительский каталог.
        Self::insert_dirent(&mut tx, parent_ino, name, new_ino)?;

        tx.commit()?;
        Ok(new_ino)
    }

    /// Читает цель симлинка `ino`: ровно `i_size` байт, без хвостового NUL, из
    /// зафиксированного образа inode.
    ///
    /// Fast-ссылки декодируются из `i_block` (`read` не должен их трогать:
    /// inline-байты не указатели на блоки); slow читаются из единственного блока
    /// данных. Не-симлинк и slow-ссылка без первого блока — [`FsError::Corrupt`]:
    /// обрезанная цель молча увела бы вызывающего на другой путь.
    pub fn read_symlink(&self, ino: u32) -> Result<Vec<u8>, FsError> {
        let inode = self.read_inode(ino)?;
        if !inode.is_symlink() {
            return Err(FsError::Corrupt);
        }
        // Решение о раскладке ограничено on-disk формой: подделанный
        // `i_blocks = 8, i_size = 0xFFFFFFF0` — `Corrupt`, а не
        // `vec![0u8; i_size]`. `i_blocks == 0` при огромном `i_size` тоже порча,
        // а не slow: эти байты — текст цели, а не карта блоков.
        match symlink::target_shape(inode.i_blocks, inode.i_size) {
            TargetShape::Fast(size) => {
                let inline = symlink::inline_bytes(&inode.i_block);
                let target = symlink::fast_target(&inline, size as u32).ok_or(FsError::Corrupt)?;
                Ok(target.to_vec())
            }
            TargetShape::Slow(size) => {
                if size == 0 {
                    return Ok(Vec::new());
                }
                if inode::block_for_offset(self, &inode, 0)?.is_none() {
                    return Err(FsError::Corrupt);
                }
                let mut out = vec![0u8; size];
                let n = self.read_file(ino, 0, &mut out)?;
                out.truncate(n);
                Ok(out)
            }
            TargetShape::Corrupt => Err(FsError::Corrupt),
        }
    }

    /// Добавляет хардлинк `name` в `parent_ino` на существующий inode
    /// `target_ino`: тот же inode, `i_links_count` на единицу больше, ни одного
    /// нового блока и без копирования содержимого.
    ///
    /// Отказ для каталога-цели — хардлинков на каталоги в ext2 нет, а
    /// `i_links_count` каталога считает `.`/`..`, а не имена — и для уже
    /// свободного inode. И дубль имени, и нелинкуемая цель дают
    /// [`FsError::AlreadyExists`], как `unlink` сообщает о непустом каталоге.
    ///
    /// Инкремент счётчика и запись каталога коммитятся в **одной** транзакции,
    /// поэтому on-disk состояние никогда не покажет счётчик ссылок, не
    /// совпадающий с числом имён, указывающих на inode.
    pub fn link(&self, parent_ino: u32, name: &str, target_ino: u32) -> Result<(), FsError> {
        self.check_new_entry(parent_ino, name)?;

        let mut tx = Tx::new(self);
        let mut target = tx.read_inode(target_ino)?;
        if target.is_dir() {
            return Err(FsError::AlreadyExists);
        }
        target.i_links_count = symlink::link_bump(target.i_links_count).ok_or(FsError::Corrupt)?;
        tx.write_inode(target_ino, &target)?;

        Self::insert_dirent(&mut tx, parent_ino, name, target_ino)?;

        tx.commit()
    }

    /// Удаляет `name` из `parent_ino`.
    ///
    /// Каталоги обязаны быть пустыми и освобождаются целиком всегда. Для
    /// остальных inode решает счётчик ссылок: если на него указывает больше
    /// одного имени, уменьшается только `i_links_count`, а data/indirect-блоки и
    /// inode освобождаются лишь для **последнего** имени. Обе ветки коммитятся в
    /// той же транзакции, что и удаление записи каталога, поэтому крах не может
    /// оставить запись, указывающую на свободный inode.
    pub fn unlink(&self, parent_ino: u32, name: &str) -> Result<(), FsError> {
        if name == "." || name == ".." {
            return Err(FsError::Corrupt);
        }
        let mut tx = Tx::new(self);
        let mut pinode = tx.read_inode(parent_ino)?;
        if !pinode.is_dir() {
            return Err(FsError::NotFound);
        }

        // Находим блок каталога с записью и дочерний inode.
        let nblocks = tx.fs.dir_blocks(&pinode)?;
        let mut found: Option<(u64, u32)> = None; // (блок каталога, ino ребёнка)
        for lbn in 0..nblocks {
            // Скан read-only (см. insert_dirent): при поиске ничего не аллоцируем.
            let Some(blk) = inode::block_for_offset(tx.fs, &pinode, lbn * BS as u64)? else {
                continue;
            };
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
        let mut child = tx.read_inode(child_ino)?;

        // Проверка пустоты каталога (читаем закоммиченное состояние; child здесь
        // не изменён).
        if child.is_dir() {
            let cblocks = tx.fs.dir_blocks(&child)?;
            for lbn in 0..cblocks {
                if let Some(cb) = inode::block_for_offset(self, &child, lbn * BS as u64)? {
                    let buf = self.read_fs_block(cb as u64)?;
                    if dir::live_entry_count(&buf)? != 0 {
                        return Err(FsError::AlreadyExists); // непустой каталог
                    }
                }
            }
        }

        // Удаляем запись каталога.
        {
            let buf = tx.block(dir_block as u64)?;
            dir::remove_from_block(buf, name)?;
        }

        // Освобождаем ребёнка по счётчику ссылок.
        //
        // Каталог освобождается целиком всегда: его `i_links_count` считает
        // ссылки `.`/`..`, а не имена, и хардлинка на каталог быть не может,
        // поэтому случай «на него ещё указывает другое имя» невозможен
        // (непустой `rmdir` приходит через `remove`). Всё остальное использует
        // чистое правило `symlink::unlink_action`: несколько имён делят inode,
        // поэтому только последнее освобождает блоки — раннее освобождение
        // уничтожило бы данные, которые читают оставшиеся имена.
        let released = if child.is_dir() {
            true
        } else {
            match symlink::unlink_action(child.i_links_count) {
                Some(UnlinkAction::DropLink) => {
                    child.i_links_count -= 1;
                    false
                }
                Some(UnlinkAction::FreeInode) => true,
                // Живая запись каталога на свободный inode — уже порча;
                // повторное «освобождение» могло бы освободить переиспользованный
                // inode.
                None => return Err(FsError::Corrupt),
            }
        };
        if released {
            tx.free_inode_blocks(&child)?;
            tx.free_inode_bit(child_ino)?;
            child.i_links_count = 0;
        }
        tx.write_inode(child_ino, &child)?;

        if child.is_dir() {
            pinode.i_links_count = pinode.i_links_count.saturating_sub(1);
            let g = ((child_ino - 1) / tx.sb.s_inodes_per_group) as usize;
            tx.gds[g].bg_used_dirs_count = tx.gds[g].bg_used_dirs_count.saturating_sub(1);
        }
        tx.write_inode(parent_ino, &pinode)?;

        tx.commit()
    }

    /// Читает до `buf.len()` байт файла `ino` с `offset`, с клампом по `i_size`.
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

    /// Усекает обычный файл до `new_size` байт.
    ///
    /// Усадка освобождает блоки за новым размером одной транзакцией журнала
    /// (снимая указатели дерева и освобождая опустевшие indirect-блоки) и
    /// обнуляет хвост граничного блока. Рост заливает промежуток нулями через
    /// обычный путь записи. `i_size` — 32-битное поле, поэтому большие размеры —
    /// `FileTooBig`.
    pub fn truncate_file(&self, ino: u32, new_size: u64) -> Result<(), FsError> {
        if new_size > u32::MAX as u64 {
            return Err(FsError::FileTooBig);
        }
        let inode0 = self.read_inode(ino)?;
        // Блоки данных есть только у обычных файлов. Inode симлинка держит цель
        // в `i_block` (fast) или в своём единственном блоке (slow) — усечение
        // переписало бы цель ссылки как указатели на блоки.
        if !inode0.is_reg() {
            return Err(FsError::Corrupt);
        }
        let old_size = inode0.i_size as u64;
        if new_size == old_size {
            return Ok(());
        }
        if new_size > old_size {
            const ZERO_CHUNK: usize = 64 * 1024;
            let zeros = [0u8; ZERO_CHUNK];
            let mut off = old_size;
            while off < new_size {
                let n = core::cmp::min(ZERO_CHUNK as u64, new_size - off) as usize;
                self.write_file(ino, off, &zeros[..n])?;
                off += n as u64;
            }
            return Ok(());
        }

        // Усадка.
        let mut tx = Tx::new(self);
        let mut inode = tx.read_inode(ino)?;
        if !inode.is_reg() {
            return Err(FsError::Corrupt);
        }
        let old_nblocks = (old_size as usize + BS - 1) / BS;
        let new_nblocks = (new_size as usize + BS - 1) / BS;
        let mut freed: u32 = 0;
        for lbn in new_nblocks..old_nblocks {
            freed += tx.clear_mapping(&mut inode, lbn as u64)?;
        }
        // Обнуляем хвост граничного блока, если новый размер не выровнен по
        // блоку и этот блок материализован.
        if new_size % BS as u64 != 0 && new_nblocks > 0 {
            let lbn = (new_nblocks - 1) as u64;
            if let Some(blk) = inode::block_for_offset(tx.fs, &inode, lbn * BS as u64)? {
                let b = tx.data_block(blk as u64, false)?;
                for x in b[(new_size % BS as u64) as usize..].iter_mut() {
                    *x = 0;
                }
            }
        }
        inode.i_size = new_size as u32;
        inode.i_blocks = inode.i_blocks.saturating_sub(freed * (BS / 512) as u32);
        tx.write_inode(ino, &inode)?;
        tx.commit()
    }

    /// Пишет `data` в файл `ino` с `offset`, аллоцируя блоки и увеличивая
    /// `i_size` по необходимости. Каждый чанк атомарен через журнал.
    ///
    /// Лог журнала вмещает только `FMT_LOG_BLOCKS` (64) блока, а
    /// `Journal::commit` отвергает транзакцию с более чем `log_blocks - 2`
    /// грязными блоками (`FsError::OutOfSpace`). Поэтому большие записи режутся
    /// на несколько транзакций не более чем по `TX_DATA_BLOCKS` data-блоков;
    /// грязные метаданные (битмапы, inode-таблица, indirect-блоки, суперблок и
    /// групповые дескрипторы) остаются заметно ниже лимита WAL. Атомарность
    /// всего файла здесь не требуется: установщик сам удаляет частично
    /// записанные файлы при ошибке (R10.4).
    pub fn write_file(&self, ino: u32, offset: u64, data: &[u8]) -> Result<usize, FsError> {
        if data.is_empty() {
            return Ok(0);
        }
        // `i_size` — 32-битное поле: запись за 4 ГиБ завернула бы `i_size` и
        // заалиасила файл на его младшие 4 ГиБ.
        if offset.saturating_add(data.len() as u64) > u32::MAX as u64 {
            return Err(FsError::FileTooBig);
        }
        // 64 data-блока на Tx — данные больше не журналируются, поэтому лимит
        // WAL должен вмещать только метаблоки.
        const TX_DATA_BLOCKS: usize = 64;

        let mut written = 0usize;
        let mut pos = offset;
        while written < data.len() {
            {
                let mut tx = Tx::new(self);
                let mut inode = tx.read_inode(ino)?;
                // Блоки данных есть только у обычных файлов (см. `truncate_file`):
                // у inode симлинка `map_or_alloc` затёр бы inline-цель.
                if !inode.is_reg() {
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
            // Транзакция отпустила `tx_lock` и `inner`, то есть прерывания снова
            // разрешены. Явная уступка не даёт длинной записи (распаковка пакета)
            // монополизировать CPU между транзакциями: композитор получает кадр
            // посреди установки, а не после неё.
            crate::task::scheduler::cooperative_checkpoint();
        }
        Ok(written)
    }
}

// ─── адаптеры VfsNode ───────────────────────────────────────────────────────

fn fs_to_vfs(e: FsError) -> VfsError {
    match e {
        FsError::NotFound => VfsError::NotFound,
        FsError::AlreadyExists => VfsError::AlreadyExists,
        FsError::NameTooLong => VfsError::InvalidArgument,
        FsError::FileTooBig => VfsError::InvalidArgument,
        FsError::OutOfSpace => VfsError::IoError,
        _ => VfsError::IoError,
    }
}

/// Пишет `(uid, gid)` в inode. ext2 хранит оба как `u16`, поэтому не
/// помещающееся значение отвергается, а не обрезается молча: `chown(70000)` не
/// должен стать владельцем 4464.
fn ext2_set_owner(fs: &Arc<Ext2Fs>, ino: u32, uid: u32, gid: u32) -> VfsResult<()> {
    if uid > u16::MAX as u32 || gid > u16::MAX as u32 {
        return Err(VfsError::InvalidArgument);
    }
    fs.set_inode_owner(ino, uid as u16, gid as u16)
        .map_err(fs_to_vfs)
}

struct Ext2Dir {
    fs: Arc<Ext2Fs>,
    ino: u32,
    name: String,
    /// Размер, снятый при создании узла (после успешного чтения inode). Фоллбэк
    /// для `size()`, если последующий `read_inode` упрётся в ошибку устройства:
    /// ответ 0 заставлял установщики копировать пустые файлы (ld-linux.so).
    cached_size: u64,
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
    fn mode(&self) -> u32 {
        self.fs.node_mode(self.ino, 0o755)
    }
    fn owner(&self) -> (u32, u32) {
        self.fs.node_owner(self.ino)
    }
    fn set_mode(&self, mode: u32) -> VfsResult<()> {
        self.fs.set_inode_mode(self.ino, mode).map_err(fs_to_vfs)
    }
    fn set_owner(&self, uid: u32, gid: u32) -> VfsResult<()> {
        ext2_set_owner(&self.fs, self.ino, uid, gid)
    }
    fn fs_stat(&self) -> Option<FsStat> {
        let sb = self.fs.superblock();
        Some(FsStat {
            block_size: BS as u64,
            blocks_total: sb.s_blocks_count as u64,
            blocks_free: sb.s_free_blocks_count as u64,
            inodes_total: sb.s_inodes_count as u64,
            inodes_free: sb.s_free_inodes_count as u64,
        })
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
    /// `ln -s`: байты цели сохраняются дословно.
    fn create_symlink(&self, name: &str, target: &[u8]) -> VfsResult<Arc<dyn VfsNode>> {
        let ino = self
            .fs
            .create_symlink(self.ino, name, target)
            .map_err(fs_to_vfs)?;
        Ext2Fs::node_for(&self.fs, ino, name).map_err(fs_to_vfs)
    }
    /// `ln` (хардлинк): второе имя для inode цели, никогда не копия.
    fn link(&self, name: &str, target: &Arc<dyn VfsNode>) -> VfsResult<Arc<dyn VfsNode>> {
        let ino = target.fs_ino();
        if ino == 0 || ino > u32::MAX as u64 {
            return Err(VfsError::InvalidArgument);
        }
        self.fs
            .link(self.ino, name, ino as u32)
            .map_err(fs_to_vfs)?;
        Ext2Fs::node_for(&self.fs, ino as u32, name).map_err(fs_to_vfs)
    }
    fn nlink(&self) -> u64 {
        self.fs
            .read_inode(self.ino)
            .map(|i| i.i_links_count as u64)
            .unwrap_or(1)
    }
    fn remove(&self, name: &str) -> VfsResult<()> {
        self.fs.unlink(self.ino, name).map_err(fs_to_vfs)
    }
    fn sync(&self) {
        self.fs.sync()
    }
    fn size(&self) -> u64 {
        match self.fs.read_inode(self.ino) {
            Ok(i) => i.i_size as u64,
            Err(_) => self.cached_size,
        }
    }
}

struct Ext2File {
    fs: Arc<Ext2Fs>,
    ino: u32,
    name: String,
    /// См. `Ext2Dir::cached_size`.
    cached_size: u64,
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
    fn mode(&self) -> u32 {
        self.fs.node_mode(self.ino, 0o644)
    }
    fn owner(&self) -> (u32, u32) {
        self.fs.node_owner(self.ino)
    }
    fn set_mode(&self, mode: u32) -> VfsResult<()> {
        self.fs.set_inode_mode(self.ino, mode).map_err(fs_to_vfs)
    }
    fn set_owner(&self, uid: u32, gid: u32) -> VfsResult<()> {
        ext2_set_owner(&self.fs, self.ino, uid, gid)
    }
    fn fs_stat(&self) -> Option<FsStat> {
        let sb = self.fs.superblock();
        Some(FsStat {
            block_size: BS as u64,
            blocks_total: sb.s_blocks_count as u64,
            blocks_free: sb.s_free_blocks_count as u64,
            inodes_total: sb.s_inodes_count as u64,
            inodes_free: sb.s_free_inodes_count as u64,
        })
    }
    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.fs.read_file(self.ino, offset, buf).map_err(fs_to_vfs)
    }
    fn write(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.fs.write_file(self.ino, offset, buf).map_err(fs_to_vfs)
    }
    fn truncate(&self, size: u64) -> VfsResult<()> {
        self.fs.truncate_file(self.ino, size).map_err(fs_to_vfs)
    }
    /// `i_links_count` в обе стороны: файл с хардлинками сообщает число имён,
    /// указывающих на него.
    fn nlink(&self) -> u64 {
        self.fs
            .read_inode(self.ino)
            .map(|i| i.i_links_count as u64)
            .unwrap_or(1)
    }
    fn size(&self) -> u64 {
        match self.fs.read_inode(self.ino) {
            Ok(i) => i.i_size as u64,
            Err(_) => self.cached_size,
        }
    }
}

/// Симлинк (ext2 `S_IFLNK`).
///
/// Читаемого содержимого у узла нет: `read`/`write`/`truncate` остаются
/// `NotSupported`, потому что у *fast*-ссылки эти байты — текст цели в `i_block`,
/// а не data-блоки. `size()` — это `i_size`, то есть длина цели: её показывает
/// `lstat` и она же верхняя граница ответа `readlink`.
///
/// Guest-видимая сторона — аддитивный `VfsNode::readlink() -> Option<String>`
/// (введён для procfs, `docs/procfs.md` §6.1), переопределённый здесь через
/// [`Self::target_bytes`].
struct Ext2Symlink {
    fs: Arc<Ext2Fs>,
    ino: u32,
    name: String,
    /// См. `Ext2Dir::cached_size`.
    cached_size: u64,
}

impl Ext2Symlink {
    /// Цель ссылки ровно как хранится (без хвостового NUL), либо `None`, если
    /// inode не является корректным симлинком или чтение с устройства не удалось.
    fn target_bytes(&self) -> Option<Vec<u8>> {
        self.fs.read_symlink(self.ino).ok()
    }
}

impl VfsNode for Ext2Symlink {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_directory(&self) -> bool {
        false
    }
    /// Подключено к механике ссылок VFS: `readlink(2)`, `lstat` и `d_type`
    /// ветвятся по этому, а `vfs::lookup_path_walk` следует по ссылке только
    /// когда это true.
    fn is_symlink(&self) -> bool {
        true
    }
    /// Сохранённая цель. VFS моделирует цели как `String` (пути ядра везде
    /// UTF-8), поэтому невалидная UTF-8 цель — возможна только в чужом образе,
    /// этот писатель хранит то, что дал `tar`, — отдаётся lossy; walker затем
    /// просто не сможет её разрешить (`ENOENT`).
    fn read_link(&self) -> Option<String> {
        self.target_bytes()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }
    fn fs_ino(&self) -> u64 {
        self.ino as u64
    }
    fn mode(&self) -> u32 {
        self.fs.node_mode(self.ino, 0o777)
    }
    fn owner(&self) -> (u32, u32) {
        self.fs.node_owner(self.ino)
    }
    fn set_mode(&self, mode: u32) -> VfsResult<()> {
        self.fs.set_inode_mode(self.ino, mode).map_err(fs_to_vfs)
    }
    fn set_owner(&self, uid: u32, gid: u32) -> VfsResult<()> {
        ext2_set_owner(&self.fs, self.ino, uid, gid)
    }
    fn fs_stat(&self) -> Option<FsStat> {
        let sb = self.fs.superblock();
        Some(FsStat {
            block_size: BS as u64,
            blocks_total: sb.s_blocks_count as u64,
            blocks_free: sb.s_free_blocks_count as u64,
            inodes_total: sb.s_inodes_count as u64,
            inodes_free: sb.s_free_inodes_count as u64,
        })
    }
    fn nlink(&self) -> u64 {
        self.fs
            .read_inode(self.ino)
            .map(|i| i.i_links_count as u64)
            .unwrap_or(1)
    }
    fn size(&self) -> u64 {
        match self.fs.read_inode(self.ino) {
            Ok(i) => i.i_size as u64,
            Err(_) => self.cached_size,
        }
    }
}

/// Inode вида, который этот писатель не создаёт (device-узел чужого образа,
/// fifo, socket, ...): виден и stat-абелен, но никогда не читается как файл,
/// поэтому неизвестный `i_mode` не попадает в карту блоков обычного файла.
struct Ext2Opaque {
    fs: Arc<Ext2Fs>,
    ino: u32,
    name: String,
    /// См. `Ext2Dir::cached_size`.
    cached_size: u64,
}

impl VfsNode for Ext2Opaque {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn fs_ino(&self) -> u64 {
        self.ino as u64
    }
    fn mode(&self) -> u32 {
        self.fs.node_mode(self.ino, 0o644)
    }
    fn owner(&self) -> (u32, u32) {
        self.fs.node_owner(self.ino)
    }
    fn set_mode(&self, mode: u32) -> VfsResult<()> {
        self.fs.set_inode_mode(self.ino, mode).map_err(fs_to_vfs)
    }
    fn set_owner(&self, uid: u32, gid: u32) -> VfsResult<()> {
        ext2_set_owner(&self.fs, self.ino, uid, gid)
    }
    fn size(&self) -> u64 {
        self.cached_size
    }
}
