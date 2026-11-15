//! Чистое построение DNS-запроса и разбор A-записи ответа.
//!
//! Это **чистое** ядро резолвера (эффектный сокетный насос живёт в
//! [`crate::net::resolve`]). Как и [`crate::net::http`], только `core` + `alloc` —
//! без сокетов, глобалов и железа — и намеренно говорит простыми байтовыми
//! срезами и октетами `[u8; 4]`, а не каким-либо wire-типом стека, так что
//! собирается без изменений в `host-tests` через `#[path]`-include, и парсер можно
//! property-тестировать на хосте.
//!
//! Обе функции закалены против враждебного ввода: каждый индекс проверен на
//! границы, указатели сжатия в ответах обрабатываются защитно, и ни один путь не
//! паникует и не читает за входным буфером.
#![allow(dead_code)]

use alloc::vec::Vec;

/// DNS QTYPE для записи IPv4-адреса (A).
pub const QTYPE_A: u16 = 1;
/// DNS QTYPE для записи IPv6-адреса (AAAA).
pub const QTYPE_AAAA: u16 = 28;
/// DNS QCLASS для интернета (IN).
pub const QCLASS_IN: u16 = 1;

/// Максимальное число «прыжков» имени при пропуске (возможно сжатого) DNS-имени.
/// Граница предотвращает зацикливание на некорректных указателях.
const MAX_NAME_STEPS: usize = 128;

/// Построить стандартный DNS-запрос для `hostname` в `out` (сначала очищенный).
///
/// Даёт одно-Question сообщение с `QTYPE=A`, `QCLASS=IN`, выставленным флагом
/// recursion-desired (RD) и transaction id `id`. QNAME — разделённые точками
/// лейблы `hostname`, каждый с длиной-префиксом, завершённые корневым (нулевым)
/// лейблом.
///
/// Возвращает `false` (оставляя `out` безопасным, но неопределённым), если
/// `hostname` нельзя закодировать: пустой hostname, пустой лейбл (`..` или
/// ведущая/завершающая точка), лейбл длиннее 63 байт или закодированное имя
/// длиннее 255 байт. Чисто и panic-free.
pub fn build_dns_query(id: u16, hostname: &str, out: &mut Vec<u8>) -> bool {
    build_dns_query_typed(id, hostname, QTYPE_A, out)
}

/// То же, что [`build_dns_query`], но с явным типом вопроса
/// ([`QTYPE_A`] или [`QTYPE_AAAA`]).
pub fn build_dns_query_typed(id: u16, hostname: &str, qtype: u16, out: &mut Vec<u8>) -> bool {
    out.clear();

    if hostname.is_empty() {
        return false;
    }

    // Провалидировать + измерить длину закодированного QNAME до вывода чего-либо
    // за заголовок, чтобы отказ не оставил частичного мусора для неверной трактовки.
    let mut name_len = 1usize; // the terminating root label
    for label in hostname.split('.') {
        if label.is_empty() || label.len() > 63 {
            return false;
        }
        name_len += 1 + label.len();
    }
    if name_len > 255 {
        return false;
    }

    // Заголовок (12 байт): ID, флаги, QDCOUNT=1, ANCOUNT=NSCOUNT=ARCOUNT=0.
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&0x0100u16.to_be_bytes()); // QR=0, Opcode=0, RD=1
    out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT

    // QNAME: лейблы с префиксом длины + корневой лейбл.
    for label in hostname.split('.') {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);

    // QTYPE, QCLASS.
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&QCLASS_IN.to_be_bytes());
    true
}

