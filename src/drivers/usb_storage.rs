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
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::drivers::partition::{self, PartitionContent};
use crate::drivers::usb_msc::{self, MscDevice, ScsiError};
use crate::drivers::usb_scsi;
use crate::drivers::usb_uevent;
use crate::drivers::usb_uevent_abi::{error, fs_class, kind};
use crate::drivers::BlockDevice;
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
    /// Последняя ошибка тома (ABI `error`); `push_state` может передать более
    /// свежую, но снимок берёт это значение.
    pub error: u8,
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
    /// `None` — устройство видно (ошибка ёмкости/блока), но блочного имени у
    /// него нет.
    pub block: Option<Arc<dyn drivers::BlockDevice>>,
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

/// Опубликовать `ERROR`-запись для имени, которое не стало `BlockDevice`
/// (4K-блок, >2 TiB, нет носителя). Устройство остаётся видимым для UI: это
/// `ADD`-состояние с `error != 0`, а не тихое исчезновение.
fn emit_error(name: &str, vendor: [u8; 16], product: [u8; 32], fs: u8, err: u8, removable: bool) {
    let record = DeviceRecord {
        dev: String::from(name),
        mount: String::new(),
        sectors: 0,
        sector_size: 512,
        vendor,
        product,
        label: [0u8; 16],
        uuid: [0u8; 16],
        fs_class: fs,
        error: err,
        removable,
        read_only: false,
        mounted: false,
        in_use: false,
        safe_to_remove: false,
        surprise: false,
        unclean: false,
        detached: false,
        usb_index: 0,
        block: None,
    };
    usb_uevent::push_state(&record, kind::ERROR, err);
}

fn fs_class_of(content: PartitionContent) -> u8 {
    match content {
        PartitionContent::PaghVolume => fs_class::PAGH_EXT2,
        PartitionContent::ForeignExt2 => fs_class::FOREIGN_EXT2,
        PartitionContent::Blank => fs_class::BLANK,
        PartitionContent::Other => fs_class::OTHER_FS,
    }
}

/// `usb0p1` с префиксом `usb0` → индекс записи GPT 0 (0-based).
fn partition_index(name: &str, prefix: &str) -> Option<usize> {
    let rest = name.strip_prefix(prefix)?.strip_prefix('p')?;
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    rest.parse::<usize>().ok().and_then(|n| n.checked_sub(1))
}

fn is_partition_of(name: &str, prefix: &str) -> bool {
    partition_index(name, prefix).is_some()
}

