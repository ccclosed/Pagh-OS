//! Own network wire types + pure encode/decode helpers.
//!
//! This replaces every `smoltcp::wire` type the kernel used. Everything here is
//! pure: byte slices in, values out, no globals, no hardware, no allocation —
//! so the module is host-includable (same discipline as `net/http.rs`) and the
//! property tests can exercise it directly.
//!
//! Layers covered:
//!   * Ethernet II framing (14-byte header)
//!   * ARP (RFC 826) packet build/parse
//!   * IPv4 header build/parse (+ fragmentation fields), internet checksum
//!   * ICMP echo request/reply
//!   * UDP datagram build/parse
//!   * TCP segment header build/parse (options: MSS, window scaling)

use alloc::string::String;
use core::fmt;
use core::fmt::Write as _;

// ─── Address types ───────────────────────────────────────────────────────────

/// A 48-bit Ethernet (MAC) address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mac(pub [u8; 6]);

impl Mac {
    /// Broadcast address.
    pub const BROADCAST: Mac = Mac([0xFF; 6]);
}

impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            m[0], m[1], m[2], m[3], m[4], m[5]
        )
    }
}

/// An IPv4 address.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ipv4Addr(pub [u8; 4]);

impl Ipv4Addr {
    pub const UNSPECIFIED: Ipv4Addr = Ipv4Addr([0, 0, 0, 0]);
    /// Limited broadcast.
    pub const BROADCAST: Ipv4Addr = Ipv4Addr([255, 255, 255, 255]);

    pub const fn new(a: u8, b: u8, c: u8, d: u8) -> Self {
        Ipv4Addr([a, b, c, d])
    }

    pub fn octets(self) -> [u8; 4] {
        self.0
    }

    pub const fn is_unspecified(self) -> bool {
        self.0[0] == 0 && self.0[1] == 0 && self.0[2] == 0 && self.0[3] == 0
    }

    pub const fn is_broadcast(self) -> bool {
        self.0[0] == 255 && self.0[1] == 255 && self.0[2] == 255 && self.0[3] == 255
    }

    /// True if `self` is inside `cidr`.
    pub fn in_subnet(self, cidr: Ipv4Cidr) -> bool {
        let mask = cidr.mask_octets();
        let mut i = 0;
        while i < 4 {
            if self.0[i] & mask[i] != cidr.addr.0[i] & mask[i] {
                return false;
            }
            i += 1;
        }
        true
    }
}

impl fmt::Display for Ipv4Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}.{}", self.0[0], self.0[1], self.0[2], self.0[3])
    }
}

impl fmt::Debug for Ipv4Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// An IPv4 subnet: address + prefix length.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ipv4Cidr {
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl Ipv4Cidr {
    pub const fn new(addr: Ipv4Addr, prefix: u8) -> Self {
        Ipv4Cidr { addr, prefix }
    }

    /// Subnet mask as octets (e.g. /24 -> [255,255,255,0]).
    pub const fn mask_octets(&self) -> [u8; 4] {
        let mut out = [0u8; 4];
        let mut i = 0usize;
        while i < 4 {
            let base = (i * 8) as u8;
            let n = if self.prefix >= base + 8 {
                8u8
            } else {
                self.prefix.saturating_sub(base)
            };
            out[i] = if n == 0 { 0 } else { u8::MAX << (8 - n) };
            i += 1;
        }
        out
    }
}

impl fmt::Display for Ipv4Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// An IPv6 address (RFC 4291).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ipv6Addr(pub [u8; 16]);

impl Ipv6Addr {
    #[allow(dead_code)]
    pub const UNSPECIFIED: Ipv6Addr = Ipv6Addr([0; 16]);
    #[allow(dead_code)]
    pub const LOOPBACK: Ipv6Addr = Ipv6Addr([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    /// ff02::1 — all nodes link-local multicast.
    pub const ALL_NODES: Ipv6Addr =
        Ipv6Addr([0xFF, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    /// ff02::2 — all routers link-local multicast.
    pub const ALL_ROUTERS: Ipv6Addr =
        Ipv6Addr([0xFF, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);

    #[allow(dead_code)]
    pub const fn new(octets: [u8; 16]) -> Self {
        Ipv6Addr(octets)
    }

    pub const fn octets(self) -> [u8; 16] {
        self.0
    }

    pub const fn is_unspecified(self) -> bool {
        let mut all_zero = true;
        let mut i = 0;
        while i < 16 {
            if self.0[i] != 0 {
                all_zero = false;
            }
            i += 1;
        }
        all_zero
    }

    /// fe80::/10.
    pub const fn is_link_local(self) -> bool {
        self.0[0] == 0xFE && (self.0[1] & 0xC0) == 0x80
    }

    /// RFC 4291 §2.5.5.2: `Some(v4)` when this is a `::ffff:a.b.c.d` mapping.
    pub const fn to_v4_mapped(self) -> Option<Ipv4Addr> {
        let mut prefix_zero = true;
        let mut i = 0;
        while i < 10 {
            if self.0[i] != 0 {
                prefix_zero = false;
            }
            i += 1;
        }
        if prefix_zero && self.0[10] == 0xFF && self.0[11] == 0xFF {
            Some(Ipv4Addr([self.0[12], self.0[13], self.0[14], self.0[15]]))
        } else {
            None
        }
    }

    /// Solicited-node multicast address ff02::1:ffXX:XXXX (RFC 4291 §4.7).
    pub const fn solicited_node(self) -> Ipv6Addr {
        Ipv6Addr([
            0xFF, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0xFF, self.0[13], self.0[14], self.0[15],
        ])
    }

    /// True if inside `cidr`.
    pub fn in_subnet(self, cidr: Ipv6Cidr) -> bool {
        let full = (cidr.prefix / 8) as usize;
        if self.0[..full] != cidr.addr.0[..full] {
            return false;
        }
        let rem = cidr.prefix % 8;
        if rem == 0 || full >= 16 {
            return true;
        }
        let mask = u8::MAX << (8 - rem);
        self.0[full] & mask == cidr.addr.0[full] & mask
    }

    /// Build a link-local address fe80::<EUI-64-derived IID> from a MAC
    /// (flip the U/L bit, insert ff:fe in the middle).
    pub const fn link_local_from_mac(mac: Mac) -> Ipv6Addr {
        let m = mac.0;
        Ipv6Addr([
            0xFE,
            0x80,
            0,
            0,
            0,
            0,
            0,
            0,
            m[0] ^ 0x02,
            m[1],
            m[2],
            0xFF,
            0xFE,
            m[3],
            m[4],
            m[5],
        ])
    }
}

impl fmt::Display for Ipv6Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // RFC 5952: lowercase hex, compress the LONGEST run of >=2 zero groups,
        // leftmost on ties.
        let groups: [u16; 8] =
            core::array::from_fn(|i| u16::from_be_bytes([self.0[i * 2], self.0[i * 2 + 1]]));
        let mut best_start = usize::MAX;
        let mut best_len = 0usize;
        let mut i = 0usize;
        while i < 8 {
            if groups[i] == 0 {
                let mut j = i;
                while j < 8 && groups[j] == 0 {
                    j += 1;
                }
                if j - i > best_len {
                    best_len = j - i;
                    best_start = i;
                }
                i = j;
            } else {
                i += 1;
            }
        }
        if best_len < 2 {
            best_start = usize::MAX;
        }

        let mut out = String::new();
        let mut i = 0usize;
        while i < 8 {
            if i == best_start {
                out.push_str("::");
                i += best_len;
                continue;
            }
            // Separator rules: nothing at string start or right after "::".
            if !out.is_empty() && !out.ends_with(':') {
                out.push(':');
            }
            let _ = write!(out, "{:x}", groups[i]);
            i += 1;
        }
        f.write_str(&out)
    }
}

impl fmt::Debug for Ipv6Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// An IPv6 subnet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ipv6Cidr {
    pub addr: Ipv6Addr,
    pub prefix: u8,
}

impl fmt::Display for Ipv6Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// Parse an IPv6 text address (with optional `::` compression). Pure.
///
/// Handles embedded IPv4 tails (`::ffff:1.2.3.4`), zone suffixes (`%eth0`,
/// ignored — this kernel has a single interface), and rejects multiple `::`.
pub fn parse_ipv6_literal(s: &str) -> Option<Ipv6Addr> {
    let s = s.split('%').next()?;

    // Split around "::"; more than one occurrence is invalid.
    let mut halves = s.split("::");
    let head = halves.next()?;
    let tail = halves.next();
    if halves.next().is_some() {
        return None;
    }

    /// Parse one colon-separated run into <=8 groups; an IPv4 dotted-quad may
    /// occupy the final two groups.
    fn parse_run(run: &str) -> Option<alloc::vec::Vec<u16>> {
        let mut groups = alloc::vec::Vec::new();
        // An empty run ("", from leading/trailing "::") contributes nothing.
        if run.is_empty() {
            return Some(groups);
        }
        for part in run.split(':') {
            if part.contains('.') {
                // IPv4 tail must be the LAST element and yield two groups.
                if groups.len() > 6 || !run.ends_with(part) {
                    return None;
                }
                let v4 = parse_ipv4_literal(part)?;
                groups.push(u16::from_be_bytes([v4.0[0], v4.0[1]]));
                groups.push(u16::from_be_bytes([v4.0[2], v4.0[3]]));
            } else {
                if part.is_empty() || part.len() > 4 {
                    return None;
                }
                groups.push(u16::from_str_radix(part, 16).ok()?);
            }
        }
        Some(groups)
    }

    let mut groups = alloc::vec::Vec::new();
    match tail {
        None => {
            // No compression present: exactly 8 groups required.
            groups = parse_run(head)?;
            if groups.len() != 8 {
                return None;
            }
        }
        Some(t) => {
            let h = parse_run(head)?;
            let t = parse_run(t)?;
            if h.len() + t.len() > 7 {
                return None;
            }
            groups.extend_from_slice(&h);
            let zeros = 8 - h.len() - t.len();
            groups.resize(groups.len() + zeros, 0);
            groups.extend_from_slice(&t);
        }
    }

    debug_assert_eq!(groups.len(), 8);
    let mut bytes = [0u8; 16];
    for (i, g) in groups.iter().enumerate() {
        bytes[i * 2..i * 2 + 2].copy_from_slice(&g.to_be_bytes());
    }
    Some(Ipv6Addr(bytes))
}

/// An IP endpoint (address + transport port), dual-stack.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct IpEndpoint {
    pub addr: IpAddr,
    pub port: u16,
}

impl IpEndpoint {
    pub const fn new(addr: IpAddr, port: u16) -> Self {
        IpEndpoint { addr, port }
    }
}

impl fmt::Display for IpEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.addr, self.port)
    }
}

