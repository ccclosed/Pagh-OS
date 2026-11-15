//! Журнал write-ahead-log (WAL) над `BlockDevice`.
//!
//! Даёт атомарные многоблочные транзакции и crash-consistent реплей. Область
//! журнала лежит **после** последнего блока ext2 (`>= s_blocks_count`), поэтому
//! видимая хосту раскладка ext2 не меняется; цели транзакций — индексы блоков
//! ext2 в `[0, fs_blocks)`.
//!
//! On-disk транзакция — `[Descriptor][Data]*N[Commit]`. Транзакция
//! *закоммичена*, если за её data-блоками сразу идёт валидный `Commit` (магия +
//! совпадающий `seq` + CRC32 по залогированным данным). `commit` пишет записи
//! лога и commit-запись, **затем** делает checkpoint данных на финальные места
//! ext2; точка долговечности — commit-запись. `recover` сканирует лог от `tail`
//! и идемпотентно реплеит только цепочку от персистентного `next_seq`,
//! останавливаясь на первой незакоммиченной, испорченной или вышедшей из порядка
//! транзакции.

#![allow(dead_code)]

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::drivers::BlockDevice;
use crate::fs::ext2::structs::{
    self, JournalCommit, JournalDescriptor, JournalSuper, BS, JCMT_MAGIC, JDESC_MAX_TARGETS,
    JDES_MAGIC, JNL_MAGIC, SECTORS_PER_BLOCK,
};
use crate::fs::FsError;

/// Расположение области журнала на устройстве (в единицах блоков ФС).
#[derive(Clone, Copy)]
pub struct JournalArea {
    /// Блок ФС с суперблоком журнала (== ext2 `s_blocks_count`).
    pub super_block: u64,
    /// Число блоков кольцевого лога (без суперблока журнала).
    pub log_blocks: u64,
    /// ext2 `s_blocks_count` — начало области журнала / граница целей.
    pub fs_blocks: u64,
}

/// Открытая транзакция: набор `(финальный блок ext2, новые BS байт)`.
pub struct Txn {
    records: Vec<(u64, Vec<u8>)>,
}

impl Txn {
    pub fn len(&self) -> usize {
        self.records.len()
    }
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// Журнал WAL.
pub struct Journal {
    dev: Arc<dyn BlockDevice>,
    super_block: u64,
    log_start: u64, // первый блок лога (super_block + 1)
    log_blocks: u64,
    fs_blocks: u64,
    head: u64, // следующая свободная позиция лога (относительно лог-пространства)
    tail: u64, // самая старая живая позиция лога
    next_seq: u64,
}

// ─── низкоуровневый блоковый I/O (блоки ФС 4096 байт через секторы 512) ─────

fn read_block(dev: &dyn BlockDevice, fs_block: u64) -> Result<Vec<u8>, FsError> {
    let mut buf = vec![0u8; BS];
    dev.read_block(fs_block * SECTORS_PER_BLOCK, &mut buf)
        .map(|_| ())
        .map_err(|_| FsError::IoError)?;
    Ok(buf)
}

fn write_block(dev: &dyn BlockDevice, fs_block: u64, data: &[u8]) -> Result<(), FsError> {
    debug_assert!(data.len() == BS);
    dev.write_block(fs_block * SECTORS_PER_BLOCK, data)
        .map(|_| ())
        .map_err(|_| FsError::IoError)
}

impl Journal {
    /// Абсолютный блок ФС для позиции лога `pos` (с заворотом кольца).
    fn log_block_addr(&self, pos: u64) -> u64 {
        self.log_start + (pos % self.log_blocks)
    }

    fn read_log(&self, pos: u64) -> Result<Vec<u8>, FsError> {
        read_block(&*self.dev, self.log_block_addr(pos))
    }

    fn write_log(&self, pos: u64, data: &[u8]) -> Result<(), FsError> {
        write_block(&*self.dev, self.log_block_addr(pos), data)
    }

    /// Пишет пустой суперблок журнала в зарезервированную область (формат).
    ///
    /// **Голова кольца тоже инвалидируется.** Лог переживает свою ФС, а
    /// `recover` всегда стартует с `tail == 0`: без обнулённого слота первая
    /// запись прошлой инкарнации (её `seq == 1` совпадает с новым `next_seq`)
    /// прошла бы проверку цепочки, и её старые образы легли бы в свежую ФС.
    /// Скан останавливается на первом слоте с неверной магией дескриптора,
    /// поэтому достаточно обнулить позицию 0.
    pub fn format(dev: &dyn BlockDevice, area: JournalArea) -> Result<(), FsError> {
        let js = JournalSuper {
            magic: JNL_MAGIC,
            head: 0,
            tail: 0,
            next_seq: 1,
            log_blocks: area.log_blocks,
            fs_blocks: area.fs_blocks,
            checksum: 0,
        };
        let mut buf = vec![0u8; BS];
        store_journal_super(&mut buf, &js);
        write_block(dev, area.super_block, &buf)?;
        write_block(dev, area.super_block + 1, &vec![0u8; BS])
    }

