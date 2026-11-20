//! Реальные сетевые данные ядра для панели и флайаута «Сети».
//!
//! Этап K дал userspace три источника (Pagh-OS 2.25.0):
//! `/proc/net/dev` — байтовые и пакетные счётчики интерфейса, `SIOCGIFADDR` /
//! `SIOCGIFNETMASK` / `SIOCGIFFLAGS` / `SIOCGIFHWADDR` / `SIOCGIFMTU` — адрес,
//! маску, флаги (`IFF_UP`/`IFF_RUNNING`), MAC и MTU. Ядро несёт один интерфейс
//! с именем `eth0` (`net::IFACE_NAME`), поэтому `read()` читает именно его; нет
//! строки в `/proc/net/dev` — канала нет, и это честный `None`, а не «онлайн».
//!
//! FFI здесь ровно четыре вызова (`socket`, `ioctl`, `close` и чтение файла
//! через `std::fs`), по образцу `fbdev.rs`: без libc-крейта, чтобы сохранить
//! нулевые зависимости и static-musl сборку.
//!
//! Скорости — не поле ядра: `rates()` считает дельту счётчиков по времени между
//! сэмплами. Отрицательная дельта (счётчик сбросился с перезагрузкой канала)
//! даёт ноль, а не гигантское число.

use std::ffi::{c_int, c_ulong};
use std::net::Ipv4Addr;
use std::time::Instant;

/// Имя интерфейса ядра (`net::IFACE_NAME`): один канал, e1000 или RNDIS.
pub const IFACE: &str = "eth0";

/// `SIOCGIFADDR` (Linux UAPI): адрес интерфейса.
pub const SIOCGIFADDR: c_ulong = 0x8915;
/// `SIOCGIFFLAGS` (Linux UAPI): флаги интерфейса (`IFF_UP`/`IFF_RUNNING`).
pub const SIOCGIFFLAGS: c_ulong = 0x8913;
/// `SIOCGIFNETMASK` (Linux UAPI): маска подсети.
pub const SIOCGIFNETMASK: c_ulong = 0x891b;
/// `SIOCGIFHWADDR` (Linux UAPI): аппаратный адрес.
pub const SIOCGIFHWADDR: c_ulong = 0x8927;

/// `IFF_UP`: интерфейс административно поднят.
pub const IFF_UP: i16 = 0x1;
/// `IFF_RUNNING`: линк реально есть (e1000 STATUS.LU / RNDIS ready).
pub const IFF_RUNNING: i16 = 0x40;

const IFNAMSIZ: usize = 16;
const IFREQ_SIZE: usize = 40;
/// Смещение `union ifr_ifru` внутри `struct ifreq`.
const IFREQ_DATA: usize = IFNAMSIZ;
const AF_INET: c_int = 2;
const SOCK_DGRAM: c_int = 2;

extern "C" {
    fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
    // Та же variadic-подпись, что в `fbdev.rs`: два объявления `ioctl` с
    // разными сигнатурами дают clashing_extern_declarations.
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn close(fd: c_int) -> c_int;
}

/// Один сэмпл интерфейса: счётчики из `/proc/net/dev` плюс ioctl-факты.
///
/// `ip`/`prefix`/`mac` — `Option`, потому что честное «адреса ещё нет» (DHCP не
/// завершился) отличается от `0.0.0.0`; `mac` отсутствует, если ioctl не отвечает.
#[derive(Clone, Debug)]
pub struct NetInfo {
    pub iface: String,
    pub ip: Option<Ipv4Addr>,
    pub prefix: Option<u8>,
    pub mac: Option<[u8; 6]>,
    /// `IFF_UP`: интерфейс поднят (в текущем ядре всегда, down/up не реализован).
    pub up: bool,
    /// `IFF_RUNNING`: линк живой (e1000 `STATUS.LU`, RNDIS ready).
    pub running: bool,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_packets: u64,
    pub tx_packets: u64,
    /// Байт в секунду из [`rates`]; `read()` оставляет нули.
    pub rx_rate: u64,
    pub tx_rate: u64,
    /// Момент сэмпла; в сравнении (см. `PartialEq`) не участвует.
    pub at: Instant,
}

