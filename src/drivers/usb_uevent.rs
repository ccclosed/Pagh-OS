//! `/dev/uevent`: push-события о блочных устройствах для root-композитора.
//!
//! Модель: один читатель (композитор). `open` эксклюзивен — второй получает
//! `EBUSY`; первый `open` кладёт initial snapshot (`seq = 0`) для каждого уже
//! перечисленного тома, иначе флешка, вставленная до старта композитора, была бы
//! невидима. Live-события продолжают монотонный `seq`; разрыв `seq` (или
//! `PUEV_IOC_SNAPSHOT`) означает «перечитай снимок и перестрой таблицу».
//!
//! Кольцо — 256 fixed-записей по 144 байта (ABI v2, `usb_uevent_abi`).
//! Переполнение инкрементит `dropped`, но не теряет состояние: пропуск `seq` всё
//! равно требует re-seed, а снимок идемпотентен по `dev`.
//!
//! Lock-дисциплина: [`snapshot`] копирует список устройств из
//! `usb_storage::snapshot()` и **отпускает** тот лок до записи в кольцо; брать
//! оба лока сразу нельзя (спинлоки нереентерабельны, и `usb-storage` берёт
//! `DeviceManager` при регистрации).

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::drivers::usb_storage::DeviceRecord;
use crate::drivers::usb_uevent_abi::{self as abi, Record};

/// Ёмкость кольца (контракт §14.4).
pub const RING_CAP: usize = 256;

const EMPTY: Record = Record::zeroed();

struct Ring {
    entries: [Record; RING_CAP],
    head: usize,
    len: usize,
    seq: u32,
    dropped: u64,
}

static RING: crate::sync::spinlock::Spinlock<Ring> = crate::sync::spinlock::Spinlock::new(Ring {
    entries: [EMPTY; RING_CAP],
    head: 0,
    len: 0,
    seq: 1,
    dropped: 0,
});

/// Единственный читатель. `false` — устройство свободно.
static READER: AtomicBool = AtomicBool::new(false);

