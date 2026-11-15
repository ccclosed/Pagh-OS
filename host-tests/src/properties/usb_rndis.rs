// Feature: USB RNDIS — дескрипторы и протокол.
//
// Свойства чистых слоёв RNDIS, которые компилирует ядро:
//   * `usb_desc::parse_rndis_interfaces` — communication + CDC Data, три
//     известные тройки class/subclass/protocol, обязательные обе bulk-стороны,
//     настройка по умолчанию и испорченные дескрипторы;
//   * `usb_rndis_proto` — сборка INIT/QUERY/SET/KEEPALIVE, разбор ответов по
//     `RequestId`/статусу и data-конверты: 8-байтный префикс, `DataOffset` от
//     его конца, батчи в одном URB, любые лживые длины.

use crate::usb_desc::{parse_rndis_interfaces, RndisIface};
use crate::usb_rndis_proto::{
    self as proto, DATA_HEADER, MAX_TRANSFER, MSG_PACKET, PACKET_FILTER_DEFAULT,
};
use proptest::prelude::*;

// ─── сборка дескрипторов ────────────────────────────────────────────────────

fn config_desc(total: u16, value: u8) -> std::vec::Vec<u8> {
    let [lo, hi] = total.to_le_bytes();
    std::vec![9, 2, lo, hi, 0, value, 0x80, 0x32, 0]
}

fn iface_desc(number: u8, alt: u8, class: u8, subclass: u8, protocol: u8) -> std::vec::Vec<u8> {
    std::vec![9, 4, number, alt, 0, class, subclass, protocol, 0]
}

fn ep_desc(addr: u8, attrs: u8, max_packet: u16, interval: u8) -> std::vec::Vec<u8> {
    let [lo, hi] = max_packet.to_le_bytes();
    std::vec![7, 5, addr, attrs, lo, hi, interval]
}

fn functional(b: &[u8]) -> std::vec::Vec<u8> {
    b.to_vec()
}

/// RNDIS-конфигурация QEMU: communication 02/02/ff + CDC Data 0A/00/00,
/// статус 0x81, bulk 0x82/0x02, bConfigurationValue = 2.
fn qemu_rndis_config() -> std::vec::Vec<u8> {
    let mut body = std::vec::Vec::new();
    body.extend(iface_desc(0, 0, 0x02, 0x02, 0xFF));
    body.extend(functional(&[5, 0x24, 0x00, 0x10, 0x01]));
    body.extend(functional(&[5, 0x24, 0x01, 0x00, 0x01]));
    body.extend(functional(&[4, 0x24, 0x02, 0x00]));
    body.extend(functional(&[5, 0x24, 0x06, 0x00, 0x01]));
    body.extend(ep_desc(0x81, 0x03, 8, 1));
    body.extend(iface_desc(1, 0, 0x0A, 0x00, 0x00));
    body.extend(ep_desc(0x82, 0x02, 64, 0));
    body.extend(ep_desc(0x02, 0x02, 64, 0));
    let mut conf = config_desc((9 + body.len()) as u16, 2);
    conf.extend(body);
    conf
}

/// Телефон: communication EF/04/01 (IAD RNDIS), data 0A/00/00, bulk 0x8e/0x0f,
/// статус 0x81, максимальные пакеты 512 (high-speed).
fn phone_rndis_config() -> std::vec::Vec<u8> {
    let mut body = std::vec::Vec::new();
    body.extend(iface_desc(0, 0, 0xEF, 0x04, 0x01));
    body.extend(ep_desc(0x81, 0x03, 8, 9));
    body.extend(iface_desc(1, 0, 0x0A, 0x00, 0x00));
    body.extend(ep_desc(0x8E, 0x02, 512, 0));
    body.extend(ep_desc(0x0F, 0x02, 512, 0));
    let mut conf = config_desc((9 + body.len()) as u16, 1);
    conf.extend(body);
    conf
}

#[test]
fn rndis_desc_parses_qemu_configuration() {
    let got = parse_rndis_interfaces(&qemu_rndis_config()).expect("QEMU RNDIS pair");
    assert_eq!(
        got,
        RndisIface {
            config_value: 2,
            comm_interface: 0,
            data_interface: 1,
            status_ep_addr: 0x81,
            status_max_packet: 8,
            status_interval: 1,
            bulk_in_addr: 0x82,
            bulk_in_max_packet: 64,
            bulk_out_addr: 0x02,
            bulk_out_max_packet: 64,
        }
    );
}

