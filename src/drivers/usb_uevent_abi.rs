//! ABI `/dev/uevent` v2: ровно 144 байта, little-endian, строки — NUL-padded
//! ASCII.
//!
//! Почему отдельный чистый модуль: композитор и Files держат эти смещения у
//! себя, и единственный способ не разъехаться — host-свойство, которое
//! фиксирует размер, каждое смещение и все значения `kind`/`flags`/`fs_class`/
//! `error`. Ядро и тесты компилируют один и тот же файл.
//!
//! v1 (128 байт) не смешивается с v2: другой размер и другая `version`, поэтому
//! v2-читатель отвергает короткую запись, а не читает её первые 128 байт как
//! «почти v2». `mount[16]` остаётся явным полем: Files открывает точку из
//! события, а не выводит её из имени устройства, и future `/media/...` не совпадёт
//! с `dev`. `uuid[16]` — ключ персистентности между reboot (`usbN` не стабилен).

/// `magic` = `"PUEV"` в порядке байтов (little-endian u32).
pub const MAGIC: u32 = 0x5645_5550;
/// Версия записи. v1 = 128 байт, v2 = 144; это единственный признак, по
/// которому читатель понимает, что `uuid` существует.
pub const VERSION: u16 = 2;
/// Размер записи v2.
pub const RECORD_SIZE: usize = 144;

/// `kind`: что произошло.
pub mod kind {
    pub const ADD: u8 = 1;
    pub const REMOVE: u8 = 2;
    pub const CHANGE: u8 = 3;
    pub const ERROR: u8 = 4;
    pub const MOUNTED: u8 = 5;
    pub const UNMOUNTED: u8 = 6;
}

/// `flags`: биты состояния. 0–5 не перенумеровываются; 6–7 append-only.
pub mod flags {
    pub const REMOVABLE: u8 = 1 << 0;
    pub const READ_ONLY: u8 = 1 << 1;
    pub const MOUNTED: u8 = 1 << 2;
    pub const IN_USE: u8 = 1 << 3;
    pub const SAFE_TO_REMOVE: u8 = 1 << 4;
    pub const SURPRISE: u8 = 1 << 5;
    /// Append-only: WAL не проверен (surprise removal / detach без replay).
    pub const UNCLEAN: u8 = 1 << 6;
    /// Append-only: том отцеплен от mount-таблицы.
    pub const DETACHED: u8 = 1 << 7;
}

/// `fs_class`: что лежит на томе, насколько это видит ядро.
pub mod fs_class {
    pub const UNKNOWN: u8 = 0;
    pub const PAGH_EXT2: u8 = 1;
    pub const FOREIGN_EXT2: u8 = 2;
    pub const BLANK: u8 = 3;
    pub const OTHER_FS: u8 = 4;
    pub const NO_MEDIA: u8 = 5;
}

/// `error`: причина отказа. 0–5 не перенумеровываются; 6–9 append-only.
pub mod error {
    pub const NONE: u8 = 0;
    pub const IO: u8 = 1;
    pub const MOUNT_FAILED: u8 = 2;
    pub const FLUSH_FAILED: u8 = 3;
    pub const UNSUPPORTED_FS: u8 = 4;
    pub const NO_MEDIA: u8 = 5;
    pub const UNSUPPORTED_BLOCK: u8 = 6;
    pub const CAPACITY_OVERFLOW: u8 = 7;
    pub const REPLAY_FAILED: u8 = 8;
    pub const BAD_MOUNT: u8 = 9;
}

/// Одна запись v2 в RAM. Поля — ровно как на проводе; кодирование/декодирование
/// побайтовое, чтобы не зависеть от выравнивания и `repr(C)` в ABI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Record {
    pub magic: u32,
    pub version: u16,
    pub kind: u8,
    pub flags: u8,
    pub fs_class: u8,
    pub error: u8,
    pub seq: u32,
    pub sectors: u64,
    pub sector_size: u32,
    pub dev: [u8; 16],
    pub mount: [u8; 16],
    pub vendor: [u8; 16],
    pub product: [u8; 32],
    pub label: [u8; 16],
    pub uuid: [u8; 16],
}

