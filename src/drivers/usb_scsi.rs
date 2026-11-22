//! Чистый SCSI: CDB Phase 1, разбор INQUIRY/READ CAPACITY/REQUEST SENSE и
//! политика sense→действие.
//!
//! Слой BOT (`usb_msc_proto`) переносит байты, но не знает, что они значат.
//! Здесь живут ровно два решения, которые можно проверить на хосте без
//! устройства: как собрать команду и что делать с ответом. Драйвер
//! (`usb_msc`) только исполняет эти решения, поэтому ошибка в sense-матрице не
//! требует QEMU, чтобы её увидеть.
//!
//! Только `core`: никаких аллокаций и зависимостей. Опкоды и раскладки — SBC-3
//! (`READ(10)`, `WRITE(10)`, `READ CAPACITY(10)`, `SYNCHRONIZE CACHE(10)`) и
//! SPC-4 (`INQUIRY`, `REQUEST SENSE`, sense key/ASC/ASCQ); копирования кода
//! чужих драйверов нет.

/// `TEST UNIT READY` (SPC-4): 6-байтная CDB без data-стадии.
pub const OP_TEST_UNIT_READY: u8 = 0x00;
/// `REQUEST SENSE` (SPC-4): 6-байтная CDB, 18 байт ответа.
pub const OP_REQUEST_SENSE: u8 = 0x03;
/// `INQUIRY` (SPC-4): 6-байтная CDB.
pub const OP_INQUIRY: u8 = 0x12;
/// `READ CAPACITY(10)` (SBC-3): 10-байтная CDB, 8 байт ответа.
pub const OP_READ_CAPACITY_10: u8 = 0x25;
/// `READ(10)` (SBC-3): 10-байтная CDB.
pub const OP_READ_10: u8 = 0x28;
/// `WRITE(10)` (SBC-3): 10-байтная CDB.
pub const OP_WRITE_10: u8 = 0x2a;
/// `SYNCHRONIZE CACHE(10)` (SBC-3): 10-байтная CDB, без data-стадии.
pub const OP_SYNCHRONIZE_CACHE_10: u8 = 0x35;
/// `READ(16)` (SBC-3): 16-байтная CDB; Phase 1 не использует (LBA > 32 бит —
/// следующий этап), но билдер готов и проверен host-тестом.
pub const OP_READ_16: u8 = 0x88;
/// `WRITE(16)` (SBC-3): 16-байтная CDB; Phase 1 не использует.
pub const OP_WRITE_16: u8 = 0x8a;
/// `READ CAPACITY(16)` (SBC-3): 16-байтная CDB, 32 байта ответа; Phase 1 не
/// использует для принятия тома, но билдер/разбор готовы.
pub const OP_READ_CAPACITY_16: u8 = 0x9e;
/// `SERVICE ACTION IN` для READ CAPACITY(16) — байт 1 CDB.
pub const SA_READ_CAPACITY_16: u8 = 0x10;

/// Размер логического блока, который поддерживает Phase 1. Partition/ext2
/// адресуют 512-байтовые секторы; 4K — отдельный этап с явным отказом.
pub const PHASE1_BLOCK_SIZE: u32 = 512;

/// `INQUIRY` в формате, который использует драйвер: EVPD=0, allocation 36.
pub fn inquiry() -> [u8; 6] {
    [OP_INQUIRY, 0, 0, 0, 36, 0]
}

/// `REQUEST SENSE`: allocation 18 (стандартный fixed-format ответ).
pub fn request_sense() -> [u8; 6] {
    [OP_REQUEST_SENSE, 0, 0, 0, 18, 0]
}

/// `TEST UNIT READY`.
pub fn test_unit_ready() -> [u8; 6] {
    [OP_TEST_UNIT_READY, 0, 0, 0, 0, 0]
}

/// `READ CAPACITY(10)`: LBA 0, allocation 0 (устройство возвращает 8 байт).
pub fn read_capacity10() -> [u8; 10] {
    [OP_READ_CAPACITY_10, 0, 0, 0, 0, 0, 0, 0, 0, 0]
}