impl PartialEq for NetInfo {
    /// Сравнение «то же, что нарисовано»: момент сэмпла и сырые счётчики
    /// исключены, иначе `Instant` в ключе давал бы damage на каждом тике.
    fn eq(&self, other: &Self) -> bool {
        self.iface == other.iface
            && self.ip == other.ip
            && self.prefix == other.prefix
            && self.mac == other.mac
            && self.up == other.up
            && self.running == other.running
            && self.rx_rate == other.rx_rate
            && self.tx_rate == other.tx_rate
    }
}

/// Строка `/proc/net/dev` для одного интерфейса (чистая, для unit-тестов).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetDevRow {
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
}

/// Разобрать `/proc/net/dev` и вернуть строку `iface`.
///
/// Формат Linux: две шапки, затем `  eth0: rx_bytes rx_packets rx_errs ... tx_bytes
/// tx_packets ...` — 16 числовых полей; ядро Pagh печатает ровно столько же
/// (K1 `format_net_dev`). Строки без `:` и шапки пропускаются; меньше 16 полей —
/// строка битая и не принимается.
pub fn parse_net_dev(text: &str, iface: &str) -> Option<NetDevRow> {
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name.trim() != iface {
            continue;
        }
        let fields: Vec<u64> = rest
            .split_whitespace()
            .map(|field| field.parse::<u64>().ok())
            .collect::<Option<_>>()?;
        if fields.len() < 16 {
            return None;
        }
        return Some(NetDevRow {
            rx_bytes: fields[0],
            rx_packets: fields[1],
            tx_bytes: fields[8],
            tx_packets: fields[9],
        });
    }
    None
}

/// Прирост в байтах в секунду между двумя сэмплами.
///
/// Интервал короче миллисекунды и «обратный» ход времени дают `(0, 0)`: делить
/// на почти ноль бессмысленно, а сброс счётчиков — не повод показать скачок.
pub fn rates(prev: &NetInfo, cur: &NetInfo) -> (u64, u64) {
    let Some(elapsed) = cur.at.checked_duration_since(prev.at) else {
        return (0, 0);
    };
    let nanos = elapsed.as_nanos();
    if nanos < 1_000_000 {
        return (0, 0);
    }
    let per_second = |delta: u64| -> u64 {
        ((delta as u128 * 1_000_000_000) / nanos).min(u64::MAX as u128) as u64
    };
    (
        per_second(cur.rx_bytes.saturating_sub(prev.rx_bytes)),
        per_second(cur.tx_bytes.saturating_sub(prev.tx_bytes)),
    )
}

/// Человеческий вид скорости: `Б/с`, `КБ/с`, `МБ/с` (одна цифра после точки).
pub fn format_rate(bytes_per_sec: u64) -> String {
    const KIB: u64 = 1024;
    if bytes_per_sec < KIB {
        format!("{bytes_per_sec} Б/с")
    } else if bytes_per_sec < KIB * KIB {
        format!("{:.1} КБ/с", bytes_per_sec as f64 / KIB as f64)
    } else {
        format!("{:.1} МБ/с", bytes_per_sec as f64 / (KIB * KIB) as f64)
    }
}

