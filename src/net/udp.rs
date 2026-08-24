//! Own UDP layer: socket table, datagram demux, and the DHCP client.
//!
//! UDP sockets are identified by a table index (the `SocketHandle` consumers
//! hold). Demux matches on the local port only — every socket binds a distinct
//! ephemeral or well-known port, which is all this stack needs (DNS queries on
//! ephemeral ports, DHCP on 68, the UDP echo service).
//!
//! The DHCP client (RFC 2131) is a small state machine driven from
//! [`DhcpClient::drive`], called once per poll. It speaks broadcast DISCOVER →
//! OFFER → REQUEST → ACK, applies the lease (address/prefix/router/DNS/lease
//! time), renews at T1, and restarts discovery when a phase stalls.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::wire::{self, udp_build, udp_parse, IpAddr, IpEndpoint, Ipv4Addr};
use super::Stack;

/// Per-socket queued-datagram cap.
const RX_QUEUE_CAP: usize = 16;
/// Largest datagram buffered per queue entry.
const MAX_DGRAM: usize = 2048;

// ─── Socket table ────────────────────────────────────────────────────────────

pub struct UdpSock {
    pub(crate) port: u16,
    pub(crate) rx: VecDeque<(Vec<u8>, IpEndpoint)>,
}

#[derive(Default)]
pub struct UdpTable {
    socks: Vec<Option<UdpSock>>,
}

impl UdpTable {
    pub const fn new() -> Self {
        UdpTable { socks: Vec::new() }
    }

    /// Open a socket bound to `port`. Returns its handle.
    pub fn open(&mut self, port: u16) -> usize {
        for (i, s) in self.socks.iter().enumerate() {
            if s.is_none() {
                self.socks[i] = Some(UdpSock {
                    port,
                    rx: VecDeque::new(),
                });
                return i;
            }
        }
        self.socks.push(Some(UdpSock {
            port,
            rx: VecDeque::new(),
        }));
        self.socks.len() - 1
    }

    pub fn close(&mut self, h: usize) {
        if let Some(s) = self.socks.get_mut(h) {
            *s = None;
        }
    }

    pub(crate) fn port_of(&self, h: usize) -> Option<u16> {
        self.socks.get(h)?.as_ref().map(|s| s.port)
    }

    /// Is `port` bound by any live socket?
    pub(crate) fn port_in_use(&self, port: u16) -> bool {
        self.socks.iter().flatten().any(|s| s.port == port)
    }

    /// Queue an incoming datagram for the socket bound to `dst_port`.
    /// Returns false when no listener exists or the queue is full.
    pub(crate) fn demux(&mut self, src: IpEndpoint, dst_port: u16, payload: &[u8]) -> bool {
        for slot in self.socks.iter_mut().flatten() {
            if slot.port == dst_port {
                if slot.rx.len() >= RX_QUEUE_CAP {
                    return false; // drop; bounded buffer
                }
                let mut data = payload.to_vec();
                data.truncate(MAX_DGRAM);
                slot.rx.push_back((data, src));
                return true;
            }
        }
        false // no listener (an ICMP port-unreachable would go here)
    }

    /// Receive one datagram if available. Returns `(len, sender_endpoint)`.
    pub fn recv(&mut self, h: usize, dst: &mut [u8]) -> Option<(usize, IpEndpoint)> {
        let sock = self.socks.get_mut(h)?.as_mut()?;
        let (data, ep) = sock.rx.pop_front()?;
        let n = core::cmp::min(data.len(), dst.len());
        dst[..n].copy_from_slice(&data[..n]);
        Some((n, ep))
    }

    #[allow(dead_code)]
    pub fn has_pending(&self, h: usize) -> bool {
        self.socks
            .get(h)
            .and_then(|s| s.as_ref())
            .map(|s| !s.rx.is_empty())
            .unwrap_or(false)
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.socks.iter().filter(|s| s.is_some()).count()
    }
}

