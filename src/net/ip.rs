//! Свой IPv4-слой: маршрутизация, фрагментация (отправка), сборка датаграмм
//! (приём) и ICMP echo.
//!
//! Заметки по устройству:
//!   * Маршрутизация: назначение внутри настроенной подсети -> ARP-разрешение
//!     самого назначения; иначе через шлюз по умолчанию; limited broadcast
//!     всегда идёт на broadcast MAC без ARP (это нужно DHCP до появления любого
//!     адреса).
//!   * Отправка: payload'ы больше MTU фрагментируются на чанки, чья байтовая
//!     длина кратна 8 (кроме последнего), как требует RFC 791; каждый фрагмент
//!     несёт один и тот же identification.
//!   * Приём: фрагментированные датаграммы собираются в ограниченных буферах
//!     (макс. 4 одновременно, время жизни 10 с); полные датаграммы диспетчеризуются
//!     в ICMP/UDP/TCP ровно один раз.
//!   * ICMP: echo-запросы отвечаются нативно; echo-ответы разрешают
//!     зарегистрированных ping-waiter'ов (`net::ping`).
//!
//! Все функции выполняются под локом стека из thread-контекста.

use alloc::vec::Vec;

use super::wire::{
    self, eth_frame, icmp_echo_build, icmp_echo_parse, icmpv6_echo_parse, ipv4_build, ipv4_parse,
    ipv6_build, ipv6_fragment_parse, ipv6_parse, IpAddr, Ipv4Addr, Ipv6Addr, Mac, ETHERTYPE_IPV4,
    ETHERTYPE_IPV6,
};
use super::Stack;

/// Ethernet MTU, анонсируемый везде (R13).
pub const MTU: usize = 1500;
/// Максимальный IP-payload на нефинальный фрагмент должен быть кратен 8 байтам.
const FRAG_UNIT: usize = 8;
/// Одновременные буферы сборки.
const REASM_CAP: usize = 4;
/// Время жизни буфера сборки (тики).
const REASM_TTL_TICKS: u64 = 10 * 1000;
/// Всего байт, которые мы когда-либо держим по всем частичным датаграммам.
const REASM_TOTAL_CAP: usize = 128 * 1024;

// ─── Сборка фрагментов ───────────────────────────────────────────────────────

struct FragBuf {
    src: IpAddr,
    dst: IpAddr,
    id: u32,
    proto: u8,
    /// Байтовое смещение -> байты фрагмента (отсортированы по смещению к завершению).
    frags: Vec<(usize, Vec<u8>)>,
    /// Ставится, когда пришёл финальный фрагмент (MF=0).
    total: Option<usize>,
    seen: u64,
}

impl FragBuf {
    fn bytes_held(&self) -> usize {
        self.frags.iter().map(|(_, f)| f.len()).sum()
    }

    fn try_complete(&self) -> Option<Vec<u8>> {
        let total = self.total?;
        // Проверка непрерывности: отсортированные фрагменты должны покрывать [0, total).
        let mut frags = self.frags.clone();
        frags.sort_by_key(|(off, _)| *off);
        let mut expect = 0usize;
        for (off, f) in &frags {
            if *off != expect {
                return None;
            }
            expect += f.len();
        }
        if expect != total {
            return None;
        }
        let mut out = Vec::with_capacity(total);
        for (_, f) in frags {
            out.extend_from_slice(&f);
        }
        Some(out)
    }
}

#[derive(Default)]
pub struct Reassembler {
    bufs: Vec<FragBuf>,
}

impl Reassembler {
    pub const fn new() -> Self {
        Reassembler { bufs: Vec::new() }
    }

    pub fn on_tick(&mut self, now: u64) {
        self.bufs.retain(|b| now < b.seen + REASM_TTL_TICKS);
    }

    fn held_total(&self) -> usize {
        self.bufs.iter().map(FragBuf::bytes_held).sum()
    }