/// Прочитать живое состояние интерфейса.
///
/// `None` — данных нет: `/proc/net/dev` недоступен или в нём нет строки `eth0`
/// (канал не перечислен). Ошибки ioctl не фатальны: счётчики остаются реальными,
/// а адрес/MAC/флаги — отсутствующими.
pub fn read() -> Option<NetInfo> {
    let text = std::fs::read_to_string("/proc/net/dev").ok()?;
    let row = parse_net_dev(&text, IFACE)?;
    let mut info = NetInfo {
        iface: IFACE.to_string(),
        ip: None,
        prefix: None,
        mac: None,
        up: false,
        running: false,
        rx_bytes: row.rx_bytes,
        tx_bytes: row.tx_bytes,
        rx_packets: row.rx_packets,
        tx_packets: row.tx_packets,
        rx_rate: 0,
        tx_rate: 0,
        at: Instant::now(),
    };
    // SAFETY: socket(2) — обычный системный вызов, аргументы-константы; fd
    // проверяется на отрицательный и закрывается ниже ровно один раз.
    let fd = unsafe { socket(AF_INET, SOCK_DGRAM, 0) };
    if fd < 0 {
        return Some(info);
    }
    let mut ifr = ifreq(IFACE);
    // SAFETY: `ifr` — живой [u8; 40] на стеке, ядро пишет не больше ifreq;
    // запрос и дескриптор проверены, указатель валиден на время вызова.
    if unsafe { ioctl(fd, SIOCGIFFLAGS, ifr.as_mut_ptr()) } == 0 {
        info.up = flags(&ifr) & IFF_UP != 0;
        info.running = flags(&ifr) & IFF_RUNNING != 0;
    }
    let mut ifr = ifreq(IFACE);
    // SAFETY: тот же контракт, что у SIOCGIFFLAGS: 40-байтный буфер на стеке.
    if unsafe { ioctl(fd, SIOCGIFADDR, ifr.as_mut_ptr()) } == 0 {
        info.ip = sockaddr_addr(&ifr);
    }
    let mut ifr = ifreq(IFACE);
    // SAFETY: 40-байтный буфер; SIOCGIFNETMASK пишет sockaddr_in в union.
    if unsafe { ioctl(fd, SIOCGIFNETMASK, ifr.as_mut_ptr()) } == 0 {
        info.prefix = sockaddr_addr(&ifr).map(|mask| u32::from(mask).count_ones() as u8);
    }
    let mut ifr = ifreq(IFACE);
    // SAFETY: 40-байтный буфер; SIOCGIFHWADDR пишет sockaddr (family + sa_data).
    if unsafe { ioctl(fd, SIOCGIFHWADDR, ifr.as_mut_ptr()) } == 0 {
        info.mac = sockaddr_mac(&ifr);
    }
    // SAFETY: fd получен из socket(2) выше и больше не используется.
    unsafe {
        close(fd);
    }
    Some(info)
}

/// `struct ifreq` с именем интерфейса и нулевым union: 40 байт, как в Linux UAPI.
fn ifreq(name: &str) -> [u8; IFREQ_SIZE] {
    let mut ifr = [0u8; IFREQ_SIZE];
    let bytes = name.as_bytes();
    let len = bytes.len().min(IFNAMSIZ - 1);
    ifr[..len].copy_from_slice(&bytes[..len]);
    ifr
}

/// `ifr_flags` — `short` в начале union.
fn flags(ifr: &[u8; IFREQ_SIZE]) -> i16 {
    i16::from_ne_bytes([ifr[IFREQ_DATA], ifr[IFREQ_DATA + 1]])
}

/// `sin_addr` из `sockaddr_in` внутри ifreq (`sa_family`, `sin_port`, `sin_addr`).
fn sockaddr_addr(ifr: &[u8; IFREQ_SIZE]) -> Option<Ipv4Addr> {
    let base = IFREQ_DATA + 4;
    let octets = [ifr[base], ifr[base + 1], ifr[base + 2], ifr[base + 3]];
    let addr = Ipv4Addr::from(octets);
    (!addr.is_unspecified()).then_some(addr)
}

