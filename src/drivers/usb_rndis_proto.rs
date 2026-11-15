//! Чистый протокол RNDIS: сообщения управления и data-пакеты.
//!
//! RNDIS — это NDIS поверх USB: хост посылает сообщения в
//! SEND_ENCAPSULATED_COMMAND, читает ответы GET_ENCAPSULATED_RESPONSE, а
//! Ethernet-кадры ходят по bulk-паре в конвертах RNDIS. Все сообщения —
//! little-endian структуры фиксированной раскладки; здесь живут их сборка и
//! разбор, без USB-транспорта, чтобы host-тесты исполняли тот же исходник, что
//! компилирует ядро, и могли подавать усечённые/лживые ответы.
//!
//! Инварианты разбора:
//!   * ни одно поле длины из устройства не используется без проверки границ;
//!   * ответ считается ответом только при совпадении типа завершения;
//!     `RequestId` проверяет вызывающий, потому что повторы — его политика;
//!   * data-конверт несёт 8-байтный префикс (тип и полная длина), а
//!     `DataOffset` отсчитывается от его конца. Поэтому кадр начинается по
//!     `8 + DataOffset`, а не по `DataOffset`: это раскладка RNDIS, и на ней
//!     сходятся и телефон, и эмулятор.
//!
//! Модуль только `core` + `alloc` и самодостаточен.

use alloc::vec;
use alloc::vec::Vec;

// ─── MessageType (RNDIS §3.1) ───────────────────────────────────────────────

pub const MSG_PACKET: u32 = 0x0000_0001;
pub const MSG_INITIALIZE: u32 = 0x0000_0002;
pub const MSG_HALT: u32 = 0x0000_0003;
pub const MSG_QUERY: u32 = 0x0000_0004;
pub const MSG_SET: u32 = 0x0000_0005;
pub const MSG_RESET: u32 = 0x0000_0006;
pub const MSG_INDICATE_STATUS: u32 = 0x0000_0007;
pub const MSG_KEEPALIVE: u32 = 0x0000_0008;

pub const CMPLT_INITIALIZE: u32 = 0x8000_0002;
pub const CMPLT_QUERY: u32 = 0x8000_0004;
pub const CMPLT_SET: u32 = 0x8000_0005;
pub const CMPLT_RESET: u32 = 0x8000_0006;
pub const CMPLT_KEEPALIVE: u32 = 0x8000_0008;

/// Завершение RNDIS с ошибкой; отличать успех от отказа обязательно, иначе
/// интерфейс поднимается с невалидным MAC и молча не получает DHCP.
pub const STATUS_SUCCESS: u32 = 0x0000_0000;

// ─── OID ────────────────────────────────────────────────────────────────────

pub const OID_GEN_PHYSICAL_MEDIUM: u32 = 0x0001_0202;
pub const OID_GEN_CURRENT_PACKET_FILTER: u32 = 0x0001_010E;
pub const OID_802_3_PERMANENT_ADDRESS: u32 = 0x0101_0101;
pub const OID_802_3_CURRENT_ADDRESS: u32 = 0x0101_0102;

/// Направленный unicast | multicast | broadcast | all-multicast. Ненулевой
/// фильтр переводит устройство в состояние, в котором оно начинает гонять
/// кадры; promiscuous не нужен стеку.
pub const PACKET_FILTER_DEFAULT: u32 = 0x0000_000F;

/// Максимальный размер одного RNDIS-конверта, который драйвер готов принять.
/// Ethernet-кадр 1518 + 44-байтный заголовок; запас на выравнивание.
pub const MAX_TRANSFER: usize = 2048;

/// Размер заголовка data-конверта: 11 dword (тип, длина, смещение/длина данных,
/// OOB/per-packet поля, VcHandle, резерв).
pub const DATA_HEADER: usize = 44;

/// `DataOffset` конверта, который строит драйвер: данные сразу за заголовком,
/// то есть 44 - 8 (смещение считается от конца 8-байтного префикса).
pub const DATA_OFFSET: u32 = (DATA_HEADER - 8) as u32;

