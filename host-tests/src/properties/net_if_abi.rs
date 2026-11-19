// Feature: ABI `ifreq`/`SIOCGIF*` (этап K2).
//
// Linux-программы узнают адрес/маску/флаги/MAC интерфейса через ioctl на
// `AF_INET`-сокете, передавая 40-байтный `struct ifreq`. Ошибка в раскладке
// (смещение union'а, порядок байт `sin_port`, тип `ifr_flags`) выглядит как
// «ifconfig показывает мусор», а не как ошибка компиляции, поэтому числа
// сверяются здесь с Linux UAPI, а не пересчитываются из кода.
//
// Чистый модуль `src/arch/x86_64/linux/net_if_abi.rs` — единственное место
// кодирования; эффективный `net_if_sys` лишь копирует буфер и заполняет поля.

use crate::net_if_abi::{
    ifname_len, is_ifreq, read_i16, read_i32, read_ifname, sockaddr_hwaddr, sockaddr_in,
    write_i16, write_i32, write_ifname, AF_INET, ARPHRD_ETHER, FLAGS_OFF, I32_OFF, IFF_BROADCAST,
    IFF_MULTICAST, IFF_RUNNING, IFF_UP, IFNAMSIZ, IFREQ_SIZE, SIOCGIFADDR, SIOCGIFFLAGS,
    SIOCGIFHWADDR, SIOCGIFINDEX, SIOCGIFMTU, SIOCGIFNAME, SIOCGIFNETMASK, SOCKADDR_IN_SIZE,
    SOCKADDR_OFF, UNION_OFF,
};
use proptest::prelude::*;

fn empty_req() -> [u8; IFREQ_SIZE] {
    [0u8; IFREQ_SIZE]
}

#[test]
fn layout_constants_match_x86_64_linux() {
    assert_eq!(IFNAMSIZ, 16);
    assert_eq!(IFREQ_SIZE, 40);
    assert_eq!(UNION_OFF, 16);
    assert_eq!(FLAGS_OFF, 16);
    assert_eq!(I32_OFF, 16);
    assert_eq!(SOCKADDR_OFF, 16);
    assert_eq!(SOCKADDR_IN_SIZE, 16);
}

#[test]
fn request_numbers_match_linux_sockios() {
    assert_eq!(SIOCGIFNAME, 0x8910);
    assert_eq!(SIOCGIFFLAGS, 0x8913);
    assert_eq!(SIOCGIFADDR, 0x8915);
    assert_eq!(SIOCGIFNETMASK, 0x891B);
    assert_eq!(SIOCGIFMTU, 0x8921);
    assert_eq!(SIOCGIFHWADDR, 0x8927);
    assert_eq!(SIOCGIFINDEX, 0x8933);
    // Значения, которые этот слой НЕ обслуживает, обязаны остаться вне
    // `is_ifreq` — иначе `ioctl` начнёт отвечать успехом на чужой запрос.
    assert!(!is_ifreq(0x8912)); // SIOCGIFCONF — отдельный этап (K3)
    assert!(!is_ifreq(0x8914)); // SIOCSIFFLAGS (down/up) — K4, не делаем
}

#[test]
fn flag_and_address_family_constants_match_linux() {
    assert_eq!(IFF_UP, 0x1);
    assert_eq!(IFF_BROADCAST, 0x2);
    assert_eq!(IFF_RUNNING, 0x40);
    assert_eq!(IFF_MULTICAST, 0x1000);
    assert_eq!(ARPHRD_ETHER, 1);
    assert_eq!(AF_INET, 2);
}

#[test]
fn is_ifreq_covers_exactly_the_implemented_requests() {
    for request in [
        SIOCGIFNAME,
        SIOCGIFFLAGS,
        SIOCGIFADDR,
        SIOCGIFNETMASK,
        SIOCGIFMTU,
        SIOCGIFHWADDR,
        SIOCGIFINDEX,
    ] {
        assert!(is_ifreq(request), "request {request:#x} must be served");
    }
    for request in [0u64, 1, 0x5413, 0x8912, 0x8914, 0x8916, 0x8928, u64::MAX] {
        assert!(
            !is_ifreq(request),
            "request {request:#x} must stay ENOTTY"
        );
    }
}

#[test]
fn ifname_is_nul_terminated_and_padded() {
    let mut req = empty_req();
    write_ifname(&mut req, "eth0");
    assert_eq!(&req[..IFNAMSIZ], b"eth0\0\0\0\0\0\0\0\0\0\0\0\0");
    assert_eq!(read_ifname(&req), Some("eth0"));
    assert_eq!(ifname_len(&req), 4);
    assert_eq!(req[IFNAMSIZ], 0, "union stays untouched by write_ifname");
}

#[test]
fn fifteen_byte_name_fills_ifnamsiz_minus_one() {
    let name = "abcdefghijklmno"; // 15 символов — максимум Linux ifname
    let mut req = empty_req();
    write_ifname(&mut req, name);
    assert_eq!(req[IFNAMSIZ - 1], 0);
    assert_eq!(read_ifname(&req), Some(name));
    assert_eq!(ifname_len(&req), 15);
}

