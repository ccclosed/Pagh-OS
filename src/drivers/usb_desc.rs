//! Чистый разбор configuration descriptor USB для HID-интерфейсов.
//!
//! Драйверу нужны ВСЕ HID-интерфейсы конфигурации, а не только первый: композитная
//! клавиатура (например, R82HE) выставляет boot-клавиатуру на интерфейсе 0 и
//! consumer/system-управление на следующих, и привязка только первого работает для
//! печати, но в одном шаге от того, чтобы принять report-protocol vendor-коллекцию
//! за boot-клавиатуру. Этот модуль обходит весь configuration descriptor и
//! возвращает каждый HID-интерфейс с данными эндпоинтов, нужными слою xHCI;
//! политика того, какие интерфейсы привязывать, живёт в `usb_hid_report`.
//!
//! Только `alloc`, без зависимостей, поэтому host property-тесты включают точный
//! исходник, который компилирует ядро (`host-tests/src/lib.rs`), и могут подавать
//! испорченные/усечённые дескрипторы, которых не пришлёт реальное устройство.

use alloc::vec::Vec;

/// Один HID-интерфейс и его первый interrupt-IN эндпоинт.
///
/// Возвращается только настройка по умолчанию (`bAlternateSetting == 0`): драйвер
/// никогда не выбирает альтернативную настройку, поэтому сбор эндпоинтов alt-1
/// конфигурировал бы то, что устройство сейчас не выставляет.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HidIface {
    /// `bConfigurationValue` из configuration descriptor (его принимает
    /// SET_CONFIGURATION).
    pub config_value: u8,
    /// `bInterfaceNumber`.
    pub interface: u8,
    /// `bInterfaceSubClass` (1 — boot-интерфейс).
    pub subclass: u8,
    /// `bInterfaceProtocol` (0 — нет, 1 — клавиатура, 2 — мышь).
    pub protocol: u8,
    /// Адрес эндпоинта; установленный бит 7 означает IN.
    pub ep_addr: u8,
    /// `wMaxPacketSize` ровно как в endpoint descriptor (биты 11-12 high-speed
    /// значения — множитель, а не размер в байтах).
    pub max_packet: u16,
    /// `bInterval` (интервал опроса; xHCI интерпретирует его по скорости).
    pub interval: u8,
}

/// Значения типов дескрипторов, используемые здесь (USB 2.0 §9.6).
const DESC_INTERFACE: u8 = 4;
const DESC_ENDPOINT: u8 = 5;

/// Обойти configuration descriptor и вернуть каждый HID-интерфейс (class 3) с
/// interrupt-IN эндпоинтом.
///
/// Разбор намеренно тотальный: усечённый или лживый дескриптор не может вызвать
/// panic или зациклиться (каждый шаг ограничен срезом), он лишь вернёт меньше
/// интерфейсов. HID-интерфейс, чей эндпоинт не interrupt-IN, пропускается:
/// interrupt-OUT-only интерфейс не может давать ввод.
pub fn parse_hid_interfaces(conf: &[u8]) -> Vec<HidIface> {
    let mut out = Vec::new();
    let Some(&config_value) = conf.get(5) else {
        return out;
    };
    let mut i = 0usize;
    // (bInterfaceNumber, bInterfaceSubClass, bInterfaceProtocol) HID-интерфейса,
    // которому принадлежит следующий endpoint descriptor, если это HID-интерфейс в
    // настройке по умолчанию.
    let mut current: Option<(u8, u8, u8)> = None;
    // Используется только первый interrupt-IN эндпоинт интерфейса; второй относится к
    // раскладке, которую драйвер не опрашивает.
    let mut have_ep = false;
    while i + 2 <= conf.len() {
        let len = conf[i] as usize;
        if len < 2 || i + len > conf.len() {
            break;
        }
        let kind = conf[i + 1];
        if kind == DESC_INTERFACE && len >= 9 {
            let number = conf[i + 2];
            let alternate = conf[i + 3];
            let class = conf[i + 5];
            let subclass = conf[i + 6];
            let protocol = conf[i + 7];
            current = if class == 3 && alternate == 0 {
                Some((number, subclass, protocol))
            } else {
                None
            };
            have_ep = false;
        } else if kind == DESC_ENDPOINT && len >= 7 {
            if let Some((number, subclass, protocol)) = current {
                let addr = conf[i + 2];
                let attrs = conf[i + 3];
                // Биты 0-1 типа передачи: 3 — interrupt; бит 7 — направление IN.
                if !have_ep && attrs & 0x03 == 3 && addr & 0x80 != 0 {
                    have_ep = true;
                    out.push(HidIface {
                        config_value,
                        interface: number,
                        subclass,
                        protocol,
                        ep_addr: addr,
                        max_packet: u16::from_le_bytes([conf[i + 4], conf[i + 5]]),
                        interval: conf[i + 6],
                    });
                }
            }
        }
        // Неизвестные/HID class-дескрипторы пропускаются по своему bLength — так обход
        // остаётся выровненным.
        i += len;
    }
    out
}
