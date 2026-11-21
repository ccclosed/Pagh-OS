//! USB mass storage: SCSI-слой над BOT и `BlockDevice` `usbN`.
//!
//! Три обязанности, намеренно разделённые:
//!   * [`probe`] — один раз при подключении: TUR, INQUIRY, READ CAPACITY(10),
//!     проверка размера блока и ёмкости;
//!   * [`scsi_read`] / [`scsi_write`] — одна команда с обработкой
//!     `CHECK CONDITION`: REQUEST SENSE, UNIT ATTENTION/TUR, один повтор для
//!     временных ошибок;
//!   * [`MscDevice`] — `BlockDevice` поверх READ(10)/WRITE(10)/
//!     SYNCHRONIZE CACHE(10), режущий запрос по data-буферу канала (64 KiB или
//!     честный 4 KiB fallback).
//!
//! Почему не «просто читать блоки»: BOT синхронный и выполняется на потоке
//! вызывающего (см. комментарий в `xhci::msc_bot_in`), поэтому любая ошибка
//! здесь должна либо вылечиться повтором/recovery, либо вернуть `Err(())` —
//! молча вернуть нули значило бы отдать ext2 мусор вместо данных.
//!
//! Границы Phase 1 названы явно: только LUN 0, только 512-байтовый логический
//! блок, только LBA ≤ 32 бит. Всё остальное — отказ с причиной в логе, без
//! усечения.

use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::drivers::usb_scsi::{self, CapacityProblem, SenseAction};
use crate::drivers::{xhci, BlockDevice};

/// Результат probe: геометрия и то, что устройство о себе рассказало.
#[derive(Clone, Copy)]
pub struct MscInfo {
    pub inquiry: usb_scsi::Inquiry,
    pub capacity: usb_scsi::Capacity,
    /// `Some` — ёмкость не принимается Phase 1 (4K-блок или >2 TiB).
    pub problem: Option<CapacityProblem>,
}

/// Ошибка SCSI-команды после обработки sense.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScsiError {
    /// Транспорт BOT отказал (timeout/stall/CSW/dead).
    Bot(xhci::MscBotError),
    /// Sense 02/3A/00: носителя нет.
    NoMedia,
    /// Sense 07/27/00: запись защищена.
    ReadOnly,
    /// Sense 05/20/00: команда не поддерживается.
    Unsupported,
    /// Прочие ошибки после исчерпания повторов.
    Failed,
}

/// Данные команды в sense-пути: как и в BOT, IN и OUT различаются только
/// направлением, поэтому повтор не дублируется.
enum ScsiData<'a> {
    In(&'a mut [u8]),
    Out(&'a [u8]),
}

impl ScsiData<'_> {
    fn is_empty(&self) -> bool {
        match self {
            ScsiData::In(buf) => buf.is_empty(),
            ScsiData::Out(buf) => buf.is_empty(),
        }
    }
}

/// Разобрать sense, если устройство ответило на команду `CHECK CONDITION`.
fn read_sense(slot_id: u8) -> Result<usb_scsi::Sense, ScsiError> {
    let mut buf = [0u8; 18];
    xhci::msc_bot_in(slot_id, &usb_scsi::request_sense(), &mut buf).map_err(ScsiError::Bot)?;
    usb_scsi::parse_sense(&buf).ok_or(ScsiError::Failed)
}