/// `sa_data[0..6]` из `sockaddr` внутри ifreq.
fn sockaddr_mac(ifr: &[u8; IFREQ_SIZE]) -> Option<[u8; 6]> {
    let base = IFREQ_DATA + 2;
    let mac = [
        ifr[base],
        ifr[base + 1],
        ifr[base + 2],
        ifr[base + 3],
        ifr[base + 4],
        ifr[base + 5],
    ];
    (mac != [0u8; 6]).then_some(mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: &str = "Inter-|   Receive                                                |  Transmit\n\
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n\
    lo:       0       0    0    0    0     0          0         0        0       0    0    0    0     0       0          0\n\
  eth0:    1234      10    1    2    0     0          0         0      567       7    0    0    0     0       0          0\n";

    fn info(rx_bytes: u64, tx_bytes: u64, at: Instant) -> NetInfo {
        NetInfo {
            iface: IFACE.to_string(),
            ip: Some(Ipv4Addr::new(10, 0, 2, 15)),
            prefix: Some(24),
            mac: Some([0x52, 0x54, 0, 12, 0x34, 0x56]),
            up: true,
            running: true,
            rx_bytes,
            tx_bytes,
            rx_packets: 0,
            tx_packets: 0,
            rx_rate: 0,
            tx_rate: 0,
            at,
        }
    }

    #[test]
    fn parses_eth0_and_skips_loopback() {
        let row = parse_net_dev(DEV, "eth0").expect("строка eth0");
        assert_eq!(
            row,
            NetDevRow {
                rx_bytes: 1234,
                rx_packets: 10,
                tx_bytes: 567,
                tx_packets: 7,
            }
        );
        // lo существует в формате, но ядро Pagh его не печатает; парсер обязан
        // не путать строки по префиксу имени.
        assert!(parse_net_dev(DEV, "lo").is_some());
        assert!(parse_net_dev(DEV, "eth").is_none());
    }

    #[test]
    fn missing_iface_and_short_rows_are_not_data() {
        assert!(parse_net_dev(DEV, "wlan0").is_none());
        assert!(parse_net_dev("only a header\n", "eth0").is_none());
        assert!(parse_net_dev("  eth0: 1 2 3\n", "eth0").is_none());
        assert!(parse_net_dev("  eth0: 1 2 x 4\n", "eth0").is_none());
    }

    #[test]
    fn rates_scale_with_elapsed_time() {
        let start = Instant::now();
        let prev = info(1_000, 500, start);
        let cur = info(3_000, 1_500, start + std::time::Duration::from_secs(2));
        assert_eq!(rates(&prev, &cur), (1_000, 500));
    }

    #[test]
    fn rates_are_zero_without_time_or_on_counter_reset() {
        let start = Instant::now();
        let prev = info(1_000, 500, start);
        // Тот же момент: делить не на что.
        assert_eq!(rates(&prev, &info(2_000, 900, start)), (0, 0));
        // Канал перезагрузился: счётчики меньше — не показываем скачок.
        let reset = info(10, 5, start + std::time::Duration::from_secs(1));
        assert_eq!(rates(&prev, &reset), (0, 0));
        // Обратный ход времени (сэмпл «из прошлого») — тоже ноль.
        let earlier = info(9_000, 9_000, start - std::time::Duration::from_secs(1));
        assert_eq!(rates(&prev, &earlier), (0, 0));
    }

    #[test]
    fn equality_ignores_the_sample_instant() {
        let start = Instant::now();
        let mut a = info(1_000, 500, start);
        let mut b = info(1_000, 500, start + std::time::Duration::from_secs(5));
        assert_eq!(a, b, "момент сэмпла не должен давать damage");
        b.rx_rate = 12;
        assert_ne!(a, b);
        a.rx_rate = 12;
        assert_eq!(a, b);
        b.running = false;
        assert_ne!(a, b);
    }

    #[test]
    fn formats_rates_in_bytes_kib_and_mib() {
        assert_eq!(format_rate(0), "0 Б/с");
        assert_eq!(format_rate(999), "999 Б/с");
        assert_eq!(format_rate(1024), "1.0 КБ/с");
        assert_eq!(format_rate(1536), "1.5 КБ/с");
        assert_eq!(format_rate(2 * 1024 * 1024), "2.0 МБ/с");
    }

    #[test]
    fn ifreq_is_linux_uapi_sized_and_zero_padded() {
        assert_eq!(IFREQ_SIZE, 40);
        let ifr = ifreq("eth0");
        assert_eq!(&ifr[..5], b"eth0\0");
        assert!(ifr[IFNAMSIZ..].iter().all(|byte| *byte == 0));
        // sockaddr_in: family=AF_INET @16, port=0 @18, sin_addr @20.
        let mut sock = ifr;
        sock[IFREQ_DATA..IFREQ_DATA + 2].copy_from_slice(&2i16.to_ne_bytes());
        sock[IFREQ_DATA + 2..IFREQ_DATA + 4].copy_from_slice(&0u16.to_ne_bytes());
        sock[IFREQ_DATA + 4..IFREQ_DATA + 8].copy_from_slice(&[10, 0, 2, 15]);
        assert_eq!(sockaddr_addr(&sock), Some(Ipv4Addr::new(10, 0, 2, 15)));
        let mut hw = ifr;
        hw[IFREQ_DATA + 2..IFREQ_DATA + 8].copy_from_slice(&[0x52, 0x54, 0, 12, 34, 56]);
        assert_eq!(sockaddr_mac(&hw), Some([0x52, 0x54, 0, 12, 34, 56]));
        assert_eq!(
            sockaddr_mac(&ifr),
            None,
            "нулевой MAC — это отсутствие данных"
        );
    }
}