impl fmt::Debug for IpEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Parse a dotted-quad literal ("10.0.2.15"). Pure.
pub fn parse_ipv4_literal(s: &str) -> Option<Ipv4Addr> {
    let mut octets = [0u8; 4];
    let mut count = 0;
    for part in s.split('.') {
        if count >= 4 {
            return None;
        }
        octets[count] = part.parse().ok()?;
        count += 1;
    }
    if count != 4 {
        return None;
    }
    Some(Ipv4Addr(octets))
}

/// Either an IPv4 or an IPv6 address. Transport endpoints and the neighbor
/// layer carry this so every socket is dual-stack.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum IpAddr {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
}

impl IpAddr {
    #[allow(dead_code)]
    pub const fn is_ipv4(self) -> bool {
        matches!(self, IpAddr::V4(_))
    }

    #[allow(dead_code)]
    pub const fn is_ipv6(self) -> bool {
        matches!(self, IpAddr::V6(_))
    }

    /// RFC 4291 §2.5.5.2: `Some(v4)` when this is a `::ffff:a.b.c.d` mapping.
    pub fn to_v4_mapped(self) -> Option<Ipv4Addr> {
        match self {
            IpAddr::V4(v4) => Some(v4),
            IpAddr::V6(v6) => v6.to_v4_mapped(),
        }
    }
}

impl fmt::Display for IpAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpAddr::V4(a) => fmt::Display::fmt(a, f),
            IpAddr::V6(a) => fmt::Display::fmt(a, f),
        }
    }
}

impl fmt::Debug for IpAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Parse a dotted-quad OR an IPv6 text address. Pure.
pub fn parse_ip_literal(s: &str) -> Option<IpAddr> {
    if s.contains(':') {
        parse_ipv6_literal(s).map(IpAddr::V6)
    } else {
        parse_ipv4_literal(s).map(IpAddr::V4)
    }
}

// ─── Internet checksum ───────────────────────────────────────────────────────

/// One's-complement internet checksum over one or more byte slices.
/// Odd lengths are padded with a zero byte. Returns the folded complement.
pub fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum = 0u32;
    let mut carry_byte: Option<u8> = None;
    for part in parts {
        let mut iter = part.iter();
        // Fold a pending odd byte from the previous slice.
        if let Some(b) = carry_byte.take() {
            match iter.next() {
                Some(&b1) => sum += u16::from_be_bytes([b, b1]) as u32,
                None => carry_byte = Some(b),
            }
        }
        let mut chunks = iter.as_slice();
        // Sum big-endian u16 pairs.
        while chunks.len() >= 2 {
            sum += u16::from_be_bytes([chunks[0], chunks[1]]) as u32;
            chunks = &chunks[2..];
        }
        if let Some(&b) = chunks.first() {
            carry_byte = Some(b);
        }
    }
    if let Some(b) = carry_byte {
        sum += (b as u32) << 8;
    }
    // Fold carries.
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Verify an internet checksum over `header` where the checksum field itself
/// lives at `cksum_off`. Returns true when the computed sum (field treated as
/// zero) equals the stored value.
pub fn checksum_valid(header: &[u8], cksum_off: usize) -> bool {
    if cksum_off + 2 > header.len() {
        return false;
    }
    // Split around the checksum field so it reads as zeros.
    let (a, b) = (&header[..cksum_off], &header[cksum_off + 2..]);
    let computed = checksum(&[a, b]);
    let stored = u16::from_be_bytes([header[cksum_off], header[cksum_off + 1]]);
    computed == stored
}

// ─── Ethernet ────────────────────────────────────────────────────────────────

pub const ETH_HDR_LEN: usize = 14;
pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETHERTYPE_IPV6: u16 = 0x86DD;