    /// Принять один не-начальный фрагмент (или начальный фрагмент неполного набора).
    #[allow(clippy::too_many_arguments)]
    fn feed(
        &mut self,
        now: u64,
        src: IpAddr,
        dst: IpAddr,
        id: u32,
        proto: u8,
        frag_off_bytes: usize,
        more: bool,
        payload: &[u8],
    ) -> Option<Vec<u8>> {
        if self.held_total() + payload.len() > REASM_TOTAL_CAP {
            return None; // drop silently; sender retransmits or times out
        }
        let buf = match self
            .bufs
            .iter_mut()
            .find(|b| b.src == src && b.dst == dst && b.id == id && b.proto == proto)
        {
            Some(b) => b,
            None => {
                // CAP превышен -> тихо дропнуть (намеренно). Заметьте: буфер
                // создаётся, даже когда ЭТОТ фрагмент последний (MF=0): фрагменты
                // приходят в любом порядке, и отказ создать буфер здесь потерял бы
                // записанный total — датаграмма тогда никогда не завершится с
                // приходом более поздних фрагментов.
                if self.bufs.len() >= REASM_CAP {
                    return None;
                }
                self.bufs.push(FragBuf {
                    src,
                    dst,
                    id,
                    proto,
                    frags: Vec::new(),
                    total: None,
                    seen: now,
                });
                self.bufs.last_mut().unwrap()
            }
        };
        buf.seen = now;
        if !buf.frags.iter().any(|(off, _)| *off == frag_off_bytes) {
            buf.frags.push((frag_off_bytes, payload.to_vec()));
        }
        if !more {
            buf.total = Some(frag_off_bytes + payload.len());
        }
        if let Some(out) = buf.try_complete() {
            self.bufs
                .retain(|b| !(b.src == src && b.dst == dst && b.id == id && b.proto == proto));
            Some(out)
        } else {
            None
        }
    }
}

// ─── Ingress ─────────────────────────────────────────────────────────────────

/// Обработать один IP-пакет, принятый с провода (`pkt` начинается с IP-заголовка).
/// Диспетчеризует по нибблу версии; оба семейства валидируют, собирают при
/// необходимости и диспетчеризуют по протоколу.
pub(crate) fn input(st: &mut Stack, pkt: &[u8], eth_src: Mac, now: u64) {
    match pkt.first().map(|b| b >> 4) {
        Some(4) => input_v4(st, pkt, now),
        Some(6) => input_v6(st, pkt, eth_src, now),
        _ => {}
    }
}

