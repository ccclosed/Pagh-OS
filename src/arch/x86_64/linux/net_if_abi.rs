//! Чистый ABI `SIOCGIF*` для x86_64-Linux: `struct ifreq` (40 байт),
//! `sockaddr` в его union'е и номера запросов.
//!
//! Это **чистая** половина ioctl-поверхности сети: кодирование/декодирование
//! 40-байтного `ifreq` без единого обращения к ядру. Эффектный слой
//! [`super::net_if_sys::if_ioctl`] только проверяет пользовательский указатель,
//! копирует буфер и заполняет поля из `net::iface_stats()` — раскладка целиком
//! здесь, поэтому `host-tests` включают ровно тот исходник, что компилирует
//! ядро (свойство `net_if_abi`).
//!
//! Раскладка сверена с Linux UAPI (`include/uapi/linux/if.h`,
//! `include/uapi/linux/sockios.h`, `include/uapi/linux/arp.h`,
//! `include/uapi/asm-generic/socket.h`):
//!
//! ```text
//! struct ifreq {                 // sizeof = 40, alignof = 8
//!     char ifr_name[IFNAMSIZ];   // 16 байт, смещение 0
//!     union {                    // 24 байта, смещение 16
//!         struct sockaddr ifr_addr;   // 16 байт @16
//!         short           ifr_flags;  // @16
//!         int             ifr_mtu;    // @16
//!         int             ifr_ifindex;// @16
//!         struct ifmap    ifr_map;    // 24 байта
//!         ...
//!     } ifr_ifru;
//! };
//! ```
//!
//! `struct sockaddr_in` — `u16 sin_family` (native LE), `u16 sin_port`
//! (network order), `u32 sin_addr`, 8 нулей; `struct sockaddr` для
//! `SIOCGIFHWADDR` несёт `sa_family = ARPHRD_ETHER` и MAC в `sa_data[0..6]`.
#![allow(dead_code)]

/// `IFNAMSIZ`: длина `ifr_name` вместе с NUL.
pub const IFNAMSIZ: usize = 16;

/// `sizeof(struct ifreq)` на x86_64: 16 (`ifr_name`) + 24 (union).
pub const IFREQ_SIZE: usize = 40;

/// Смещение union'а `ifr_ifru` внутри `ifreq`.
pub const UNION_OFF: usize = IFNAMSIZ;

/// Смещение `ifr_flags` (`short`) внутри `ifreq`.
pub const FLAGS_OFF: usize = UNION_OFF;

/// Смещение `ifr_mtu`/`ifr_ifindex` (`int`) внутри `ifreq`.
pub const I32_OFF: usize = UNION_OFF;

/// Смещение `struct sockaddr` внутри `ifreq`.
pub const SOCKADDR_OFF: usize = UNION_OFF;

/// `sizeof(struct sockaddr_in)` — ровно столько занимает union при `SIOCGIFADDR`.
pub const SOCKADDR_IN_SIZE: usize = 16;

/// `sizeof(struct ifconf)` на x86_64: `int ifc_len` (4) + 4 байта выравнивания
/// + `char *ifc_buf` (8) = 16.
pub const IFCONF_SIZE: usize = 16;

/// Смещение `ifc_buf` внутри `struct ifconf` (после `int` + padding).
pub const IFCONF_BUF_OFF: usize = 8;

// ─── запросы (SIOC*) ────────────────────────────────────────────────────────
//
// Значения — из `linux/sockios.h`; ioctl в Linux принимает их как
// `unsigned long`, поэтому константы u64 и сравниваются с `sys_ioctl`-аргументом
// без расширений.

/// `SIOCGIFNAME`: имя интерфейса по `ifr_ifindex`.
pub const SIOCGIFNAME: u64 = 0x8910;
/// `SIOCGIFCONF`: перечисление интерфейсов в массив `ifreq` (аргумент —
/// `struct ifconf`, не `ifreq`).
pub const SIOCGIFCONF: u64 = 0x8912;
/// `SIOCGIFFLAGS`: `short ifr_flags` (IFF_*).
pub const SIOCGIFFLAGS: u64 = 0x8913;
/// `SIOCGIFADDR`: `sockaddr_in ifr_addr`.
pub const SIOCGIFADDR: u64 = 0x8915;
/// `SIOCGIFNETMASK`: `sockaddr_in ifr_netmask`.
pub const SIOCGIFNETMASK: u64 = 0x891B;
/// `SIOCGIFMTU`: `int ifr_mtu`.
pub const SIOCGIFMTU: u64 = 0x8921;
/// `SIOCGIFHWADDR`: `sockaddr ifr_hwaddr` (family = ARPHRD_ETHER).
pub const SIOCGIFHWADDR: u64 = 0x8927;
/// `SIOCGIFINDEX`: `int ifr_ifindex`.
pub const SIOCGIFINDEX: u64 = 0x8933;