/// Build an Ethernet II frame: `dst/src` MACs, `ethertype`, `payload`.
/// Returns a fresh heap buffer.
pub fn eth_frame(dst: Mac, src: Mac, ethertype: u16, payload: &[u8]) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::with_capacity(ETH_HDR_LEN + payload.len());
    out.extend_from_slice(&dst.0);
    out.extend_from_slice(&src.0);
    out.extend_from_slice(&ethertype.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Parse an Ethernet header. Returns `(dst, src, ethertype, payload)`.
pub fn eth_parse(frame: &[u8]) -> Option<(Mac, Mac, u16, &[u8])> {
    if frame.len() < ETH_HDR_LEN {
        return None;
    }
    let mut d = [0u8; 6];
    let mut s = [0u8; 6];
    d.copy_from_slice(&frame[0..6]);
    s.copy_from_slice(&frame[6..12]);
    let et = u16::from_be_bytes([frame[12], frame[13]]);
    Some((Mac(d), Mac(s), et, &frame[ETH_HDR_LEN..]))
}

// ─── ARP ─────────────────────────────────────────────────────────────────────

pub const ARP_HTYPE_ETHERNET: u16 = 1;
pub const ARP_PTYPE_IPV4: u16 = 0x0800;
pub const ARP_REQUEST: u16 = 1;
pub const ARP_REPLY: u16 = 2;
/// htype(2)+ptype(2)+hlen(1)+plen(1)+oper(2)+sha(6)+spa(4)+tha(6)+tpa(4)
pub const ARP_PKT_LEN: usize = 28;

/// Build an ARP packet (Ethernet hardware, IPv4 protocol).
pub fn arp_build(
    operation: u16,
    sender_mac: Mac,
    sender_ip: Ipv4Addr,
    target_mac: Mac,
    target_ip: Ipv4Addr,
) -> alloc::vec::Vec<u8> {
    let mut p = alloc::vec::Vec::with_capacity(ARP_PKT_LEN);
    p.extend_from_slice(&ARP_HTYPE_ETHERNET.to_be_bytes());
    p.extend_from_slice(&ARP_PTYPE_IPV4.to_be_bytes());
    p.push(6); // hlen
    p.push(4); // plen
    p.extend_from_slice(&operation.to_be_bytes());
    p.extend_from_slice(&sender_mac.0);
    p.extend_from_slice(&sender_ip.0);
    p.extend_from_slice(&target_mac.0);
    p.extend_from_slice(&target_ip.0);
    p
}

/// Parsed ARP packet fields.
pub struct ArpPacket {
    pub operation: u16,
    pub sender_mac: Mac,
    pub sender_ip: Ipv4Addr,
    pub target_ip: Ipv4Addr,
}

/// Parse an ARP packet (must be the 28-byte Ethernet/IPv4 shape).
pub fn arp_parse(p: &[u8]) -> Option<ArpPacket> {
    if p.len() < ARP_PKT_LEN {
        return None;
    }
    let htype = u16::from_be_bytes([p[0], p[1]]);
    let ptype = u16::from_be_bytes([p[2], p[3]]);
    if htype != ARP_HTYPE_ETHERNET || ptype != ARP_PTYPE_IPV4 || p[4] != 6 || p[5] != 4 {
        return None;
    }
    Some(ArpPacket {
        operation: u16::from_be_bytes([p[6], p[7]]),
        sender_mac: Mac([p[8], p[9], p[10], p[11], p[12], p[13]]),
        sender_ip: Ipv4Addr([p[14], p[15], p[16], p[17]]),
        target_ip: Ipv4Addr([p[24], p[25], p[26], p[27]]),
    })
}

// ─── IPv4 ────────────────────────────────────────────────────────────────────

pub const IPV4_HDR_MIN: usize = 20;
pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;
pub const PROTO_ICMPV6: u8 = 58;

/// Flags/fragment-offset word helpers.
#[allow(dead_code)]
pub const IPV4_FLAG_DF: u16 = 1 << 14;
pub const IPV4_FLAG_MF: u16 = 1 << 13;

/// Build an IPv4 header (IHL=5, no options) followed by `payload`, computing
/// the header checksum. Returns a fresh heap buffer containing the full packet.
pub fn ipv4_build(
    src: Ipv4Addr,
    dst: Ipv4Addr,
    proto: u8,
    id: u16,
    flags_frag: u16,
    ttl: u8,
    payload: &[u8],
) -> alloc::vec::Vec<u8> {
    let total_len = (IPV4_HDR_MIN + payload.len()) as u16;
    let mut p = alloc::vec::Vec::with_capacity(total_len as usize);
    p.push(0x45); // version 4, IHL 5
    p.push(0x00); // DSCP/ECN
    p.extend_from_slice(&total_len.to_be_bytes());
    p.extend_from_slice(&id.to_be_bytes());
    p.extend_from_slice(&flags_frag.to_be_bytes());
    p.push(ttl);
    p.push(proto);
    p.extend_from_slice(&[0, 0]); // checksum placeholder
    p.extend_from_slice(&src.0);
    p.extend_from_slice(&dst.0);
    let csum = checksum(&[&p[..IPV4_HDR_MIN]]);
    p[10] = (csum >> 8) as u8;
    p[11] = (csum & 0xFF) as u8;
    p.extend_from_slice(payload);
    p
}

/// Parsed IPv4 header.
#[allow(dead_code)]
pub struct Ipv4Header {
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub proto: u8,
    pub ttl: u8,
    pub ident: u16,
    pub flags_frag: u16,
    /// Header length in bytes (IHL * 4).
    pub hdr_len: usize,
    /// Total datagram length claimed by the header (<= actual slice len).
    pub total_len: usize,
}

/// Parse an IPv4 header and validate its checksum. Returns the header plus the
/// payload slice (trimmed to `total_len` when the frame carried padding).
pub fn ipv4_parse(p: &[u8]) -> Option<(Ipv4Header, &[u8])> {
    if p.len() < IPV4_HDR_MIN {
        return None;
    }
    if p[0] >> 4 != 4 {
        return None;
    }
    let ihl = (p[0] & 0x0F) as usize * 4;
    if ihl < IPV4_HDR_MIN || p.len() < ihl {
        return None;
    }
    if !checksum_valid(&p[..ihl], 10) {
        return None;
    }
    let total_len = u16::from_be_bytes([p[2], p[3]]) as usize;
    if total_len < ihl || total_len > p.len() {
        // Truncated datagram: reject (we do not attempt partial delivery).
        return None;
    }
    Some((
        Ipv4Header {
            src: Ipv4Addr([p[12], p[13], p[14], p[15]]),
            dst: Ipv4Addr([p[16], p[17], p[18], p[19]]),
            proto: p[9],
            ttl: p[8],
            ident: u16::from_be_bytes([p[4], p[5]]),
            flags_frag: u16::from_be_bytes([p[6], p[7]]),
            hdr_len: ihl,
            total_len,
        },
        &p[ihl..total_len],
    ))
}

// ─── IPv6 ────────────────────────────────────────────────────────────────────

pub const IPV6_HDR_LEN: usize = 40;
/// Extension-header next-header values this stack understands enough to skip.
pub const IPV6_EXT_HOPBYHOP: u8 = 0;
pub const IPV6_EXT_ROUTING: u8 = 43;
pub const IPV6_EXT_FRAGMENT: u8 = 44;
pub const IPV6_EXT_DESTOPT: u8 = 60;
pub const IPV6_NO_NEXT: u8 = 59;

/// Build an IPv6 header (fixed 40 bytes) followed by `payload`.
#[allow(clippy::too_many_arguments)]
pub fn ipv6_build(
    src: Ipv6Addr,
    dst: Ipv6Addr,
    next_header: u8,
    hop_limit: u8,
    payload: &[u8],
) -> alloc::vec::Vec<u8> {
    let mut p = alloc::vec::Vec::with_capacity(IPV6_HDR_LEN + payload.len());
    // 32-bit ver/tc/flow word: version 6 + zero traffic class + zero label.
    p.extend_from_slice(&[0x60, 0x00, 0x00, 0x00]);
    let len = payload.len() as u16;
    p.extend_from_slice(&len.to_be_bytes());
    p.push(next_header);
    p.push(hop_limit);
    p.extend_from_slice(&src.0);
    p.extend_from_slice(&dst.0);
    debug_assert_eq!(p.len(), IPV6_HDR_LEN);
    p.extend_from_slice(payload);
    p
}

/// Parsed IPv6 fixed header.
pub struct Ipv6Header {
    pub src: Ipv6Addr,
    pub dst: Ipv6Addr,
    /// Upper-layer protocol number AFTER skipping extensions.
    pub proto: u8,
    #[allow(dead_code)]
    pub hop_limit: u8,
    /// Byte offset of the upper-layer payload inside `pkt`.
    pub payload_off: usize,
}

/// Parse an IPv6 packet: fixed header + skip Hop-by-Hop / Routing /
/// Destination-Options extension headers. Fragmented packets are returned with
/// `proto == IPV6_EXT_FRAGMENT` and `payload_off` pointing at the fragment
/// header so the caller can run reassembly.
pub fn ipv6_parse(pkt: &[u8]) -> Option<(Ipv6Header, &[u8])> {
    if pkt.len() < IPV6_HDR_LEN || pkt[0] >> 4 != 6 {
        return None;
    }
    let payload_len = u16::from_be_bytes([pkt[4], pkt[5]]) as usize;
    if pkt.len() < IPV6_HDR_LEN + payload_len {
        return None; // truncated
    }
    let end = IPV6_HDR_LEN + payload_len;
    let mut hdr = Ipv6Header {
        src: Ipv6Addr([0; 16]),
        dst: Ipv6Addr([0; 16]),
        proto: pkt[6],
        hop_limit: pkt[7],
        payload_off: IPV6_HDR_LEN,
    };
    hdr.src = Ipv6Addr(pkt[8..24].try_into().ok()?);
    hdr.dst = Ipv6Addr(pkt[24..40].try_into().ok()?);

    // Skip non-fragment extension headers.
    let mut nh = pkt[6];
    let mut off = IPV6_HDR_LEN;
    let mut hops = 0;
    while matches!(nh, IPV6_EXT_HOPBYHOP | IPV6_EXT_ROUTING | IPV6_EXT_DESTOPT) {
        if off >= end {
            return None;
        }
        let ext_len = ((pkt[off + 1] as usize) + 1) * 8;
        if off + ext_len > end {
            return None;
        }
        nh = pkt[off];
        off += ext_len;
        hops += 1;
        if hops > 8 {
            return None;
        }
    }
    hdr.proto = nh;
    hdr.payload_off = off;
    Some((hdr, &pkt[off..end]))
}

/// Parse a fragment header at `payload` start (proto was 44). Returns
/// `(ident, offset_bytes, more_fragments, data_after_frag_header)`.
pub fn ipv6_fragment_parse(payload: &[u8]) -> Option<(u32, usize, bool, &[u8])> {
    if payload.len() < 8 {
        return None;
    }
    let ident = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let word = u16::from_be_bytes([payload[4], payload[5]]);
    let more = word & 0x0001 != 0;
    let off_units = word >> 3;
    Some((ident, off_units as usize * 8, more, &payload[8..]))
}

// ─── ICMP ────────────────────────────────────────────────────────────────────

pub const ICMP_ECHO_REPLY: u8 = 0;
#[allow(dead_code)]
pub const ICMP_DEST_UNREACH: u8 = 3;
pub const ICMP_ECHO_REQUEST: u8 = 8;

/// Build an ICMP echo (request or reply) message with a valid checksum.
pub fn icmp_echo_build(kind: u8, ident: u16, seq: u16, payload: &[u8]) -> alloc::vec::Vec<u8> {
    let mut p = alloc::vec::Vec::with_capacity(8 + payload.len());
    p.push(kind);
    p.push(0); // code
    p.extend_from_slice(&[0, 0]); // checksum placeholder
    p.extend_from_slice(&ident.to_be_bytes());
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(payload);
    let csum = checksum(&[&p]);
    p[2] = (csum >> 8) as u8;
    p[3] = (csum & 0xFF) as u8;
    p
}

/// Parse an ICMP echo message. Returns `(kind, ident, seq, payload)`.
pub fn icmp_echo_parse(p: &[u8]) -> Option<(u8, u16, u16, &[u8])> {
    if p.len() < 8 {
        return None;
    }
    let kind = p[0];
    if kind != ICMP_ECHO_REQUEST && kind != ICMP_ECHO_REPLY {
        return None;
    }
    if !checksum_valid(p, 2) {
        return None;
    }
    Some((
        kind,
        u16::from_be_bytes([p[4], p[5]]),
        u16::from_be_bytes([p[6], p[7]]),
        &p[8..],
    ))
}

// ─── ICMPv6 + Neighbor Discovery (RFC 4861) ─────────────────────────────────

pub const ICMPV6_ROUTER_SOLICIT: u8 = 133;
pub const ICMPV6_ECHO_REQUEST: u8 = 128;
pub const ICMPV6_ECHO_REPLY: u8 = 129;
pub const ICMPV6_ROUTER_ADVERT: u8 = 134;
pub const ICMPV6_NEIGHBOR_SOLICIT: u8 = 135;
pub const ICMPV6_NEIGHBOR_ADVERT: u8 = 136;

/// NDP option kinds.
pub const ND_OPT_SOURCE_LL: u8 = 1;
pub const ND_OPT_TARGET_LL: u8 = 2;
pub const ND_OPT_PREFIX_INFO: u8 = 3;

/// Build an ICMPv6 message with the MANDATORY pseudo-header checksum.
/// `msg` must have its checksum bytes (offsets 2..4) zeroed — this helper
/// writes them.
#[allow(clippy::too_many_arguments)]
pub fn icmpv6_build(
    src: Ipv6Addr,
    dst: Ipv6Addr,
    kind: u8,
    code: u8,
    body: &[u8],
) -> alloc::vec::Vec<u8> {
    let mut p = alloc::vec::Vec::with_capacity(4 + body.len());
    p.push(kind);
    p.push(code);
    p.extend_from_slice(&[0, 0]); // checksum placeholder
    p.extend_from_slice(body);

    let mut buf = alloc::vec::Vec::with_capacity(44 + body.len());
    buf.extend_from_slice(&src.0);
    buf.extend_from_slice(&dst.0);
    buf.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
    buf.extend_from_slice(&[0, 0, 0, PROTO_ICMPV6]);
    buf.extend_from_slice(&p);
    let csum = checksum(&[&buf]);
    p[2] = (csum >> 8) as u8;
    p[3] = (csum & 0xFF) as u8;
    p
}

/// Parse + verify an ICMPv6 echo request/reply. Returns `(kind, ident, seq, payload)`.
pub fn icmpv6_echo_parse(
    src: Ipv6Addr,
    dst: Ipv6Addr,
    msg: &[u8],
) -> Option<(u8, u16, u16, &[u8])> {
    if msg.len() < 8 {
        return None;
    }
    let kind = msg[0];
    if kind != ICMPV6_ECHO_REQUEST && kind != ICMPV6_ECHO_REPLY {
        return None;
    }
    // Verify pseudo-header checksum with the stored value zeroed.
    let stored = u16::from_be_bytes([msg[2], msg[3]]);
    let mut buf = alloc::vec::Vec::with_capacity(40 + msg.len());
    buf.extend_from_slice(&src.0);
    buf.extend_from_slice(&dst.0);
    buf.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    buf.extend_from_slice(&[0, 0, 0, PROTO_ICMPV6]);
    buf.extend_from_slice(msg);
    buf[40 + 2] = 0;
    buf[40 + 3] = 0;
    if checksum(&[&buf]) != stored {
        return None;
    }
    Some((
        kind,
        u16::from_be_bytes([msg[4], msg[5]]),
        u16::from_be_bytes([msg[6], msg[7]]),
        &msg[8..],
    ))
}

/// A parsed Neighbor Solicitation / Advertisement.
pub struct NdpPacket {
    pub is_solicit: bool,
    /// NA flags: solicited (bit 6), override (bit 5), router (bit 7).
    #[allow(dead_code)]
    pub na_flags: u8,
    pub target: Ipv6Addr,
    /// Source/target link-layer address option when present.
    pub ll_addr: Option<Mac>,
}

pub fn ndp_parse(src: Ipv6Addr, dst: Ipv6Addr, msg: &[u8]) -> Option<NdpPacket> {
    if msg.len() < 24 {
        return None;
    }
    let kind = msg[0];
    if !matches!(kind, ICMPV6_NEIGHBOR_SOLICIT | ICMPV6_NEIGHBOR_ADVERT) {
        return None;
    }
    // Checksum verification (same pseudo-header shape as any ICMPv6).
    let stored = u16::from_be_bytes([msg[2], msg[3]]);
    let mut buf = alloc::vec::Vec::with_capacity(40 + msg.len());
    buf.extend_from_slice(&src.0);
    buf.extend_from_slice(&dst.0);
    buf.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    buf.extend_from_slice(&[0, 0, 0, PROTO_ICMPV6]);
    buf.extend_from_slice(msg);
    buf[40 + 2] = 0;
    buf[40 + 3] = 0;
    if checksum(&[&buf]) != stored {
        return None;
    }

    let target = Ipv6Addr(msg[8..24].try_into().ok()?);
    let mut ll_addr = None;
    // Walk options after the fixed 24 bytes.
    let mut i = 24usize;
    while i < msg.len() {
        let opt = msg[i];
        if opt == 0 {
            break; // padding
        }
        if i + 1 >= msg.len() {
            break;
        }
        let len_units = msg[i + 1] as usize;
        if len_units == 0 {
            break;
        }
        let len = len_units * 8;
        if i + len > msg.len() {
            break;
        }
        match (opt, len) {
            (ND_OPT_SOURCE_LL, 8) | (ND_OPT_TARGET_LL, 8) => {
                ll_addr = Some(Mac([
                    msg[i + 2],
                    msg[i + 3],
                    msg[i + 4],
                    msg[i + 5],
                    msg[i + 6],
                    msg[i + 7],
                ]));
            }
            _ => {}
        }
        i += len;
    }

    Some(NdpPacket {
        is_solicit: kind == ICMPV6_NEIGHBOR_SOLICIT,
        na_flags: msg[4],
        target,
        ll_addr,
    })
}

/// Prefix Information option from a Router Advertisement (RFC 4862 §4.6.2).
pub struct PrefixInfo {
    pub on_link: bool,
    pub autonomous: bool,
    pub valid_lifetime_secs: u32,
    #[allow(dead_code)]
    pub preferred_lifetime_secs: u32,
    pub prefix: Ipv6Addr,
    pub prefix_len: u8,
}

/// Extract the first Prefix Information option from an RA message body
/// (`msg` starts at the ICMPv6 header). Verifies the checksum.
pub fn ra_parse_prefix(src: Ipv6Addr, dst: Ipv6Addr, msg: &[u8]) -> Option<PrefixInfo> {
    if msg.len() < 16 || msg[0] != ICMPV6_ROUTER_ADVERT {
        return None;
    }
    let stored = u16::from_be_bytes([msg[2], msg[3]]);
    let mut buf = alloc::vec::Vec::with_capacity(40 + msg.len());
    buf.extend_from_slice(&src.0);
    buf.extend_from_slice(&dst.0);
    buf.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    buf.extend_from_slice(&[0, 0, 0, PROTO_ICMPV6]);
    buf.extend_from_slice(msg);
    buf[40 + 2] = 0;
    buf[40 + 3] = 0;
    if checksum(&[&buf]) != stored {
        return None;
    }

    let mut i = 16usize; // RA fixed part: hop limit, flags, lifetime(2), reachable, retrans
    while i < msg.len() {
        let opt = msg[i];
        if opt == 0 || i + 1 >= msg.len() {
            break;
        }
        let len_units = msg[i + 1] as usize;
        if len_units == 0 {
            break;
        }
        let len = len_units * 8;
        if i + len > msg.len() {
            break;
        }
        if opt == ND_OPT_PREFIX_INFO && len >= 32 {
            // Layout: type(0) len(1) PREFIX-LEN(2) FLAGS(3) valid(4..8)
            //         preferred(8..12) reserved(12..16) prefix(16..32).
            let prefix_len = msg[i + 2];
            let flags = msg[i + 3];
            let valid = u32::from_be_bytes([msg[i + 4], msg[i + 5], msg[i + 6], msg[i + 7]]);
            let preferred = u32::from_be_bytes([msg[i + 8], msg[i + 9], msg[i + 10], msg[i + 11]]);
            return Some(PrefixInfo {
                on_link: flags & 0x80 != 0,
                autonomous: flags & 0x40 != 0,
                valid_lifetime_secs: valid,
                preferred_lifetime_secs: preferred,
                prefix_len,
                prefix: Ipv6Addr(msg[i + 16..i + 32].try_into().ok()?),
            });
        }
        i += len;
    }
    None
}

/// Build a Router Solicitation (RFC 4861 §4.1) with our source LL option.
pub fn router_solicit(src_ip: Ipv6Addr, src_mac: Mac) -> alloc::vec::Vec<u8> {
    // body: flags(4) + option(2+6)
    let mut body = alloc::vec![0u8, 0, 0, 0];
    body.extend_from_slice(&[ND_OPT_SOURCE_LL, 1]);
    body.extend_from_slice(&src_mac.0);
    icmpv6_build(
        src_ip,
        Ipv6Addr::ALL_ROUTERS,
        ICMPV6_ROUTER_SOLICIT,
        0,
        &body,
    )
}

/// Build a Neighbor Solicitation for `target` (checksum included).
pub fn neighbor_solicit(src_ip: Ipv6Addr, src_mac: Mac, target: Ipv6Addr) -> alloc::vec::Vec<u8> {
    let mut body = alloc::vec::Vec::with_capacity(24 + 8);
    body.extend_from_slice(&[0, 0, 0, 0]); // reserved
    body.extend_from_slice(&target.0);
    // Source link-layer address option.
    body.extend_from_slice(&[ND_OPT_SOURCE_LL, 1]);
    body.extend_from_slice(&src_mac.0);
    icmpv6_build(
        src_ip,
        target.solicited_node(),
        ICMPV6_NEIGHBOR_SOLICIT,
        0,
        &body,
    )
}

/// Build an advertised Neighbor Advertisement for `target` from our MAC.
pub fn neighbor_advert(
    src_ip: Ipv6Addr,
    dst_ip: Ipv6Addr,
    #[allow(unused_variables)] dst_mac: Mac,
    src_mac: Mac,
    target: Ipv6Addr,
    solicited: bool,
) -> alloc::vec::Vec<u8> {
    let mut body = alloc::vec::Vec::with_capacity(24 + 8);
    body.push(if solicited { 0x20 | 0x80 } else { 0x80 }); // R|S|O bits
    body.extend_from_slice(&[0, 0, 0]); // pad to flags+reserved(4)
    body.extend_from_slice(&target.0);
    body.extend_from_slice(&[ND_OPT_TARGET_LL, 1]);
    body.extend_from_slice(&src_mac.0);
    icmpv6_build(src_ip, dst_ip, ICMPV6_NEIGHBOR_ADVERT, 0, &body)
}

/// Multicast MAC for an IPv6 multicast destination: 33:33:<last 4 octets>.
pub fn ipv6_multicast_mac(addr: Ipv6Addr) -> Mac {
    Mac([0x33, 0x33, addr.0[12], addr.0[13], addr.0[14], addr.0[15]])
}

// ─── UDP ─────────────────────────────────────────────────────────────────────

pub const UDP_HDR_LEN: usize = 8;

/// Build a UDP datagram with pseudo-header checksum. Returns the full datagram
/// ready to hand to [`ipv4_build`] / [`ipv6_build`].
pub fn udp_build(src: IpEndpoint, dst: IpEndpoint, payload: &[u8]) -> alloc::vec::Vec<u8> {
    let udp_len = (UDP_HDR_LEN + payload.len()) as u16;
    let mut p = alloc::vec::Vec::with_capacity(UDP_HDR_LEN + payload.len());
    p.extend_from_slice(&src.port.to_be_bytes());
    p.extend_from_slice(&dst.port.to_be_bytes());
    p.extend_from_slice(&udp_len.to_be_bytes());
    p.extend_from_slice(&[0, 0]); // checksum placeholder
    p.extend_from_slice(payload);

    // Pseudo-header checksum (RFC 768 / RFC 2460 §8.1). Mandatory for IPv6.
    let mut buf = alloc::vec::Vec::with_capacity(40 + p.len());
    match (&src.addr, &dst.addr) {
        (&IpAddr::V4(s), &IpAddr::V4(d)) => {
            buf.extend_from_slice(&s.0);
            buf.extend_from_slice(&d.0);
            buf.extend_from_slice(&[0, PROTO_UDP]);
            buf.extend_from_slice(&udp_len.to_be_bytes());
        }
        (&IpAddr::V6(s), &IpAddr::V6(d)) => {
            buf.extend_from_slice(&s.0);
            buf.extend_from_slice(&d.0);
            buf.extend_from_slice(&(udp_len as u32).to_be_bytes());
            buf.extend_from_slice(&[0, 0, 0, PROTO_UDP]);
        }
        _ => {} // mixed-family endpoints are a caller bug; checksum will mismatch
    }
    buf.extend_from_slice(&p);
    let csum = checksum(&[&buf]);
    let csum = if csum == 0 { 0xFFFF } else { csum };
    p[6] = (csum >> 8) as u8;
    p[7] = (csum & 0xFF) as u8;
    p
}

/// Parse a UDP datagram (payload of an IPv4/IPv6 packet). Returns
/// `(src_endpoint, dst_endpoint, payload)`; verifies the checksum when non-zero.
/// For IPv6 the checksum is mandatory and always verified.
pub fn udp_parse(
    src_ip: IpAddr,
    dst_ip: IpAddr,
    dgram: &[u8],
) -> Option<(IpEndpoint, IpEndpoint, &[u8])> {
    if dgram.len() < UDP_HDR_LEN {
        return None;
    }
    let sp = u16::from_be_bytes([dgram[0], dgram[1]]);
    let dp = u16::from_be_bytes([dgram[2], dgram[3]]);
    let len = u16::from_be_bytes([dgram[4], dgram[5]]) as usize;
    if len < UDP_HDR_LEN || len > dgram.len() {
        return None;
    }
    let stored = u16::from_be_bytes([dgram[6], dgram[7]]);
    let v6_mandatory = matches!(src_ip, IpAddr::V6(_));
    if stored != 0 || v6_mandatory {
        let mut verify = alloc::vec::Vec::with_capacity(40 + len);
        match (&src_ip, &dst_ip) {
            (&IpAddr::V4(s), &IpAddr::V4(d)) => {
                verify.extend_from_slice(&s.0);
                verify.extend_from_slice(&d.0);
                verify.extend_from_slice(&[0, PROTO_UDP]);
                verify.extend_from_slice(&(len as u16).to_be_bytes());
                verify.extend_from_slice(&dgram[..len]);
                verify[12 + 6] = 0;
                verify[12 + 7] = 0;
            }
            (&IpAddr::V6(s), &IpAddr::V6(d)) => {
                verify.extend_from_slice(&s.0);
                verify.extend_from_slice(&d.0);
                verify.extend_from_slice(&(len as u32).to_be_bytes());
                verify.extend_from_slice(&[0, 0, 0, PROTO_UDP]);
                verify.extend_from_slice(&dgram[..len]);
                verify[40 + 6] = 0;
                verify[40 + 7] = 0;
            }
            _ => return None,
        }
        if checksum(&[&verify]) != stored {
            return None;
        }
    }
    Some((
        IpEndpoint::new(src_ip, sp),
        IpEndpoint::new(dst_ip, dp),
        &dgram[UDP_HDR_LEN..len],
    ))
}

// ─── TCP ─────────────────────────────────────────────────────────────────────

pub const TCP_HDR_MIN: usize = 20;

// TCP header flag bits (byte 13).
pub const TCP_FIN: u8 = 1 << 0;
pub const TCP_SYN: u8 = 1 << 1;
pub const TCP_RST: u8 = 1 << 2;
pub const TCP_PSH: u8 = 1 << 3;
pub const TCP_ACK: u8 = 1 << 4;
#[allow(dead_code)]
pub const TCP_URG: u8 = 1 << 5;

/// Parsed TCP segment header (+ parsed interesting options).
pub struct TcpHeader {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    /// Header length in bytes (data offset * 4).
    #[allow(dead_code)]
    pub hdr_len: usize,
    /// Advertised MSS option value (absent -> 536 default).
    pub mss: Option<u16>,
    /// Window-scale option factor (absent -> no scaling negotiated).
    pub wscale: Option<u8>,
    /// SACK-Permitted option present (kind 4, SYN only).
    pub sack_permitted: bool,
    /// SACK blocks (kind 5), left edge inclusive / right edge exclusive.
    pub sack_blocks: heapless_shim::Vec<(u32, u32), 4>,
}

impl core::fmt::Debug for TcpHeader {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TcpHeader")
            .field("src_port", &self.src_port)
            .field("dst_port", &self.dst_port)
            .field("seq", &self.seq)
            .field("ack", &self.ack)
            .field("flags", &self.flags)
            .field("window", &self.window)
            .field("mss", &self.mss)
            .field("wscale", &self.wscale)
            .field("sack_permitted", &self.sack_permitted)
            .finish()
    }
}