    /// Открывает журнал: читает и валидирует суперблок.
    ///
    /// Проверяется персистентный CRC32 (`store_journal_super`): оборванный или
    /// мусорный суперблок не должен сойти за состояние `head`/`tail` и
    /// заставить реплей стартовать с произвольных позиций.
    pub fn open(dev: Arc<dyn BlockDevice>, area: JournalArea) -> Result<Self, FsError> {
        let buf = read_block(&*dev, area.super_block)?;
        let js = load_journal_super(&buf)?;
        if js.magic != JNL_MAGIC {
            return Err(FsError::BadJournal);
        }
        if js.log_blocks == 0 {
            return Err(FsError::BadJournal);
        }
        Ok(Journal {
            dev,
            super_block: area.super_block,
            log_start: area.super_block + 1,
            log_blocks: js.log_blocks,
            fs_blocks: js.fs_blocks,
            head: js.head,
            tail: js.tail,
            next_seq: js.next_seq,
        })
    }

    fn persist_super(&self) -> Result<(), FsError> {
        let js = JournalSuper {
            magic: JNL_MAGIC,
            head: self.head,
            tail: self.tail,
            next_seq: self.next_seq,
            log_blocks: self.log_blocks,
            fs_blocks: self.fs_blocks,
            checksum: 0,
        };
        let mut buf = vec![0u8; BS];
        store_journal_super(&mut buf, &js);
        write_block(&*self.dev, self.super_block, &buf)
    }

    /// Начинает новую (пустую) транзакцию.
    pub fn begin(&self) -> Txn {
        Txn {
            records: Vec::new(),
        }
    }

    /// Добавляет запись блока в транзакцию. `contents` дополняется/обрезается
    /// ровно до `BS` байт; `final_block` — индекс целевого блока ext2.
    pub fn log_block(&self, txn: &mut Txn, final_block: u64, contents: &[u8]) {
        let mut block = vec![0u8; BS];
        let n = core::cmp::min(contents.len(), BS);
        block[..n].copy_from_slice(&contents[..n]);
        // Если одна цель логируется дважды, побеждает поздняя запись.
        if let Some(existing) = txn.records.iter_mut().find(|(t, _)| *t == final_block) {
            existing.1 = block;
        } else {
            txn.records.push((final_block, block));
        }
    }

