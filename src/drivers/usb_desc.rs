//! Чистый разбор configuration descriptor USB: HID-интерфейсы и RNDIS-пара.
//!
//! Драйверу нужны ВСЕ HID-интерфейсы конфигурации, а не только первый: композитная
//! клавиатура (например, R82HE) выставляет boot-клавиатуру на интерфейсе 0 и
//! consumer/system-управление на следующих, и привязка только первого работает для
//! печати, но в одном шаге от того, чтобы принять report-protocol vendor-коллекцию
//! за boot-клавиатуру. Этот модуль обходит весь configuration descriptor и
//! возвращает каждый HID-интерфейс с данными эндпоинтов, нужными слою xHCI;
//! политика того, какие интерфейсы привязывать, живёт в `usb_hid_report`.
//!
//! Тот же обход находит пару интерфейсов RNDIS: communication-интерфейс с
//! interrupt-IN статусом и CDC Data с bulk-IN/bulk-OUT. Устройство выбирает
//! class/subclass/protocol по-разному (RNDIS, wireless controller, CDC ACM
//! vendor), поэтому распознавание идёт по набору известных троек, а не по
//! VID:PID.
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
const DESC_CONFIG: u8 = 2;
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

// ─── RNDIS: communication + CDC Data ────────────────────────────────────────

/// Пара интерфейсов RNDIS и все эндпоинты, нужные драйверу.
///
/// `config_value` принимает SET_CONFIGURATION. `comm_interface` — интерфейс,
/// которому адресуются class-запросы SEND/GET_ENCAPSULATED_COMMAND; он же несёт
/// interrupt-IN уведомления. `data_interface` — CDC Data с bulk-парой.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RndisIface {
    /// `bConfigurationValue` конфигурации, в которой нашлась пара.
    pub config_value: u8,
    /// Номер communication-интерфейса (class 0xEF/0xE0/0x02).
    pub comm_interface: u8,
    /// Номер CDC Data интерфейса (class 0x0A).
    pub data_interface: u8,
    /// Адрес interrupt-IN эндпоинта уведомлений (установлен бит 7).
    pub status_ep_addr: u8,
    /// `wMaxPacketSize` эндпоинта уведомлений.
    pub status_max_packet: u16,
    /// `bInterval` эндпоинта уведомлений.
    pub status_interval: u8,
    /// Адрес bulk-IN эндпоинта (кадры от устройства).
    pub bulk_in_addr: u8,
    /// `wMaxPacketSize` bulk-IN.
    pub bulk_in_max_packet: u16,
    /// Адрес bulk-OUT эндпоинта (кадры к устройству).
    pub bulk_out_addr: u8,
    /// `wMaxPacketSize` bulk-OUT.
    pub bulk_out_max_packet: u16,
}

#[derive(Clone, Copy, Default)]
struct EpInfo {
    addr: u8,
    max_packet: u16,
    interval: u8,
}

#[derive(Clone, Copy)]
struct CommCandidate {
    iface: u8,
    status: EpInfo,
}

#[derive(Clone, Copy)]
struct DataCandidate {
    iface: u8,
    bulk_in: EpInfo,
    bulk_out: EpInfo,
}

/// Распознать communication-интерфейс RNDIS по тройке class/subclass/protocol.
///
/// Известные варианты одной и той же структуры: RNDIS (EF/04/01), wireless
/// controller (E0/01/03) и «ACM flavour» (02/02/FF), которым пользуются и
/// телефоны, и QEMU. CDC ECM (02/06/..) сюда намеренно не входит: у него другая
/// семантика управления, а драйвера для неё нет.
fn is_rndis_comm(class: u8, subclass: u8, protocol: u8) -> bool {
    (class == 0xEF && subclass == 0x04 && protocol == 0x01)
        || (class == 0xE0 && subclass == 0x01 && protocol == 0x03)
        || (class == 0x02 && subclass == 0x02 && protocol == 0xFF)
}

fn endpoint_info(conf: &[u8], i: usize) -> EpInfo {
    EpInfo {
        addr: conf[i + 2],
        max_packet: u16::from_le_bytes([conf[i + 4], conf[i + 5]]),
        interval: conf[i + 6],
    }
}

fn is_interrupt_in(ep: EpInfo) -> bool {
    ep.addr & 0x80 != 0 && (ep.addr & 0x0F) != 0
}