/// TEST UNIT READY и ожидание готовности носителя.
///
/// Ожидание идёт под `CONTROLLER`, а спинлок выключает прерывания, поэтому
/// «поспать» нельзя: 100 мс × 10 — верхняя граница, после которой устройство
/// объявляется неготовым, а не опрашивается бесконечно.
fn wait_ready(slot_id: u8) -> Result<(), ScsiError> {
    for attempt in 0..10 {
        match xhci::msc_bot_in(slot_id, &usb_scsi::test_unit_ready(), &mut []) {
            Ok(_) => return Ok(()),
            Err(xhci::MscBotError::Failed) => match read_sense(slot_id) {
                Ok(sense) => match usb_scsi::sense_action(sense) {
                    SenseAction::RetryTur => {
                        if attempt < 9 {
                            xhci::msc_settle_us(100_000);
                            continue;
                        }
                        return Err(ScsiError::Failed);
                    }
                    SenseAction::NoMedia => return Err(ScsiError::NoMedia),
                    SenseAction::ReadOnly => return Err(ScsiError::ReadOnly),
                    SenseAction::Unsupported => return Err(ScsiError::Unsupported),
                    _ => return Err(ScsiError::Failed),
                },
                Err(e) => return Err(e),
            },
            Err(xhci::MscBotError::Dead) | Err(xhci::MscBotError::NoDevice) => {
                return Err(ScsiError::Failed)
            }
            Err(e) => return Err(ScsiError::Bot(e)),
        }
    }
    Err(ScsiError::Failed)
}

fn bot_for(slot_id: u8, cdb: &[u8], data: &mut ScsiData<'_>) -> Result<usize, xhci::MscBotError> {
    match data {
        ScsiData::In(buf) => xhci::msc_bot_in(slot_id, cdb, buf),
        ScsiData::Out(buf) => xhci::msc_bot_out(slot_id, cdb, buf),
    }
}

/// Выполнить SCSI-команду, разобрав `CHECK CONDITION`.
///
/// Повторы ограничены: UNIT ATTENTION и «becoming ready» лечатся TUR + один
/// повтор, временная ошибка чтения/записи — один повтор, aborted — один повтор
/// после Reset Recovery (его уже сделал транспорт). Неизвестная комбинация
/// sense — отказ, а не бесконечный цикл.
fn scsi_common(slot_id: u8, cdb: &[u8], data: &mut ScsiData<'_>) -> Result<usize, ScsiError> {
    let mut attempt = 0u8;
    loop {
        match bot_for(slot_id, cdb, data) {
            Ok(n) => return Ok(n),
            Err(xhci::MscBotError::Failed) => {
                let sense = read_sense(slot_id)?;
                let action = usb_scsi::sense_action(sense);
                crate::warn!(
                    "usb-msc: CHECK CONDITION key={:#x} asc={:#x} ascq={:#x} -> {:?}",
                    sense.key,
                    sense.asc,
                    sense.ascq,
                    action
                );
                match action {
                    SenseAction::Ok => {
                        // Команда сообщила Failed, но sense говорит NO SENSE: для
                        // команды без данных это успех, для READ/WRITE — нет
                        // (данные не пришли, и выдавать буфер за прочитанный нельзя).
                        if data.is_empty() {
                            return Ok(0);
                        }
                        return Err(ScsiError::Failed);
                    }
                    SenseAction::RetryTur => {
                        if attempt >= 1 {
                            return Err(ScsiError::Failed);
                        }
                        attempt += 1;
                        wait_ready(slot_id)?;
                        continue;
                    }
                    SenseAction::RetryOnce | SenseAction::Aborted => {
                        if attempt >= 1 {
                            return Err(ScsiError::Failed);
                        }
                        attempt += 1;
                        continue;
                    }
                    SenseAction::NoMedia => return Err(ScsiError::NoMedia),
                    SenseAction::ReadOnly => return Err(ScsiError::ReadOnly),
                    SenseAction::Unsupported => return Err(ScsiError::Unsupported),
                    SenseAction::Fatal => return Err(ScsiError::Failed),
                }
            }
            Err(e) => return Err(ScsiError::Bot(e)),
        }
    }
}

/// Одна SCSI-команда с IN data-стадией.
pub fn scsi_read(slot_id: u8, cdb: &[u8], data: &mut [u8]) -> Result<usize, ScsiError> {
    scsi_common(slot_id, cdb, &mut ScsiData::In(data))
}