/// Проверить, что Question-запись с `pos` несёт ровно QNAME-лейблы `hostname`
/// (байт-в-байт, как их закодировал запрос) плюс QTYPE `qtype` / QCLASS IN.
/// Возвращает позицию сразу за вопросом.
///
/// Ответ, чей вопрос не совпадает с отправленным, — не ответ на наш запрос
/// (off-path spoof или устаревший/чужой reply) и должен быть отвергнут. Указатель
/// сжатия внутри *вопроса* ответа на одно-вопросный запрос законно не
/// встречается (вопрос начинается со смещения 12, до него нечего указывать) —
/// считаем это несовпадением.
fn verify_question(buf: &[u8], mut pos: usize, hostname: &str, qtype: u16) -> Option<usize> {
    for label in hostname.split('.') {
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            return None;
        }
        let len = *buf.get(pos)?;
        if len & 0xC0 != 0 || len as usize != bytes.len() {
            return None;
        }
        pos += 1;
        if buf.get(pos..pos + bytes.len())? != bytes {
            return None;
        }
        pos += bytes.len();
    }
    // Завершающий корневой лейбл, затем QTYPE + QCLASS.
    if *buf.get(pos)? != 0 {
        return None;
    }
    pos += 1;
    if buf.len() < pos + 4 {
        return None;
    }
    let qt = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
    let qc = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]);
    if qt != qtype || qc != QCLASS_IN {
        return None;
    }
    Some(pos + 4)
}

/// Разобрать первый A-record (IPv4) ответ из DNS-ответа в `buf`.
///
/// Возвращает `Some([a, b, c, d])` для первой answer RR с TYPE A и RDATA ровно в
/// четыре байта. Возвращает `None` при любом из: буфер короче 12-байтового
/// заголовка, несовпадение transaction-id с `expected_id`, сообщение не является
/// ответом (QR снят), ненулевой RCODE (например NXDOMAIN), секция вопроса не
/// повторяет ровно отправленный `expected_name`/A-вопрос, A-ответа нет или любая
/// некорректная/усечённая запись. Никогда не индексирует за `buf` и не паникует.
pub fn parse_dns_a_response(buf: &[u8], expected_id: u16, expected_name: &str) -> Option<[u8; 4]> {
    if buf.len() < 12 {
        return None;
    }

    let id = u16::from_be_bytes([buf[0], buf[1]]);
    if id != expected_id {
        return None;
    }

    let flags = u16::from_be_bytes([buf[2], buf[3]]);
    // Для ответа QR (бит 15) должен быть выставлен.
    if flags & 0x8000 == 0 {
        return None;
    }
    // RCODE (младшие 4 бита) должен быть 0 (NoError); NXDOMAIN(3) и прочие -> None.
    if flags & 0x000F != 0 {
        return None;
    }

    let qdcount = u16::from_be_bytes([buf[4], buf[5]]);
    let ancount = u16::from_be_bytes([buf[6], buf[7]]);

    // Мы всегда шлём один вопрос; ответ должен повторить ровно его.
    if qdcount != 1 {
        return None;
    }
    let mut pos = verify_question(buf, 12, expected_name, QTYPE_A)?;

    // Идти по answer RR в поисках первой A-записи.
    for _ in 0..ancount {
        pos = skip_name(buf, pos)?;
        // Фиксированные поля RR: TYPE(2) CLASS(2) TTL(4) RDLENGTH(2) = 10 байт.
        let fields_end = pos.checked_add(10)?;
        if fields_end > buf.len() {
            return None;
        }
        let rtype = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let rdlength = u16::from_be_bytes([buf[pos + 8], buf[pos + 9]]) as usize;
        let rdata = fields_end;
        let rdata_end = rdata.checked_add(rdlength)?;
        if rdata_end > buf.len() {
            return None;
        }
        if rtype == QTYPE_A && rdlength == 4 {
            return Some([buf[rdata], buf[rdata + 1], buf[rdata + 2], buf[rdata + 3]]);
        }
        pos = rdata_end;
    }

    None
}