/// Tiny fixed-capacity Vec stand-in for option parsing without allocation.
pub mod heapless_shim {
    use core::mem::MaybeUninit;

    /// Fixed-capacity stack vector for up to N items.
    pub struct Vec<T, const N: usize> {
        len: usize,
        items: [MaybeUninit<T>; N],
    }

    impl<T: Copy, const N: usize> Vec<T, N> {
        pub const fn new() -> Self {
            Vec {
                len: 0,
                items: [const { MaybeUninit::uninit() }; N],
            }
        }

        pub fn push(&mut self, v: T) {
            if self.len < N {
                self.items[self.len] = MaybeUninit::new(v);
                self.len += 1;
            }
        }

        pub fn iter(&self) -> impl Iterator<Item = &T> {
            self.items[..self.len]
                .iter()
                .map(|slot| unsafe { slot.assume_init_ref() })
        }
    }

    impl<T: Copy, const N: usize> Default for Vec<T, N> {
        fn default() -> Self {
            Self::new()
        }
    }

    impl<T: Copy, const N: usize> Vec<T, N> {
        /// Convenience constructor for literals (tests and diagnostics).
        #[allow(dead_code)]
        pub fn from_slice(src: &[T]) -> Self {
            let mut v = Vec::new();
            for &x in src {
                v.push(x);
            }
            v
        }
    }
}

