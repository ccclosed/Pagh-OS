//! Чистый проводной формат USB Bulk-Only Transport (BOT 1.0): CBW 31 байт и
//! CSW 13 байт.
//!
//! Транспорт отделён от SCSI намеренно: CBW/CSW — это конверт, в который
//! кладётся любая CDB; ошибки конверта (подпись, тег, остаток) лечатся Reset
//! Recovery, а ошибки команды — REQUEST SENSE. Смешивать эти два слоя значило
//! бы, например, отвечать на «устройство прислало мусор вместо CSW» попыткой
//! прочитать sense, которой устройство не поймёт.
//!
//! Модуль только `core` (без `alloc`), поэтому host-свойства исполняют ровно
//! тот исходник, который компилирует ядро. Никакого GPL-кода: раскладки взяты
//! из публичной спецификации USB Mass Storage Class BOT 1.0 §6.1/§6.2.

/// Размер команды-обёртки (dCBWLength): 31 значащий байт.
pub const CBW_SIZE: usize = 31;
/// Размер статуса-обёртки: 13 значащих байт; хвоник буфера может быть больше.
pub const CSW_SIZE: usize = 13;

/// `dCBWSignature` = `"USBC"` в порядке байтов на проводе.
pub const CBW_SIGNATURE: u32 = 0x4342_5355;
/// `dCSWSignature` = `"USBS"` в порядке байтов на проводе.
pub const CSW_SIGNATURE: u32 = 0x5342_5355;

/// Направление data-стадии, как его несёт `bmCBWFlags` (бит 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Устройство → хост (READ, INQUIRY, READ CAPACITY).
    In,
    /// Хост → устройство (WRITE).
    Out,
}

impl Direction {
    fn flag(self) -> u8 {
        match self {
            Direction::In => 0x80,
            Direction::Out => 0,
        }
    }
}

/// Разобранная CBW. Нужна host-тестам и диагностике; драйвер только собирает
/// CBW, а не разбирает его.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cbw {
    pub tag: u32,
    pub transfer_len: u32,
    pub direction: Direction,
    pub lun: u8,
    /// Длина значащей части CDB (1..=16).
    pub cb_len: u8,
    pub cb: [u8; 16],
}

/// Собрать CBW. `cdb` — 1..=16 байт; `lun` — 0..=15; всё остальное — `None`,
/// потому что CBW с такими полями устройство обязано отвергнуть, и отправлять
/// его значит тратить Reset Recovery.
pub fn encode_cbw(
    tag: u32,
    transfer_len: u32,
    direction: Direction,
    lun: u8,
    cdb: &[u8],
) -> Option<[u8; CBW_SIZE]> {
    if cdb.is_empty() || cdb.len() > 16 || lun > 15 {
        return None;
    }
    let mut out = [0u8; CBW_SIZE];
    out[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
    out[4..8].copy_from_slice(&tag.to_le_bytes());
    out[8..12].copy_from_slice(&transfer_len.to_le_bytes());
    out[12] = direction.flag();
    out[13] = lun;
    out[14] = cdb.len() as u8;
    out[15..15 + cdb.len()].copy_from_slice(cdb);
    Some(out)
}

/// Разобрать CBW: подпись, диапазоны `bCBWLUN`/`bCBWCBLength`, CDB.
pub fn decode_cbw(buf: &[u8]) -> Option<Cbw> {
    if buf.len() < CBW_SIZE {
        return None;
    }
    if u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) != CBW_SIGNATURE {
        return None;
    }
    let tag = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let transfer_len = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let direction = if buf[12] & 0x80 != 0 {
        Direction::In
    } else {
        Direction::Out
    };
    let lun = buf[13];
    let cb_len = buf[14];
    if lun > 15 || cb_len == 0 || cb_len > 16 {
        return None;
    }
    let mut cb = [0u8; 16];
    cb.copy_from_slice(&buf[15..31]);
    Some(Cbw {
        tag,
        transfer_len,
        direction,
        lun,
        cb_len,
        cb,
    })
}

/// `bCSWStatus` (BOT 1.0, таблица 6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CswStatus {
    Passed,
    Failed,
    PhaseError,
    /// Неизвестное значение: считать провалом, а не гадать.
    Unknown(u8),
}

/// Разобранный CSW.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Csw {
    pub tag: u32,
    /// Сколько байт data-стадии устройство не передало.
    pub residue: u32,
    pub status: CswStatus,
}

/// Почему CSW непригоден. Каждый вариант — Reset Recovery, разный только в
/// диагностике: короткое чтение не доказывает, что устройство врёт, а неверная
/// подпись — доказывает.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CswError {
    /// Меньше 13 байт: short read.
    Short,
    /// `dCSWSignature != "USBS"`.
    BadSignature,
    /// `dCSWTag` не совпал с тегом CBW.
    TagMismatch { expected: u32, got: u32 },
}