/// Прочитать устройство и зарегистрировать его блочные имена.
fn bring_up(slot_id: u8) -> bool {
    let Some(index) = next_index() else {
        return false;
    };
    let name = format!("usb{}", index);
    let info = match usb_msc::probe(slot_id) {
        Ok(info) => info,
        Err(e) => {
            let (err, fs) = if e == ScsiError::NoMedia {
                (error::NO_MEDIA, fs_class::NO_MEDIA)
            } else {
                (error::IO, fs_class::UNKNOWN)
            };
            crate::warn!("usb-msc: slot {}: probe failed: {:?}", slot_id, e);
            emit_error(&name, [0; 16], [0; 32], fs, err, false);
            return false;
        }
    };
    let vendor = copy_field(&info.inquiry.vendor);
    let product = copy_field(&info.inquiry.product);
    crate::info!(
        "usb-msc: inquiry vendor=\"{}\" product=\"{}\" rev=\"{}\" pdt={} removable={}",
        sanitize(&info.inquiry.vendor),
        sanitize(&info.inquiry.product),
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
        emit_error(
            &name,
            vendor,
            product,
            fs_class::OTHER_FS,
            error::UNSUPPORTED_FS,
            info.inquiry.removable,
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
            emit_error(
                &name,
                vendor,
                product,
                fs_class::UNKNOWN,
                error::UNSUPPORTED_BLOCK,
                info.inquiry.removable,
            );
            return false;
        }
        Some(usb_scsi::CapacityProblem::Overflow) => {
            crate::warn!(
                "usb-msc: {}: capacity overflow; READ CAPACITY(16) required; device not registered",
                name
            );
            emit_error(
                &name,
                vendor,
                product,
                fs_class::UNKNOWN,
                error::CAPACITY_OVERFLOW,
                info.inquiry.removable,
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
    let block: Arc<dyn drivers::BlockDevice> = dev.clone();
    let count = partition::register_partitions(&block);

    // Имена, которые видны пользователю: разделы, а если таблицы нет —
    // superfloppy (целое устройство). Целое устройство с таблицей не является
    // томом, поэтому в uevent/снимке его нет.
    let mut names: Vec<String> = Vec::new();
    if count == 0 {
        let content = partition::classify(&*dev);
        crate::info!(
            "usb-msc: {}: no partition table; whole-device fs_class={}",
            name,
            fs_class_name(content)
        );
        names.push(name.clone());
    } else {
        for (candidate, _sectors) in drivers::list_blocks() {
            if is_partition_of(&candidate, &name) {
                names.push(candidate);
            }
        }
    }

    for node_name in names {
        let Some(node) = drivers::get_block(&node_name) else {
            continue;
        };
        let content = partition::classify(&*node);
        let fs = fs_class_of(content);
        let mut uuid = [0u8; 16];
        let mut label = [0u8; 16];
        if matches!(
            content,
            PartitionContent::PaghVolume | PartitionContent::ForeignExt2
        ) {
            if let Some((u, l)) = crate::fs::ext2::Ext2Fs::volume_identity(&*node) {
                uuid = u;
                label = l;
            }
        }
        // pagh `format` пишет нулевой s_uuid; в этом случае идентичность GPT
        // раздела — следующий по надёжности ключ (MBR/unknown — нули честно).
        if uuid == [0u8; 16] && count > 0 {
            if let Some(part) = partition_index(&node_name, &name) {
                if let Some(guid) = partition::gpt_unique_guid(&*block, part) {
                    uuid = guid;
                }
            }
        }
        let record = DeviceRecord {
            dev: node_name,
            mount: String::new(),
            sectors: node.sector_count(),
            sector_size: 512,
            vendor,
            product,
            label,
            uuid,
            fs_class: fs,
            error: error::NONE,
            removable: info.inquiry.removable,
            read_only: false,
            mounted: false,
            in_use: false,
            safe_to_remove: false,
            surprise: false,
            unclean: false,
            detached: false,
            usb_index: index,
            block: Some(node),
        };
        DEVICES.lock().push(record.clone());
        usb_uevent::push_state(&record, kind::ADD, error::NONE);
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
    // События — до удаления из таблицы: снимок, пришедший между шагами, должен
    // видеть прежнее состояние с `surprise`, а не «устройство исчезло молча».
    let records: Vec<DeviceRecord> = DEVICES
        .lock()
        .iter()
        .filter(|record| record.usb_index == index)
        .cloned()
        .collect();
    let mut keep: Vec<DeviceRecord> = Vec::new();
    for mut record in records.into_iter() {
        let was_mounted = record.mounted && !record.detached;
        if was_mounted {
            // Mount остаётся «stale» до umount2: I/O уже даёт EIO, а том
            // нельзя объявлять clean. `unclean` — это «WAL не проверен».
            record.surprise = true;
            record.unclean = true;
            record.safe_to_remove = false;
            record.detached = true;
            usb_uevent::push_state(&record, kind::ERROR, error::IO);
        }
        record.mounted = false;
        record.mount = String::new();
        record.safe_to_remove = false;
        usb_uevent::push_state(&record, kind::REMOVE, error::NONE);
        if was_mounted {
            // Оставить запись, пока `umount2` не снимет stale-mount и не
            // отправит UNMOUNTED: иначе у umount нет состояния для события.
            keep.push(record);
        }
    }
    {
        let mut devices = DEVICES.lock();
        devices
            .retain(|record| record.usb_index != index || keep.iter().any(|k| k.dev == record.dev));
        for updated in keep {
            if let Some(slot) = devices.iter_mut().find(|r| r.dev == updated.dev) {
                *slot = updated;
            }
        }
    }
    drivers::unregister_usb(&prefix);
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

// ─── mount(2)/umount2(2): только pagh-ext2 USB-тома ─────────────────────────

/// Активное монтирование, созданное `mount(2)`.
///
/// `fs` держит журнал/суперблок живыми до `umount2`; `root` — корневой узел,
/// который уже вставлен в VFS. `dev` — тот же `Arc`, что у `BlockDevice`, чтобы
/// `umount2` видел dead-флаг после исчезновения устройства.
struct ActiveMount {
    source: String,
    target: String,
    #[allow(dead_code)] // kept alive: journal/superblock for open descriptors
    fs: Arc<crate::fs::ext2::Ext2Fs>,
    #[allow(dead_code)] // kept alive through unmount
    root: Arc<dyn crate::vfs::VfsNode>,
    dev: Arc<dyn drivers::BlockDevice>,
}

static ACTIVE_MOUNTS: crate::sync::spinlock::Spinlock<Vec<ActiveMount>> =
    crate::sync::spinlock::Spinlock::new(Vec::new());

/// Почему `mount(2)`/`umount2(2)` отказал.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountError {
    /// Блочного устройства с таким именем нет.
    NoDevice,
    /// Том структурно ext2, но не pagh (mount переписал бы чужие счётчики).
    NotPagh,
    /// Уже смонтировано (или точка занята).
    Busy,
    /// I/O, replay или запись в VFS.
    Io,
    /// Аргументы/флаги не по контракту.
    Invalid,
}

/// Итог успешного `umount2`: clean → 0, dirty (dead без MNT_FORCE) → EIO.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UmountOutcome {
    Clean,
    DirtyDetach,
    Forced,
}

fn mutate_record(dev: &str, change: impl FnOnce(&mut DeviceRecord)) -> Option<DeviceRecord> {
    let mut devices = DEVICES.lock();
    let record = devices.iter_mut().find(|record| record.dev == dev)?;
    change(record);
    Some(record.clone())
}

/// `mount(2)` для USB-тома: source — `usbN`/`usbNpM`, target — `"/" + source`
/// (проверку формы делает syscall-слой). Монтируется только pagh-том:
/// `Ext2Fs::mount_fs` пишет group descriptors и резервирует WAL, поэтому чужой
/// ext2 здесь — отказ, а не «тихо поправим счётчики».
pub fn mount(source: &str, target: &str) -> Result<(), MountError> {
    if !source.starts_with("usb") {
        return Err(MountError::NoDevice);
    }
    {
        let mounts = ACTIVE_MOUNTS.lock();
        if mounts
            .iter()
            .any(|m| m.source == source || m.target == target)
        {
            return Err(MountError::Busy);
        }
    }
    if crate::vfs::mount_snapshot()
        .iter()
        .any(|entry| entry.point == target)
    {
        return Err(MountError::Busy);
    }
    let dev = drivers::get_block(source).ok_or(MountError::NoDevice)?;
    if !crate::fs::ext2::Ext2Fs::is_pagh_volume(&*dev) {
        crate::warn!(
            "usb-storage: refusing to mount {}: not a pagh ext2 volume (mount would rewrite foreign group descriptors)",
            source
        );
        if let Some(record) = mutate_record(source, |record| {
            record.fs_class = fs_class::FOREIGN_EXT2;
            record.error = error::UNSUPPORTED_FS;
        }) {
            usb_uevent::push_state(&record, kind::ERROR, error::UNSUPPORTED_FS);
        }
        return Err(MountError::NotPagh);
    }
    let fs = match crate::fs::ext2::Ext2Fs::mount_fs(dev.clone()) {
        Ok(fs) => fs,
        Err(e) => {
            // `mount_fs` делает Journal::recover до публикации; отказ здесь —
            // это fail-closed replay (или I/O), и том нельзя объявлять живым.
            crate::warn!(
                "usb-storage: mounting {} failed ({:?}); replay failed closed",
                source,
                e
            );
            if let Some(record) = mutate_record(source, |record| {
                record.error = error::REPLAY_FAILED;
                record.unclean = true;
                record.safe_to_remove = false;
            }) {
                usb_uevent::push_state(&record, kind::ERROR, error::REPLAY_FAILED);
            }
            return Err(MountError::Io);
        }
    };
    let root = fs.root_node();
    if let Err(e) = crate::vfs::mount_at_named(target, root.clone(), "ext2") {
        crate::warn!("usb-storage: mount_at {} failed: {:?}", target, e);
        return Err(MountError::Io);
    }
    ACTIVE_MOUNTS.lock().push(ActiveMount {
        source: String::from(source),
        target: String::from(target),
        fs,
        root,
        dev,
    });
    if let Some(record) = mutate_record(source, |record| {
        record.mount = String::from(target);
        record.mounted = true;
        record.detached = false;
        record.safe_to_remove = false;
        record.error = error::NONE;
        // Успешный replay снимает `unclean`: журнал проверен.
        record.unclean = false;
    }) {
        usb_uevent::push_state(&record, kind::MOUNTED, error::NONE);
    }
    crate::info!("usb-storage: mounted {} at {}", source, target);
    Ok(())
}

/// `umount2(2)`. `force` — `MNT_FORCE`, допустим только для dead-устройства.
///
/// Живой том: busy → `EBUSY`; flush → `SYNCHRONIZE CACHE`; ошибка flush
/// оставляет том смонтированным (UI не должен показывать «можно отключать»).
/// Мёртвый том: detach разрешён, но без `MNT_FORCE` это `EIO` + `unclean`,
/// потому что durable-гарантии нет.
pub fn umount(target: &str, force: bool) -> Result<UmountOutcome, MountError> {
    let (source, dev) = {
        let mounts = ACTIVE_MOUNTS.lock();
        let Some(m) = mounts.iter().find(|m| m.target == target) else {
            return Err(MountError::Invalid);
        };
        (m.source.clone(), Arc::clone(&m.dev))
    };
    let dead = dev.is_dead();
    if force && !dead {
        // Принудительный отрыв живой ФС с открытыми писателями запрещён.
        return Err(MountError::Invalid);
    }
    let busy = crate::task::compat::fds_open_under(target);
    if !dead {
        if busy {
            return Err(MountError::Busy);
        }
        if let Err(()) = dev.flush() {
            crate::warn!(
                "usb-storage: flush of {} failed; mount stays (not safe to remove)",
                source
            );
            if let Some(record) = mutate_record(&source, |record| {
                record.error = error::FLUSH_FAILED;
                record.mounted = true;
                record.safe_to_remove = false;
            }) {
                usb_uevent::push_state(&record, kind::ERROR, error::FLUSH_FAILED);
            }
            return Err(MountError::Io);
        }
    }
    if let Err(e) = crate::vfs::unmount_at(target) {
        crate::warn!("usb-storage: unmount_at {} failed: {:?}", target, e);
        return Err(MountError::Io);
    }
    ACTIVE_MOUNTS.lock().retain(|m| m.target != target);
    let outcome = if dead {
        if force {
            UmountOutcome::Forced
        } else {
            UmountOutcome::DirtyDetach
        }
    } else {
        UmountOutcome::Clean
    };
    if let Some(record) = mutate_record(&source, |record| {
        record.mount = String::new();
        record.mounted = false;
        record.detached = true;
        match outcome {
            UmountOutcome::Clean => {
                record.unclean = false;
                record.safe_to_remove = true;
            }
            UmountOutcome::DirtyDetach => {
                record.unclean = true;
                record.safe_to_remove = false;
            }
            UmountOutcome::Forced => {
                record.unclean = true;
                record.safe_to_remove = true;
            }
        }
        record.error = match outcome {
            UmountOutcome::Clean | UmountOutcome::Forced => error::NONE,
            UmountOutcome::DirtyDetach => error::IO,
        };
    }) {
        let err = record.error;
        usb_uevent::push_state(&record, kind::UNMOUNTED, err);
    }
    if dev.is_dead() {
        // Том исчез: запись больше не описывает присутствующее устройство.
        DEVICES.lock().retain(|record| record.dev != source);
    }
    crate::info!("usb-storage: unmounted {} ({:?})", target, outcome);
    Ok(outcome)
}

/// Смонтирован ли `source` (для `mount(2)`/диагностики).
pub fn is_mounted(source: &str) -> bool {
    ACTIVE_MOUNTS.lock().iter().any(|m| m.source == source)
}