fn le32(buf: &[u8], at: usize) -> Option<u32> {
    let b = buf.get(at..at + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn put32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

// ─── Сборка сообщений ───────────────────────────────────────────────────────

/// INITIALIZE_MSG: 24 байта, major/minor 1.0, объявленный максимум передачи.
pub fn encode_initialize(xid: u32, max_transfer: u32) -> Vec<u8> {
    let mut msg = vec![0u8; 24];
    put32(&mut msg, 0, MSG_INITIALIZE);
    put32(&mut msg, 4, 24);
    put32(&mut msg, 8, xid);
    put32(&mut msg, 12, 1); // MajorVersion
    put32(&mut msg, 16, 0); // MinorVersion
    put32(&mut msg, 20, max_transfer);
    msg
}

/// QUERY_MSG: 28 байт заголовка + `info_len` байт информационного буфера.
///
/// Информационный буфер нулевой, но его длина не ноль: часть устройств (и
/// эмулятор) отвергает запрос, у которого `InformationBufferOffset` указывает
/// ровно на конец сообщения. `info_len` — требование вызывающего (4 для
/// physical medium, 48 для MAC), а не догадка драйвера.
pub fn encode_query(xid: u32, oid: u32, info_len: usize) -> Vec<u8> {
    let mut msg = vec![0u8; 28 + info_len];
    put32(&mut msg, 0, MSG_QUERY);
    put32(&mut msg, 4, (28 + info_len) as u32);
    put32(&mut msg, 8, xid);
    put32(&mut msg, 12, oid);
    put32(&mut msg, 16, info_len as u32);
    put32(&mut msg, 20, 20); // InformationBufferOffset от RequestId (8 + 20 = 28)
    put32(&mut msg, 24, 0); // DeviceVcHandle
    msg
}

/// SET_MSG: 28 байт заголовка + 4-байтное значение.
pub fn encode_set(xid: u32, oid: u32, value: u32) -> Vec<u8> {
    let mut msg = vec![0u8; 32];
    put32(&mut msg, 0, MSG_SET);
    put32(&mut msg, 4, 32);
    put32(&mut msg, 8, xid);
    put32(&mut msg, 12, oid);
    put32(&mut msg, 16, 4);
    put32(&mut msg, 20, 20);
    put32(&mut msg, 24, 0);
    put32(&mut msg, 28, value);
    msg
}

/// KEEPALIVE_MSG: 12 байт; устройство обязано ответить, иначе связь считается
/// потерянной.
pub fn encode_keepalive(xid: u32) -> Vec<u8> {
    let mut msg = vec![0u8; 12];
    put32(&mut msg, 0, MSG_KEEPALIVE);
    put32(&mut msg, 4, 12);
    put32(&mut msg, 8, xid);
    msg
}

// ─── Разбор ответов ─────────────────────────────────────────────────────────

/// Общий заголовок ответа: тип, полная длина, RequestId, статус.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResponseHeader {
    pub msg_type: u32,
    pub msg_len: usize,
    pub request_id: u32,
    pub status: u32,
}

/// Прочитать общий заголовок и проверить, что `msg_len` не выходит за буфер.
pub fn parse_header(buf: &[u8]) -> Option<ResponseHeader> {
    let msg_type = le32(buf, 0)?;
    let msg_len = le32(buf, 4)? as usize;
    if msg_len < 16 || msg_len > buf.len() {
        return None;
    }
    Some(ResponseHeader {
        msg_type,
        msg_len,
        request_id: le32(buf, 8)?,
        status: le32(buf, 12)?,
    })
}

/// INITIALIZE_CMPLT: 52 байта; вызывающему нужны статус и объявленный максимум.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitComplete {
    pub status: u32,
    pub max_transfer_size: u32,
    pub medium: u32,
}

pub fn parse_initialize_complete(buf: &[u8]) -> Option<InitComplete> {
    let h = parse_header(buf)?;
    if h.msg_type != CMPLT_INITIALIZE || h.msg_len < 52 {
        return None;
    }
    Some(InitComplete {
        status: h.status,
        // Поля ответа после Status: 16 Major, 20 Minor, 24 DeviceFlags,
        // 28 Medium, 32 MaxPacketsPerTransfer, 36 MaxTransferSize.
        medium: le32(buf, 28)?,
        max_transfer_size: le32(buf, 36)?,
    })
}

/// QUERY_CMPLT: заголовок 24 байта + информационный буфер по смещению от
/// RequestId. Возвращает статус и точный срез данных.
#[derive(Clone, Copy, Debug)]
pub struct QueryComplete<'a> {
    pub status: u32,
    pub info: &'a [u8],
}

