//! Own IPv4 layer: routing, fragmentation (send), datagram reassembly
//! (receive), and ICMP echo.
//!
//! Design notes:
//!   * Routing: destination inside the configured subnet -> ARP-resolve the
//!     destination itself; otherwise via the default gateway; limited
//!     broadcast always goes to the broadcast MAC without ARP (this is what
//!     DHCP needs before any address is configured).
//!   * Sending: payloads larger than the MTU are fragmented into chunks whose
//!     byte length is a multiple of 8 (except the final one), as RFC 791
//!     requires; every fragment carries the same identification.
//!   * Receiving: fragmented datagrams are reassembled in bounded buffers
//!     (max 4 concurrent, 10 s lifetime); complete datagrams are dispatched to
//!     ICMP/UDP/TCP exactly once.
//!   * ICMP: echo requests are answered natively; echo replies resolve
//!     registered ping waiters (`net::ping`).
//!
//! All functions run under the stack lock from thread context.

use alloc::vec::Vec;

use super::wire::{
    self, eth_frame, icmp_echo_build, icmp_echo_parse, icmpv6_echo_parse, ipv4_build, ipv4_parse,
    ipv6_build, ipv6_fragment_parse, ipv6_parse, IpAddr, Ipv4Addr, Ipv6Addr, Mac, ETHERTYPE_IPV4,
    ETHERTYPE_IPV6,
};
use super::Stack;

/// Ethernet MTU advertised everywhere (R13).
pub const MTU: usize = 1500;
/// Maximum IP payload per non-final fragment must be a multiple of 8 bytes.
const FRAG_UNIT: usize = 8;
/// Concurrent reassembly buffers.
const REASM_CAP: usize = 4;
/// Reassembly buffer lifetime (ticks).
const REASM_TTL_TICKS: u64 = 10 * 1000;
/// Total bytes we will ever hold across all partial datagrams.
const REASM_TOTAL_CAP: usize = 128 * 1024;

// ─── Fragment reassembly ─────────────────────────────────────────────────────

struct FragBuf {
    src: IpAddr,
    dst: IpAddr,
    id: u32,
    proto: u8,
    /// Byte offset -> fragment bytes (sorted by offset at completion time).
    frags: Vec<(usize, Vec<u8>)>,
    /// Set when the final fragment (MF=0) has arrived.
    total: Option<usize>,
    seen: u64,
}

impl FragBuf {
    fn bytes_held(&self) -> usize {
        self.frags.iter().map(|(_, f)| f.len()).sum()
    }