/// Одна SCSI-команда с OUT data-стадией (WRITE(10)).
pub fn scsi_write(slot_id: u8, cdb: &[u8], data: &[u8]) -> Result<usize, ScsiError> {
    scsi_common(slot_id, cdb, &mut ScsiData::Out(data))
}

/// Прочитать INQUIRY + READ CAPACITY(10) и проверить, что Phase 1 это умеет.
///
/// TUR здесь не формальность: флешка после вставки может ответить UNIT
/// ATTENTION на первую команду, и `wait_ready` съедает его до INQUIRY.
pub fn probe(slot_id: u8) -> Result<MscInfo, ScsiError> {
    wait_ready(slot_id)?;
    let mut inq = [0u8; 36];
    let got = scsi_read(slot_id, &usb_scsi::inquiry(), &mut inq)?;
    if got < 36 {
        return Err(ScsiError::Failed);
    }
    let inquiry = usb_scsi::parse_inquiry(&inq).ok_or(ScsiError::Failed)?;
    let mut cap = [0u8; 8];
    let got = scsi_read(slot_id, &usb_scsi::read_capacity10(), &mut cap)?;
    if got < 8 {
        return Err(ScsiError::Failed);
    }
    let capacity = usb_scsi::parse_capacity10(&cap).ok_or(ScsiError::Failed)?;
    let problem = usb_scsi::capacity_problem(capacity.last_lba, capacity.block_length);
    Ok(MscInfo {
        inquiry,
        capacity,
        problem,
    })
}

/// Phase 1 `BlockDevice` поверх MSC LUN 0.
pub struct MscDevice {
    name: String,
    slot_id: u8,
    sectors: u64,
    vendor: [u8; 8],
    product: [u8; 16],
    revision: [u8; 4],
    removable: bool,
    read_only: AtomicBool,
    dead: Arc<AtomicBool>,
    /// Одна строка на устройство, а не на каждый flush: дешёвые мосты не
    /// поддерживают SYNCHRONIZE CACHE, и WAL обязан узнать об этом один раз.
    sync_unsupported_logged: AtomicBool,
}

impl MscDevice {
    pub fn new(name: String, slot_id: u8, info: MscInfo, dead: Arc<AtomicBool>) -> Self {
        Self {
            name,
            slot_id,
            sectors: info.capacity.last_lba as u64 + 1,
            vendor: info.inquiry.vendor,
            product: info.inquiry.product,
            revision: info.inquiry.revision,
            removable: info.inquiry.removable,
            read_only: AtomicBool::new(false),
            dead,
            sync_unsupported_logged: AtomicBool::new(false),
        }
    }

    pub fn slot_id(&self) -> u8 {
        self.slot_id
    }

    pub fn vendor(&self) -> [u8; 8] {
        self.vendor
    }

    pub fn product(&self) -> [u8; 16] {
        self.product
    }

    pub fn revision(&self) -> [u8; 4] {
        self.revision
    }

    pub fn removable(&self) -> bool {
        self.removable
    }

    pub fn read_only(&self) -> bool {
        self.read_only.load(Ordering::SeqCst)
    }

    pub fn dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    /// Сколько секторов за одну BOT-команду позволяет data-буфер канала.
    fn sectors_per_command(&self) -> Option<u16> {
        let max = xhci::msc_max_transfer(self.slot_id);
        let sectors = (max / 512).min(u16::MAX as usize);
        if sectors == 0 {
            None
        } else {
            Some(sectors as u16)
        }
    }

    fn chunk_read(&self, block: u64, buf: &mut [u8]) -> Result<usize, ()> {
        let max_sectors = self.sectors_per_command().ok_or(())?;
        let mut done = 0usize;
        while done < buf.len() {
            let remaining = buf.len() - done;
            let chunk = remaining.min(max_sectors as usize * 512);
            let lba = block + (done / 512) as u64;
            if lba > u32::MAX as u64 {
                return Err(());
            }
            let cdb = usb_scsi::read10(lba as u32, (chunk / 512) as u16);
            let got = scsi_read(self.slot_id, &cdb, &mut buf[done..done + chunk]).map_err(|e| {
                crate::warn!(
                    "usb-msc: {} READ(10) lba={} failed: {:?}",
                    self.name,
                    lba,
                    e
                );
            })?;
            if got != chunk {
                crate::warn!(
                    "usb-msc: {} READ(10) lba={} short: {}/{} bytes",
                    self.name,
                    lba,
                    got,
                    chunk
                );
                return Err(());
            }
            done += chunk;
        }
        Ok(done)
    }