    /// Коммитит транзакцию: пишет дескриптор + данные + commit-запись в лог,
    /// делает checkpoint данных на финальные места ext2, затем сдвигает head и
    /// персистит суперблок журнала.
    ///
    /// Checkpoint синхронный, поэтому всё залогированное в `[tail, head)` уже
    /// на финальных местах к моменту сдвига head: при завороте кольца в tail
    /// живые записи отбрасываются (`tail = head`), и проверка свободного места
    /// ниже страхует только текущую транзакцию.
    pub fn commit(&mut self, txn: Txn) -> Result<(), FsError> {
        let count = txn.records.len();
        if count == 0 {
            return Ok(());
        }
        if count > JDESC_MAX_TARGETS {
            // Один дескриптор не описывает больше блоков, чем этот предел.
            return Err(FsError::OutOfSpace);
        }
        // Нужно count + 2 слота лога (дескриптор + данные + commit) и хотя бы
        // один запасной сверх того: арифметика свободного места ниже читает
        // `head == tail` как «пусто», поэтому запись не должна заполнять
        // кольцо ровно до конца.
        if (count as u64) + 3 > self.log_blocks {
            return Err(FsError::OutOfSpace);
        }
        let used = if self.head >= self.tail {
            self.head - self.tail
        } else {
            self.head + self.log_blocks - self.tail
        };
        let free = self.log_blocks - used;
        if (count as u64) + 2 > free {
            // Всё между tail и head уже зачекпойнчено (синхронно, до сдвига
            // head); освобождаем весь лог под эту транзакцию, вместо того чтобы
            // заворачиваться прямо поверх живых записей.
            //
            // Освобождённый `tail` обязан стать долговечным *до* записи ниже:
            // завернувшаяся запись начнётся с `head` и ляжет на самую старую
            // живую запись — ровно слот, который называл старый on-disk `tail`.
            // Крах после commit-записи, но до `persist_super`, оставил бы
            // on-disk `tail`, не указывающий на дескриптор; `recover`
            // остановился бы на нём и потерял закоммиченную транзакцию.
            self.tail = self.head;
            self.persist_super()?;
            self.dev.flush().map_err(|_| FsError::IoError)?;
        }

        // Проверяем, что каждая цель лежит внутри региона ext2.
        for (target, contents) in &txn.records {
            if *target >= self.fs_blocks || contents.len() != BS {
                return Err(FsError::Corrupt);
            }
        }

        let seq = self.next_seq;
        let desc_pos = self.head;

        // 1. Блок дескриптора.
        let mut desc = JournalDescriptor {
            magic: JDES_MAGIC,
            seq,
            count: count as u32,
            targets: [0u64; JDESC_MAX_TARGETS],
        };
        for (i, (target, _)) in txn.records.iter().enumerate() {
            desc.targets[i] = *target;
        }
        let mut desc_buf = vec![0u8; BS];
        store_descriptor(&mut desc_buf, &desc);
        self.write_log(desc_pos, &desc_buf)?;

        // 2. Data-записи в лог (пока НЕ на финальные места).
        for (i, (_target, contents)) in txn.records.iter().enumerate() {
            self.write_log(desc_pos + 1 + i as u64, contents)?;
        }

        // 3. Commit-запись: CRC32 по всем data-блокам.
        let data_refs: Vec<&[u8]> = txn.records.iter().map(|(_, c)| c.as_slice()).collect();
        let checksum = structs::crc32_slices(&data_refs);
        let cmt = JournalCommit {
            magic: JCMT_MAGIC,
            seq,
            data_checksum: checksum,
            _pad: 0,
        };
        let mut cmt_buf = vec![0u8; BS];
        store_commit(&mut cmt_buf, &cmt);
        self.write_log(desc_pos + 1 + count as u64, &cmt_buf)?;

        // === Транзакция теперь атомарна: commit-запись делает её реплеящейся.
        //
        // ...но только когда она *долговечна*. На устройстве с волатильным
        // кэшем записи лога выше подтверждены, но не сохранены, поэтому flush
        // до checkpoint'а: крах отсюда и дальше всё ещё сможет реплеить
        // закоммиченную транзакцию из лога.
        self.dev.flush().map_err(|_| FsError::IoError)?;

        // 4. Checkpoint: копируем залогированные блоки на финальные места ext2.
        for (target, contents) in &txn.records {
            write_block(&*self.dev, *target, contents)?;
        }

        // Зачекпойнченные образы тоже обязаны стать долговечными *до* сдвига
        // head ниже, который объявляет лог пустым: иначе крах оставил бы лог,
        // считающийся реплеинутым, и суперблок с тем же утверждением, когда
        // данные ещё только в кэше устройства — закоммиченная транзакция
        // потеряна.
        self.dev.flush().map_err(|_| FsError::IoError)?;

        // 5. Сдвигаем head и персистим суперблок журнала.
        self.head = (desc_pos + count as u64 + 2) % self.log_blocks;
        self.next_seq = seq + 1;
        self.persist_super()?;
        Ok(())
    }