/// Обойти configuration descriptor и вернуть первую полную пару RNDIS.
///
/// Разбор тотальный, как у [`parse_hid_interfaces`]: испорченный bLength не
/// паникует и не зацикливает обход. Пара собирается только из настройки по
/// умолчанию (alt 0) и только когда у communication-интерфейса есть interrupt-IN,
/// а у data-интерфейса — обе bulk-стороны: половина пары бесполезна, и выдавать
/// её за устройство было бы хуже, чем не найти ничего.
///
/// Data-интерфейс ищется сначала как соседний по номеру (`comm + 1`, обычная
/// раскладка IAD), затем — как первый свободный. Это покрывает устройства,
/// которые нумеруют интерфейсы не подряд, не привязывая к конкретному VID:PID.
pub fn parse_rndis_interfaces(conf: &[u8]) -> Option<RndisIface> {
    if conf.len() < 9 || conf[0] < 9 || conf[1] != DESC_CONFIG {
        return None;
    }
    let config_value = conf[5];

    let mut comms: Vec<CommCandidate> = Vec::new();
    let mut datas: Vec<DataCandidate> = Vec::new();
    let mut comm: Option<CommCandidate> = None;
    let mut data: Option<DataCandidate> = None;
    let mut kind = 0u8; // 0 — не наша, 1 — communication, 2 — CDC Data

    let mut i = 0usize;
    while i + 2 <= conf.len() {
        let len = conf[i] as usize;
        if len < 2 || i + len > conf.len() {
            break;
        }
        let desc = conf[i + 1];
        if desc == DESC_INTERFACE && len >= 9 {
            // Закрыть предыдущий интерфейс, если он был нашей роли.
            if let Some(c) = comm.take() {
                if c.status.addr != 0 {
                    comms.push(c);
                }
            }
            if let Some(d) = data.take() {
                if d.bulk_in.addr != 0 && d.bulk_out.addr != 0 {
                    datas.push(d);
                }
            }
            let number = conf[i + 2];
            let alt = conf[i + 3];
            let class = conf[i + 5];
            let subclass = conf[i + 6];
            let protocol = conf[i + 7];
            kind = if alt == 0 && is_rndis_comm(class, subclass, protocol) {
                1
            } else if alt == 0 && class == 0x0A {
                2
            } else {
                0
            };
            if kind == 1 {
                comm = Some(CommCandidate {
                    iface: number,
                    status: EpInfo::default(),
                });
            } else if kind == 2 {
                data = Some(DataCandidate {
                    iface: number,
                    bulk_in: EpInfo::default(),
                    bulk_out: EpInfo::default(),
                });
            }
        } else if desc == DESC_ENDPOINT && len >= 7 {
            let ep = endpoint_info(conf, i);
            if kind == 1 {
                if let Some(c) = comm.as_mut() {
                    if c.status.addr == 0 && is_interrupt_in(ep) && ep.max_packet & 0x07FF != 0 {
                        c.status = ep;
                    }
                }
            } else if kind == 2 {
                if let Some(d) = data.as_mut() {
                    // bmAttributes, биты 0-1: 2 — bulk. Проверка направления
                    // разделяет пару, даже если устройство перепутало порядок.
                    let bulk = conf[i + 3] & 0x03 == 2;
                    if bulk && ep.addr & 0x80 != 0 && d.bulk_in.addr == 0 {
                        d.bulk_in = ep;
                    } else if bulk && ep.addr & 0x80 == 0 && d.bulk_out.addr == 0 {
                        d.bulk_out = ep;
                    }
                }
            }
        }
        // Неизвестные class-дескрипторы (CDC functional, IAD) пропускаются по
        // bLength, поэтому обход остаётся выровненным.
        i += len;
    }
    if let Some(c) = comm.take() {
        if c.status.addr != 0 {
            comms.push(c);
        }
    }
    if let Some(d) = data.take() {
        if d.bulk_in.addr != 0 && d.bulk_out.addr != 0 {
            datas.push(d);
        }
    }

    // Сначала соседняя пара (comm+1), затем первый свободный data-интерфейс.
    let mut used: Vec<bool> = Vec::new();
    used.resize(datas.len(), false);
    let mut pair: Option<(CommCandidate, usize)> = None;
    for c in comms.iter() {
        let found = datas
            .iter()
            .enumerate()
            .position(|(di, d)| !used[di] && d.iface == c.iface.wrapping_add(1));
        if let Some(di) = found {
            used[di] = true;
            pair = Some((*c, di));
            break;
        }
    }
    if pair.is_none() {
        for c in comms.iter() {
            if let Some(di) = datas.iter().enumerate().position(|(di, _)| !used[di]) {
                used[di] = true;
                pair = Some((*c, di));
                break;
            }
        }
    }
    let (c, di) = pair?;
    let d = datas.get(di)?;
    Some(RndisIface {
        config_value,
        comm_interface: c.iface,
        data_interface: d.iface,
        status_ep_addr: c.status.addr,
        status_max_packet: c.status.max_packet,
        status_interval: c.status.interval,
        bulk_in_addr: d.bulk_in.addr,
        bulk_in_max_packet: d.bulk_in.max_packet,
        bulk_out_addr: d.bulk_out.addr,
        bulk_out_max_packet: d.bulk_out.max_packet,
    })
}