impl Record {
    /// Нулевая запись с корректными `magic`/`version`: ring инициализируется ею,
    /// а `encode` всегда переписывает все 144 байта целиком.
    pub const fn zeroed() -> Self {
        Self {
            magic: MAGIC,
            version: VERSION,
            kind: 0,
            flags: 0,
            fs_class: 0,
            error: 0,
            seq: 0,
            sectors: 0,
            sector_size: 512,
            dev: [0; 16],
            mount: [0; 16],
            vendor: [0; 16],
            product: [0; 32],
            label: [0; 16],
            uuid: [0; 16],
        }
    }

    /// Закодировать запись в буфер ровно 144 байта.
    pub fn encode(&self, out: &mut [u8; RECORD_SIZE]) {
        out.fill(0);
        out[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        out[4..6].copy_from_slice(&VERSION.to_le_bytes());
        out[6] = self.kind;
        out[7] = self.flags;
        out[8] = self.fs_class;
        out[9] = self.error;
        // 10..12 — reserved0 = 0.
        out[12..16].copy_from_slice(&self.seq.to_le_bytes());
        out[16..24].copy_from_slice(&self.sectors.to_le_bytes());
        out[24..28].copy_from_slice(&self.sector_size.to_le_bytes());
        out[28..44].copy_from_slice(&self.dev);
        out[44..60].copy_from_slice(&self.mount);
        out[60..76].copy_from_slice(&self.vendor);
        out[76..108].copy_from_slice(&self.product);
        out[108..124].copy_from_slice(&self.label);
        // 124..128 — reserved1 = 0.
        out[128..144].copy_from_slice(&self.uuid);
    }

    /// Декодировать запись. `None` — чужой `magic` или `version != 2`, то есть
    /// запись нельзя интерпретировать как v2 (в том числе v1).
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < RECORD_SIZE {
            return None;
        }
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let version = u16::from_le_bytes([buf[4], buf[5]]);
        if magic != MAGIC || version != VERSION {
            return None;
        }
        let mut record = Self::zeroed();
        record.kind = buf[6];
        record.flags = buf[7];
        record.fs_class = buf[8];
        record.error = buf[9];
        record.seq = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        record.sectors = u64::from_le_bytes([
            buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23],
        ]);
        record.sector_size = u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]);
        record.dev.copy_from_slice(&buf[28..44]);
        record.mount.copy_from_slice(&buf[44..60]);
        record.vendor.copy_from_slice(&buf[60..76]);
        record.product.copy_from_slice(&buf[76..108]);
        record.label.copy_from_slice(&buf[108..124]);
        record.uuid.copy_from_slice(&buf[128..144]);
        Some(record)
    }
}

/// Скопировать строку в NUL-padded ASCII-поле: не-ASCII → `?`, длина
/// обрезается до `field.len() - 1`, остаток — NUL.
pub fn set_str(field: &mut [u8], text: &str) {
    field.fill(0);
    let limit = field.len().saturating_sub(1);
    for (dst, ch) in field.iter_mut().zip(text.bytes()).take(limit) {
        *dst = if (0x20..=0x7e).contains(&ch) {
            ch
        } else {
            b'?'
        };
    }
}

/// Скопировать строку в новое NUL-padded поле фиксированного размера.
pub fn copy_str<const N: usize>(text: &str) -> [u8; N] {
    let mut out = [0u8; N];
    set_str(&mut out, text);
    out
}