/// Поддерживается ли запрос этим слоем (иначе `ENOTTY`, как раньше).
pub fn is_ifreq(request: u64) -> bool {
    matches!(
        request,
        SIOCGIFNAME
            | SIOCGIFCONF
            | SIOCGIFFLAGS
            | SIOCGIFADDR
            | SIOCGIFNETMASK
            | SIOCGIFMTU
            | SIOCGIFHWADDR
            | SIOCGIFINDEX
    )
}

// ─── флаги интерфейса (IFF_*) ───────────────────────────────────────────────

/// Интерфейс административно поднят.
pub const IFF_UP: i16 = 0x1;
/// Поддерживает broadcast.
pub const IFF_BROADCAST: i16 = 0x2;
/// Линк поднят (carrier).
pub const IFF_RUNNING: i16 = 0x40;
/// Поддерживает multicast.
pub const IFF_MULTICAST: i16 = 0x1000;

/// `ARPHRD_ETHER` — `sa_family` у `SIOCGIFHWADDR` для Ethernet.
pub const ARPHRD_ETHER: u16 = 1;

/// `AF_INET` — `sin_family` у `SIOCGIFADDR`/`SIOCGIFNETMASK`.
pub const AF_INET: u16 = 2;

// ─── статические проверки раскладки ─────────────────────────────────────────
//
// `repr(C)`-зеркала выравнивают ровно так же, как C на x86_64; `assert!` в
// `const`-контексте падает на этапе компиляции, если UAPI-числа разойдутся с
// тем, что кодируют функции ниже.

#[repr(C)]
struct IfreqLayout {
    ifr_name: [u8; IFNAMSIZ],
    ifr_ifru: [u8; IFREQ_SIZE - IFNAMSIZ],
}

#[repr(C)]
struct IfconfLayout {
    ifc_len: i32,
    // Явный padding: `char *` в C выравнивается на 8, а `[u8; 8]` — на 1,
    // поэтому без этого поля repr(C) положил бы указатель на смещение 4.
    _pad: [u8; IFCONF_BUF_OFF - 4],
    ifc_buf: [u8; 8],
}

#[repr(C)]
struct SockaddrInLayout {
    sin_family: u16,
    sin_port: u16,
    sin_addr: [u8; 4],
    sin_zero: [u8; 8],
}

const _: () = assert!(core::mem::size_of::<IfreqLayout>() == IFREQ_SIZE);
const _: () = assert!(core::mem::offset_of!(IfreqLayout, ifr_ifru) == UNION_OFF);
const _: () = assert!(core::mem::size_of::<SockaddrInLayout>() == SOCKADDR_IN_SIZE);
const _: () = assert!(core::mem::offset_of!(SockaddrInLayout, sin_addr) == 4);
const _: () = assert!(core::mem::offset_of!(SockaddrInLayout, sin_zero) == 8);
const _: () = assert!(core::mem::size_of::<IfconfLayout>() == IFCONF_SIZE);
const _: () = assert!(core::mem::offset_of!(IfconfLayout, ifc_buf) == IFCONF_BUF_OFF);

// ─── ifr_name ───────────────────────────────────────────────────────────────

/// Длина имени в `ifr_name`: до первого NUL, иначе весь `IFNAMSIZ`.
///
/// Linux заполняет имя через `strscpy`, поэтому имя короче 16 байт всегда
/// NUL-terminated; «16 байт без NUL» — легальное (хоть и невозможное для
/// настоящего имени) состояние буфера, которое нельзя считать ошибкой чтения.
pub fn ifname_len(req: &[u8; IFREQ_SIZE]) -> usize {
    req[..IFNAMSIZ]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(IFNAMSIZ)
}

/// Имя интерфейса из `ifr_name` или `None`, если это не UTF-8.
///
/// Ядро сравнивает имена побайтово; `None` здесь значит «имени, которое можно
/// сравнить со строкой, нет» — вызывающий отвечает `ENODEV`, а не паникует.
pub fn read_ifname(req: &[u8; IFREQ_SIZE]) -> Option<&str> {
    core::str::from_utf8(&req[..ifname_len(req)]).ok()
}

