//! USB storage: потоки подключения/отключения MSC-устройств.
//!
//! `xhci::poll()` только перечисляет устройство и складывает его slot id в
//! очередь; BOT-команды (probe, разделы) выполняются здесь, потому что `poll`
//! держит `CONTROLLER`, а спинлок нереентерабелен. Этот поток — единственный
//! читатель очередей [`xhci::take_msc_pending`]/[`xhci::take_msc_removed`].
//!
//! Имена `usbN` выдаются монотонным счётчиком и не переиспользуются: старые
//! `usb0p1` не должны алиаситься на новое устройство, пока открытые fd или
//! mount-точки ещё ссылаются на прежнее. Идентичность тома между reboot — это
//! `uuid`/label в uevent (S1c), а не `usbN`.
//!
//! Auto-mount здесь отсутствует намеренно: ядро только перечисляет и публикует
//! событие; `mount(2)` вызывает root-композитор (S1d).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::drivers::partition::{self, PartitionContent};
use crate::drivers::usb_msc::{self, MscDevice};
use crate::drivers::usb_scsi;
use crate::drivers::{self, xhci};

/// Следующий `usbN`. Монотонный и не переиспользуемый в пределах загрузки.
static NEXT_USB_INDEX: AtomicU32 = AtomicU32::new(0);

/// Снимок одного перечисленного тома для uevent и диагностики. Поля те же,
/// что в 144-байтовой записи v2; держим их рядом, чтобы `ADD`/`CHANGE` не
/// собирались из разных источников.
#[derive(Clone)]
pub struct DeviceRecord {
    pub dev: String,
    pub mount: String,
    pub sectors: u64,
    pub sector_size: u32,
    pub vendor: [u8; 16],
    pub product: [u8; 32],
    pub label: [u8; 16],
    pub uuid: [u8; 16],
    pub fs_class: u8,
    pub removable: bool,
    pub read_only: bool,
    pub mounted: bool,
    pub in_use: bool,
    pub safe_to_remove: bool,
    pub surprise: bool,
    pub unclean: bool,
    pub detached: bool,
    /// `usbN` устройства (не имя раздела) — нужен, чтобы снять все имена.
    pub usb_index: u32,
    /// Прямая ссылка на устройство: после unregister `get_block` его уже не
    /// найдёт, а `umount2` обязан видеть dead-флаг.
    pub block: Arc<dyn drivers::BlockDevice>,
}

/// Все известные тома в порядке подключения. Читается snapshot'ом uevent.
static DEVICES: crate::sync::spinlock::Spinlock<alloc::vec::Vec<DeviceRecord>> =
    crate::sync::spinlock::Spinlock::new(alloc::vec::Vec::new());

/// Выдать следующий `usbN`; `None` после исчерпания разумного диапазона.
fn next_index() -> Option<u32> {
    let idx = NEXT_USB_INDEX.fetch_add(1, Ordering::SeqCst);
    if idx > 999_999 {
        crate::warn!("usb-storage: usb index space exhausted; device not registered");
        None
    } else {
        Some(idx)
    }
}