/// Send one UDP datagram from `local_port` to `remote` through the stack.
pub(crate) fn send_from(
    st: &mut Stack,
    local_port: u16,
    data: &[u8],
    remote: IpEndpoint,
) -> Result<(), ()> {
    if data.len() > super::ip::MTU - wire::IPV4_HDR_MIN - wire::UDP_HDR_LEN {
        return Err(()); // oversized for one datagram (no send-side frag for UDP)
    }
    let now = crate::task::scheduler::ticks();
    let src = IpEndpoint::new(source_addr(st), local_port);
    if !families_match(src.addr, remote.addr) {
        return Err(());
    }
    let dgram = udp_build(src, remote, data);
    super::ip::output(st, None, remote.addr, wire::PROTO_UDP, &dgram, now);
    Ok(())
}

/// Handle one incoming UDP datagram (checksum already verified by `udp_parse`).
pub(crate) fn input(st: &mut Stack, src_ip: IpAddr, dst_ip: IpAddr, dgram: &[u8]) {
    let Some((src_ep, dst_ep, payload)) = udp_parse(src_ip, dst_ip, dgram) else {
        return;
    };
    // The DHCP socket gets first refusal so boot-phase packets land even when
    // other sockets exist.
    if dst_ep.port == DhcpClient::local_port() {
        st.dhcp.on_datagram(src_ep, payload);
        return;
    }
    st.udp.demux(src_ep, dst_ep.port, payload);
}

// ─── DHCP client ─────────────────────────────────────────────────────────────

const DHCP_CLIENT_PORT: u16 = 68;
const DHCP_SERVER_PORT: u16 = 67;

// DHCP message types (option 53).
const DHCPDISCOVER: u8 = 1;
const DHCPOFFER: u8 = 2;
const DHCPREQUEST: u8 = 3;
const DHCPACK: u8 = 5;

/// Retry cadence for Discover/Request phases.
const RETRY_TICKS: u64 = 2000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Init,
    Discovering,
    Requesting,
    Bound,
}

/// Lease state applied by `net::poll` once an ACK arrives.
pub struct DhcpLease {
    pub addr: Ipv4Addr,
    pub prefix: u8,
    pub router: Option<Ipv4Addr>,
    pub dns_servers: Vec<Ipv4Addr>,
    /// Lease lifetime in seconds (kept for future renewal tuning).
    #[allow(dead_code)]
    pub lease_secs: u32,
}

pub struct DhcpClient {
    socket_opened: bool,
    phase: Phase,
    xid: u32,
    server_id: Ipv4Addr,
    retry_deadline: u64,
    t1_deadline: Option<u64>,
    lease_ticks_total: u64,
    rx: VecDeque<(IpEndpoint, Vec<u8>)>,
}

impl Default for DhcpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl DhcpClient {
    pub const fn new() -> Self {
        DhcpClient {
            socket_opened: false,
            phase: Phase::Init,
            xid: 0x3903_F326, // replaced with a tick-derived id at discovery
            server_id: Ipv4Addr::UNSPECIFIED,
            retry_deadline: 0,
            t1_deadline: None,
            lease_ticks_total: 0,
            rx: VecDeque::new(),
        }
    }

    pub(crate) const fn local_port() -> u16 {
        DHCP_CLIENT_PORT
    }

    fn on_datagram(&mut self, _src: IpEndpoint, payload: &[u8]) {
        if payload.len() > MAX_DGRAM {
            return;
        }
        self.rx.push_back((_src, payload.to_vec()));
        if self.rx.len() > RX_QUEUE_CAP {
            self.rx.pop_front();
        }
    }