#[test]
fn rndis_desc_parses_phone_pair() {
    let got = parse_rndis_interfaces(&phone_rndis_config()).expect("phone RNDIS pair");
    assert_eq!(got.config_value, 1);
    assert_eq!(got.comm_interface, 0);
    assert_eq!(got.data_interface, 1);
    assert_eq!(got.status_ep_addr, 0x81);
    assert_eq!(got.bulk_in_addr, 0x8E);
    assert_eq!(got.bulk_in_max_packet, 512);
    assert_eq!(got.bulk_out_addr, 0x0F);
    assert_eq!(got.bulk_out_max_packet, 512);
}

#[test]
fn rndis_desc_supports_wireless_controller_and_non_adjacent_data() {
    let mut body = std::vec::Vec::new();
    body.extend(iface_desc(2, 0, 0xE0, 0x01, 0x03));
    body.extend(ep_desc(0x83, 0x03, 8, 4));
    body.extend(iface_desc(5, 0, 0x0A, 0x00, 0x00));
    body.extend(ep_desc(0x8F, 0x02, 512, 0));
    body.extend(ep_desc(0x0E, 0x02, 512, 0));
    let mut conf = config_desc((9 + body.len()) as u16, 4);
    conf.extend(body);
    let got = parse_rndis_interfaces(&conf).expect("wireless RNDIS pair");
    assert_eq!(got.config_value, 4);
    assert_eq!(got.comm_interface, 2);
    assert_eq!(got.data_interface, 5);
    assert_eq!(got.bulk_in_addr, 0x8F);
    assert_eq!(got.bulk_out_addr, 0x0E);
}

#[test]
fn rndis_desc_requires_both_bulk_directions_and_status_in() {
    // Нет bulk-IN.
    let mut body = std::vec::Vec::new();
    body.extend(iface_desc(0, 0, 0xEF, 0x04, 0x01));
    body.extend(ep_desc(0x81, 0x03, 8, 9));
    body.extend(iface_desc(1, 0, 0x0A, 0x00, 0x00));
    body.extend(ep_desc(0x02, 0x02, 512, 0));
    let mut conf = config_desc((9 + body.len()) as u16, 1);
    conf.extend(body);
    assert!(parse_rndis_interfaces(&conf).is_none());

    // Нет interrupt-IN у communication-интерфейса.
    let mut body = std::vec::Vec::new();
    body.extend(iface_desc(0, 0, 0xEF, 0x04, 0x01));
    body.extend(ep_desc(0x01, 0x02, 8, 9));
    body.extend(iface_desc(1, 0, 0x0A, 0x00, 0x00));
    body.extend(ep_desc(0x82, 0x02, 512, 0));
    body.extend(ep_desc(0x02, 0x02, 512, 0));
    let mut conf = config_desc((9 + body.len()) as u16, 1);
    conf.extend(body);
    assert!(parse_rndis_interfaces(&conf).is_none());
}

#[test]
fn rndis_desc_ignores_alternate_settings_and_ecm() {
    // CDC Data на alt 1 (как у QEMU CDC ECM): драйвер alt не выбирает.
    let mut body = std::vec::Vec::new();
    body.extend(iface_desc(0, 0, 0xEF, 0x04, 0x01));
    body.extend(ep_desc(0x81, 0x03, 8, 9));
    body.extend(iface_desc(1, 0, 0x0A, 0x00, 0x00));
    body.extend(iface_desc(1, 1, 0x0A, 0x00, 0x00));
    body.extend(ep_desc(0x82, 0x02, 512, 0));
    body.extend(ep_desc(0x02, 0x02, 512, 0));
    let mut conf = config_desc((9 + body.len()) as u16, 1);
    conf.extend(body);
    assert!(parse_rndis_interfaces(&conf).is_none());

    // CDC ECM (02/06/00) — не RNDIS.
    let mut body = std::vec::Vec::new();
    body.extend(iface_desc(0, 0, 0x02, 0x06, 0x00));
    body.extend(ep_desc(0x81, 0x03, 8, 9));
    body.extend(iface_desc(1, 0, 0x0A, 0x00, 0x00));
    body.extend(ep_desc(0x82, 0x02, 512, 0));
    body.extend(ep_desc(0x02, 0x02, 512, 0));
    let mut conf = config_desc((9 + body.len()) as u16, 1);
    conf.extend(body);
    assert!(parse_rndis_interfaces(&conf).is_none());
}

