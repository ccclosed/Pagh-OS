//! Own link layer: Ethernet + ARP (RFC 826).
//!
//! [`ArpTable`] answers "which MAC belongs to IPv4 address X?" for the stack.
//! When the answer is unknown, outgoing frames are parked in a bounded pending
//! queue and an ARP request is broadcast (rate-limited to one request per
//! target per second); replies populate the cache and flush pending frames in
//! arrival order. Entries age out; pending frames expire.
//!
//! Everything runs under the stack lock in thread context (the net poll loop),
//! so no internal locking is needed.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::wire::{
    self, eth_frame, Ipv4Addr, Ipv6Addr, Mac, ETHERTYPE_ARP, ETHERTYPE_IPV6, ETH_HDR_LEN,
};
use super::Stack;

/// Cache capacity. QEMU user-net has exactly one peer; real LANs a handful.
const CACHE_SIZE: usize = 16;
/// Frames parked while awaiting resolution.
const PENDING_CAP: usize = 32;
/// Cache entry lifetime (ticks).
const ENTRY_TTL_TICKS: u64 = 60 * 1000;
/// Give up on a pending frame after this long.
const PENDING_TTL_TICKS: u64 = 5 * 1000;
/// Minimum interval between re-requests for the same target.
const REQUEST_RETRY_TICKS: u64 = 1000;
/// How many retries before dropping the resolution attempt (and its frames).
const MAX_REQUESTS: usize = 3;

#[derive(Clone, Copy)]
struct Entry {
    ip: Ipv4Addr,
    mac: Mac,
    expires: u64,
}

struct Pending {
    dst_ip: Ipv4Addr,
    ethertype: u16,
    /// Frame payload AFTER the Ethernet header (the L3 packet).
    payload: Vec<u8>,
    enqueued: u64,
}

pub struct ArpTable {
    entries: [Option<Entry>; CACHE_SIZE],
    /// Last-request bookkeeping per unresolved target: (ip, last_req_tick, tries).
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
        // Update in place if present.
        for slot in self.entries.iter_mut().flatten() {
            if slot.ip == ip {
                slot.mac = mac;
                slot.expires = now + ENTRY_TTL_TICKS;
                return;
            }
        }
        // Otherwise: prefer a free slot, then an expired one, then index 0.
        let mut victim: Option<usize> = self.entries.iter().position(|s| s.is_none());
        if victim.is_none() {
            victim = self
                .entries
                .iter()
                .position(|s| s.map(|e| e.expires <= now).unwrap_or(false));
        }
        let v = victim.unwrap_or(0);
        self.entries[v] = Some(Entry {
            ip,
            mac,
            expires: now + ENTRY_TTL_TICKS,
        });
    }

    /// Queue `payload` (an L3 packet) for delivery to `dst_ip` once its MAC is
    /// known. Returns false when the queue is full (frame dropped).
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

    /// Broadcast an ARP request for `ip` (rate-limited). `my_ip` may be
    /// unspecified during DHCP (sender protocol address 0.0.0.0).
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
        // Requests are broadcast: never go through resolution ourselves.
        let frame = eth_frame(Mac::BROADCAST, my_mac, ETHERTYPE_ARP, &pkt);
        crate::drivers::e1000::send(&frame);
    }

    /// Handle an incoming ARP packet. On REQUEST for our address (or broadcast)
    /// reply with our MAC; on any valid packet learn sender_ip -> sender_mac.
    pub fn input(&mut self, my_mac: Mac, my_ip: Option<Ipv4Addr>, pkt: &[u8], now: u64) {
        let Some(p) = wire::arp_parse(pkt) else {
            return;
        };

        // Learn from every packet (gratuitous/replies/requests alike).
        if !p.sender_ip.is_unspecified() {
            self.insert(p.sender_ip, p.sender_mac, now);
        }

        match p.operation {
            wire::ARP_REQUEST => {
                // Reply only when someone asks for US.
                let wants_us = my_ip.map(|i| i == p.target_ip).unwrap_or(false);
                if wants_us {
                    let reply = wire::arp_build(
                        wire::ARP_REPLY,
                        my_mac,
                        p.target_ip,
                        p.sender_mac,
                        p.sender_ip,
                    );
                    // Unicast reply straight to the requester's MAC.
                    let frame = eth_frame(p.sender_mac, my_mac, ETHERTYPE_ARP, &reply);
                    crate::drivers::e1000::send(&frame);
                }
            }
            wire::ARP_REPLY => {
                // Resolution complete: stop requesting, flush pending frames.
                self.inflight.retain(|(t, _, _)| *t != p.sender_ip);
                let mut ready: VecDeque<Pending> = VecDeque::new();
                while let Some(fr) = self.pending.pop_front() {
                    if fr.dst_ip == p.sender_ip {
                        let frame = eth_frame(p.sender_mac, my_mac, fr.ethertype, &fr.payload);
                        crate::drivers::e1000::send(&frame);
                    } else {
                        ready.push_back(fr);
                    }
                }
                self.pending = ready;
            }
            _ => {}
        }
    }

    /// Age out entries, expired pending frames, and dead resolution attempts.
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

    /// Number of frames currently waiting for resolution (diagnostics/tests).
    #[allow(dead_code)]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// Total Ethernet overhead constant re-exported for callers sizing buffers.
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

