//! Свой link-слой: Ethernet + ARP (RFC 826).
//!
//! [`ArpTable`] отвечает на «какой MAC принадлежит IPv4-адресу X?» для стека.
//! Когда ответ неизвестен, исходящие кадры паркуются в ограниченной очереди
//! ожидания, а ARP-запрос широковещательно рассылается (rate-limited до одного
//! запроса на цель в секунду); ответы заполняют кэш и сливают ожидающие кадры в
//! порядке прихода. Записи стареют; ожидающие кадры истекают.
//!
//! Всё выполняется под локом стека в thread-контексте (цикл опроса сети), так
//! что внутренний локинг не нужен.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::wire::{
    self, eth_frame, Ipv4Addr, Ipv6Addr, Mac, ETHERTYPE_ARP, ETHERTYPE_IPV6, ETH_HDR_LEN,
};
use super::Stack;

/// Ёмкость кэша. У QEMU user-net ровно один peer; у реальных LAN — единицы.
const CACHE_SIZE: usize = 16;
/// Кадры, припаркованные в ожидании разрешения.
const PENDING_CAP: usize = 32;
/// Время жизни записи кэша (тики).
const ENTRY_TTL_TICKS: u64 = 60 * 1000;
/// Сдаться на ожидающем кадре после стольких тиков.
const PENDING_TTL_TICKS: u64 = 5 * 1000;
/// Минимальный интервал между повторными запросами для одной цели.
const REQUEST_RETRY_TICKS: u64 = 1000;
/// Сколько попыток до отказа от разрешения (и его кадров).
const MAX_REQUESTS: usize = 3;

#[derive(Clone, Copy)]
struct Entry {
    ip: Ipv4Addr,
    mac: Mac,
    expires: u64,
    /// Когда это соответствие последний раз (пере)изучено — выбор LRU-жертвы.
    updated: u64,
}

struct Pending {
    dst_ip: Ipv4Addr,
    ethertype: u16,
    /// Payload кадра ПОСЛЕ Ethernet-заголовка (пакет L3).
    payload: Vec<u8>,
    enqueued: u64,
}

pub struct ArpTable {
    entries: [Option<Entry>; CACHE_SIZE],
    /// Учёт последнего запроса на неразрешённую цель: (ip, tick последнего запроса, попытки).
    inflight: Vec<(Ipv4Addr, u64, usize)>,
    pending: VecDeque<Pending>,
}

impl ArpTable {
    pub const fn new() -> Self {
        ArpTable {
            entries: [None; CACHE_SIZE],
            inflight: Vec::new(),
            pending: VecDeque::new(),
        }
    }

    pub fn lookup(&self, ip: Ipv4Addr) -> Option<Mac> {
        self.entries
            .iter()
            .find_map(|e| e.filter(|e| e.ip == ip).map(|e| e.mac))
    }