    /// Advance the state machine one step. Returns `Some(lease)` exactly once
    /// per newly-acquired lease.
    pub fn drive(&mut self, st: &mut Stack, now: u64) -> Option<DhcpLease> {
        match self.phase {
            Phase::Init => {
                if !self.socket_opened {
                    st.udp.open(DHCP_CLIENT_PORT);
                    self.socket_opened = true;
                }
                let mac = st.mac;
                let pkt = dhcp_packet(
                    self.xid,
                    DHCPDISCOVER,
                    mac.0,
                    Ipv4Addr::UNSPECIFIED,
                    Ipv4Addr::UNSPECIFIED,
                    &[],
                );
                let remote =
                    IpEndpoint::new(IpAddr::V4(wire::Ipv4Addr::BROADCAST), DHCP_SERVER_PORT);
                let _ = send_from(st, DHCP_CLIENT_PORT, &pkt, remote);
                self.phase = Phase::Discovering;
                self.retry_deadline = now + RETRY_TICKS;
                None
            }
            Phase::Discovering => {
                while let Some((_ep, pkt)) = self.rx.pop_front() {
                    if let Some(offer) = parse_reply(&pkt, self.xid, DHCPOFFER) {
                        self.server_id = offer.server_id;
                        let mac = st.mac;
                        let mut opts = Vec::new();
                        opts.extend_from_slice(&[54, 4]);
                        opts.extend_from_slice(&offer.server_id.0);
                        opts.extend_from_slice(&[50, 4]);
                        opts.extend_from_slice(&offer.yiaddr.0);
                        let pkt = dhcp_packet(
                            self.xid,
                            DHCPREQUEST,
                            mac.0,
                            Ipv4Addr::UNSPECIFIED,
                            Ipv4Addr::BROADCAST,
                            &opts,
                        );
                        let remote = IpEndpoint::new(
                            IpAddr::V4(wire::Ipv4Addr::BROADCAST),
                            DHCP_SERVER_PORT,
                        );
                        let _ = send_from(st, DHCP_CLIENT_PORT, &pkt, remote);
                        self.phase = Phase::Requesting;
                        self.retry_deadline = now + RETRY_TICKS;
                        break;
                    }
                }
                if self.phase == Phase::Discovering && now >= self.retry_deadline {
                    self.phase = Phase::Init; // re-DISCOVER on the next drive
                }
                None
            }
            Phase::Requesting => {
                while let Some((_ep, pkt)) = self.rx.pop_front() {
                    if let Some(ack) = parse_reply(&pkt, self.xid, DHCPACK) {
                        let lease_secs = ack.lease_secs.max(60);
                        self.lease_ticks_total =
                            crate::arch::x86_64::apic::ms_to_ticks(lease_secs as u64 * 1000);
                        self.t1_deadline = Some(now + self.lease_ticks_total / 2);
                        self.phase = Phase::Bound;
                        return Some(DhcpLease {
                            addr: ack.yiaddr,
                            prefix: ack.prefix,
                            router: ack.router,
                            dns_servers: ack.dns,
                            lease_secs,
                        });
                    }
                }
                if now >= self.retry_deadline {
                    self.phase = Phase::Init; // back to discover
                }
                None
            }
            Phase::Bound => {
                // Renewal at T1: unicast REQUEST to the leasing server with
                // ciaddr filled. If the network vanished entirely, the poll
                // loop's fallback logic handles deconfiguration.
                if let Some(t1) = self.t1_deadline {
                    if now >= t1 {
                        let addr = st.cidr.map(|c| c.addr).unwrap_or(Ipv4Addr::UNSPECIFIED);
                        let mac = st.mac;
                        let pkt =
                            dhcp_packet(self.xid, DHCPREQUEST, mac.0, addr, self.server_id, &[]);
                        let remote = IpEndpoint::new(IpAddr::V4(self.server_id), DHCP_SERVER_PORT);
                        let _ = send_from(st, DHCP_CLIENT_PORT, &pkt, remote);
                        self.t1_deadline = Some(now + self.lease_ticks_total / 2);
                    }
                }
                // Consume any late replies quietly (configuration unchanged).
                while let Some((_ep, _pkt)) = self.rx.pop_front() {}
                None
            }
        }
    }
}

// ─── DHCP wire format ────────────────────────────────────────────────────────

const DHCP_COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
const OPT_MSG_TYPE: u8 = 53;
const OPT_END: u8 = 255;