/// Обработать один IPv6-пакет. Также обслуживает NDP (NS/NA) и Router
/// Advertisements (SLAAC): RA с autonomous on-link префиксом настраивает наш
/// глобальный адрес и маршрутизатор по умолчанию (RFC 4862).
fn input_v6(st: &mut Stack, pkt: &[u8], eth_src: Mac, now: u64) {
    let Some((h, payload)) = ipv6_parse(pkt) else {
        return;
    };

    // Принимать пакеты, адресованные нам (глобальный SLAAC-адрес или link-local),
    // плюс node-local/link-local multicast'ы, на которые опирается NDP.
    let ll = st.ll_addr6;
    let global = st.cidr6.map(|c| c.addr);
    let dst = h.dst;
    let ours_unicast = Some(dst) == global || dst == ll;
    let ours_mcast = dst == Ipv6Addr::ALL_NODES
        || global.map(|g| dst == g.solicited_node()).unwrap_or(false)
        || dst == ll.solicited_node();
    if !ours_unicast && !ours_mcast {
        return;
    }

    if h.proto == wire::PROTO_ICMPV6 {
        // Сначала NDP (своё пространство сообщений), затем RA, затем echo.
        let is_ndp = matches!(
            payload.first().copied(),
            Some(wire::ICMPV6_NEIGHBOR_SOLICIT) | Some(wire::ICMPV6_NEIGHBOR_ADVERT)
        );
        let is_ra = payload.first() == Some(&wire::ICMPV6_ROUTER_ADVERT);
        if is_ndp || is_ra {
            // Anti-off-link-spoof проверки RFC 4861: ND/RA-сообщения валидны
            // только с hop limit 255 (у forward'нутого пакета он уменьшен) и
            // on-link (link-local или unspecified для DAD solicitation)
            // источником. Off-link атакующий не может подделать ни то, ни другое.
            if h.hop_limit != 255 {
                return;
            }
            if is_ra {
                // Источники RA — всегда link-local адреса маршрутизаторов.
                if !h.src.is_link_local() {
                    return;
                }
            } else if !h.src.is_unspecified() && !h.src.is_link_local() {
                return;
            }
        }
        if is_ndp {
            if let Some(p) = wire::ndp_parse(h.src, h.dst, payload) {
                // Link-layer отправитель — Ethernet-источник, а не байты из
                // IPv6-заголовка (там были next-header/hop-limit/src).
                let mac = Mac(eth_src.0);
                // (временно вынесено, чтобы занять остальной стек)
                let mut nd = core::mem::take(&mut st.nd);
                nd.input(st, p, h.src, mac, now);
                st.nd = nd;
            }
            return;
        }
        if is_ra {
            if let Some(pi) = wire::ra_parse_prefix(h.src, h.dst, payload) {
                ra_apply(st, h.src, pi);
            }
            return;
        }
    }

    // Сборка фрагментов (fragment-заголовок несёт реальный upper proto).
    let dispatch: Option<(u8, alloc::vec::Vec<u8>)> = if h.proto == wire::IPV6_EXT_FRAGMENT {
        let Some((ident, frag_off, more, rest)) = ipv6_fragment_parse(payload) else {
            return;
        };
        let upper_proto = rest_first(rest);
        st.reasm6
            .feed(
                now,
                IpAddr::V6(h.src),
                IpAddr::V6(h.dst),
                ident,
                upper_proto,
                frag_off,
                more,
                rest,
            )
            .map(|full| (upper_proto, full))
    } else {
        Some((h.proto, payload.to_vec()))
    };
    let Some((proto, data)) = dispatch else {
        return;
    };

    match proto {
        wire::PROTO_ICMPV6 => icmpv6_input(st, h.src, h.dst, &data),
        wire::PROTO_UDP => super::udp::input(st, IpAddr::V6(h.src), IpAddr::V6(h.dst), &data),
        wire::PROTO_TCP => {
            super::tcp::TcpTable::input(st, IpAddr::V6(h.src), IpAddr::V6(h.dst), &data)
        }
        _ => {}
    }
}

/// Первый байт fragment-заголовка — NEXT HEADER вложенных данных.
fn rest_first(rest: &[u8]) -> u8 {
    rest.first().copied().unwrap_or(wire::IPV6_NO_NEXT)
}

/// Применить префикс Router Advertisement (SLAAC). Идентификатор интерфейса
/// берётся из нашего EUI-64 link-local адреса, так что настроенный адрес стабилен
/// между загрузками.
fn ra_apply(st: &mut Stack, router: Ipv6Addr, pi: wire::PrefixInfo) {
    if !pi.on_link || !pi.autonomous || pi.valid_lifetime_secs == 0 || pi.prefix_len == 0 {
        return;
    }
    if pi.prefix_len > 64 {
        return; // IIDs are built from /64 prefixes (RFC 4862 §5.5.3)
    }
    let mut octets = [0u8; 16];
    let nbytes = (pi.prefix_len / 8) as usize;
    octets[..nbytes].copy_from_slice(&pi.prefix.0[..nbytes]);
    octets[8..].copy_from_slice(&st.ll_addr6.0[8..]);
    let addr = Ipv6Addr(octets);

    let changed = st.cidr6.map(|c| c.addr) != Some(addr);
    st.cidr6 = Some(wire::Ipv6Cidr {
        addr,
        prefix: pi.prefix_len,
    });
    st.v6_gateway = Some(router);
    if changed {
        crate::info!("net: SLAAC address {} via router {}", addr, router);
        // Непрошеный NA, чтобы соседи быстро нас узнали (RX в promiscuous, так что
        // solicited-node multicast-группе явный join не нужен).
        let adv = wire::neighbor_advert(
            addr,
            Ipv6Addr::ALL_NODES,
            Mac::BROADCAST,
            st.mac,
            addr,
            false,
        );
        let pkt = wire::ipv6_build(addr, Ipv6Addr::ALL_NODES, wire::PROTO_ICMPV6, 255, &adv);
        let frame = eth_frame(Mac::BROADCAST, st.mac, ETHERTYPE_IPV6, &pkt);
        crate::drivers::e1000::send(&frame);
    }
}