/// Прочитать NUL-padded ASCII-поле (для логов/тестов).
pub fn get_str(field: &[u8]) -> &str {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    core::str::from_utf8(&field[..end]).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Record {
        let mut r = Record::zeroed();
        r.kind = kind::ADD;
        r.flags = flags::REMOVABLE | flags::SAFE_TO_REMOVE | flags::DETACHED;
        r.fs_class = fs_class::PAGH_EXT2;
        r.error = error::BAD_MOUNT;
        r.seq = 0x1122_3344;
        r.sectors = 0x0102_0304_0506_0708;
        r.sector_size = 512;
        set_str(&mut r.dev, "usb0p1");
        set_str(&mut r.mount, "/usb0p1");
        set_str(&mut r.vendor, "QEMU");
        set_str(&mut r.product, "QEMU HARDDISK");
        set_str(&mut r.label, "PAGH");
        r.uuid = [0xAB; 16];
        r
    }

    #[test]
    fn size_and_every_offset_match_the_contract() {
        assert_eq!(core::mem::size_of::<Record>(), RECORD_SIZE);
        let mut buf = [0u8; RECORD_SIZE];
        sample().encode(&mut buf);
        assert_eq!(&buf[0..4], b"PUEV");
        assert_eq!(u16::from_le_bytes([buf[4], buf[5]]), 2);
        assert_eq!(buf[6], kind::ADD);
        assert_eq!(
            buf[7],
            flags::REMOVABLE | flags::SAFE_TO_REMOVE | flags::DETACHED
        );
        assert_eq!(buf[8], fs_class::PAGH_EXT2);
        assert_eq!(buf[9], error::BAD_MOUNT);
        assert_eq!(&buf[10..12], &[0, 0], "reserved0");
        assert_eq!(
            u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            0x1122_3344
        );
        assert_eq!(
            u64::from_le_bytes([
                buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23]
            ]),
            0x0102_0304_0506_0708
        );
        assert_eq!(
            u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]),
            512
        );
        assert_eq!(&buf[28..34], b"usb0p1");
        assert_eq!(buf[34], 0);
        assert_eq!(&buf[44..51], b"/usb0p1");
        assert_eq!(&buf[60..64], b"QEMU");
        assert_eq!(&buf[76..89], b"QEMU HARDDISK");
        assert_eq!(&buf[108..112], b"PAGH");
        assert_eq!(&buf[124..128], &[0, 0, 0, 0], "reserved1");
        assert_eq!(&buf[128..144], &[0xAB; 16]);
    }

    #[test]
    fn round_trip_preserves_every_field() {
        let mut buf = [0u8; RECORD_SIZE];
        let original = sample();
        original.encode(&mut buf);
        let decoded = Record::decode(&buf).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn decode_rejects_v1_short_foreign_magic_and_wrong_version() {
        let mut buf = [0u8; RECORD_SIZE];
        sample().encode(&mut buf);
        assert!(Record::decode(&buf[..128]).is_none(), "v1 128 B is not v2");
        let mut foreign = buf;
        foreign[0..4].copy_from_slice(b"XXXX");
        assert!(Record::decode(&foreign).is_none());
        let mut v1 = buf;
        v1[4..6].copy_from_slice(&1u16.to_le_bytes());
        assert!(Record::decode(&v1).is_none());
    }

    #[test]
    fn every_flag_and_error_value_has_its_documented_bit_or_number() {
        assert_eq!(flags::REMOVABLE, 0x01);
        assert_eq!(flags::READ_ONLY, 0x02);
        assert_eq!(flags::MOUNTED, 0x04);
        assert_eq!(flags::IN_USE, 0x08);
        assert_eq!(flags::SAFE_TO_REMOVE, 0x10);
        assert_eq!(flags::SURPRISE, 0x20);
        assert_eq!(flags::UNCLEAN, 0x40);
        assert_eq!(flags::DETACHED, 0x80);
        assert_eq!(
            [
                kind::ADD,
                kind::REMOVE,
                kind::CHANGE,
                kind::ERROR,
                kind::MOUNTED,
                kind::UNMOUNTED
            ],
            [1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            [
                error::NONE,
                error::IO,
                error::MOUNT_FAILED,
                error::FLUSH_FAILED,
                error::UNSUPPORTED_FS,
                error::NO_MEDIA,
                error::UNSUPPORTED_BLOCK,
                error::CAPACITY_OVERFLOW,
                error::REPLAY_FAILED,
                error::BAD_MOUNT
            ],
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
        );
    }

    #[test]
    fn strings_truncate_and_replace_non_ascii() {
        let mut field = [0xAAu8; 16];
        set_str(&mut field, "usb0p123456789012345");
        assert_eq!(get_str(&field), "usb0p1234567890");
        assert_eq!(field[15], 0);
        let mut small = [0u8; 4];
        set_str(&mut small, "aé");
        assert_eq!(get_str(&small), "a??"); // UTF-8 bytes are two non-ASCII bytes
    }
}