proptest! {
    /// Произвольные байты не паникуют; найденная пара всегда имеет IN-статус и
    /// bulk-эндпоинты в разные стороны.
    #[test]
    fn rndis_desc_is_total_on_arbitrary_bytes(
        bytes in proptest::collection::vec(any::<u8>(), 0..256)
    ) {
        if let Some(p) = parse_rndis_interfaces(&bytes) {
            prop_assert_eq!(p.status_ep_addr & 0x80, 0x80);
            prop_assert_eq!(p.bulk_in_addr & 0x80, 0x80);
            prop_assert_eq!(p.bulk_out_addr & 0x80, 0x00);
            prop_assert_ne!(p.bulk_in_max_packet & 0x07FF, 0);
            prop_assert_ne!(p.bulk_out_max_packet & 0x07FF, 0);
        }
    }

    /// Усечение валидного дескриптора не изобретает пару, которой не было в
    /// полном разборе: результат является префиксом по факту наличия пары.
    #[test]
    fn rndis_desc_truncation_is_monotone(cut in 0usize..256) {
        let full = phone_rndis_config();
        let cut = cut.min(full.len());
        if parse_rndis_interfaces(&full[..cut]).is_some() {
            prop_assert!(parse_rndis_interfaces(&full).is_some());
        }
    }
}

// ─── протокол: сборка ───────────────────────────────────────────────────────

#[test]
fn rndis_message_layouts_are_pinned() {
    let init = proto::encode_initialize(7, MAX_TRANSFER as u32);
    assert_eq!(init.len(), 24);
    assert_eq!(&init[0..4], &proto::MSG_INITIALIZE.to_le_bytes());
    assert_eq!(&init[4..8], &24u32.to_le_bytes());
    assert_eq!(&init[8..12], &7u32.to_le_bytes());
    assert_eq!(&init[12..16], &1u32.to_le_bytes());
    assert_eq!(&init[20..24], &(MAX_TRANSFER as u32).to_le_bytes());

    let query = proto::encode_query(9, proto::OID_802_3_PERMANENT_ADDRESS, 48);
    assert_eq!(query.len(), 76);
    assert_eq!(&query[4..8], &76u32.to_le_bytes());
    assert_eq!(&query[16..20], &48u32.to_le_bytes());
    assert_eq!(&query[20..24], &20u32.to_le_bytes());

    let set = proto::encode_set(3, proto::OID_GEN_CURRENT_PACKET_FILTER, PACKET_FILTER_DEFAULT);
    assert_eq!(set.len(), 32);
    assert_eq!(&set[12..16], &proto::OID_GEN_CURRENT_PACKET_FILTER.to_le_bytes());
    assert_eq!(&set[28..32], &PACKET_FILTER_DEFAULT.to_le_bytes());
}

// ─── протокол: data-конверты ────────────────────────────────────────────────