/// Прочитать устройство и зарегистрировать его блочные имена.
fn bring_up(slot_id: u8) -> bool {
    let info = match usb_msc::probe(slot_id) {
        Ok(info) => info,
        Err(e) => {
            crate::warn!("usb-msc: slot {}: probe failed: {:?}", slot_id, e);
            return false;
        }
    };
    let Some(index) = next_index() else {
        return false;
    };
    let name = format!("usb{}", index);
    let vendor = sanitize(&info.inquiry.vendor);
    let product = sanitize(&info.inquiry.product);
    crate::info!(
        "usb-msc: inquiry vendor=\"{}\" product=\"{}\" rev=\"{}\" pdt={} removable={}",
        vendor,
        product,
        sanitize(&info.inquiry.revision),
        info.inquiry.pdt,
        info.inquiry.removable as u8
    );
    if info.inquiry.pdt != 0 {
        // PDT != direct access — CD-ROM/tape/vendor. Честный отказ: Phase 1
        // умеет только диски.
        crate::warn!(
            "usb-msc: {}: peripheral device type {:#x} is not direct-access; not a disk",
            name,
            info.inquiry.pdt
        );
        return false;
    }
    match info.problem {
        Some(usb_scsi::CapacityProblem::UnsupportedBlock(block)) => {
            crate::warn!(
                "usb-msc: {}: unsupported block size {} (only 512); device not registered",
                name,
                block
            );
            return false;
        }
        Some(usb_scsi::CapacityProblem::Overflow) => {
            crate::warn!(
                "usb-msc: {}: capacity overflow; READ CAPACITY(16) required; device not registered",
                name
            );
            return false;
        }
        None => {}
    }
    let Some(dead) = xhci::msc_dead_flag(slot_id) else {
        crate::warn!("usb-msc: {}: slot {} vanished during probe", name, slot_id);
        return false;
    };
    let dev = Arc::new(MscDevice::new(name.clone(), slot_id, info, dead));
    crate::info!(
        "usb-msc: dev={} last_lba={} block={} sectors={}",
        name,
        info.capacity.last_lba,
        info.capacity.block_length,
        info.capacity.last_lba as u64 + 1
    );
    drivers::register_block(dev.clone() as Arc<dyn drivers::BlockDevice>);
    xhci::msc_set_usb_index(slot_id, index);
    let record = DeviceRecord {
        dev: name.clone(),
        mount: String::new(),
        sectors: info.capacity.last_lba as u64 + 1,
        sector_size: 512,
        vendor: copy_field(&info.inquiry.vendor),
        product: copy_field(&info.inquiry.product),
        label: [0u8; 16],
        uuid: [0u8; 16],
        fs_class: 0,
        removable: info.inquiry.removable,
        read_only: false,
        mounted: false,
        in_use: false,
        safe_to_remove: false,
        surprise: false,
        unclean: false,
        detached: false,
        usb_index: index,
        block: dev.clone() as Arc<dyn drivers::BlockDevice>,
    };
    DEVICES.lock().push(record);
    let block: Arc<dyn drivers::BlockDevice> = dev.clone();
    let count = partition::register_partitions(&block);
    if count == 0 {
        // Superfloppy: таблицы нет, ФС лежит с сектора 0. Целое устройство уже
        // зарегистрировано, поэтому его можно классифицировать и (после
        // mount(2) от композитора) смонтировать как есть.
        let content = partition::classify(&*dev);
        crate::info!(
            "usb-msc: {}: no partition table; whole-device fs_class={}",
            name,
            fs_class_name(content)
        );
    }
    true
}

fn fs_class_name(content: PartitionContent) -> &'static str {
    match content {
        PartitionContent::PaghVolume => "pagh_ext2",
        PartitionContent::ForeignExt2 => "foreign_ext2",
        PartitionContent::Blank => "blank",
        PartitionContent::Other => "other_fs",
    }
}

/// Снять с регистрации все имена исчезнувшего устройства.
fn remove(index: u32) {
    let prefix = format!("usb{}", index);
    drivers::unregister_usb(&prefix);
    DEVICES.lock().retain(|record| record.usb_index != index);
    crate::info!("usb-msc: removed dev={} (slot gone)", prefix);
}

/// Копия ASCII-поля в массив фиксированного размера (для uevent-записи).
fn copy_field<const N: usize>(src: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    usb_scsi::ascii_field(src, &mut out);
    out
}

/// Человекочитаемая строка из ASCII-поля (для логов).
fn sanitize(src: &[u8]) -> String {
    let mut buf = [0u8; 32];
    let n = usb_scsi::ascii_field(src, &mut buf);
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// Поток `usb-storage`: probe новых устройств и снятие исчезнувших.
///
/// Просыпается каждый тик; работа ограничена одним устройством за проход, чтобы
/// probe не задерживал обработку REMOVE (и наоборот).
pub fn usb_storage_thread() {
    loop {
        let pending = xhci::take_msc_pending();
        if let Some(slot) = pending.into_iter().next() {
            bring_up(slot);
        }
        let removed = xhci::take_msc_removed();
        for removal in removed {
            if let Some(index) = removal.usb_index {
                remove(index);
            } else {
                crate::debug!(
                    "usb-msc: slot {} disconnected before it was registered",
                    removal.slot_id
                );
            }
        }
        crate::task::scheduler::sleep_ticks(1);
    }
}

/// Снимок устройств для uevent initial snapshot (S1c). Копия, чтобы не держать
/// лок `DEVICES` во время записи в ring.
pub fn snapshot() -> alloc::vec::Vec<DeviceRecord> {
    DEVICES.lock().clone()
}