/// Обработать один ICMPv6 echo-обмен (запросы отвечаются; ответы разрешают ping).
fn icmpv6_input(st: &mut Stack, src: Ipv6Addr, dst: Ipv6Addr, msg: &[u8]) {
    match icmpv6_echo_parse(src, dst, msg) {
        Some((wire::ICMPV6_ECHO_REQUEST, ident, seq, data)) => {
            let mut body = alloc::vec::Vec::with_capacity(4 + data.len());
            body.extend_from_slice(&ident.to_be_bytes());
            body.extend_from_slice(&seq.to_be_bytes());
            body.extend_from_slice(data);
            let reply = wire::icmpv6_build(dst, src, wire::ICMPV6_ECHO_REPLY, 0, &body);
            output(
                st,
                Some(IpAddr::V6(dst)),
                IpAddr::V6(src),
                wire::PROTO_ICMPV6,
                &reply,
                crate::task::scheduler::ticks(),
            );
        }
        Some((wire::ICMPV6_ECHO_REPLY, rid, rseq, _data)) => {
            if let Some(p) = st
                .pings
                .iter_mut()
                .find(|p| !p.done && p.ident == rid && (p.seq_any || p.seq == rseq))
            {
                p.rtt = Some(crate::task::scheduler::ticks().saturating_sub(p.started));
                p.done = true;
            }
        }
        _ => {}
    }
}

/// Обработать один IPv4-пакет, принятый с провода (`pkt` начинается с IP-
/// заголовка). Валидирует, собирает при необходимости, диспетчеризует по протоколу.
fn input_v4(st: &mut Stack, pkt: &[u8], now: u64) {
    let Some((h, payload)) = ipv4_parse(pkt) else {
        return;
    };

    // Принимать только пакеты, адресованные нам (или limited broadcast — нужен,
    // пока не настроены, во время DHCP).
    let ours = match st.cidr {
        Some(c) => h.dst == c.addr || h.dst.is_broadcast(),
        None => h.dst.is_broadcast(),
    };
    if !ours {
        return;
    }

    let mf = h.flags_frag & wire::IPV4_FLAG_MF != 0;
    let off_units = h.flags_frag & 0x1FFF;
    let frag_off = (off_units as usize) * FRAG_UNIT;

    let dispatch = if mf || frag_off != 0 {
        match st.reasm.feed(
            now,
            IpAddr::V4(h.src),
            IpAddr::V4(h.dst),
            h.ident as u32,
            h.proto,
            frag_off,
            mf,
            payload,
        ) {
            Some(full) => full,
            None => return, // still incomplete / dropped
        }
    } else {
        payload.to_vec()
    };

    match h.proto {
        wire::PROTO_ICMP => icmp_input(st, h.src, h.dst, &dispatch),
        wire::PROTO_UDP => super::udp::input(st, IpAddr::V4(h.src), IpAddr::V4(h.dst), &dispatch),
        wire::PROTO_TCP => {
            super::tcp::TcpTable::input(st, IpAddr::V4(h.src), IpAddr::V4(h.dst), &dispatch)
        }
        _ => {}
    }
}

// ─── ICMP ────────────────────────────────────────────────────────────────────

fn icmp_input(st: &mut Stack, src: Ipv4Addr, dst: Ipv4Addr, msg: &[u8]) {
    match icmp_echo_parse(msg) {
        Some((wire::ICMP_ECHO_REQUEST, ident, seq, data)) => {
            // Отвечать на echo-запросы нативно (поменять тип 8 -> 0); отвечать
            // запрашивающему с адреса, на который он к нам обратился.
            let reply = icmp_echo_build(wire::ICMP_ECHO_REPLY, ident, seq, data);
            let now = crate::task::scheduler::ticks();
            output(
                st,
                Some(IpAddr::V4(dst)),
                IpAddr::V4(src),
                wire::PROTO_ICMP,
                &reply,
                now,
            );
        }
        Some((wire::ICMP_ECHO_REPLY, rid, rseq, _data)) => {
            // Разрешить подходящий ожидающий ping.
            if let Some(p) = st
                .pings
                .iter_mut()
                .find(|p| !p.done && p.ident == rid && (p.seq_any || p.seq == rseq))
            {
                p.rtt = Some(crate::task::scheduler::ticks().saturating_sub(p.started));
                p.done = true;
            }
        }
        _ => {}
    }
}