    fn insert(&mut self, ip: Ipv4Addr, mac: Mac, now: u64) {
        // Обновить на месте, если уже есть.
        for slot in self.entries.iter_mut().flatten() {
            if slot.ip == ip {
                slot.mac = mac;
                slot.expires = now + ENTRY_TTL_TICKS;
                slot.updated = now;
                return;
            }
        }
        // Иначе: предпочесть свободный слот, затем истёкший, затем вытеснить
        // наименее недавно (пере)изученную запись (LRU). Фолбэк на индекс 0
        // недостижим, пока CACHE_SIZE > 0, но держит поиск тотальным.
        let victim = self
            .entries
            .iter()
            .position(|s| s.is_none())
            .or_else(|| {
                self.entries
                    .iter()
                    .position(|s| s.map(|e| e.expires <= now).unwrap_or(false))
            })
            .or_else(|| {
                self.entries
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, s)| s.map(|e| e.updated).unwrap_or(0))
                    .map(|(i, _)| i)
            })
            .unwrap_or(0);
        self.entries[victim] = Some(Entry {
            ip,
            mac,
            expires: now + ENTRY_TTL_TICKS,
            updated: now,
        });
    }

    /// Поставить `payload` (пакет L3) в очередь на доставку `dst_ip`, как только
    /// его MAC станет известен. Возвращает false, когда очередь полна (кадр отброшен).
    pub fn enqueue_pending(
        &mut self,
        dst_ip: Ipv4Addr,
        ethertype: u16,
        payload: Vec<u8>,
        now: u64,
    ) -> bool {
        if self.pending.len() >= PENDING_CAP {
            return false;
        }
        self.pending.push_back(Pending {
            dst_ip,
            ethertype,
            payload,
            enqueued: now,
        });
        true
    }

    /// Широковещательный ARP-запрос для `ip` (rate-limited). `my_ip` может быть
    /// не задан во время DHCP (sender protocol address 0.0.0.0).
    pub fn request(&mut self, my_mac: Mac, my_ip: Ipv4Addr, ip: Ipv4Addr, now: u64) {
        match self.inflight.iter_mut().find(|(t, _, _)| *t == ip) {
            Some((_, last, tries)) => {
                if now < *last + REQUEST_RETRY_TICKS {
                    return;
                }
                *last = now;
                *tries += 1;
            }
            None => {
                self.inflight.push((ip, now, 1));
            }
        }

        let pkt = wire::arp_build(wire::ARP_REQUEST, my_mac, my_ip, Mac([0; 6]), ip);
        // Запросы широковещательные: сами никогда не проходим через разрешение.
        let frame = eth_frame(Mac::BROADCAST, my_mac, ETHERTYPE_ARP, &pkt);
        crate::net::link::send(&frame);
    }

    /// Обработать входящий ARP-пакет. На REQUEST для нашего адреса ответить нашим
    /// MAC; на REPLY на одно из НАШИХ ожидающих разрешений изучить
    /// sender_ip -> sender_mac и слить ожидающие кадры.
    ///
    /// Изучение намеренно ограничено (anti-spoofing): незапрошенные ответы и
    /// запросы о третьих лицах никогда не трогают кэш — атакующий не может
    /// переназначить произвольный IP широковещательным crafted-пакетом.
    pub fn input(&mut self, my_mac: Mac, my_ip: Option<Ipv4Addr>, pkt: &[u8], now: u64) {
        let Some(p) = wire::arp_parse(pkt) else {
            return;
        };

        match p.operation {
            wire::ARP_REQUEST => {
                // Отвечать, только когда спрашивают про НАС. Такой запрос —
                // легитимный первый контакт от запрашивающего, так что его
                // sender-соответствие можно изучить (пара sender/target
                // согласуется с запросом нашего адреса).
                let wants_us = my_ip.map(|i| i == p.target_ip).unwrap_or(false);
                if wants_us {
                    if !p.sender_ip.is_unspecified() {
                        self.insert(p.sender_ip, p.sender_mac, now);
                    }
                    let reply = wire::arp_build(
                        wire::ARP_REPLY,
                        my_mac,
                        p.target_ip,
                        p.sender_mac,
                        p.sender_ip,
                    );
                    // Unicast-ответ прямо на MAC запрашивающего.
                    let frame = eth_frame(p.sender_mac, my_mac, ETHERTYPE_ARP, &reply);
                    crate::net::link::send(&frame);
                }
            }
            wire::ARP_REPLY => {
                // Результат разрешения: принимается, только пока у НАС есть
                // ожидающий запрос на этот адрес. Тогда прекратить запросы и
                // слить припаркованные для него кадры.
                let expected = self
                    .inflight
                    .iter()
                    .any(|(t, _, _)| *t == p.sender_ip && !p.sender_ip.is_unspecified());
                if expected {
                    self.insert(p.sender_ip, p.sender_mac, now);
                    self.inflight.retain(|(t, _, _)| *t != p.sender_ip);
                    let mut ready: VecDeque<Pending> = VecDeque::new();
                    while let Some(fr) = self.pending.pop_front() {
                        if fr.dst_ip == p.sender_ip {
                            let frame = eth_frame(p.sender_mac, my_mac, fr.ethertype, &fr.payload);
                            crate::net::link::send(&frame);
                        } else {
                            ready.push_back(fr);
                        }
                    }
                    self.pending = ready;
                }
            }
            _ => {}
        }
    }

    /// Старение записей, истёкших ожидающих кадров и мёртвых попыток разрешения.
    pub fn on_tick(&mut self, now: u64) {
        for slot in self.entries.iter_mut() {
            if let Some(e) = slot {
                if e.expires <= now {
                    *slot = None;
                }
            }
        }
        self.pending
            .retain(|p| now < p.enqueued + PENDING_TTL_TICKS);
        self.inflight
            .retain(|(_, last, tries)| now < *last + PENDING_TTL_TICKS && *tries <= MAX_REQUESTS);
    }

    /// Число кадров, ждущих разрешения (диагностика/тесты).
    #[allow(dead_code)]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// Константа полных Ethernet-накладных, ре-экспортируется для вызывающих, считающих буферы.