/// `READ CAPACITY(16)`: SERVICE ACTION IN, allocation 32 (future).
pub fn read_capacity16() -> [u8; 16] {
    [
        OP_READ_CAPACITY_16,
        SA_READ_CAPACITY_16,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        32,
        0,
        0,
    ]
}

/// `READ(10)`: `len` — число блоков (1..=65535), не байт.
pub fn read10(lba: u32, len: u16) -> [u8; 10] {
    let b = lba.to_be_bytes();
    let n = len.to_be_bytes();
    [OP_READ_10, 0, b[0], b[1], b[2], b[3], 0, n[0], n[1], 0]
}

/// `WRITE(10)`: `len` — число блоков (1..=65535), не байт.
pub fn write10(lba: u32, len: u16) -> [u8; 10] {
    let b = lba.to_be_bytes();
    let n = len.to_be_bytes();
    [OP_WRITE_10, 0, b[0], b[1], b[2], b[3], 0, n[0], n[1], 0]
}

/// `SYNCHRONIZE CACHE(10)` для всего устройства: LBA 0, блоков 0.
pub fn synchronize_cache10() -> [u8; 10] {
    [OP_SYNCHRONIZE_CACHE_10, 0, 0, 0, 0, 0, 0, 0, 0, 0]
}

/// `READ(16)` (future): LBA 8 байт, число блоков 4 байта.
pub fn read16(lba: u64, len: u32) -> [u8; 16] {
    let b = lba.to_be_bytes();
    let n = len.to_be_bytes();
    [
        OP_READ_16, 0, b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], n[0], n[1], n[2], n[3], 0, 0,
    ]
}

/// `WRITE(16)` (future): LBA 8 байт, число блоков 4 байта.
pub fn write16(lba: u64, len: u32) -> [u8; 16] {
    let b = lba.to_be_bytes();
    let n = len.to_be_bytes();
    [
        OP_WRITE_16,
        0,
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        n[0],
        n[1],
        n[2],
        n[3],
        0,
        0,
    ]
}

/// Разобранный ответ INQUIRY.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inquiry {
    /// Peripheral device type, младшие 5 бит байта 0.
    pub pdt: u8,
    /// RMB: носитель съёмный (флешка), а не фиксированный диск.
    pub removable: bool,
    /// 8 ASCII-байт vendor.
    pub vendor: [u8; 8],
    /// 16 ASCII-байт product.
    pub product: [u8; 16],
    /// 4 ASCII-байта revision.
    pub revision: [u8; 4],
}

/// Разобрать стандартный INQUIRY. `None`, если ответ короче 36 байт: поля
/// product/revision тогда недостоверны, и выдавать обрезанный ответ за
/// устройство нельзя.
pub fn parse_inquiry(data: &[u8]) -> Option<Inquiry> {
    if data.len() < 36 {
        return None;
    }
    let mut vendor = [0u8; 8];
    vendor.copy_from_slice(&data[8..16]);
    let mut product = [0u8; 16];
    product.copy_from_slice(&data[16..32]);
    let mut revision = [0u8; 4];
    revision.copy_from_slice(&data[32..36]);
    Some(Inquiry {
        pdt: data[0] & 0x1f,
        removable: data[1] & 0x80 != 0,
        vendor,
        product,
        revision,
    })
}

/// `READ CAPACITY(10)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    /// Последний адресуемый LBA; у устройства `sector_count = last_lba + 1`.
    pub last_lba: u32,
    /// Размер логического блока в байтах.
    pub block_length: u32,
}

/// Разобрать 8-байтный ответ READ CAPACITY(10).
pub fn parse_capacity10(data: &[u8]) -> Option<Capacity> {
    if data.len() < 8 {
        return None;
    }
    Some(Capacity {
        last_lba: u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
        block_length: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
    })
}