// ─── Egress ──────────────────────────────────────────────────────────────────

/// Послать один L3-payload к `dst_ip` по `proto`. Обрабатывает выбор next-hop,
/// разрешение соседа через ARP/NDP (с очередью, пока не разрешено) и IPv4-
/// фрагментацию (IPv6 никогда не фрагментируется на TX по RFC 2460 §4.5).
///
/// `src_override` позволяет DHCP отправлять с 0.0.0.0 до конфигурации; передайте
/// `None`, чтобы использовать настроенный адрес соответствующего семейства.
pub(crate) fn output(
    st: &mut Stack,
    src_override: Option<IpAddr>,
    dst_ip: IpAddr,
    proto: u8,
    payload: &[u8],
    now: u64,
) {
    match dst_ip {
        IpAddr::V4(dst) => output_v4(
            st,
            src_override.and_then(|s| s.to_v4_mapped()),
            dst,
            proto,
            payload,
            now,
        ),
        IpAddr::V6(dst) => output_v6(st, src_override_v6(src_override), dst, proto, payload, now),
    }
}

fn src_override_v6(o: Option<IpAddr>) -> Option<Ipv6Addr> {
    match o {
        Some(IpAddr::V6(v6)) => Some(v6),
        _ => None,
    }
}

fn output_v4(
    st: &mut Stack,
    src_override: Option<Ipv4Addr>,
    dst_ip: Ipv4Addr,
    proto: u8,
    payload: &[u8],
    now: u64,
) {
    let src = src_override
        .or_else(|| st.cidr.map(|c| c.addr))
        .unwrap_or(Ipv4Addr::UNSPECIFIED);

    // Выбор next-hop.
    let next_hop: Ipv4Addr = if dst_ip.is_broadcast() {
        dst_ip
    } else if let Some(c) = st.cidr {
        if dst_ip.in_subnet(c) {
            dst_ip
        } else if let Some(gw) = st.gateway {
            gw
        } else {
            return; // no route
        }
    } else {
        return; // nothing routable while unconfigured (except broadcast above)
    };

    // Фрагментировать при необходимости.
    let max_payload = MTU - wire::IPV4_HDR_MIN;
    let fragments = fragment_chunks(payload, max_payload);
    let count = fragments.len();
    let id = st.next_ip_ident;
    st.next_ip_ident = st.next_ip_ident.wrapping_add(1);
    for (i, chunk) in fragments.into_iter().enumerate() {
        let more = i + 1 < count;
        let mut flags_frag = ((chunk.offset / FRAG_UNIT) as u16) & 0x1FFF;
        if more {
            flags_frag |= wire::IPV4_FLAG_MF;
        }

        let packet = ipv4_build(src, dst_ip, proto, id, flags_frag, 64, chunk.data);
        send_v4(st, next_hop, &packet, now);
    }
}

fn output_v6(
    st: &mut Stack,
    src_override: Option<Ipv6Addr>,
    dst_ip: Ipv6Addr,
    proto: u8,
    payload: &[u8],
    _now: u64,
) {
    if payload.len() > MTU - wire::IPV6_HDR_LEN {
        return; // packet too big and IPv6 never fragments locally (RFC 2460)
    }

    // Выбор источника: link-local назначение -> link-local источник; иначе
    // глобальный SLAAC-адрес, когда он есть.
    let ll = st.ll_addr6;
    let global = st.cidr6.map(|c| c.addr);
    let src: Ipv6Addr = src_override.unwrap_or(if dst_ip.is_link_local() {
        ll
    } else {
        global.unwrap_or(ll)
    });

    // Next hop: multicast отображается прямо в его MAC; on-link/link-local цели
    // разрешаются сами; всё остальное идёт через router из RA.
    let on_link = dst_ip.is_link_local() || st.cidr6.is_some_and(|c| dst_ip.in_subnet(c));
    let next_hop: Option<Ipv6Addr> = if dst_is_multicast(dst_ip.octets()) || on_link {
        Some(dst_ip)
    } else {
        st.v6_gateway
    };
    let Some(next_hop) = next_hop else { return }; // no v6 route yet

    let packet = ipv6_build(src, dst_ip, proto, 64, payload);
    send_v6(st, next_hop, &packet);
}