/// Занять устройство под одного читателя. `false` — уже занято (`EBUSY`).
pub fn claim_reader() -> bool {
    READER
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// Отпустить читателя (close последнего fd).
pub fn release_reader() {
    READER.store(false, Ordering::SeqCst);
}

/// Есть ли непрочитанные записи — для `poll(POLLIN)`.
pub fn has_events() -> bool {
    RING.lock().len != 0
}

/// Сколько записей потеряно из-за переполнения (диагностика; не состояние).
pub fn dropped() -> u64 {
    RING.lock().dropped
}

fn push_record(mut record: Record, snapshot: bool) {
    let mut ring = RING.lock();
    if snapshot {
        record.seq = 0;
    } else {
        ring.seq = ring.seq.wrapping_add(1);
        record.seq = ring.seq;
    }
    if ring.len == RING_CAP {
        ring.dropped = ring.dropped.saturating_add(1);
        return;
    }
    let index = (ring.head + ring.len) % RING_CAP;
    ring.entries[index] = record;
    ring.len += 1;
}

/// Забрать самую старую запись. `false` — кольцо пусто.
pub fn pop(out: &mut [u8; abi::RECORD_SIZE]) -> bool {
    let mut ring = RING.lock();
    if ring.len == 0 {
        return false;
    }
    ring.entries[ring.head].encode(out);
    ring.head = (ring.head + 1) % RING_CAP;
    ring.len -= 1;
    true
}

/// Флаги состояния в терминах ABI.
fn flags_of(state: &DeviceRecord, surprise_override: Option<bool>) -> u8 {
    let mut flags = 0u8;
    if state.removable {
        flags |= abi::flags::REMOVABLE;
    }
    if state.read_only {
        flags |= abi::flags::READ_ONLY;
    }
    if state.mounted {
        flags |= abi::flags::MOUNTED;
    }
    if state.in_use {
        flags |= abi::flags::IN_USE;
    }
    if state.safe_to_remove {
        flags |= abi::flags::SAFE_TO_REMOVE;
    }
    if surprise_override.unwrap_or(state.surprise) {
        flags |= abi::flags::SURPRISE;
    }
    if state.unclean {
        flags |= abi::flags::UNCLEAN;
    }
    if state.detached {
        flags |= abi::flags::DETACHED;
    }
    flags
}

/// Собрать v2-запись из состояния тома.
pub fn record_of(state: &DeviceRecord, kind: u8, error: u8) -> Record {
    let mut record = Record::zeroed();
    record.kind = kind;
    record.flags = flags_of(state, None);
    record.fs_class = state.fs_class;
    record.error = if error != abi::error::NONE {
        error
    } else {
        state.error
    };
    record.sectors = state.sectors;
    record.sector_size = state.sector_size;
    record.dev = abi::copy_str(&state.dev);
    record.mount = abi::copy_str(&state.mount);
    record.vendor = state.vendor;
    record.product = state.product;
    record.label = state.label;
    record.uuid = state.uuid;
    record
}

/// Опубликовать событие текущего состояния (live, `seq++`).
pub fn push_state(state: &DeviceRecord, kind: u8, error: u8) {
    let record = record_of(state, kind, error);
    push_record(record, false);
    crate::debug!(
        "usb-uevent: kind={} dev={} fs_class={} error={} mounted={}",
        kind,
        abi::get_str(&record.dev),
        record.fs_class,
        record.error,
        state.mounted as u8
    );
}

/// Снимок: `ADD` для каждого известного тома с `seq = 0`.
///
/// Список копируется до взятия лока кольца (см. lock-дисциплину в шапке).
pub fn snapshot() {
    let states: Vec<DeviceRecord> = crate::drivers::usb_storage::snapshot();
    for state in states.iter() {
        let record = record_of(state, abi::kind::ADD, abi::error::NONE);
        push_record(record, true);
    }
}

/// `PUEV_IOC_SNAPSHOT` = `0x5501`: снова положить снимок (для `seq`-разрыва).
pub const PUEV_IOC_SNAPSHOT: u64 = 0x5501;

/// Исполнить единственный ioctl устройства. `false` — `ENOTTY`.
pub fn ioctl(request: u64) -> bool {
    if request == PUEV_IOC_SNAPSHOT {
        snapshot();
        true
    } else {
        false
    }
}

/// `true`, если запись — снимок (`seq == 0`); используется диагностикой/tests.
pub fn is_snapshot(record: &Record) -> bool {
    record.seq == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(name: &str) -> DeviceRecord {
        DeviceRecord {
            dev: alloc::string::String::from(name),
            mount: alloc::string::String::new(),
            sectors: 1024,
            sector_size: 512,
            vendor: *b"QEMU\0\0\0\0\0\0\0\0",
            product: [0; 32],
            label: [0; 16],
            uuid: [0u8; 16],
            fs_class: abi::fs_class::PAGH_EXT2,
            error: 0,
            removable: true,
            read_only: false,
            mounted: false,
            in_use: false,
            safe_to_remove: false,
            surprise: false,
            unclean: false,
            detached: false,
            usb_index: 0,
            block: None,
        }
    }

    #[test]
    fn record_encodes_state_and_flags() {
        let mut s = state("usb0p1");
        s.mounted = true;
        s.unclean = true;
        s.detached = true;
        s.label = *b"PAGH\0\0\0\0\0\0\0\0\0\0\0\0";
        let r = record_of(&s, abi::kind::UNMOUNTED, abi::error::FLUSH_FAILED);
        assert_eq!(r.kind, abi::kind::UNMOUNTED);
        assert_eq!(r.error, abi::error::FLUSH_FAILED);
        assert_eq!(
            r.flags,
            abi::flags::REMOVABLE
                | abi::flags::MOUNTED
                | abi::flags::UNCLEAN
                | abi::flags::DETACHED
        );
        assert_eq!(abi::get_str(&r.dev), "usb0p1");
        assert_eq!(abi::get_str(&r.label), "PAGH");
    }
}