    /// Реплей закоммиченных транзакций при монтировании.
    ///
    /// Сканирует лог от `tail`: транзакция реплеится, если она закоммичена
    /// (валидные descriptor + commit, совпадающие seq + CRC32) **и** её номер
    /// ещё не подтверждён персистентным суперблоком журнала. Реплей применяет
    /// каждый data-блок на финальное место (идемпотентная перезапись); скан
    /// останавливается на первой незакоммиченной, испорченной или вышедшей из
    /// порядка транзакции, остальное не трогает. Затем лог сбрасывается в пустое
    /// состояние.
    ///
    /// Живость записи решают `seq` против персистентного `next_seq`, а **не**
    /// `head == tail`: `commit` персистит сдвиг head после checkpoint'а, поэтому
    /// `head == tail` после краха означает «суперблок не двигался», а не «лог
    /// пуст». Записи с `seq < next_seq` подтверждены более ранним суперблоком,
    /// уже зачекпойнчены и **пропускаются**, а не реплеятся: в завернувшемся
    /// кольце устаревшие записи после `head` несут старые номера, поэтому старые
    /// образы никогда не лягут поверх новых. Реплеится только цепочка
    /// `seq == next_seq` и дальше.
    pub fn recover(&mut self) -> Result<u32, FsError> {
        // JNL_MAGIC уже проверил open().
        let mut pos = self.tail;
        let mut replayed: u32 = 0;
        let mut expected = self.next_seq;
        let mut last_seq: Option<u64> = None;
        // Осматриваем не больше одного круга кольца, чтобы порча не зациклила
        // скан.
        let mut budget = self.log_blocks;

        while budget > 0 {
            let desc_buf = self.read_log(pos)?;
            let desc = load_descriptor(&desc_buf);
            if desc.magic != JDES_MAGIC {
                break; // здесь ничего валидного
            }
            let count = desc.count as u64;
            if count == 0 || count as usize > JDESC_MAX_TARGETS || count + 2 > self.log_blocks {
                break; // структурно невозможный дескриптор
            }

            if last_seq.is_none() && desc.seq < expected {
                // Подтверждена более ранним суперблоком: уже зачекпойнчена.
                // Переступаем: именно реплей наложил бы старый образ поверх
                // новых данных.
                let step = count + 2;
                pos = (pos + step) % self.log_blocks;
                budget = budget.saturating_sub(step);
                continue;
            }
            if desc.seq != expected {
                break; // разрыв / дубль / порядок нарушен: остальное не трогаем
            }

            // Перечитываем data-блоки и проверяем commit-запись.
            let mut data_blocks: Vec<Vec<u8>> = Vec::with_capacity(count as usize);
            for i in 0..count {
                data_blocks.push(self.read_log(pos + 1 + i)?);
            }
            let commit_pos = pos + 1 + count;
            let cmt_buf = self.read_log(commit_pos)?;
            let cmt = load_commit(&cmt_buf);

            let data_refs: Vec<&[u8]> = data_blocks.iter().map(|b| b.as_slice()).collect();
            let checksum = structs::crc32_slices(&data_refs);

            let committed =
                cmt.magic == JCMT_MAGIC && cmt.seq == desc.seq && cmt.data_checksum == checksum;
            if !committed {
                break; // стоп на первой неполной/испорченной txn; остальное в мусор
            }

            // Повторно применяем (идемпотентно) на финальные места.
            for i in 0..count as usize {
                let target = desc.targets[i];
                if target >= self.fs_blocks {
                    return Err(FsError::Corrupt);
                }
                write_block(&*self.dev, target, &data_blocks[i])?;
            }

            replayed += 1;
            last_seq = Some(desc.seq);
            expected = desc.seq + 1;
            pos = (commit_pos + 1) % self.log_blocks;
            budget = budget.saturating_sub(count + 2);
        }

        // После реплея всё живое зачекпойнчено; опустошаем лог. Реплеинутые
        // образы блоков обязаны дойти до стабильного хранилища раньше, чем
        // персистится опустошённый лог, иначе второй крах потерял бы
        // транзакцию, которую этот recover только что объявил реплеинутой.
        self.tail = pos;
        self.head = pos;
        if let Some(seq) = last_seq {
            self.next_seq = core::cmp::max(self.next_seq, seq + 1);
            self.dev.flush().map_err(|_| FsError::IoError)?;
        }
        self.persist_super()?;
        Ok(replayed)
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }
}

// ─── (де)сериализация структур журнала ──────────────────────────────────────

fn store_journal_super(buf: &mut [u8], js: &JournalSuper) {
    // Сериализация по раскладке repr(C), затем проставляем хвостовой CRC32 по
    // всем предшествующим байтам структуры.
    unsafe { structs::write_struct(buf, js) };
    let size = core::mem::size_of::<JournalSuper>();
    let csum_off = size - 4;
    let checksum = structs::crc32(&buf[..csum_off]);
    structs::write_u32(buf, csum_off, checksum);
}

/// Читает суперблок журнала и проверяет его CRC32 (хвостовое поле `checksum`
/// покрывает все предшествующие байты структуры).
pub(crate) fn load_journal_super(buf: &[u8]) -> Result<JournalSuper, FsError> {
    let js: JournalSuper = unsafe { structs::read_struct(buf) };
    let size = core::mem::size_of::<JournalSuper>();
    let stored = structs::read_u32(buf, size - 4);
    if structs::crc32(&buf[..size - 4]) != stored {
        return Err(FsError::BadJournal);
    }
    Ok(js)
}

fn store_descriptor(buf: &mut [u8], desc: &JournalDescriptor) {
    unsafe { structs::write_struct(buf, desc) };
}

fn load_descriptor(buf: &[u8]) -> JournalDescriptor {
    unsafe { structs::read_struct(buf) }
}

fn store_commit(buf: &mut [u8], cmt: &JournalCommit) {
    unsafe { structs::write_struct(buf, cmt) };
}

fn load_commit(buf: &[u8]) -> JournalCommit {
    unsafe { structs::read_struct(buf) }
}