/// `READ CAPACITY(16)` (future): 32-байтный ответ, LBA 8 байт + block length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity16 {
    pub last_lba: u64,
    pub block_length: u32,
}

/// Разобрать 32-байтный ответ READ CAPACITY(16).
pub fn parse_capacity16(data: &[u8]) -> Option<Capacity16> {
    if data.len() < 12 {
        return None;
    }
    Some(Capacity16 {
        last_lba: u64::from_be_bytes([
            data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
        ]),
        block_length: u32::from_be_bytes([data[8], data[9], data[10], data[11]]),
    })
}

/// Почему ёмкость не принимается в Phase 1. Оба варианта — явный отказ, а не
/// усечение: 4K-блоки и LBA > 32 бит нельзя честно адресовать текущим слоем.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapacityProblem {
    /// Размер логического блока не 512.
    UnsupportedBlock(u32),
    /// `last_lba == 0xFFFF_FFFF` — устройство может быть >2 TiB, и READ(10) всё
    /// равно не покроет его.
    Overflow,
}

/// Проверить, годится ли `READ CAPACITY(10)` для Phase 1. `None` — годится.
pub fn capacity_problem(last_lba: u32, block_length: u32) -> Option<CapacityProblem> {
    if block_length != PHASE1_BLOCK_SIZE {
        return Some(CapacityProblem::UnsupportedBlock(block_length));
    }
    if last_lba == u32::MAX {
        return Some(CapacityProblem::Overflow);
    }
    None
}

/// `REQUEST SENSE` (fixed format, 18 байт).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sense {
    pub response: u8,
    pub key: u8,
    pub asc: u8,
    pub ascq: u8,
}

/// Разобрать fixed-format sense (SPC-4 §4.5.3). Меньше 14 байт — ответ
/// недостоверен; `None`.
pub fn parse_sense(data: &[u8]) -> Option<Sense> {
    if data.len() < 14 {
        return None;
    }
    Some(Sense {
        response: data[0] & 0x7f,
        key: data[2] & 0x0f,
        asc: data[12],
        ascq: data[13],
    })
}

/// Что драйвер делает после `CHECK CONDITION` (таблица спеки §8.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SenseAction {
    /// NO SENSE: команда фактически прошла.
    Ok,
    /// UNIT ATTENTION / becoming ready: прогнать TUR и повторить команду.
    RetryTur,
    /// Временная ошибка: повторить команду один раз.
    RetryOnce,
    /// Носителя нет: EIO + `no_media`.
    NoMedia,
    /// Запись защищена.
    ReadOnly,
    /// Команда не поддерживается устройством.
    Unsupported,
    /// Aborted command: Reset Recovery и один повтор.
    Aborted,
    /// Всё остальное: EIO без повторов.
    Fatal,
}

/// Отобразить sense key/ASC/ASCQ в действие драйвера.
///
/// Матрица намеренно узкая: неизвестная комбинация — `Fatal`, потому что
/// повторять команду, которую устройство отвергло по непонятной причине, значит
/// маскировать реальную ошибку. `Ok` только для NO SENSE и response code 0.
pub fn sense_action(sense: Sense) -> SenseAction {
    // Fixed-format текущая ошибка (0x70) или отложенная (0x71). Прочие коды
    // (например, descriptor format) драйвер не запрашивает.
    if sense.response != 0x70 && sense.response != 0x71 {
        return SenseAction::Fatal;
    }
    match (sense.key, sense.asc, sense.ascq) {
        (0x0, 0x00, 0x00) => SenseAction::Ok,
        (0x2, 0x3a, 0x00) => SenseAction::NoMedia,
        (0x2, 0x04, 0x01) => SenseAction::RetryTur,
        (0x2, 0x04, 0x02) => SenseAction::RetryTur,
        (0x3, 0x11, 0x00) => SenseAction::RetryOnce,
        (0x3, 0x0c, 0x00) => SenseAction::RetryOnce,
        (0x5, 0x20, 0x00) => SenseAction::Unsupported,
        (0x5, 0x24, 0x00) => SenseAction::Fatal,
        (0x6, 0x28, 0x00) => SenseAction::RetryTur,
        (0x6, 0x29, 0x00) => SenseAction::RetryTur,
        (0x7, 0x27, 0x00) => SenseAction::ReadOnly,
        (0xb, 0x00, 0x00) => SenseAction::Aborted,
        _ => SenseAction::Fatal,
    }
}