/// Разобрать первый AAAA-record (IPv6) ответ из DNS-ответа в `buf`.
///
/// Те же правила валидации, что у [`parse_dns_a_response`]; RDATA ответа должно
/// быть ровно 16 байт.
pub fn parse_dns_aaaa_response(
    buf: &[u8],
    expected_id: u16,
    expected_name: &str,
) -> Option<[u8; 16]> {
    if buf.len() < 12 {
        return None;
    }

    let id = u16::from_be_bytes([buf[0], buf[1]]);
    if id != expected_id {
        return None;
    }

    let flags = u16::from_be_bytes([buf[2], buf[3]]);
    if flags & 0x8000 == 0 || flags & 0x000F != 0 {
        return None;
    }

    let qdcount = u16::from_be_bytes([buf[4], buf[5]]);
    let ancount = u16::from_be_bytes([buf[6], buf[7]]);

    if qdcount != 1 {
        return None;
    }
    let mut pos = verify_question(buf, 12, expected_name, QTYPE_AAAA)?;

    for _ in 0..ancount {
        pos = skip_name(buf, pos)?;
        let fields_end = pos.checked_add(10)?;
        if fields_end > buf.len() {
            return None;
        }
        let rtype = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let rdlength = u16::from_be_bytes([buf[pos + 8], buf[pos + 9]]) as usize;
        let rdata = fields_end;
        let rdata_end = rdata.checked_add(rdlength)?;
        if rdata_end > buf.len() {
            return None;
        }
        if rtype == QTYPE_AAAA && rdlength == 16 {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&buf[rdata..rdata_end]);
            return Some(octets);
        }
        pos = rdata_end;
    }

    None
}

/// Пройти за (возможно сжатым) DNS-именем, начинающимся с `pos`, вернув индекс
/// первого байта после имени *в потоке записей*.
///
/// Для имени, завершённого указателем сжатия, это индекс сразу за двумя байтами
/// указателя (по нему не идём — вызывающим нужно только пропустить имя, никогда
/// его не декодируя). Возвращает `None` на любом некорректном вводе: индекс вне
/// диапазона, зарезервированные биты длины или более [`MAX_NAME_STEPS`] лейблов
/// (зацикливание). Никогда не индексирует за `buf`.
fn skip_name(buf: &[u8], mut pos: usize) -> Option<usize> {
    let mut steps = 0usize;
    loop {
        steps += 1;
        if steps > MAX_NAME_STEPS {
            return None;
        }
        let len = *buf.get(pos)?;
        match len & 0xC0 {
            0x00 => {
                if len == 0 {
                    // Корневой лейбл: имя кончается здесь.
                    return Some(pos + 1);
                }
                // Обычный лейбл: пропустить байт длины + байты лейбла.
                pos = pos.checked_add(1 + len as usize)?;
                if pos > buf.len() {
                    return None;
                }
            }
            0xC0 => {
                // Указатель сжатия (два байта); убедиться, что второй байт
                // существует, затем имя кончается здесь в потоке.
                buf.get(pos + 1)?;
                return Some(pos + 2);
            }
            // 0x40 / 0x80: зарезервированные типы лейблов -> некорректно.
            _ => return None,
        }
    }
}

/// Разобрать IPv4-литерал dotted-quad (например `"10.0.2.3"`) в четыре октета
/// или `None` для любого некорректного ввода (неверное число октетов,
/// пустой/слишком длинный октет, не-цифровой байт или октет вне `0..=255`).
/// Чисто и panic-free; позволяет [`crate::net::resolve`] короткозамкнуть литерал
/// без выдачи запроса.
pub fn parse_ipv4_literal(s: &str) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut idx = 0usize;
    for part in s.split('.') {
        if idx >= 4 || part.is_empty() || part.len() > 3 {
            return None;
        }
        let mut value: u16 = 0;
        for b in part.bytes() {
            if !b.is_ascii_digit() {
                return None;
            }
            value = value * 10 + (b - b'0') as u16;
        }
        if value > 255 {
            return None;
        }
        octets[idx] = value as u8;
        idx += 1;
    }
    if idx != 4 {
        return None;
    }
    Some(octets)
}