pub fn parse_query_complete(buf: &[u8]) -> Option<QueryComplete<'_>> {
    let h = parse_header(buf)?;
    if h.msg_type != CMPLT_QUERY || h.msg_len < 24 {
        return None;
    }
    let info_len = le32(buf, 16)? as usize;
    let info_offset = le32(buf, 20)? as usize;
    if h.status != STATUS_SUCCESS {
        return Some(QueryComplete {
            status: h.status,
            info: &[],
        });
    }
    // Смещение отсчитывается от RequestId (байт 8), как в запросе.
    let start = 8usize.checked_add(info_offset)?;
    let end = start.checked_add(info_len)?;
    if info_len == 0 || start < 24 || end > h.msg_len || end > buf.len() {
        return None;
    }
    Some(QueryComplete {
        status: h.status,
        info: &buf[start..end],
    })
}

/// SET/KEEPALIVE/RESET CMPLT: статус без информационного буфера.
pub fn parse_simple_complete(buf: &[u8], expected: u32) -> Option<ResponseHeader> {
    let h = parse_header(buf)?;
    if h.msg_type != expected {
        return None;
    }
    Some(h)
}

// ─── Data-конверты ──────────────────────────────────────────────────────────

/// Собрать data-конверт в `out` и вернуть его длину.
///
/// `out` обязан быть не меньше `DATA_HEADER + frame.len()`. Заголовок
/// заполняется нулями целиком, включая OOB/per-packet поля: устройство не
/// должно получить мусор из прошлой передачи.
pub fn encode_data_into(frame: &[u8], out: &mut [u8]) -> Option<usize> {
    let total = DATA_HEADER.checked_add(frame.len())?;
    if out.len() < total {
        return None;
    }
    out[..DATA_HEADER].fill(0);
    put32(out, 0, MSG_PACKET);
    put32(out, 4, total as u32);
    put32(out, 8, DATA_OFFSET);
    put32(out, 12, frame.len() as u32);
    out[DATA_HEADER..total].copy_from_slice(frame);
    Some(total)
}

/// Разобрать один конверт и вернуть срез Ethernet-кадра.
///
/// Поддерживаются обе формы, которые встречаются в поле:
///   * стандартный заголовок с `DataOffset`: кадр по `8 + DataOffset`;
///   * компактный конверт (только 8-байтный префикс и кадр следом), у которого
///     поля смещения/длины нулевые.
///
/// Любое несоответствие длин — отказ на этот конверт, а не чтение за границей.
pub fn parse_data_packet(buf: &[u8]) -> Option<(&[u8], usize)> {
    let msg_type = le32(buf, 0)?;
    let msg_len = le32(buf, 4)? as usize;
    if msg_type != MSG_PACKET || msg_len < 8 || msg_len > buf.len() {
        return None;
    }
    if msg_len >= 16 {
        let data_offset = le32(buf, 8)? as usize;
        let data_len = le32(buf, 12)? as usize;
        // Стандартная форма: DataOffset может быть 0..=36, а кадр обязан
        // начинаться не раньше префикса и заканчиваться внутри конверта.
        if data_len > 0 {
            let start = 8usize.checked_add(data_offset)?;
            let end = start.checked_add(data_len)?;
            if start >= 8 && end <= msg_len {
                return Some((&buf[start..end], msg_len));
            }
            return None;
        }
    }
    // Компактная форма: кадр сразу после 8-байтного префикса.
    if msg_len > 8 {
        return Some((&buf[8..msg_len], msg_len));
    }
    None
}