const fn dst_is_multicast(octets: [u8; 16]) -> bool {
    octets[0] == 0xFF
}

struct Chunk<'a> {
    offset: usize,
    data: &'a [u8],
}

/// Разбить `payload` на чанки размера MTU, длины которых кратны 8, кроме
/// последнего. Влезающий payload даёт один чанк с нулевым смещением.
fn fragment_chunks(payload: &[u8], max_payload: usize) -> Vec<Chunk<'_>> {
    let mut out = Vec::new();
    if payload.len() <= max_payload {
        out.push(Chunk {
            offset: 0,
            data: payload,
        });
        return out;
    }
    // Округлить размер на фрагмент вниз до кратного 8.
    let unit = max_payload - max_payload % FRAG_UNIT;
    let mut off = 0usize;
    while off < payload.len() {
        let end = core::cmp::min(off + unit, payload.len());
        out.push(Chunk {
            offset: off,
            data: &payload[off..end],
        });
        off = end;
    }
    out
}

/// Отдать IPv4-пакет link-слою, разрешив MAC next-hop через ARP (broadcast-
/// назначения обходят разрешение полностью).
fn send_v4(st: &mut Stack, next_hop: Ipv4Addr, packet: &[u8], now: u64) {
    if next_hop.is_broadcast() {
        let frame = eth_frame(Mac::BROADCAST, st.mac, ETHERTYPE_IPV4, packet);
        crate::drivers::e1000::send(&frame);
        return;
    }
    let mac = match st.arp.lookup(next_hop) {
        Some(m) => m,
        None => {
            // Припарковать кадр и спросить.
            st.arp
                .enqueue_pending(next_hop, ETHERTYPE_IPV4, packet.to_vec(), now);
            st.arp.request(
                st.mac,
                st.cidr.map(|c| c.addr).unwrap_or(Ipv4Addr::UNSPECIFIED),
                next_hop,
                now,
            );
            return;
        }
    };
    let frame = eth_frame(mac, st.mac, ETHERTYPE_IPV4, packet);
    crate::drivers::e1000::send(&frame);
}

/// Отдать IPv6-пакет link-слою, разрешив MAC next-hop через NDP (multicast-
/// назначения отображаются прямо в MAC 33:33:*).
fn send_v6(st: &mut Stack, next_hop: Ipv6Addr, packet: &[u8]) {
    let dst_mac = if dst_is_multicast(next_hop.octets()) {
        wire::ipv6_multicast_mac(next_hop)
    } else {
        match st.nd.lookup(next_hop) {
            Some(m) => m,
            None => {
                st.nd
                    .enqueue_pending(next_hop, packet.to_vec(), crate::task::scheduler::ticks());
                st.nd.solicit(
                    st.mac,
                    st.ll_addr6,
                    next_hop,
                    crate::task::scheduler::ticks(),
                );
                return;
            }
        }
    };
    let frame = eth_frame(dst_mac, st.mac, ETHERTYPE_IPV6, packet);
    crate::drivers::e1000::send(&frame);
}

// ─── Ожидающие ping ──────────────────────────────────────────────────────────

/// Один ожидающий `net::ping`.
pub struct PingWaiter {
    pub ident: u16,
    pub seq: u16,
    pub started: u64,
    /// Совпасть с любым sequence number (мы шлём ровно один probe).
    pub seq_any: bool,
    pub done: bool,
    pub rtt: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_split_on_eight_byte_boundaries() {
        let payload = vec![7u8; 2000];
        let chunks = fragment_chunks(&payload, 1480);
        assert!(chunks.len() >= 2);
        let mut covered = 0usize;
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.offset, covered);
            if i + 1 < chunks.len() {
                assert_eq!(c.data.len() % FRAG_UNIT, 0);
            }
            covered += c.data.len();
        }
        assert_eq!(covered, 2000);
    }

    #[test]
    fn small_payload_is_one_fragment() {
        let payload = vec![1u8; 100];
        let chunks = fragment_chunks(&payload, 1480);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].offset, 0);
    }
}