/// Build a minimal BOOTREQUEST/DHCP packet (fixed header + cookie + options).
fn dhcp_packet(
    xid: u32,
    msg_type: u8,
    chaddr: [u8; 6],
    ciaddr: Ipv4Addr,
    dst_hint_unused: Ipv4Addr,
    extra_options: &[u8],
) -> Vec<u8> {
    let _ = dst_hint_unused;
    let mut p = Vec::with_capacity(240 + extra_options.len() + 24);
    p.push(1); // op: BOOTREQUEST
    p.push(1); // htype: ethernet
    p.push(6); // hlen
    p.push(0); // hops
    p.extend_from_slice(&xid.to_be_bytes());
    p.extend_from_slice(&[0, 0]); // secs
                                  // Broadcast flag set: QEMU's DHCP server answers broadcasts fine even
                                  // before we have an address.
    p.extend_from_slice(&[0x80, 0x00]);
    p.extend_from_slice(&ciaddr.0); // ciaddr
    p.extend_from_slice(&[0; 4]); // yiaddr
    p.extend_from_slice(&[0; 4]); // siaddr
    p.extend_from_slice(&[0; 4]); // giaddr
    let mut ch = [0u8; 16];
    ch[..6].copy_from_slice(&chaddr);
    p.extend_from_slice(&ch); // chaddr
    p.extend_from_slice(&[0u8; 64]); // sname
    p.extend_from_slice(&[0u8; 128]); // file
    p.extend_from_slice(&DHCP_COOKIE);
    // Options.
    p.extend_from_slice(&[OPT_MSG_TYPE, 1, msg_type]);
    // Parameter request list: subnet(1), router(3), dns(6), lease(51), server(54)
    p.extend_from_slice(&[55, 5, 1, 3, 6, 51, 54]);
    p.extend_from_slice(extra_options);
    p.push(OPT_END);
    p
}

struct ParsedReply {
    yiaddr: Ipv4Addr,
    prefix: u8,
    server_id: Ipv4Addr,
    router: Option<Ipv4Addr>,
    dns: Vec<Ipv4Addr>,
    lease_secs: u32,
}

/// Parse a DHCP reply (BOOTREPLY), requiring `want_type` in option 53 and a
/// matching xid.
fn parse_reply(pkt: &[u8], xid: u32, want_type: u8) -> Option<ParsedReply> {
    // Fixed header is 236 bytes + 4-byte cookie.
    if pkt.len() < 240 || pkt[0] != 2 {
        return None;
    }
    let rx_xid = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);
    if rx_xid != xid {
        return None;
    }
    if pkt[236..240] != DHCP_COOKIE {
        return None;
    }

    let yiaddr = Ipv4Addr([pkt[16], pkt[17], pkt[18], pkt[19]]);

    let mut out = ParsedReply {
        yiaddr,
        prefix: 24, // default when option 1 absent
        server_id: Ipv4Addr::UNSPECIFIED,
        router: None,
        dns: Vec::new(),
        lease_secs: 3600,
    };
    let mut msg_type: Option<u8> = None;

    let mut i = 240usize;
    while i < pkt.len() {
        let opt = pkt[i];
        match opt {
            OPT_END => break,
            0 => i += 1, // pad
            kind => {
                if i + 1 >= pkt.len() {
                    break;
                }
                let len = pkt[i + 1] as usize;
                if i + 2 + len > pkt.len() {
                    break;
                }
                let v = &pkt[i + 2..i + 2 + len];
                match kind {
                    OPT_MSG_TYPE if len >= 1 => msg_type = Some(v[0]),
                    1 if len == 4 => {
                        // Subnet mask -> prefix length.
                        out.prefix = v.iter().filter(|&&b| b == 0xFF).count() as u8 * 8;
                        // Handle non-contiguous masks defensively: count leading FFs only.
                        let mut prefix = 0u8;
                        for &b in v {
                            if b == 0xFF {
                                prefix += 8;
                            } else {
                                break;
                            }
                        }
                        if prefix == 0 && v[0] != 0 {
                            // Non-byte-aligned mask: derive from leading ones of the partial octet.
                            let mut b = v[0];
                            while b & 0x80 != 0 {
                                prefix += 1;
                                b <<= 1;
                            }
                        }
                        out.prefix = prefix.clamp(1, 32);
                    }
                    3 if len >= 4 => {
                        out.router = Some(Ipv4Addr([v[0], v[1], v[2], v[3]]));
                    }
                    6 => {
                        let mut j = 0;
                        while j + 4 <= v.len() {
                            out.dns.push(Ipv4Addr([v[j], v[j + 1], v[j + 2], v[j + 3]]));
                            j += 4;
                        }
                    }
                    51 if len == 4 => {
                        out.lease_secs = u32::from_be_bytes([v[0], v[1], v[2], v[3]]);
                    }
                    54 if len == 4 => {
                        out.server_id = Ipv4Addr([v[0], v[1], v[2], v[3]]);
                    }
                    _ => {}
                }
                i += 2 + len;
            }
        }
    }

    if msg_type != Some(want_type) {
        return None;
    }
    Some(out)
}