/// Вызвать `f` для каждого кадра в батче bulk-IN.
///
/// Один URB может нести несколько конвертов; обход идёт по `MessageLength` и
/// останавливается на первом некорректном, возвращая число доставленных
/// кадров. `max_packets` ограничивает работу даже на лживом буфере, целиком
/// состоящем из валидных восьмибайтных заголовков.
pub fn for_each_data_packet(buf: &[u8], max_packets: usize, mut f: impl FnMut(&[u8])) -> usize {
    let mut at = 0usize;
    let mut count = 0usize;
    while at + 8 <= buf.len() && count < max_packets {
        let Some((frame, msg_len)) = parse_data_packet(&buf[at..]) else {
            break;
        };
        if frame.len() >= 14 {
            f(frame);
            count += 1;
        }
        if msg_len == 0 {
            break;
        }
        at += msg_len;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_layout_is_24_bytes() {
        let msg = encode_initialize(7, MAX_TRANSFER as u32);
        assert_eq!(msg.len(), 24);
        assert_eq!(le32(&msg, 0), Some(MSG_INITIALIZE));
        assert_eq!(le32(&msg, 4), Some(24));
        assert_eq!(le32(&msg, 8), Some(7));
        assert_eq!(le32(&msg, 12), Some(1));
        assert_eq!(le32(&msg, 20), Some(MAX_TRANSFER as u32));
    }

    #[test]
    fn query_payload_starts_after_header() {
        let msg = encode_query(9, OID_802_3_PERMANENT_ADDRESS, 48);
        assert_eq!(msg.len(), 76);
        assert_eq!(le32(&msg, 4), Some(76));
        assert_eq!(le32(&msg, 16), Some(48));
        assert_eq!(le32(&msg, 20), Some(20));
        assert_eq!(msg[28..].len(), 48);
    }

    #[test]
    fn set_carries_four_byte_value() {
        let msg = encode_set(3, OID_GEN_CURRENT_PACKET_FILTER, PACKET_FILTER_DEFAULT);
        assert_eq!(msg.len(), 32);
        assert_eq!(le32(&msg, 4), Some(32));
        assert_eq!(le32(&msg, 12), Some(OID_GEN_CURRENT_PACKET_FILTER));
        assert_eq!(le32(&msg, 16), Some(4));
        assert_eq!(le32(&msg, 28), Some(PACKET_FILTER_DEFAULT));
    }

    #[test]
    fn data_roundtrip_and_batch() {
        let frame = [0xAAu8; 60];
        let mut buf = [0u8; 256];
        let n = encode_data_into(&frame, &mut buf).unwrap();
        assert_eq!(n, DATA_HEADER + frame.len());
        let (got, used) = parse_data_packet(&buf[..n]).unwrap();
        assert_eq!(used, n);
        assert_eq!(got, &frame);

        let mut batch = [0u8; 512];
        let n1 = encode_data_into(&frame, &mut batch).unwrap();
        let n2 = encode_data_into(&frame[..20], &mut batch[n1..]).unwrap();
        let mut seen = 0;
        let count = for_each_data_packet(&batch[..n1 + n2], 8, |f| {
            assert_eq!(f.len(), if seen == 0 { 60 } else { 20 });
            seen += 1;
        });
        assert_eq!(count, 2);
        assert_eq!(seen, 2);
    }

    #[test]
    fn initialize_complete_fields_are_at_spec_offsets() {
        let mut buf = [0u8; 52];
        put32(&mut buf, 0, CMPLT_INITIALIZE);
        put32(&mut buf, 4, 52);
        put32(&mut buf, 8, 42);
        put32(&mut buf, 12, STATUS_SUCCESS);
        put32(&mut buf, 16, 1);
        put32(&mut buf, 20, 0);
        put32(&mut buf, 24, 1); // connectionless
        put32(&mut buf, 28, 0); // 802.3
        put32(&mut buf, 32, 1); // max packets per transfer
        put32(&mut buf, 36, 1558); // max transfer
        let init = parse_initialize_complete(&buf).unwrap();
        assert_eq!(init.medium, 0);
        assert_eq!(init.max_transfer_size, 1558);
    }

    #[test]
    fn query_response_offset_is_relative_to_request_id() {
        let mut buf = [0u8; 30];
        put32(&mut buf, 0, CMPLT_QUERY);
        put32(&mut buf, 4, 30);
        put32(&mut buf, 8, 11);
        put32(&mut buf, 12, STATUS_SUCCESS);
        put32(&mut buf, 16, 6);
        put32(&mut buf, 20, 16); // 8 + 16 = 24
        buf[24..30].copy_from_slice(&[0, 1, 2, 3, 4, 5]);
        let q = parse_query_complete(&buf).unwrap();
        assert_eq!(q.info, &[0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn hostile_lengths_are_refused() {
        assert!(parse_data_packet(&[1, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]).is_none());
        let mut buf = [0u8; 16];
        put32(&mut buf, 0, MSG_PACKET);
        put32(&mut buf, 4, 16);
        put32(&mut buf, 8, 0xFFFF_FFFF);
        put32(&mut buf, 12, 0xFFFF_FFFF);
        assert!(parse_data_packet(&buf).is_none());
        // Усечённый query-ответ не должен выдать срез за своими границами.
        let mut q = [0u8; 24];
        put32(&mut q, 0, CMPLT_QUERY);
        put32(&mut q, 4, 24);
        put32(&mut q, 8, 1);
        put32(&mut q, 12, STATUS_SUCCESS);
        put32(&mut q, 16, 6);
        put32(&mut q, 20, 16);
        assert!(parse_query_complete(&q).is_none());
    }
}