#[allow(dead_code)]
pub const FRAME_MAX: usize = ETH_HDR_LEN + 1500;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_roundtrip_and_expiry() {
        let mut t = ArpTable::new();
        t.insert(Ipv4Addr::new(10, 0, 2, 2), Mac([1, 2, 3, 4, 5, 6]), 0);
        assert_eq!(
            t.lookup(Ipv4Addr::new(10, 0, 2, 2)),
            Some(Mac([1, 2, 3, 4, 5, 6]))
        );
        assert_eq!(t.lookup(Ipv4Addr::new(10, 0, 2, 3)), None);
        t.on_tick(ENTRY_TTL_TICKS + 1);
        assert_eq!(t.lookup(Ipv4Addr::new(10, 0, 2, 2)), None);
    }
}

// ─── Таблица соседей IPv6 (NDP) ───────────────────────────────────────────────

// Зеркалит ARP-таблицу: кэш + очередь ожидания + rate-limited solicitation'ы.
// Обмен по проводу — Neighbor Solicitation / Advertisement вместо ARP, а запросы
// идут на solicited-node multicast-адрес цели.

/// Hop limit для всех обменов NDP (RFC 4861: должен быть 255).
const ND_HOP_LIMIT: u8 = 255;

pub struct NdpTable {
    entries: [Option<(Ipv6Addr, Mac, u64)>; CACHE_SIZE],
    inflight: Vec<(Ipv6Addr, u64, usize)>,
    pending: VecDeque<(Ipv6Addr, Vec<u8>, u64)>,
}

impl Default for NdpTable {
    fn default() -> Self {
        Self::new()
    }
}

impl NdpTable {
    pub const fn new() -> Self {
        NdpTable {
            entries: [None; CACHE_SIZE],
            inflight: Vec::new(),
            pending: VecDeque::new(),
        }
    }

    pub fn lookup(&self, ip: Ipv6Addr) -> Option<Mac> {
        self.entries
            .iter()
            .find_map(|e| e.filter(|e| e.0 == ip).map(|e| e.1))
    }

    fn insert(&mut self, ip: Ipv6Addr, mac: Mac, now: u64) {
        for slot in self.entries.iter_mut().flatten() {
            if slot.0 == ip {
                slot.1 = mac;
                slot.2 = now + ENTRY_TTL_TICKS;
                return;
            }
        }
        let victim = self
            .entries
            .iter()
            .position(|s| s.is_none())
            .or_else(|| {
                self.entries
                    .iter()
                    .position(|s| s.map(|e| e.2 <= now).unwrap_or(false))
            })
            .unwrap_or(0);
        self.entries[victim] = Some((ip, mac, now + ENTRY_TTL_TICKS));
    }

    /// Поставить пакет L3 в очередь для `dst`, пока его MAC не разрешён.
    #[allow(dead_code)]
    pub fn enqueue_pending(&mut self, dst: Ipv6Addr, payload: Vec<u8>, now: u64) -> bool {
        if self.pending.len() >= PENDING_CAP {
            return false;
        }
        self.pending.push_back((dst, payload, now));
        true
    }