// Silence unused import when built without tests exercising `vec!`.
#[cfg(test)]
use alloc::vec as _vec_macro_alias;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_close_reuses_slots() {
        let mut t = UdpTable::new();
        let a = t.open(1000);
        let b = t.open(1001);
        assert_ne!(a, b);
        t.close(a);
        let c = t.open(1002);
        assert_eq!(c, a, "slot reuse");
        assert!(t.has_pending(b) == false);
    }

    #[test]
    fn demux_routes_by_port() {
        let mut t = UdpTable::new();
        let h = t.open(5353);
        let src = IpEndpoint::new(Ipv4Addr::new(10, 0, 2, 2), 53);
        assert!(t.demux(src, 5353, b"abc"));
        let mut buf = [0u8; 8];
        let (n, ep) = t.recv(h, &mut buf).unwrap();
        assert_eq!(n, 3);
        assert_eq!(ep, src);
        assert!(!t.has_pending(h));
    }

    #[test]
    fn dhcp_packet_shape() {
        let p = dhcp_packet(
            0xDEAD_BEEF,
            DHCPDISCOVER,
            [1, 2, 3, 4, 5, 6],
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            &[],
        );
        assert_eq!(p[0], 1); // BOOTREQUEST
        assert_eq!(&p[4..8], &0xDEAD_BEEFu32.to_be_bytes());
        assert_eq!(&p[236..240], &DHCP_COOKIE);
        assert!(*p.last().unwrap() == OPT_END);
    }

    #[test]
    fn parse_offer_roundtrip() {
        let mut p = dhcp_packet(
            42,
            DHCPOFFER,
            [0; 6],
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            &[],
        );
        p[0] = 2; // BOOTREPLY
        p[4..8].copy_from_slice(&42u32.to_be_bytes());
        p[16..20].copy_from_slice(&[10, 0, 2, 15]); // yiaddr
                                                    // Append options: 53/1=OFFER, 54/4=server, 51/4=lease, 1/4=mask, 3/4=router
        let mut opts: Vec<u8> = vec![53, 1, DHCPOFFER];
        opts.extend_from_slice(&[54, 4, 10, 0, 2, 2]);
        opts.extend_from_slice(&[51, 4, 0, 0, 14, 16]); // 3600s
        opts.extend_from_slice(&[1, 4, 255, 255, 255, 0]);
        opts.extend_from_slice(&[3, 4, 10, 0, 2, 2]);
        opts.push(255);
        p.truncate(240);
        p.extend_from_slice(&opts);

        let r = parse_reply(&p, 42, DHCPOFFER).expect("parses");
        assert_eq!(r.yiaddr, Ipv4Addr::new(10, 0, 2, 15));
        assert_eq!(r.server_id, Ipv4Addr::new(10, 0, 2, 2));
        assert_eq!(r.router, Some(Ipv4Addr::new(10, 0, 2, 2)));
        assert_eq!(r.lease_secs, 3600);
        assert_eq!(r.prefix, 24);
    }
}

/// Best source address for an outgoing datagram of the same family as `remote`.
pub(crate) fn source_addr(st: &Stack) -> IpAddr {
    match st.cidr.map(|c| c.addr) {
        Some(v4) => IpAddr::V4(v4),
        None => match st.cidr6 {
            Some(c) => IpAddr::V6(c.addr),
            None => IpAddr::V4(wire::Ipv4Addr::UNSPECIFIED),
        },
    }
}

fn families_match(a: IpAddr, b: IpAddr) -> bool {
    matches!(
        (a, b),
        (IpAddr::V4(_), IpAddr::V4(_)) | (IpAddr::V6(_), IpAddr::V6(_))
    )
}