/// Разобрать CSW. Тег сверяется с ожидаемым здесь, а не вызывающим: это
/// единственное место, где известны оба значения, и перепутать их нельзя.
pub fn decode_csw(buf: &[u8], expected_tag: u32) -> Result<Csw, CswError> {
    if buf.len() < CSW_SIZE {
        return Err(CswError::Short);
    }
    if u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) != CSW_SIGNATURE {
        return Err(CswError::BadSignature);
    }
    let tag = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if tag != expected_tag {
        return Err(CswError::TagMismatch {
            expected: expected_tag,
            got: tag,
        });
    }
    let residue = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let status = match buf[12] {
        0 => CswStatus::Passed,
        1 => CswStatus::Failed,
        2 => CswStatus::PhaseError,
        other => CswStatus::Unknown(other),
    };
    Ok(Csw {
        tag,
        residue,
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const READ10: [u8; 10] = [0x28, 0, 0, 0, 0, 0, 0, 0, 1, 0];

    #[test]
    fn cbw_is_exactly_31_bytes_with_documented_offsets() {
        let cbw = encode_cbw(0x1122_3344, 512, Direction::In, 0, &READ10).unwrap();
        assert_eq!(cbw.len(), CBW_SIZE);
        assert_eq!(&cbw[0..4], b"USBC");
        assert_eq!(cbw[4..8], 0x1122_3344u32.to_le_bytes());
        assert_eq!(cbw[8..12], 512u32.to_le_bytes());
        assert_eq!(cbw[12], 0x80, "bit 7 marks an IN data stage");
        assert_eq!(cbw[13], 0);
        assert_eq!(cbw[14], 10);
        assert_eq!(&cbw[15..25], &READ10);
        assert!(cbw[25..].iter().all(|b| *b == 0), "CBWCB padding is zero");
    }

    #[test]
    fn cbw_out_has_direction_bit_clear_and_round_trips() {
        let cbw = encode_cbw(7, 0, Direction::Out, 3, &[0x35, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(cbw[12] & 0x80, 0);
        let parsed = decode_cbw(&cbw).unwrap();
        assert_eq!(parsed.tag, 7);
        assert_eq!(parsed.transfer_len, 0);
        assert_eq!(parsed.direction, Direction::Out);
        assert_eq!(parsed.lun, 3);
        assert_eq!(parsed.cb_len, 6);
        assert_eq!(&parsed.cb[..6], &[0x35, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn cbw_rejects_bad_lun_empty_and_oversized_cdb() {
        assert!(encode_cbw(1, 0, Direction::In, 16, &READ10).is_none());
        assert!(encode_cbw(1, 0, Direction::In, 0, &[]).is_none());
        assert!(encode_cbw(1, 0, Direction::In, 0, &[0u8; 17]).is_none());
        // Максимальная CDB (16 байт) влезает ровно.
        assert!(encode_cbw(1, 0, Direction::In, 0, &[0u8; 16]).is_some());
    }

    #[test]
    fn decode_cbw_rejects_a_foreign_signature_and_bad_lengths() {
        let mut cbw = encode_cbw(1, 0, Direction::In, 0, &READ10).unwrap();
        cbw[0] ^= 0xFF;
        assert!(decode_cbw(&cbw).is_none());
        let mut cbw = encode_cbw(1, 0, Direction::In, 0, &READ10).unwrap();
        cbw[14] = 0;
        assert!(decode_cbw(&cbw).is_none());
        let mut cbw = encode_cbw(1, 0, Direction::In, 0, &READ10).unwrap();
        cbw[14] = 17;
        assert!(decode_cbw(&cbw).is_none());
        assert!(decode_cbw(&[0u8; 30]).is_none(), "short buffer");
    }

    fn csw(tag: u32, residue: u32, status: u8) -> [u8; CSW_SIZE] {
        let mut out = [0u8; CSW_SIZE];
        out[0..4].copy_from_slice(&CSW_SIGNATURE.to_le_bytes());
        out[4..8].copy_from_slice(&tag.to_le_bytes());
        out[8..12].copy_from_slice(&residue.to_le_bytes());
        out[12] = status;
        out
    }

    #[test]
    fn csw_decodes_all_three_statuses() {
        for (raw, want) in [
            (0u8, CswStatus::Passed),
            (1, CswStatus::Failed),
            (2, CswStatus::PhaseError),
            (0x7F, CswStatus::Unknown(0x7F)),
        ] {
            let parsed = decode_csw(&csw(0xABCD, 13, raw), 0xABCD).unwrap();
            assert_eq!(parsed.status, want);
            assert_eq!(parsed.residue, 13);
            assert_eq!(parsed.tag, 0xABCD);
        }
    }

    #[test]
    fn csw_errors_are_distinct() {
        assert_eq!(decode_csw(&[0u8; 12], 1), Err(CswError::Short));
        let mut buf = csw(1, 0, 0);
        buf[0] = 0;
        assert_eq!(decode_csw(&buf, 1), Err(CswError::BadSignature));
        assert_eq!(
            decode_csw(&csw(9, 0, 0), 8),
            Err(CswError::TagMismatch {
                expected: 8,
                got: 9
            })
        );
    }
}