// ─── IPv6 neighbor table (NDP) ───────────────────────────────────────────────
//
// Mirrors the ARP table: cache + pending queue + rate-limited solicitations.
// The wire exchange is Neighbor Solicitation / Advertisement instead of ARP,
// and requests go to the target's solicited-node multicast address.

/// Hop limit for all NDP exchanges (RFC 4861: must be 255).
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

    /// Queue an L3 packet for `dst` while its MAC is unresolved.
    #[allow(dead_code)]
    pub fn enqueue_pending(&mut self, dst: Ipv6Addr, payload: Vec<u8>, now: u64) -> bool {
        if self.pending.len() >= PENDING_CAP {
            return false;
        }
        self.pending.push_back((dst, payload, now));
        true
    }

    /// Broadcast a Neighbor Solicitation for `ip`.
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
        // Full IPv6 packet to the solicited-node multicast address.
        let icmp = wire::neighbor_solicit(my_ip, my_mac, ip);
        let mcast_ip = ip.solicited_node();
        let pkt = wire::ipv6_build(my_ip, mcast_ip, wire::PROTO_ICMPV6, ND_HOP_LIMIT, &icmp);
        let frame = eth_frame(
            wire::ipv6_multicast_mac(mcast_ip),
            my_mac,
            ETHERTYPE_IPV6,
            &pkt,
        );
        crate::drivers::e1000::send(&frame);
    }

    /// Handle an incoming ICMPv6 NDP packet (already checksum-verified).
    /// `eth_src` is the Ethernet-level sender (used for learning when the
    /// packet carries no link-layer address option). Solicitations for one of
    /// OUR addresses are answered with a Neighbor Advertisement.
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
            // Learn the sender even from solicitations (RFC 4861 §7.2.3).
            if !src_ip.is_unspecified() {
                if let Some(mac) = p.ll_addr {
                    self.insert(src_ip, mac, now);
                }
            }
            // Answer only when someone asks for one of OUR addresses.
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
                crate::drivers::e1000::send(&frame);
            }
        } else {
            // Advertisement: learn target -> link-layer mapping.
            let learned = p.ll_addr.unwrap_or(eth_src);
            self.insert(p.target, learned, now);
            self.inflight.retain(|(t, _, _)| *t != p.target);
            // Flush frames waiting on this neighbor.
            let mut keep: VecDeque<(Ipv6Addr, Vec<u8>, u64)> = VecDeque::new();
            while let Some(fr) = self.pending.pop_front() {
                if fr.0 == p.target {
                    let frame = eth_frame(learned, my_mac, ETHERTYPE_IPV6, &fr.1);
                    crate::drivers::e1000::send(&frame);
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