/// Скопировать ASCII-поле (vendor/product/label) в буфер фиксированной длины:
/// обрезать хвостовые пробелы/NUL, непечатаемое заменить на `?`, остаток
/// заполнить NUL. Возвращает длину значащей части до NUL.
///
/// Именно эта функция питает uevent-поля `vendor[16]`/`product[32]`; одна
/// реализация на драйвер и тесты исключает расхождение «в логе одно, в
/// событии другое».
pub fn ascii_field(src: &[u8], out: &mut [u8]) -> usize {
    if out.is_empty() {
        return 0;
    }
    let limit = out.len() - 1; // место под NUL
    let mut end = src.len().min(limit);
    while end > 0 && (src[end - 1] == b' ' || src[end - 1] == 0) {
        end -= 1;
    }
    for (dst, byte) in out.iter_mut().zip(src.iter()).take(end) {
        *dst = if (0x20..=0x7e).contains(byte) {
            *byte
        } else {
            b'?'
        };
    }
    for byte in out.iter_mut().skip(end) {
        *byte = 0;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_cdbs_are_big_endian_and_documented() {
        assert_eq!(
            read10(0x0102_0304, 0x0102),
            [0x28, 0, 1, 2, 3, 4, 0, 1, 2, 0]
        );
        assert_eq!(
            write10(0x1122_3344, 0x00ff),
            [0x2a, 0, 0x11, 0x22, 0x33, 0x44, 0, 0x00, 0xff, 0]
        );
        assert_eq!(
            read16(0x0102_0304_0506_0708, 0x1122_3344),
            [0x88, 0, 1, 2, 3, 4, 5, 6, 7, 8, 0x11, 0x22, 0x33, 0x44, 0, 0]
        );
        assert_eq!(synchronize_cache10(), [0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(test_unit_ready(), [0, 0, 0, 0, 0, 0]);
        assert_eq!(read_capacity10(), [0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(inquiry(), [0x12, 0, 0, 0, 36, 0]);
        assert_eq!(request_sense(), [0x03, 0, 0, 0, 18, 0]);
        let rc16 = read_capacity16();
        assert_eq!(rc16[0], 0x9e);
        assert_eq!(rc16[1], 0x10);
        assert_eq!(rc16[13], 32);
    }

    #[test]
    fn inquiry_parses_pdt_removable_and_fields() {
        let mut data = [0x20u8; 36];
        data[0] = 0x00; // direct access
        data[1] = 0x80; // removable
        data[8..16].copy_from_slice(b"QEMU    ");
        data[16..32].copy_from_slice(b"QEMU HARDDISK   ");
        data[32..36].copy_from_slice(b"2.5+");
        let inq = parse_inquiry(&data).unwrap();
        assert_eq!(inq.pdt, 0);
        assert!(inq.removable);
        assert_eq!(&inq.vendor, b"QEMU    ");
        assert_eq!(&inq.product, b"QEMU HARDDISK   ");
        assert_eq!(&inq.revision, b"2.5+");
        // Младшие 5 бит PDT: qualifier в старших трёх не должен протекать.
        data[0] = 0xe0 | 0x05;
        assert_eq!(parse_inquiry(&data).unwrap().pdt, 5);
        assert!(parse_inquiry(&data[..35]).is_none());
    }

    #[test]
    fn capacity_rejects_4k_and_overflow_without_truncation() {
        let cap = parse_capacity10(&[0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x02, 0x00]).unwrap();
        assert_eq!(cap.last_lba, 0x1000);
        assert_eq!(cap.block_length, 512);
        assert_eq!(capacity_problem(cap.last_lba, cap.block_length), None);
        assert_eq!(
            capacity_problem(7, 4096),
            Some(CapacityProblem::UnsupportedBlock(4096))
        );
        assert_eq!(
            capacity_problem(u32::MAX, 512),
            Some(CapacityProblem::Overflow)
        );
        // block_length проверяется раньше overflow: 4K-устройству нельзя
        // приписать причину capacity_overflow.
        assert_eq!(
            capacity_problem(u32::MAX, 4096),
            Some(CapacityProblem::UnsupportedBlock(4096))
        );
        let c16 = parse_capacity16(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(c16.last_lba, 0x0000_0001_0000_0000);
        assert_eq!(c16.block_length, 4096);
    }

    #[test]
    fn sense_matrix_covers_the_documented_table() {
        let s = |key, asc, ascq| Sense {
            response: 0x70,
            key,
            asc,
            ascq,
        };
        assert_eq!(sense_action(s(0, 0, 0)), SenseAction::Ok);
        assert_eq!(sense_action(s(2, 0x3a, 0)), SenseAction::NoMedia);
        assert_eq!(sense_action(s(2, 0x04, 1)), SenseAction::RetryTur);
        assert_eq!(sense_action(s(2, 0x04, 2)), SenseAction::RetryTur);
        assert_eq!(sense_action(s(3, 0x11, 0)), SenseAction::RetryOnce);
        assert_eq!(sense_action(s(3, 0x0c, 0)), SenseAction::RetryOnce);
        assert_eq!(sense_action(s(5, 0x20, 0)), SenseAction::Unsupported);
        assert_eq!(sense_action(s(5, 0x24, 0)), SenseAction::Fatal);
        assert_eq!(sense_action(s(6, 0x28, 0)), SenseAction::RetryTur);
        assert_eq!(sense_action(s(6, 0x29, 0)), SenseAction::RetryTur);
        assert_eq!(sense_action(s(7, 0x27, 0)), SenseAction::ReadOnly);
        assert_eq!(sense_action(s(0xb, 0, 0)), SenseAction::Aborted);
        assert_eq!(sense_action(s(1, 0x12, 0)), SenseAction::Fatal);
        let mut deferred = s(6, 0x28, 0);
        deferred.response = 0x71;
        assert_eq!(sense_action(deferred), SenseAction::RetryTur);
        let mut descriptor = s(0, 0, 0);
        descriptor.response = 0x72;
        assert_eq!(sense_action(descriptor), SenseAction::Fatal);
    }

    #[test]
    fn parse_sense_reads_fixed_format_offsets() {
        let mut data = [0u8; 18];
        data[0] = 0x70;
        data[2] = 0x06;
        data[12] = 0x28;
        data[13] = 0x00;
        let sense = parse_sense(&data).unwrap();
        assert_eq!(sense.key, 6);
        assert_eq!(sense.asc, 0x28);
        assert_eq!(sense.ascq, 0);
        assert!(parse_sense(&data[..13]).is_none());
    }

    #[test]
    fn ascii_field_trims_and_replaces_non_printable() {
        let mut out = [0xAAu8; 16];
        let n = ascii_field(b"QEMU    ", &mut out);
        assert_eq!(n, 4);
        assert_eq!(&out[..5], b"QEMU\0");
        assert!(out[5..].iter().all(|b| *b == 0));
        let mut out = [0u8; 8];
        let n = ascii_field(b"ab\x01cd     ", &mut out);
        assert_eq!(n, 5);
        assert_eq!(&out[..5], b"ab?cd");
        // Слишком длинное поле обрезается, оставляя NUL.
        let mut out = [0xAAu8; 4];
        let n = ascii_field(b"ABCDEFGH", &mut out);
        assert_eq!(n, 3);
        assert_eq!(&out, b"ABC\0");
    }
}