/// Parse a TCP segment header and validate its checksum against the enclosing
/// IPv4 pseudo-header.
pub fn tcp_parse(src_ip: IpAddr, dst_ip: IpAddr, seg: &[u8]) -> Option<(TcpHeader, &[u8])> {
    if seg.len() < TCP_HDR_MIN {
        return None;
    }
    let data_off = (seg[12] >> 4) as usize * 4;
    if data_off < TCP_HDR_MIN || seg.len() < data_off {
        return None;
    }
    let mut hdr = TcpHeader {
        src_port: u16::from_be_bytes([seg[0], seg[1]]),
        dst_port: u16::from_be_bytes([seg[2], seg[3]]),
        seq: u32::from_be_bytes([seg[4], seg[5], seg[6], seg[7]]),
        ack: u32::from_be_bytes([seg[8], seg[9], seg[10], seg[11]]),
        flags: seg[13],
        window: u16::from_be_bytes([seg[14], seg[15]]),
        hdr_len: data_off,
        mss: None,
        wscale: None,
        sack_permitted: false,
        sack_blocks: heapless_shim::Vec::new(),
    };

    // Walk options for MSS (kind 2) and window scale (kind 3).
    let mut i = TCP_HDR_MIN;
    while i < data_off {
        let kind = seg[i];
        match kind {
            0 => break,  // EOL
            1 => i += 1, // NOP
            n => {
                if i + 1 >= data_off {
                    break;
                }
                let len = seg[i + 1] as usize;
                if len < 2 || i + len > data_off {
                    break;
                }
                match (n, len) {
                    (2, 4) => hdr.mss = Some(u16::from_be_bytes([seg[i + 2], seg[i + 3]])),
                    (3, 3) => hdr.wscale = Some(seg[i + 2]),
                    (4, 2) => hdr.sack_permitted = true,
                    (5, l) if l >= 10 => {
                        // Up to 4 SACK blocks of (left, right) edges.
                        let mut j = i + 2;
                        while j + 8 <= i + l && hdr.sack_blocks.iter().count() < 4 {
                            let left =
                                u32::from_be_bytes([seg[j], seg[j + 1], seg[j + 2], seg[j + 3]]);
                            let right = u32::from_be_bytes([
                                seg[j + 4],
                                seg[j + 5],
                                seg[j + 6],
                                seg[j + 7],
                            ]);
                            hdr.sack_blocks.push((left, right));
                            j += 8;
                        }
                    }
                    _ => {}
                }
                i += len;
            }
        }
    }

    // Checksum over pseudo-header + segment: recompute with the field treated
    // as zero and require an exact match with the stored value.
    let stored = u16::from_be_bytes([seg[16], seg[17]]);
    let mut buf = alloc::vec::Vec::with_capacity(40 + seg.len());
    match (&src_ip, &dst_ip) {
        (&IpAddr::V4(s), &IpAddr::V4(d)) => {
            buf.extend_from_slice(&s.0);
            buf.extend_from_slice(&d.0);
            buf.extend_from_slice(&[0, PROTO_TCP]);
            buf.extend_from_slice(&(seg.len() as u16).to_be_bytes());
            const PH: usize = 12;
            buf.extend_from_slice(seg);
            buf[PH + 16] = 0;
            buf[PH + 17] = 0;
        }
        (&IpAddr::V6(s), &IpAddr::V6(d)) => {
            buf.extend_from_slice(&s.0);
            buf.extend_from_slice(&d.0);
            buf.extend_from_slice(&(seg.len() as u32).to_be_bytes());
            buf.extend_from_slice(&[0, 0, 0, PROTO_TCP]);
            const PH: usize = 40;
            buf.extend_from_slice(seg);
            buf[PH + 16] = 0;
            buf[PH + 17] = 0;
        }
        _ => return None,
    }
    if checksum(&[&buf]) != stored {
        return None;
    }

    Some((hdr, &seg[data_off..]))
}