// ─── Mass storage: Bulk-Only Transport (class 8/6/0x50) ─────────────────────

/// Один интерфейс USB mass storage (BOT): обе bulk-точки и их максимальные
/// пакеты.
///
/// Phase 1 обслуживает только LUN 0 и только первое полное устройство на
/// интерфейсе. `interface` нужен для class-запросов `GET_MAX_LUN` и
/// Bulk-Only Reset (`wIndex`); `config_value` — для SET_CONFIGURATION.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MscIface {
    /// `bConfigurationValue` конфигурации, в которой найден интерфейс.
    pub config_value: u8,
    /// `bInterfaceNumber` (он же `wIndex` class-запросов).
    pub interface: u8,
    /// Адрес bulk-IN эндпоинта (бит 7 установлен).
    pub bulk_in_addr: u8,
    /// `wMaxPacketSize` bulk-IN.
    pub bulk_in_max_packet: u16,
    /// Адрес bulk-OUT эндпоинта (бит 7 снят).
    pub bulk_out_addr: u8,
    /// `wMaxPacketSize` bulk-OUT.
    pub bulk_out_max_packet: u16,
}

/// Найти первый mass-storage интерфейс BOT (class 8, subclass 6, protocol
/// 0x50) с обеими bulk-точками в настройке по умолчанию (alt 0).
///
/// Разбор тотальный: испорченный bLength не паникует и не зацикливает обход,
/// а половина пары (только IN или только OUT) интерфейсом не считается —
/// устройство без data-стадии в одну сторону не может быть диском.
///
/// Multi-LUN здесь не различается: интерфейс один, LUN выбирается CBW-полем,
/// и Phase 1 его не читает (всегда 0).
pub fn parse_msc_interfaces(conf: &[u8]) -> Option<MscIface> {
    if conf.len() < 9 || conf[0] < 9 || conf[1] != DESC_CONFIG {
        return None;
    }
    let config_value = conf[5];
    let mut current = false;
    let mut in_ep: Option<(u8, u16)> = None;
    let mut out_ep: Option<(u8, u16)> = None;
    let mut current_iface = 0u8;
    let mut i = 0usize;
    while i + 2 <= conf.len() {
        let len = conf[i] as usize;
        if len < 2 || i + len > conf.len() {
            break;
        }
        let kind = conf[i + 1];
        if kind == DESC_INTERFACE && len >= 9 {
            // Закрыть предыдущий интерфейс: только полная пара становится
            // результатом, и только если это был наш класс.
            if current && in_ep.is_some() && out_ep.is_some() {
                let (in_addr, in_mps) = in_ep.unwrap();
                let (out_addr, out_mps) = out_ep.unwrap();
                return Some(MscIface {
                    config_value,
                    interface: current_iface,
                    bulk_in_addr: in_addr,
                    bulk_in_max_packet: in_mps,
                    bulk_out_addr: out_addr,
                    bulk_out_max_packet: out_mps,
                });
            }
            current_iface = conf[i + 2];
            let alternate = conf[i + 3];
            let class = conf[i + 5];
            let subclass = conf[i + 6];
            let protocol = conf[i + 7];
            current = alternate == 0 && class == 8 && subclass == 6 && protocol == 0x50;
            in_ep = None;
            out_ep = None;
        } else if kind == DESC_ENDPOINT && len >= 7 && current {
            let addr = conf[i + 2];
            let attrs = conf[i + 3];
            let mps = u16::from_le_bytes([conf[i + 4], conf[i + 5]]);
            // bmAttributes bits 0-1 == 2: bulk. Направление — бит 7 адреса.
            if attrs & 0x03 == 2 && mps & 0x07FF != 0 {
                if addr & 0x80 != 0 {
                    if in_ep.is_none() {
                        in_ep = Some((addr, mps));
                    }
                } else if out_ep.is_none() {
                    out_ep = Some((addr, mps));
                }
            }
        }
        i += len;
    }
    if current && in_ep.is_some() && out_ep.is_some() {
        let (in_addr, in_mps) = in_ep.unwrap();
        let (out_addr, out_mps) = out_ep.unwrap();
        return Some(MscIface {
            config_value,
            interface: current_iface,
            bulk_in_addr: in_addr,
            bulk_in_max_packet: in_mps,
            bulk_out_addr: out_addr,
            bulk_out_max_packet: out_mps,
        });
    }
    None
}