/// Записать имя интерфейса в `ifr_name` (остаток — нули).
///
/// Имена длиннее `IFNAMSIZ - 1` обрезаются, как `strscpy`: ядро не выпускает
/// имён из 16 байт без NUL, и обрезка — единственный способ остаться
/// NUL-terminated.
pub fn write_ifname(req: &mut [u8; IFREQ_SIZE], name: &str) {
    req[..IFNAMSIZ].fill(0);
    let n = name.len().min(IFNAMSIZ - 1);
    req[..n].copy_from_slice(&name.as_bytes()[..n]);
}

// ─── скалярные поля union'а ─────────────────────────────────────────────────

/// Прочитать `short` по смещению union'а (`ifr_flags`).
pub fn read_i16(req: &[u8; IFREQ_SIZE], off: usize) -> i16 {
    i16::from_ne_bytes([req[off], req[off + 1]])
}

/// Записать `short` по смещению union'а.
pub fn write_i16(req: &mut [u8; IFREQ_SIZE], off: usize, value: i16) {
    req[off..off + 2].copy_from_slice(&value.to_ne_bytes());
}

/// Прочитать `int` по смещению union'а (`ifr_mtu`/`ifr_ifindex`).
pub fn read_i32(req: &[u8; IFREQ_SIZE], off: usize) -> i32 {
    i32::from_ne_bytes([req[off], req[off + 1], req[off + 2], req[off + 3]])
}

/// Записать `int` по смещению union'а.
pub fn write_i32(req: &mut [u8; IFREQ_SIZE], off: usize, value: i32) {
    req[off..off + 4].copy_from_slice(&value.to_ne_bytes());
}

// ─── struct ifconf (SIOCGIFCONF) ────────────────────────────────────────────

/// Прочитать `ifc_len` из `struct ifconf` (смещение 0).
pub fn read_ifconf_len(conf: &[u8; IFCONF_SIZE]) -> i32 {
    i32::from_ne_bytes([conf[0], conf[1], conf[2], conf[3]])
}

/// Записать `ifc_len` (сколько байт реально занял массив `ifreq`).
pub fn write_ifconf_len(conf: &mut [u8; IFCONF_SIZE], len: i32) {
    conf[..4].copy_from_slice(&len.to_ne_bytes());
}

/// Прочитать пользовательский указатель `ifc_buf` (смещение 8).
pub fn read_ifconf_buf(conf: &[u8; IFCONF_SIZE]) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&conf[IFCONF_BUF_OFF..IFCONF_BUF_OFF + 8]);
    u64::from_ne_bytes(raw)
}

/// Записать `ifc_buf` (нужно тестам и полному round-trip'у структуры).
pub fn write_ifconf_buf(conf: &mut [u8; IFCONF_SIZE], buf: u64) {
    conf[IFCONF_BUF_OFF..IFCONF_BUF_OFF + 8].copy_from_slice(&buf.to_ne_bytes());
}

// ─── sockaddr ───────────────────────────────────────────────────────────────

/// `struct sockaddr_in` для IPv4: family `AF_INET`, порт в network order,
/// адрес, 8 нулей.
///
/// `port` у `SIOCGIFADDR`/`SIOCGIFNETMASK` всегда 0 (Linux отдаёт `sin_port`
/// как есть из конфигурации, а у интерфейса порта нет); параметр оставлен,
/// чтобы функция была полной и host-свойство могло проверить порядок байт.
pub fn sockaddr_in(addr: [u8; 4], port: u16) -> [u8; SOCKADDR_IN_SIZE] {
    let mut out = [0u8; SOCKADDR_IN_SIZE];
    out[0..2].copy_from_slice(&AF_INET.to_ne_bytes());
    out[2..4].copy_from_slice(&port.to_be_bytes());
    out[4..8].copy_from_slice(&addr);
    // out[8..16] остаются нулями (sin_zero).
    out
}

/// `struct sockaddr` для `SIOCGIFHWADDR`: `sa_family = ARPHRD_ETHER`,
/// `sa_data[0..6] = MAC`, `sa_data[6..14]` — нули.
pub fn sockaddr_hwaddr(mac: [u8; 6]) -> [u8; SOCKADDR_IN_SIZE] {
    let mut out = [0u8; SOCKADDR_IN_SIZE];
    out[0..2].copy_from_slice(&ARPHRD_ETHER.to_ne_bytes());
    out[2..8].copy_from_slice(&mac);
    out
}