/// Build a TCP segment. `options` must be a multiple of 4 bytes (already
/// padded by the caller). The checksum is computed over the pseudo-header.
#[allow(clippy::too_many_arguments)]
pub fn tcp_build(
    src: IpEndpoint,
    dst: IpEndpoint,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    options: &[u8],
    payload: &[u8],
) -> alloc::vec::Vec<u8> {
    let hdr_len = TCP_HDR_MIN + options.len();
    let total = hdr_len + payload.len();
    let mut p = alloc::vec::Vec::with_capacity(total);
    p.extend_from_slice(&src.port.to_be_bytes());
    p.extend_from_slice(&dst.port.to_be_bytes());
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&ack.to_be_bytes());
    p.push(((hdr_len as u8) / 4) << 4);
    p.push(flags);
    p.extend_from_slice(&window.to_be_bytes());
    p.extend_from_slice(&[0, 0]); // checksum placeholder
    p.extend_from_slice(&[0, 0]); // urgent pointer
    debug_assert!(options.len().is_multiple_of(4));
    p.extend_from_slice(options);
    p.extend_from_slice(payload);

    let mut buf = alloc::vec::Vec::with_capacity(40 + total);
    match (&src.addr, &dst.addr) {
        (&IpAddr::V4(s), &IpAddr::V4(d)) => {
            buf.extend_from_slice(&s.0);
            buf.extend_from_slice(&d.0);
            buf.extend_from_slice(&[0, PROTO_TCP]);
            buf.extend_from_slice(&(total as u16).to_be_bytes());
        }
        (&IpAddr::V6(s), &IpAddr::V6(d)) => {
            buf.extend_from_slice(&s.0);
            buf.extend_from_slice(&d.0);
            buf.extend_from_slice(&(total as u32).to_be_bytes());
            buf.extend_from_slice(&[0, 0, 0, PROTO_TCP]);
        }
        _ => {} // mixed families are a caller bug; the checksum will mismatch
    }
    buf.extend_from_slice(&p);
    let csum = checksum(&[&buf]);
    p[16] = (csum >> 8) as u8;
    p[17] = (csum & 0xFF) as u8;
    p
}