proptest! {
    /// Любой кадр 14..=1518 байт переживает конверт и разбор без потерь; заголовок
    /// объявляет полную длину, а DataOffset считает от конца 8-байтного префикса.
    #[test]
    fn rndis_data_roundtrip(frame in proptest::collection::vec(any::<u8>(), 14..=1518)) {
        let mut buf = std::vec![0u8; DATA_HEADER + frame.len()];
        let n = proto::encode_data_into(&frame, &mut buf).expect("fits");
        prop_assert_eq!(n, buf.len());
        prop_assert_eq!(&buf[0..4], &MSG_PACKET.to_le_bytes());
        prop_assert_eq!(&buf[4..8], &(n as u32).to_le_bytes());
        prop_assert_eq!(&buf[8..12], &proto::DATA_OFFSET.to_le_bytes());
        prop_assert_eq!(&buf[12..16], &(frame.len() as u32).to_le_bytes());
        let (got, used) = proto::parse_data_packet(&buf).expect("parses");
        prop_assert_eq!(used, n);
        prop_assert_eq!(got, &frame[..]);
    }

    /// Батч из нескольких конвертов в одном URB доставляется целиком и по порядку.
    #[test]
    fn rndis_data_batch_roundtrip(frames in proptest::collection::vec(
        proptest::collection::vec(any::<u8>(), 14..=400), 1..=8))
    {
        let mut buf = std::vec::Vec::new();
        for f in &frames {
            let mut one = std::vec![0u8; DATA_HEADER + f.len()];
            let n = proto::encode_data_into(f, &mut one).expect("fits");
            buf.extend_from_slice(&one[..n]);
        }
        let mut seen = std::vec::Vec::new();
        let count = proto::for_each_data_packet(&buf, 32, |f| seen.push(f.to_vec()));
        prop_assert_eq!(count, frames.len());
        prop_assert_eq!(seen, frames);
    }

    /// Произвольный буфер: разбор не паникует и не выдаёт кадр длиннее буфера.
    #[test]
    fn rndis_data_parser_is_total(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let mut total = 0usize;
        let mut min_len = usize::MAX;
        let count = proto::for_each_data_packet(&bytes, 32, |f| {
            min_len = min_len.min(f.len());
            total += f.len();
        });
        prop_assert!(count <= 32);
        prop_assert!(total <= bytes.len());
        if count > 0 {
            prop_assert!(min_len >= 14);
        }
    }

    /// Усечение батча доставляет только целые конверты: количество не растёт, а
    /// каждый доставленный кадр совпадает с полным разбором.
    #[test]
    fn rndis_data_truncation_is_prefix(
        frames in proptest::collection::vec(
            proptest::collection::vec(any::<u8>(), 14..=200), 1..=4),
        cut in 0usize..800,
    ) {
        let mut buf = std::vec::Vec::new();
        for f in &frames {
            let mut one = std::vec![0u8; DATA_HEADER + f.len()];
            let n = proto::encode_data_into(f, &mut one).expect("fits");
            buf.extend_from_slice(&one[..n]);
        }
        let cut = cut.min(buf.len());
        let mut full = std::vec::Vec::new();
        proto::for_each_data_packet(&buf, 32, |f| full.push(f.to_vec()));
        let mut partial = std::vec::Vec::new();
        proto::for_each_data_packet(&buf[..cut], 32, |f| partial.push(f.to_vec()));
        prop_assert!(partial.len() <= full.len());
        for (i, f) in partial.iter().enumerate() {
            prop_assert_eq!(f, &full[i]);
        }
    }
}

// ─── протокол: ответы управления ────────────────────────────────────────────

proptest! {
    /// Любые байты ответа разбираются без паники; если разбор вернул данные,
    /// они лежат внутри объявленной длины сообщения.
    #[test]
    fn rndis_control_parser_is_total(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        if let Some(q) = proto::parse_query_complete(&bytes) {
            prop_assert!(q.info.len() <= bytes.len());
        }
        let _ = proto::parse_initialize_complete(&bytes);
        let _ = proto::parse_simple_complete(&bytes, proto::CMPLT_SET);
        let _ = proto::parse_header(&bytes);
    }
}

#[test]
fn rndis_query_complete_rejects_out_of_bounds_info() {
    // Информационный буфер заявлен за концом ответа.
    let mut buf = std::vec![0u8; 24];
    buf[0..4].copy_from_slice(&proto::CMPLT_QUERY.to_le_bytes());
    buf[4..8].copy_from_slice(&24u32.to_le_bytes());
    buf[8..12].copy_from_slice(&1u32.to_le_bytes());
    buf[12..16].copy_from_slice(&proto::STATUS_SUCCESS.to_le_bytes());
    buf[16..20].copy_from_slice(&6u32.to_le_bytes());
    buf[20..24].copy_from_slice(&16u32.to_le_bytes());
    assert!(proto::parse_query_complete(&buf).is_none());

    // Корректный ответ: 6 байт MAC лежат по 8 + 16.
    let mut buf = std::vec![0u8; 30];
    buf[0..4].copy_from_slice(&proto::CMPLT_QUERY.to_le_bytes());
    buf[4..8].copy_from_slice(&30u32.to_le_bytes());
    buf[8..12].copy_from_slice(&1u32.to_le_bytes());
    buf[12..16].copy_from_slice(&proto::STATUS_SUCCESS.to_le_bytes());
    buf[16..20].copy_from_slice(&6u32.to_le_bytes());
    buf[20..24].copy_from_slice(&16u32.to_le_bytes());
    buf[24..30].copy_from_slice(&[0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    let q = proto::parse_query_complete(&buf).expect("valid query response");
    assert_eq!(q.info, &[0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
}