    fn chunk_write(&self, block: u64, buf: &[u8]) -> Result<usize, ()> {
        if self.read_only.load(Ordering::SeqCst) {
            return Err(());
        }
        let max_sectors = self.sectors_per_command().ok_or(())?;
        let mut done = 0usize;
        while done < buf.len() {
            let remaining = buf.len() - done;
            let chunk = remaining.min(max_sectors as usize * 512);
            let lba = block + (done / 512) as u64;
            if lba > u32::MAX as u64 {
                return Err(());
            }
            let cdb = usb_scsi::write10(lba as u32, (chunk / 512) as u16);
            let got = scsi_write(self.slot_id, &cdb, &buf[done..done + chunk]).map_err(|e| {
                if e == ScsiError::ReadOnly {
                    self.note_write_protected();
                }
                crate::warn!(
                    "usb-msc: {} WRITE(10) lba={} failed: {:?}",
                    self.name,
                    lba,
                    e
                );
            })?;
            if got != chunk {
                return Err(());
            }
            done += chunk;
        }
        Ok(done)
    }

    /// Отметить, что устройство ответило WRITE PROTECTED; используется и
    /// sense-путём, и будущим uevent `read_only`.
    pub fn note_write_protected(&self) {
        self.read_only.store(true, Ordering::SeqCst);
    }
}

impl BlockDevice for MscDevice {
    fn name(&self) -> &str {
        &self.name
    }

    fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<usize, ()> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(());
        }
        if buf.is_empty() {
            return Ok(0);
        }
        if buf.len() % 512 != 0 {
            // Усечение до сектора молча вернуло бы меньше байт, чем ext2 просил,
            // и следующий слой интерпретировал бы это как конец файла.
            return Err(());
        }
        let end = block
            .checked_add((buf.len() as u64).div_ceil(512))
            .ok_or(())?;
        if end > self.sectors {
            return Err(());
        }
        self.chunk_read(block, buf)
    }

    fn write_block(&self, block: u64, buf: &[u8]) -> Result<usize, ()> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(());
        }
        if buf.is_empty() {
            return Ok(0);
        }
        if buf.len() % 512 != 0 {
            return Err(());
        }
        let end = block
            .checked_add((buf.len() as u64).div_ceil(512))
            .ok_or(())?;
        if end > self.sectors {
            return Err(());
        }
        self.chunk_write(block, buf)
    }

    fn sector_count(&self) -> u64 {
        self.sectors
    }

    fn flush(&self) -> Result<(), ()> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(());
        }
        match scsi_write(self.slot_id, &usb_scsi::synchronize_cache10(), &[]) {
            Ok(_) => Ok(()),
            Err(ScsiError::Unsupported) | Err(ScsiError::Failed) => {
                if !self.sync_unsupported_logged.swap(true, Ordering::SeqCst) {
                    crate::warn!(
                        "usb-msc: {} SYNCHRONIZE CACHE unsupported; write-through durability is not guaranteed",
                        self.name
                    );
                }
                Err(())
            }
            Err(e) => {
                crate::warn!("usb-msc: {} SYNCHRONIZE CACHE failed: {:?}", self.name, e);
                Err(())
            }
        }
    }
}

impl core::fmt::Debug for MscDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MscDevice")
            .field("name", &self.name)
            .field("slot", &self.slot_id)
            .field("sectors", &self.sectors)
            .field("dead", &self.dead.load(Ordering::SeqCst))
            .finish()
    }
}