/// Encode up to four SACK blocks as a padded TCP option block
/// (NOP NOP + kind 5). Empty input yields an empty block.
pub fn sack_option(blocks: &[(u32, u32)]) -> alloc::vec::Vec<u8> {
    let n = blocks.len().min(4);
    if n == 0 {
        return alloc::vec::Vec::new();
    }
    let mut o = alloc::vec::Vec::with_capacity(2 + 2 + 8 * n);
    o.extend_from_slice(&[1, 1]); // NOPs align kind 5 to its final position
    o.push(5);
    o.push((2 + 8 * n) as u8);
    for &(l, r) in &blocks[..n] {
        o.extend_from_slice(&l.to_be_bytes());
        o.extend_from_slice(&r.to_be_bytes());
    }
    o
}

/// SACK-Permitted option (kind 4) for SYN/SYN-ACK segments.
pub fn sack_permitted_option() -> alloc::vec::Vec<u8> {
    alloc::vec![4, 2]
}

// ─── Tests (host-includable) ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_rfc1071_example() {
        // Classic example: sum of 0001 f203 f4f5 f6f7 -> checksum 220d.
        let data = [0x00u8, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(checksum(&[&data]), 0x220d);
    }

    #[test]
    fn checksum_odd_length_across_slices() {
        // Same bytes split oddly across slices must agree.
        let data = [0x00u8, 0x01, 0xf2, 0x03, 0xf4];
        let whole = checksum(&[&data]);
        let split = checksum(&[&data[..2], &data[2..]]);
        assert_eq!(whole, split);
    }

    #[test]
    fn ipv4_roundtrip_and_bad_checksum() {
        let pkt = ipv4_build(
            Ipv4Addr::new(10, 0, 2, 15),
            Ipv4Addr::new(10, 0, 2, 2),
            PROTO_TCP,
            0x1234,
            0,
            64,
            &[1, 2, 3, 4],
        );
        let (h, payload) = ipv4_parse(&pkt).expect("parses");
        assert_eq!(h.src, Ipv4Addr::new(10, 0, 2, 15));
        assert_eq!(h.dst, Ipv4Addr::new(10, 0, 2, 2));
        assert_eq!(payload, &[1, 2, 3, 4]);

        // Corrupt a HEADER byte (the payload is not covered by the header
        // checksum): parsing must reject it.
        let mut bad = pkt.clone();
        bad[8] ^= 0xFF; // TTL
        assert!(ipv4_parse(&bad).is_none());
    }

    #[test]
    fn udp_roundtrip() {
        let src = IpEndpoint::new(IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)), 68);
        let dst = IpEndpoint::new(IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255)), 67);
        let dgram = udp_build(src, dst, b"hello");
        let (s, d, payload) = udp_parse(src.addr, dst.addr, &dgram).expect("parses");
        assert_eq!(s, src);
        assert_eq!(d, dst);
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn tcp_roundtrip_with_options() {
        let src = IpEndpoint::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 12345);
        let dst = IpEndpoint::new(IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8)), 443);
        // MSS + WS options, padded to a 4-byte boundary with NOP.
        let opts = [2u8, 4, 0x05, 0xB4, 1, 3, 3, 7];
        let seg = tcp_build(src, dst, 100, 200, TCP_SYN | TCP_ACK, 4096, &opts, b"body");
        let (h, payload) = tcp_parse(src.addr, dst.addr, &seg).expect("parses");
        assert_eq!(h.src_port, 12345);
        assert_eq!(h.dst_port, 443);
        assert_eq!(h.seq, 100);
        assert_eq!(h.ack, 200);
        assert_eq!(h.flags, TCP_SYN | TCP_ACK);
        assert_eq!(h.window, 4096);
        assert_eq!(h.mss, Some(1460));
        assert_eq!(h.wscale, Some(7));
        assert_eq!(payload, b"body");
    }

    #[test]
    fn tcp_bad_checksum_rejected() {
        let src = IpEndpoint::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 1);
        let dst = IpEndpoint::new(IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8)), 2);
        let mut seg = tcp_build(src, dst, 0, 0, TCP_ACK, 100, &[], b"x");
        seg[20] ^= 0xFF;
        assert!(tcp_parse(src.addr, dst.addr, &seg).is_none());
    }

    #[test]
    fn subnet_membership() {
        let cidr = Ipv4Cidr::new(Ipv4Addr::new(10, 0, 2, 15), 24);
        assert_eq!(cidr.mask_octets(), [255, 255, 255, 0]);
        assert!(Ipv4Addr::new(10, 0, 2, 99).in_subnet(cidr));
        assert!(!Ipv4Addr::new(10, 0, 3, 1).in_subnet(cidr));

        let p8 = Ipv4Cidr::new(Ipv4Addr::new(10, 0, 2, 15), 8);
        assert_eq!(p8.mask_octets(), [255, 0, 0, 0]);
        let p0 = Ipv4Cidr::new(Ipv4Addr::new(10, 0, 2, 15), 0);
        assert_eq!(p0.mask_octets(), [0, 0, 0, 0]);
        let p32 = Ipv4Cidr::new(Ipv4Addr::new(10, 0, 2, 15), 32);
        assert_eq!(p32.mask_octets(), [255, 255, 255, 255]);
    }

    #[test]
    fn ipv6_display_roundtrip() {
        // Compression of the longest zero run, RFC 5952 style.
        let a = parse_ipv6_literal("2001:db8:0:0:0:0:0:1").unwrap();
        assert_eq!(a.to_string(), "2001:db8::1");
        let b = parse_ipv6_literal("fe80::1").unwrap();
        assert!(b.is_link_local());
        assert_eq!(b.to_string(), "fe80::1");

        // Round-trips through our own parser both ways.
        for text in [
            "::",
            "::1",
            "fe80::",
            "2001:db8:85a3::8a2e:370:7334",
            "::ffff:192.168.1.1",
        ] {
            let parsed = parse_ipv6_literal(text).expect(text);
            assert_eq!(parse_ipv6_literal(&parsed.to_string()), Some(parsed));
        }

        // Invalid forms rejected.
        assert!(parse_ipv6_literal("1:2:3:4:5:6:7:8:9").is_none());
        assert!(parse_ipv6_literal("::::").is_none());
        assert!(parse_ipv6_literal("12345::").is_none());
    }

    #[test]
    fn v4_mapped_conversion() {
        let m = parse_ipv6_literal("::ffff:10.0.2.15").unwrap();
        assert_eq!(m.to_v4_mapped(), Some(Ipv4Addr::new(10, 0, 2, 15)));
        let not_mapped = parse_ipv6_literal("2001:db8::1").unwrap();
        assert_eq!(not_mapped.to_v4_mapped(), None);
        assert_eq!(
            IpAddr::V6(m).to_v4_mapped(),
            Some(Ipv4Addr::new(10, 0, 2, 15))
        );
    }

    #[test]
    fn ipv6_udp_tcp_checksum_roundtrip() {
        let src = IpEndpoint::new(IpAddr::V6(parse_ipv6_literal("fe80::1").unwrap()), 50000);
        let dst = IpEndpoint::new(IpAddr::V6(parse_ipv6_literal("fe80::2").unwrap()), 53);
        let dgram = udp_build(src, dst, b"v6payload");
        let (s, d, payload) = udp_parse(src.addr, dst.addr, &dgram).expect("v6 udp parses");
        assert_eq!(s, src);
        assert_eq!(d, dst);
        assert_eq!(payload, b"v6payload");

        // Corrupted datagram rejected (checksum is mandatory for v6).
        let mut bad = dgram.clone();
        bad[8] ^= 0xFF;
        assert!(udp_parse(src.addr, dst.addr, &bad).is_none());

        // TCP over v6.
        let csrc = IpEndpoint::new(src.addr, 40000);
        let cdst = IpEndpoint::new(dst.addr, 443);
        let seg = tcp_build(csrc, cdst, 1, 2, TCP_ACK, 1024, &[], b"body6");
        let (_, body) = tcp_parse(src.addr, dst.addr, &seg).expect("v6 tcp parses");
        assert_eq!(body, b"body6");
    }

    #[test]
    fn ndp_packet_roundtrip() {
        let my_ip = parse_ipv6_literal("fe80::1").unwrap();
        let target = parse_ipv6_literal("fe80::2").unwrap();
        let my_mac = Mac([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);

        let solicit = neighbor_solicit(my_ip, my_mac, target);
        // Solicitation goes to the solicited-node multicast address:
        // ff02::1:ffXX:XXXX carries the target's low 24 bits.
        let sn = target.solicited_node();
        assert_eq!(&sn.0[..12], &[0xFF, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01]);
        assert_eq!(&sn.0[13..], &target.0[13..]);
        let p = ndp_parse(target, my_ip.solicited_node(), &solicit).expect("solicit parses");
        assert!(p.is_solicit);
        assert_eq!(p.target, target);
        assert_eq!(p.ll_addr, Some(my_mac));

        let advert = neighbor_advert(target, my_ip, my_mac, my_mac, target, true);
        // Packet source is the advertiser's address (the target).
        let p = ndp_parse(target, my_ip, &advert).expect("advert parses");
        assert!(!p.is_solicit);
        // Builder emits R(0x80) + Solicited(0x20).
        assert_eq!(p.na_flags, 0xA0);
        assert_eq!(p.ll_addr, Some(my_mac));

        // Corrupt checksum -> rejected.
        let mut bad = advert.clone();
        bad[2] ^= 0xFF;
        assert!(ndp_parse(my_ip, my_ip, &bad).is_none());
    }

    #[test]
    fn ra_prefix_parse_and_slaac_address() {
        let router_ip = parse_ipv6_literal("fe80::2").unwrap();
        let my_mac = Mac([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);

        // Build a minimal RA carrying one Prefix Information option:
        // /64 prefix 2001:db8:abcd:: on-link+autonomous, valid 3600s.
        // Body after the 4-byte ICMPv6 header.
        let mut body = alloc::vec::Vec::new();
        body.push(64); // current hop limit
        body.push(0x18); // M/O flags
        body.extend_from_slice(&1800u16.to_be_bytes()); // router lifetime
        body.extend_from_slice(&[0, 0, 0, 0]); // reachable time
        body.extend_from_slice(&[0, 0, 0, 0]); // retransmit timer
                                               // Prefix Information option (16 bytes of fixed part + 16-byte prefix).
        body.extend_from_slice(&[ND_OPT_PREFIX_INFO, 4]);
        body.push(64); // prefix length
        body.push(0xC0); // L=1, A=1
        body.extend_from_slice(&3600u32.to_be_bytes()); // valid
        body.extend_from_slice(&1800u32.to_be_bytes()); // preferred
        body.extend_from_slice(&[0, 0, 0, 0]); // reserved
        let prefix = Ipv6Addr([
            0x20, 0x01, 0x0D, 0xB8, 0xAB, 0xCD, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        body.extend_from_slice(&prefix.0);
        let msg = icmpv6_build(
            router_ip,
            Ipv6Addr::ALL_NODES,
            ICMPV6_ROUTER_ADVERT,
            0,
            &body,
        );

        let pi = ra_parse_prefix(router_ip, Ipv6Addr::ALL_NODES, &msg).expect("RA parses");
        assert!(pi.on_link && pi.autonomous);
        assert_eq!(pi.prefix_len, 64);
        assert_eq!(pi.valid_lifetime_secs, 3600);

        // SLAAC address = prefix || EUI-64 IID from MAC.
        let ll = Ipv6Addr::link_local_from_mac(my_mac);
        let mut addr = [0u8; 16];
        addr[..8].copy_from_slice(&pi.prefix.0[..8]);
        addr[8..].copy_from_slice(&ll.0[8..]);
        let slaac = Ipv6Addr(addr);
        assert_eq!(slaac.to_string(), "2001:db8:abcd:0:5054:ff:fe12:3456");
        assert!(slaac.in_subnet(Ipv6Cidr {
            addr: pi.prefix,
            prefix: 64
        }));

        // Corrupt checksum -> rejected.
        let mut bad = msg.clone();
        bad[3] ^= 0xFF;
        assert!(ra_parse_prefix(router_ip, Ipv6Addr::ALL_NODES, &bad).is_none());
    }

    #[test]
    fn sack_option_encoding() {
        assert_eq!(sack_option(&[]), Vec::<u8>::new());
        let o = sack_option(&[(100, 200)]);
        assert_eq!(o, vec![1, 1, 5, 10, 0, 0, 0, 100, 0, 0, 0, 200]);
        let two = sack_option(&[(100, 200), (300, 400)]);
        assert_eq!(two.len(), 2 + 2 + 16);
        // Block layout: [NOP,NOP,kind,len] then left/right pairs.
        assert_eq!(&two[4..12], &[0u8, 0, 0, 100, 0, 0, 0, 200]);
        assert_eq!(&two[12..20], &[0u8, 0, 1, 44, 0, 0, 1, 144]);
    }

    #[test]
    fn literal_parser() {
        assert_eq!(
            parse_ipv4_literal("10.0.2.15"),
            Some(Ipv4Addr::new(10, 0, 2, 15))
        );
        assert_eq!(parse_ipv4_literal("1.2.3"), None);
        assert_eq!(parse_ipv4_literal("1.2.3.4.5"), None);
        assert_eq!(parse_ipv4_literal("a.b.c.d"), None);
    }
}