#[test]
fn overlong_name_is_truncated_not_overflowed() {
    let mut req = empty_req();
    write_ifname(&mut req, "abcdefghijklmnopqrstuvwxyz");
    assert_eq!(read_ifname(&req), Some("abcdefghijklmno"));
    assert_eq!(req[IFNAMSIZ - 1], 0);
}

#[test]
fn name_without_nul_reads_as_full_sixteen_bytes() {
    let mut req = empty_req();
    req[..IFNAMSIZ].copy_from_slice(b"0123456789abcdef");
    assert_eq!(ifname_len(&req), IFNAMSIZ);
    assert_eq!(read_ifname(&req), Some("0123456789abcdef"));
}

#[test]
fn invalid_utf8_name_is_none_not_panic() {
    let mut req = empty_req();
    req[0] = 0xff;
    req[1] = 0xfe;
    assert_eq!(read_ifname(&req), None);
}

#[test]
fn embedded_nul_ends_the_name() {
    let mut req = empty_req();
    req[..6].copy_from_slice(b"et\0h0\0");
    assert_eq!(ifname_len(&req), 2);
    assert_eq!(read_ifname(&req), Some("et"));
}

#[test]
fn sockaddr_in_layout_and_byte_order() {
    let sa = sockaddr_in([10, 0, 2, 15], 1234);
    // sin_family — native (x86_64 LE): AF_INET = 2 -> 02 00.
    assert_eq!(&sa[..2], &[0x02, 0x00]);
    // sin_port — network order: на LE 1234 -> 04 d2.
    assert_eq!(&sa[2..4], &[0x04, 0xd2]);
    assert_eq!(&sa[4..8], &[10, 0, 2, 15]);
    assert_eq!(&sa[8..], &[0u8; 8]);
    assert_eq!(sockaddr_in([255, 255, 255, 0], 0)[4..8], [255, 255, 255, 0]);
}

#[test]
fn sockaddr_hwaddr_layout() {
    let sa = sockaddr_hwaddr([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    assert_eq!(&sa[..2], &[0x01, 0x00]); // ARPHRD_ETHER = 1, native LE
    assert_eq!(&sa[2..8], &[0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    assert_eq!(&sa[8..], &[0u8; 8]);
}

#[test]
fn i16_and_i32_round_trip_at_union_offset() {
    let mut req = empty_req();
    write_i16(&mut req, FLAGS_OFF, 0x1043u16 as i16);
    assert_eq!(read_i16(&req, FLAGS_OFF), 0x1043u16 as i16);
    write_i32(&mut req, I32_OFF, 1500);
    assert_eq!(read_i32(&req, I32_OFF), 1500);
    write_i32(&mut req, I32_OFF, -1);
    assert_eq!(read_i32(&req, I32_OFF), -1);
    // Запись union'а не трогает имя.
    assert_eq!(&req[..IFNAMSIZ], &[0u8; IFNAMSIZ]);
}

proptest! {
    /// Имя без NUL длиной 0..=24: `write_ifname` обрезает до 15 байт и всегда
    /// оставляет NUL; чтение возвращает ровно записанный префикс.
    #[test]
    fn ifname_round_trip_is_truncation_to_fifteen(
        name in "[A-Za-z0-9_.-]{0,24}"
    ) {
        let mut req = empty_req();
        write_ifname(&mut req, &name);
        let expected = &name[..name.len().min(IFNAMSIZ - 1)];
        prop_assert_eq!(read_ifname(&req), Some(expected));
        prop_assert_eq!(ifname_len(&req), expected.len());
        prop_assert_eq!(req[IFNAMSIZ - 1], 0);
    }

    /// `sockaddr_in` — биекция: адрес в байтах 4..8, family/port фиксированы.
    #[test]
    fn sockaddr_in_round_trips_arbitrary_addresses(
        addr in proptest::array::uniform4(0u8..=255),
        port in any::<u16>(),
    ) {
        let sa = sockaddr_in(addr, port);
        prop_assert_eq!(&sa[..2], &AF_INET.to_ne_bytes());
        prop_assert_eq!(&sa[2..4], &port.to_be_bytes());
        prop_assert_eq!(&sa[4..8], &addr);
        prop_assert_eq!(&sa[8..], &[0u8; 8]);
    }

    /// `sockaddr` для MAC: family Ethernet и ровно шесть байт адреса.
    #[test]
    fn sockaddr_hwaddr_round_trips_arbitrary_macs(
        mac in proptest::array::uniform6(0u8..=255),
    ) {
        let sa = sockaddr_hwaddr(mac);
        prop_assert_eq!(&sa[..2], &ARPHRD_ETHER.to_ne_bytes());
        prop_assert_eq!(&sa[2..8], &mac);
        prop_assert_eq!(&sa[8..], &[0u8; 8]);
    }

    /// `int`/`short` union-поля переживают любой битовый шаблон.
    #[test]
    fn scalar_union_fields_round_trip(v16: i16, v32: i32, off in 16usize..=36) {
        let mut req = empty_req();
        write_i16(&mut req, off, v16);
        prop_assert_eq!(read_i16(&req, off), v16);
        // int требует 4 байта: держим смещение в пределах union'а.
        if off <= IFREQ_SIZE - 4 {
            write_i32(&mut req, off, v32);
            prop_assert_eq!(read_i32(&req, off), v32);
        }
    }
}