    fn try_complete(&self) -> Option<Vec<u8>> {
        let total = self.total?;
        // Contiguity check: sorted fragments must cover [0, total).
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

    /// Feed one non-initial fragment (or initial fragment of an incomplete set).
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
                // CAP exceeded -> drop silently (deliberate). Note the buffer
                // is created even when THIS fragment is the last one (MF=0):
                // fragments arrive in any order, and refusing to open the
                // buffer here would lose the recorded total — the datagram
                // could then never complete as later fragments arrive.
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

/// Handle one IP packet received off the wire (`pkt` starts at the IP header).
/// Dispatches on the version nibble; both families validate, reassemble when
/// needed, and dispatch by protocol.
pub(crate) fn input(st: &mut Stack, pkt: &[u8], eth_src: Mac, now: u64) {
    match pkt.first().map(|b| b >> 4) {
        Some(4) => input_v4(st, pkt, now),
        Some(6) => input_v6(st, pkt, eth_src, now),
        _ => {}
    }
}

/// Handle one IPv6 packet. Also serves NDP (NS/NA) and Router Advertisements
/// (SLAAC): an RA with an autonomous on-link prefix configures our global
/// address and default router (RFC 4862).
fn input_v6(st: &mut Stack, pkt: &[u8], eth_src: Mac, now: u64) {
    let Some((h, payload)) = ipv6_parse(pkt) else {
        return;
    };

    // Accept packets addressed to us (global SLAAC address or link-local),
    // plus the node-local/link-local multicasts NDP relies on.
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
        // NDP first (its own message space), then RA, then echo.
        let is_ndp = matches!(
            payload.first().copied(),
            Some(wire::ICMPV6_NEIGHBOR_SOLICIT) | Some(wire::ICMPV6_NEIGHBOR_ADVERT)
        );
        let is_ra = payload.first() == Some(&wire::ICMPV6_ROUTER_ADVERT);
        if is_ndp || is_ra {
            // RFC 4861 anti-off-link-spoof checks: ND/RA messages are valid
            // only with hop limit 255 (an forwarded packet has been decremented)
            // and an on-link (link-local, or unspecified for DAD solicitation)
            // source. An off-link attacker cannot forge either.
            if h.hop_limit != 255 {
                return;
            }
            if is_ra {
                // RA sources are always routers' link-local addresses.
                if !h.src.is_link_local() {
                    return;
                }
            } else if !h.src.is_unspecified() && !h.src.is_link_local() {
                return;
            }
        }
        if is_ndp {
            if let Some(p) = wire::ndp_parse(h.src, h.dst, payload) {
                // The link-layer sender is the Ethernet source, not bytes from
                // inside the IPv6 header (those were next-header/hop-limit/src).
                let mac = Mac(eth_src.0);
                // (moved out temporarily so it can use the rest of the stack)
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

    // Fragment reassembly (fragment header carries the real upper proto).
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

/// The fragment header's first byte is the NEXT HEADER of the enclosed data.
fn rest_first(rest: &[u8]) -> u8 {
    rest.first().copied().unwrap_or(wire::IPV6_NO_NEXT)
}

/// Apply a Router Advertisement prefix (SLAAC). The interface identifier comes
/// from our EUI-64-derived link-local address, so the configured address is
/// stable across boots.
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
        // Unsolicited NA so neighbors learn us promptly (RX is promiscuous, so
        // the solicited-node multicast group needs no explicit join).
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

/// Handle one ICMPv6 echo exchange (requests answered; replies resolve pings).
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

/// Handle one IPv4 packet received off the wire (`pkt` starts at the IP
/// header). Validates, reassembles when needed, dispatches by protocol.
fn input_v4(st: &mut Stack, pkt: &[u8], now: u64) {
    let Some((h, payload)) = ipv4_parse(pkt) else {
        return;
    };

    // Accept only packets addressed to us (or limited broadcast — needed while
    // unconfigured during DHCP).
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
            // Answer echo requests natively (swap type 8 -> 0); reply to the
            // requester from the address it addressed us at.
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
            // Resolve a matching pending ping.
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

/// Send one L3 payload toward `dst_ip` over `proto`. Handles next-hop
/// selection, neighbor resolution via ARP/NDP (queueing when unresolved), and
/// IPv4 fragmentation (IPv6 is never fragmented on TX per RFC 2460 §4.5).
///
/// `src_override` lets DHCP send from 0.0.0.0 before configuration; pass
/// `None` to use the configured address of the matching family.
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

    // Next-hop selection.
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

    // Fragment as needed.
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

    // Source selection: link-local destination -> link-local source; otherwise
    // the SLAAC global address when present.
    let ll = st.ll_addr6;
    let global = st.cidr6.map(|c| c.addr);
    let src: Ipv6Addr = src_override.unwrap_or(if dst_ip.is_link_local() {
        ll
    } else {
        global.unwrap_or(ll)
    });

    // Next hop: multicast maps straight to its MAC; on-link/link-local targets
    // resolve themselves; everything else goes via the RA-provided router.
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

/// Split `payload` into MTU-sized chunks whose lengths are multiples of 8
/// except the final one. A payload that fits yields a single zero-offset chunk.
fn fragment_chunks(payload: &[u8], max_payload: usize) -> Vec<Chunk<'_>> {
    let mut out = Vec::new();
    if payload.len() <= max_payload {
        out.push(Chunk {
            offset: 0,
            data: payload,
        });
        return out;
    }
    // Round the per-fragment size down to a multiple of 8.
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

/// Hand an IPv4 packet to the link layer, resolving the next hop's MAC through
/// ARP (broadcast destinations bypass resolution entirely).
fn send_v4(st: &mut Stack, next_hop: Ipv4Addr, packet: &[u8], now: u64) {
    if next_hop.is_broadcast() {
        let frame = eth_frame(Mac::BROADCAST, st.mac, ETHERTYPE_IPV4, packet);
        crate::drivers::e1000::send(&frame);
        return;
    }
    let mac = match st.arp.lookup(next_hop) {
        Some(m) => m,
        None => {
            // Park the frame and ask.
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

/// Hand an IPv6 packet to the link layer, resolving the next hop's MAC through
/// NDP (multicast destinations map directly to 33:33:* MACs).
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

// ─── Ping waiters ────────────────────────────────────────────────────────────

/// One outstanding `net::ping`.
pub struct PingWaiter {
    pub ident: u16,
    pub seq: u16,
    pub started: u64,
    /// Match any sequence number (we send exactly one probe).
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