    /// Широковещательный Neighbor Solicitation для `ip`.
    pub fn solicit(&mut self, my_mac: Mac, my_ip: Ipv6Addr, ip: Ipv6Addr, now: u64) {
        match self.inflight.iter_mut().find(|(t, _, _)| *t == ip) {
            Some((_, last, tries)) => {
                if now < *last + REQUEST_RETRY_TICKS {
                    return;
                }
                *last = now;
                *tries += 1;
            }
            None => {
                self.inflight.push((ip, now, 1));
            }
        }
        // Полный IPv6-пакет на solicited-node multicast-адрес.
        let icmp = wire::neighbor_solicit(my_ip, my_mac, ip);
        let mcast_ip = ip.solicited_node();
        let pkt = wire::ipv6_build(my_ip, mcast_ip, wire::PROTO_ICMPV6, ND_HOP_LIMIT, &icmp);
        let frame = eth_frame(
            wire::ipv6_multicast_mac(mcast_ip),
            my_mac,
            ETHERTYPE_IPV6,
            &pkt,
        );
        crate::net::link::send(&frame);
    }

    /// Обработать входящий ICMPv6 NDP-пакет (уже проверенный по checksum).
    /// `eth_src` — отправитель на Ethernet-уровне (используется для изучения,
    /// когда пакет не несёт опции link-layer address). Solicitations на один из
    /// НАШИХ адресов отвечаются Neighbor Advertisement.
    pub fn input(
        &mut self,
        st: &mut Stack,
        p: wire::NdpPacket,
        src_ip: Ipv6Addr,
        eth_src: Mac,
        now: u64,
    ) {
        let my_mac = st.mac;
        if p.is_solicit {
            // Изучить отправителя даже из solicitation (RFC 4861 §7.2.3).
            if !src_ip.is_unspecified() {
                if let Some(mac) = p.ll_addr {
                    self.insert(src_ip, mac, now);
                }
            }
            // Отвечать, только когда спрашивают про один из НАШИХ адресов.
            let wants_us = st.cidr6.map(|c| c.addr) == Some(p.target) || st.ll_addr6 == p.target;
            if wants_us {
                let adv = wire::neighbor_advert(
                    p.target, // source = the address being asked about
                    src_ip,   // reply unicast to the asker
                    eth_src,  // their MAC (learned above or on-wire)
                    my_mac, p.target, true,
                );
                let pkt =
                    wire::ipv6_build(p.target, src_ip, wire::PROTO_ICMPV6, ND_HOP_LIMIT, &adv);
                let frame = eth_frame(eth_src, my_mac, ETHERTYPE_IPV6, &pkt);
                crate::net::link::send(&frame);
            }
        } else {
            // Advertisement: изучить target -> link-layer mapping.
            let learned = p.ll_addr.unwrap_or(eth_src);
            self.insert(p.target, learned, now);
            self.inflight.retain(|(t, _, _)| *t != p.target);
            // Слить кадры, ждущие этого соседа.
            let mut keep: VecDeque<(Ipv6Addr, Vec<u8>, u64)> = VecDeque::new();
            while let Some(fr) = self.pending.pop_front() {
                if fr.0 == p.target {
                    let frame = eth_frame(learned, my_mac, ETHERTYPE_IPV6, &fr.1);
                    crate::net::link::send(&frame);
                } else {
                    keep.push_back(fr);
                }
            }
            self.pending = keep;
        }
    }

    pub fn on_tick(&mut self, now: u64) {
        for slot in self.entries.iter_mut() {
            if let Some(e) = slot {
                if e.2 <= now {
                    *slot = None;
                }
            }
        }
        self.pending.retain(|p| now < p.2 + PENDING_TTL_TICKS);
        self.inflight
            .retain(|(_, last, tries)| now < *last + PENDING_TTL_TICKS && *tries <= MAX_REQUESTS);
    }

    #[allow(dead_code)]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}