#[cfg(test)]
mod msc_tests {
    use super::*;

    fn config_desc(total: u16, value: u8) -> alloc::vec::Vec<u8> {
        let [lo, hi] = total.to_le_bytes();
        alloc::vec![9, 2, lo, hi, 0, value, 0x80, 0x32, 0]
    }

    fn iface_desc(
        number: u8,
        alt: u8,
        class: u8,
        subclass: u8,
        protocol: u8,
    ) -> alloc::vec::Vec<u8> {
        alloc::vec![9, 4, number, alt, 0, class, subclass, protocol, 0]
    }

    fn ep_desc(addr: u8, attrs: u8, max_packet: u16) -> alloc::vec::Vec<u8> {
        let [lo, hi] = max_packet.to_le_bytes();
        alloc::vec![7, 5, addr, attrs, lo, hi, 0]
    }

    /// QEMU `usb-storage`: один интерфейс 08/06/50, bulk-IN 0x81 и bulk-OUT
    /// 0x02, high-speed max packet 512, bConfigurationValue 1.
    fn qemu_msc_config() -> alloc::vec::Vec<u8> {
        let mut body = alloc::vec::Vec::new();
        body.extend(iface_desc(0, 0, 8, 6, 0x50));
        body.extend(ep_desc(0x81, 0x02, 512));
        body.extend(ep_desc(0x02, 0x02, 512));
        let mut conf = config_desc((9 + body.len()) as u16, 1);
        conf.extend(body);
        conf
    }

    #[test]
    fn parses_qemu_msc_interface() {
        let iface = parse_msc_interfaces(&qemu_msc_config()).unwrap();
        assert_eq!(iface.config_value, 1);
        assert_eq!(iface.interface, 0);
        assert_eq!(iface.bulk_in_addr, 0x81);
        assert_eq!(iface.bulk_out_addr, 0x02);
        assert_eq!(iface.bulk_in_max_packet, 512);
        assert_eq!(iface.bulk_out_max_packet, 512);
    }

    #[test]
    fn skips_hid_before_msc_and_alt_settings() {
        let mut body = alloc::vec::Vec::new();
        body.extend(iface_desc(0, 0, 3, 1, 1));
        body.extend(ep_desc(0x81, 0x03, 8));
        body.extend(iface_desc(1, 1, 8, 6, 0x50)); // alt 1 — не выбираем
        body.extend(ep_desc(0x82, 0x02, 64));
        body.extend(ep_desc(0x03, 0x02, 64));
        body.extend(iface_desc(2, 0, 8, 6, 0x50));
        body.extend(ep_desc(0x84, 0x02, 64));
        body.extend(ep_desc(0x05, 0x02, 64));
        let mut conf = config_desc((9 + body.len()) as u16, 3);
        conf.extend(body);
        let iface = parse_msc_interfaces(&conf).unwrap();
        assert_eq!(iface.interface, 2);
        assert_eq!(iface.bulk_in_addr, 0x84);
        assert_eq!(iface.bulk_out_addr, 0x05);
        assert_eq!(iface.config_value, 3);
    }

    #[test]
    fn half_a_pair_is_not_a_device() {
        let mut body = alloc::vec::Vec::new();
        body.extend(iface_desc(0, 0, 8, 6, 0x50));
        body.extend(ep_desc(0x81, 0x02, 64));
        // Нет bulk-OUT: интерфейс негоден.
        let mut conf = config_desc((9 + body.len()) as u16, 1);
        conf.extend(body);
        assert!(parse_msc_interfaces(&conf).is_none());
    }

    #[test]
    fn wrong_subclass_or_protocol_is_not_bot_phase1() {
        let mut body = alloc::vec::Vec::new();
        body.extend(iface_desc(0, 0, 8, 5, 0x50)); // SFF-8070, не 6
        body.extend(ep_desc(0x81, 0x02, 64));
        body.extend(ep_desc(0x02, 0x02, 64));
        let mut conf = config_desc((9 + body.len()) as u16, 1);
        conf.extend(body);
        assert!(parse_msc_interfaces(&conf).is_none());
    }

    #[test]
    fn truncated_and_hostile_descriptors_do_not_panic() {
        let mut conf = qemu_msc_config();
        conf.truncate(conf.len() - 3);
        let _ = parse_msc_interfaces(&conf);
        let mut hostile = qemu_msc_config();
        hostile[9] = 255; // bLength интерфейса за пределами буфера
        assert!(parse_msc_interfaces(&hostile).is_none());
        assert!(parse_msc_interfaces(&[]).is_none());
        assert!(parse_msc_interfaces(&[0xFFu8; 40]).is_none());
    }
}
