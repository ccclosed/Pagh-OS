//! Networking subsystem — the kernel's OWN TCP/IP stack.
//!
//! This is a from-scratch replacement for the previous smoltcp + virtio-net
//! pairing. The layering, bottom-up:
//!
//!   * [`crate::drivers::e1000`] — the NIC driver (polled, no IRQs).
//!   * [`arp`] — Ethernet framing decisions + ARP cache/resolution.
//!   * [`ip`] — IPv4 routing/fragmentation/reassembly + ICMP echo.
//!   * [`udp`] — UDP sockets + the DHCP client.
//!   * [`tcp`] — the TCP state machine, windows, timers, congestion control.
//!   * [`wire`] — pure wire-format types and checksums (host-testable).
//!
//! All mutable network state lives in [`Stack`] behind one spinlock ([`NET`]).
//! The locking discipline carried over from the previous design: every entry
//! point runs in THREAD context (the dedicated net thread, or shell/syscall
//! threads driving bounded locked-step pumps). Nothing ever touches these
//! structures from interrupt context, so lock nesting cannot deadlock.
//!
//! Bring-up sequence:
//!   1. [`init`] enumerates PCI, attaches the e1000 (`drivers::e1000::attach`)
//!      and builds an unconfigured [`Stack`].
//!   2. [`net_thread`] repeatedly calls [`poll`]: RX frames are drained and
//!      dispatched, the DHCP client runs (falling back to the static QEMU
//!      user-net address `10.0.2.15/24` gw `10.0.2.2` after a timeout), TCP
//!      output/retransmission steps run, and the demo echo services are fed.
//!   3. ICMP echo is answered natively by [`ip`]; `ping` uses it end-to-end.
//!
//! If no NIC is present, [`init`] returns `Err(NetError::NoDevice)`; the caller
//! logs a warning and boot continues (R17.3).

pub mod arp;
pub mod ca_bundle;
pub mod dns;
pub mod hostname;
pub mod http;
pub mod http_fetch;
pub mod ip;
pub mod progress;
pub mod tcp;
pub mod tls;
pub mod tls_chain;
pub mod tls_verify;
pub mod udp;
pub mod wire;
pub mod x509;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub use wire::{IpAddr, IpEndpoint, Ipv4Addr, Ipv4Cidr, Ipv6Addr};

use crate::sync::spinlock::Spinlock;
use crate::task::scheduler;
use crate::{info, warn};

/// A transport-layer socket handle (TCP or UDP — both tables index from 0).
pub type SocketHandle = usize;

pub use wire::Mac;

/// Errors produced by the networking subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    /// No usable NIC was discovered, so networking is unavailable.
    NoDevice,
    /// A NIC was found but initialisation failed, or a socket operation could
    /// not complete.
    DeviceInit,
}

/// Static fallback address used when DHCP does not complete (R13.3). These are
/// the well-known QEMU user-mode networking values.
const FALLBACK_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const FALLBACK_PREFIX: u8 = 24;
const FALLBACK_GW: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

/// QEMU user-mode networking always answers DNS at this address, so it is the
/// resolver of last resort (R13.3).
const QEMU_DNS: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);

/// Per-step `spin_loop` hint count between DNS pump steps (mirrors `nc_echo`).
const DNS_STEP_SPIN: u32 = 20_000;
/// DNS resolve timeout (~700 ms).
const DNS_TIMEOUT_TICKS: u64 = crate::arch::x86_64::apic::ms_to_ticks(700);
/// DNS query retransmit interval: if no answer has arrived after this many
/// ticks, the same query (same id) is sent again until the deadline.
const DNS_RESEND_TICKS: u64 = crate::arch::x86_64::apic::ms_to_ticks(100);
/// Hard upper bound on DNS pump iterations, independent of the clock.
const DNS_MAX_STEPS: u32 = 20_000;
/// DHCP timeout (~5 s) before the static fallback applies (R13.3).
const DHCP_TIMEOUT_TICKS: u64 = crate::arch::x86_64::apic::ms_to_ticks(5_000);

/// Current IP configuration, reported by `ifconfig` (see [`ip_config`]).
#[derive(Debug, Clone, Copy)]
pub struct IpConfig {
    /// Assigned address + prefix (e.g. `10.0.2.15/24`).
    pub addr: Ipv4Cidr,
    /// Default gateway.
    pub gateway: Ipv4Addr,
    /// The NIC's hardware (MAC) address.
    pub mac: Mac,
}

/// Result of a one-shot [`nc_echo`] client exchange.
#[derive(Debug, Clone)]
pub enum NcResult {
    /// The connection could not be established (refused / unreachable / timed
    /// out before reaching the `Established` state).
    Failed,
    /// The connection was established; `bytes` holds whatever was received back
    /// (for an echo server this mirrors the payload that was sent).
    Echoed(Vec<u8>),
}

/// All mutable networking state, owned behind a single lock.
///
/// Submodules reach into these fields directly (`pub(crate)`), which keeps the
/// layering flat: each layer is a plain function over `&mut Stack`.
pub struct Stack {
    // Interface
    pub(crate) mac: Mac,

    // Address configuration — IPv4 (DHCP or static fallback)
    pub(crate) cidr: Option<Ipv4Cidr>,
    pub(crate) gateway: Option<Ipv4Addr>,
    pub(crate) dns_servers: Vec<Ipv4Addr>,
    pub(crate) configured: bool,
    pub(crate) deadline_tick: Option<u64>,
    pub(crate) dhcp_fallback_used: bool,

    // Address configuration — IPv6 (link-local always; global via SLAAC)
    pub(crate) ll_addr6: Ipv6Addr,
    pub(crate) cidr6: Option<wire::Ipv6Cidr>,
    pub(crate) v6_gateway: Option<Ipv6Addr>,
    /// Set once a Router Solicitation has gone out this boot.
    pub(crate) rs_sent: bool,

    // Ephemeral port allocation
    pub(crate) next_eph: u16,

    // Layers
    pub(crate) arp: arp::ArpTable,
    pub(crate) nd: arp::NdpTable,
    pub(crate) reasm: ip::Reassembler,
    pub(crate) reasm6: ip::Reassembler,
    pub(crate) udp: udp::UdpTable,
    pub(crate) tcp: tcp::TcpTable,
    pub(crate) dhcp: udp::DhcpClient,
    pub(crate) pings: Vec<ip::PingWaiter>,

    // Demo services
    pub(crate) udp_echo_handle: Option<SocketHandle>,
    pub(crate) udp_echo_port: u16,
    pub(crate) tcp_echo_port: Option<u16>,

    // IP identification counter for fragmentation
    pub(crate) next_ip_ident: u16,
}

static NET: Spinlock<Option<Stack>> = Spinlock::new(None);

/// A fresh pseudo-random u32 for ISS/xid purposes. Hardware entropy when
/// available, otherwise tick-mixed xorshift (never security-critical here).
pub(crate) fn random_u32() -> u32 {
    static STATE: Spinlock<u32> = Spinlock::new(0x9E37_79B9);
    let mut g = STATE.lock();
    let mut x = *g ^ crate::security::entropy::secure_u64().unwrap_or(scheduler::ticks()) as u32;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *g = x;
    x
}

/// Allocate an ephemeral local port not currently bound by any TCP or UDP
/// socket. MUST be called with `NET` already locked (see
/// [`ephemeral_port_locked`]); kept as documentation of the discipline.
fn _ephemeral_port_requires_held_lock() {}

// ─── Init ────────────────────────────────────────────────────────────────────

/// Initialize the networking subsystem (R13.1): attach the e1000 and build the
/// (still unconfigured) stack. Address acquisition happens in [`poll`] once the
/// net thread runs.
///
/// Returns `Err(NetError::NoDevice)` if no usable NIC is present; the caller
/// logs and continues booting (R17.3).
pub fn init() -> Result<(), NetError> {
    let devices = crate::drivers::pci::enumerate();
    let mac_bytes = crate::drivers::e1000::attach(&devices).map_err(|_| NetError::NoDevice)?;
    let mac = Mac(mac_bytes);

    let stack = Stack {
        mac,
        cidr: None,
        gateway: None,
        dns_servers: Vec::new(),
        configured: false,
        deadline_tick: None,
        dhcp_fallback_used: false,
        ll_addr6: wire::Ipv6Addr::link_local_from_mac(mac),
        cidr6: None,
        v6_gateway: None,
        rs_sent: false,
        next_eph: 49152,
        arp: arp::ArpTable::new(),
        nd: arp::NdpTable::new(),
        reasm: ip::Reassembler::new(),
        reasm6: ip::Reassembler::new(),
        udp: udp::UdpTable::new(),
        tcp: tcp::TcpTable::new(),
        dhcp: udp::DhcpClient::new(),
        pings: Vec::new(),
        udp_echo_handle: None,
        udp_echo_port: 0,
        tcp_echo_port: None,
        next_ip_ident: 1,
    };

    *NET.lock() = Some(stack);
    info!("net: stack built over e1000 (awaiting DHCP, static fallback after timeout)");
    Ok(())
}

// ─── Poll loop ───────────────────────────────────────────────────────────────

impl Stack {
    /// Advance the whole stack once. Called by [`poll`] and (crate-internally)
    /// by the bounded locked-step pumps in `http_fetch`/`tls`.
    pub(crate) fn step(&mut self) {
        let now = scheduler::ticks();

        // 1. RX: drain every completed frame off the NIC.
        let mut frame = [0u8; 2048];
        while let Some(n) = crate::drivers::e1000::recv(&mut frame) {
            self.input_frame(&frame[..n], now);
        }

        // 2. Neighbor/fragment bookkeeping for both families + RS once.
        self.arp.on_tick(now);
        self.nd.on_tick(now);
        self.reasm.on_tick(now);
        self.reasm6.on_tick(now);
        if !self.rs_sent {
            self.rs_sent = true;
            let icmp = wire::router_solicit(self.ll_addr6, self.mac);
            let pkt = wire::ipv6_build(
                self.ll_addr6,
                wire::Ipv6Addr::ALL_ROUTERS,
                wire::PROTO_ICMPV6,
                255,
                &icmp,
            );
            let frame = wire::eth_frame(
                wire::ipv6_multicast_mac(wire::Ipv6Addr::ALL_ROUTERS),
                self.mac,
                wire::ETHERTYPE_IPV6,
                &pkt,
            );
            crate::drivers::e1000::send(&frame);
        }

        // 3. DHCP: run discovery until configured; renew once bound.
        //     (moved out temporarily so it can borrow the rest of the stack)
        {
            let mut client = core::mem::take(&mut self.dhcp);
            let lease = client.drive(self, now);
            self.dhcp = client;
            if let Some(lease) = lease {
                self.cidr = Some(Ipv4Cidr::new(lease.addr, lease.prefix));
                self.gateway = lease.router;
                self.dns_servers = lease.dns_servers;
                self.configured = true;
                info!(
                    "net: DHCP lease acquired: {} gw {}",
                    self.cidr.unwrap(),
                    lease
                        .router
                        .map(|r| r.to_string())
                        .unwrap_or_else(|| String::from("-"))
                );
            }
        }

        // 4. DHCP timeout -> static fallback, applied exactly once (R13.3).
        if !self.configured && !self.dhcp_fallback_used {
            if self.deadline_tick.is_none() {
                self.deadline_tick = Some(scheduler::ticks() + DHCP_TIMEOUT_TICKS);
            }
            if let Some(deadline) = self.deadline_tick {
                if scheduler::ticks() >= deadline {
                    self.cidr = Some(Ipv4Cidr::new(FALLBACK_IP, FALLBACK_PREFIX));
                    self.gateway = Some(FALLBACK_GW);
                    self.configured = true;
                    self.dhcp_fallback_used = true;
                    info!(
                        "net: DHCP timed out, static fallback {} gw {}",
                        FALLBACK_IP, FALLBACK_GW
                    );
                }
            }
        }

        // 5. TCP transmit/retransmit pass.
        tcp::TcpTable::poll_all(self, now);

        // 6. Demo services.
        self.service_udp_echo();
        self.service_tcp_echo();

        // 7. Ping waiters are removed by `net::ping` itself once resolved.
    }

    /// Dispatch one received Ethernet frame up the layers.
    fn input_frame(&mut self, frame: &[u8], now: u64) {
        let Some((dst, _src, ethertype, payload)) = wire::eth_parse(frame) else {
            return;
        };
        // Accept: our unicast, broadcast, and IPv6 multicast (33:33:*).
        // The e1000 runs promiscuous; everything else is dropped here.
        let ipv6_mcast = dst.0[0] == 0x33 && dst.0[1] == 0x33;
        if dst != self.mac && dst != wire::Mac::BROADCAST && !ipv6_mcast {
            return;
        }
        match ethertype {
            wire::ETHERTYPE_ARP => {
                self.arp
                    .input(self.mac, self.cidr.map(|c| c.addr), payload, now)
            }
            // ip::input dispatches on the version nibble (v4 and v6); NDP needs
            // the real Ethernet-level sender MAC.
            wire::ETHERTYPE_IPV4 | wire::ETHERTYPE_IPV6 => ip::input(self, payload, _src, now),
            _ => {}
        }
    }

    fn service_udp_echo(&mut self) {
        let Some(h) = self.udp_echo_handle else {
            return;
        };
        let mut buf = [0u8; 2048];
        while let Some((n, ep)) = self.udp.recv(h, &mut buf) {
            let _ = udp::send_from(self, self.udp_echo_port, &buf[..n], ep);
        }
    }

    fn service_tcp_echo(&mut self) {
        let Some(port) = self.tcp_echo_port else {
            return;
        };
        for h in self.tcp.handles() {
            let matches_port = self
                .tcp
                .get(h)
                .map(|s| s.local_port == port)
                .unwrap_or(false);
            if !matches_port {
                continue;
            }
            let state = self.tcp.get(h).map(|s| s.state()).unwrap();
            match state {
                tcp::State::Established | tcp::State::CloseWait => {
                    // Echo buffered bytes back.
                    let mut buf = [0u8; 1024];
                    loop {
                        let n = self
                            .tcp
                            .get_mut(h)
                            .map(|s| s.recv_slice(&mut buf))
                            .unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        let data = buf[..n].to_vec();
                        let _ = self.tcp.get_mut(h).map(|s| s.send_slice(&data));
                    }
                    // Peer closed and we've echoed everything: close our half.
                    if state == tcp::State::CloseWait {
                        let empty = self.tcp.get(h).map(|s| !s.can_recv()).unwrap_or(true);
                        if empty {
                            if let Some(sock) = self.tcp.get_mut(h) {
                                sock.close();
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Advance the network stack once (called by [`net_thread`] and by the bounded
/// locked-step pumps). Never runs in IRQ context.
pub fn poll() {
    let mut guard = NET.lock();
    if let Some(s) = guard.as_mut() {
        s.step();
    }
}

/// The networking kernel thread entry point (R13.4). Loops forever: advance
/// the stack, then sleep cooperatively until the next timer tick.
pub fn net_thread() {
    info!("net: poll thread started");
    let mut polls: u64 = 0;
    loop {
        poll();
        polls += 1;
        // Heartbeat: if this line stops appearing while the system hangs,
        // the net thread (or the scheduler feeding it ticks) is dead.
        if polls.is_multiple_of(crate::arch::x86_64::apic::TICK_HZ * 5) {}
        scheduler::sleep_ticks(1);
    }
}

// ─── TCP public API (unchanged surface from the smoltcp era) ────────────────

/// Open an outbound TCP connection to `remote` and return its socket handle.
pub fn tcp_connect(remote: IpEndpoint) -> Result<SocketHandle, NetError> {
    tcp_connect_buffered(remote, 4096, 4096)
}

/// Open an outbound TCP connection like [`tcp_connect`], with explicitly sized
/// rx/tx buffers (TLS needs multi-KiB records; fetches want large windows).
pub fn tcp_connect_buffered(
    remote: IpEndpoint,
    rx_bytes: usize,
    tx_bytes: usize,
) -> Result<SocketHandle, NetError> {
    let mut guard = NET.lock();
    let state = guard.as_mut().ok_or(NetError::NoDevice)?;
    let port = ephemeral_port_locked(state);
    state
        .tcp
        .connect(remote, port, rx_bytes, tx_bytes)
        .map_err(|_| NetError::DeviceInit)
}

/// Is the TCP connection fully established?
pub fn tcp_established(handle: SocketHandle) -> bool {
    let mut g = NET.lock();
    let Some(s) = g.as_mut() else { return false };
    s.step();
    s.tcp
        .get(handle)
        .map(|k| k.state() == tcp::State::Established)
        .unwrap_or(false)
}

/// Did the connection fail before ever establishing (refused/unreachable)?
pub fn tcp_dead_before_established(handle: SocketHandle) -> bool {
    let g = NET.lock();
    match g.as_ref().and_then(|s| s.tcp.get(handle)) {
        None => true, // gone => dead
        Some(k) => k.refused_flag() || (!k.ever_established() && k.state() == tcp::State::Closed),
    }
}

/// Poll once and enqueue as much of `data` as fits the peer window. Returns
/// the number of bytes accepted into the transmit buffer.
pub fn tcp_send_chunk(handle: SocketHandle, data: &[u8]) -> usize {
    let mut g = NET.lock();
    let Some(s) = g.as_mut() else { return 0 };
    s.step();
    s.tcp
        .get_mut(handle)
        .map(|k| k.send_slice(data))
        .unwrap_or(0)
}

/// Poll once and drain received bytes into `dst`. Returns bytes copied.
pub fn tcp_recv_chunk(handle: SocketHandle, dst: &mut [u8]) -> usize {
    let mut g = NET.lock();
    let Some(s) = g.as_mut() else { return 0 };
    s.step();
    s.tcp
        .get_mut(handle)
        .map(|k| k.recv_slice(dst))
        .unwrap_or(0)
}

/// Remote half closed and nothing left to read: read(2) should return 0.
pub fn tcp_rx_at_eof(handle: SocketHandle) -> bool {
    let mut g = NET.lock();
    let Some(s) = g.as_mut() else { return true };
    s.step();
    s.tcp.get(handle).map(|k| k.eof_visible()).unwrap_or(true)
}

/// Half-close our TX then give the close handshake a few polls and reclaim
/// the socket slot.
pub fn tcp_close(handle: SocketHandle) {
    let mut g = NET.lock();
    let Some(s) = g.as_mut() else { return };
    if let Some(k) = s.tcp.get_mut(handle) {
        k.close();
    }
    for _ in 0..4 {
        s.step();
    }
    s.tcp.remove(handle);
}

// ─── UDP public API ──────────────────────────────────────────────────────────

/// Open an UDP socket bound to an ephemeral local port on 0.0.0.0.
pub fn udp_open() -> Result<SocketHandle, NetError> {
    let mut guard = NET.lock();
    let state = guard.as_mut().ok_or(NetError::NoDevice)?;
    let port = ephemeral_port_locked(state);
    Ok(state.udp.open(port))
}

/// Allocate an ephemeral local port (49152..=65535) not currently bound by any
/// TCP or UDP socket. Caller MUST hold `NET`.
fn ephemeral_port_locked(s: &mut Stack) -> u16 {
    loop {
        let p = s.next_eph;
        s.next_eph = if s.next_eph == u16::MAX {
            49152
        } else {
            s.next_eph + 1
        };
        if !s.udp.port_in_use(p) {
            return p;
        }
    }
}

/// Send one datagram to `remote`.
pub fn udp_sendto(handle: SocketHandle, data: &[u8], remote: IpEndpoint) -> Result<(), NetError> {
    let mut guard = NET.lock();
    let state = guard.as_mut().ok_or(NetError::NoDevice)?;
    let port = state.udp.port_of(handle).ok_or(NetError::DeviceInit)?;
    udp::send_from(state, port, data, remote).map_err(|_| NetError::DeviceInit)
}

/// Remove a UDP socket from the set (reclaim buffers). Idempotent.
#[allow(dead_code)]
pub fn udp_remove(handle: SocketHandle) {
    let mut g = NET.lock();
    if let Some(s) = g.as_mut() {
        s.udp.close(handle);
    }
}

/// Receive one datagram if available (caller drives polling between calls).
pub fn udp_recvfrom(handle: SocketHandle, dst: &mut [u8]) -> Option<(usize, IpEndpoint)> {
    let mut g = NET.lock();
    let s = g.as_mut()?;
    s.step();
    s.udp.recv(handle, dst)
}

// ─── Services / diagnostics ──────────────────────────────────────────────────

/// Enable a UDP echo service bound to `port` (R14.2).
pub fn udp_echo_enable(port: u16) {
    let mut guard = NET.lock();
    let Some(state) = guard.as_mut() else {
        warn!("net: udp_echo_enable: no interface");
        return;
    };
    let h = state.udp.open(port);
    state.udp_echo_handle = Some(h);
    state.udp_echo_port = port;
    info!("net: UDP echo enabled on port {}", port);
}

/// Start a TCP echo listener bound to `port` (R14.3). Each accepted connection
/// becomes its own socket; the listener accepts further clients immediately.
pub fn tcp_echo_listen(port: u16) {
    let mut guard = NET.lock();
    let Some(state) = guard.as_mut() else {
        warn!("net: tcp_echo_listen: no interface");
        return;
    };
    match state.tcp.listen(port) {
        Ok(_) => {
            state.tcp_echo_port = Some(port);
            info!("net: TCP echo listening on port {}", port);
        }
        Err(_) => warn!("net: tcp_echo_listen: listen {} failed", port),
    }
}

/// Current IP configuration for `ifconfig`, or `None` if no NIC is present or
/// no address has been assigned yet.
pub fn ip_config() -> Option<IpConfig> {
    let guard = NET.lock();
    let state = guard.as_ref()?;
    let cidr = state.cidr?;
    Some(IpConfig {
        addr: cidr,
        gateway: state.gateway.unwrap_or(Ipv4Addr::UNSPECIFIED),
        mac: state.mac,
    })
}

/// Current IPv6 configuration for `ifconfig`, or `None` before SLAAC.
#[derive(Debug, Clone, Copy)]
pub struct Ip6Config {
    pub addr: wire::Ipv6Cidr,
}

/// The SLAAC-configured global address, if any.
pub fn ip6_config() -> Option<Ip6Config> {
    let guard = NET.lock();
    let state = guard.as_ref()?;
    Some(Ip6Config { addr: state.cidr6? })
}

/// The RA-learned default router (always link-local), if any.
#[allow(dead_code)]
pub fn ip6_gateway() -> Option<Ipv6Addr> {
    let guard = NET.lock();
    guard.as_ref()?.v6_gateway
}

/// The first usable DNS server, or `None` when no address is configured yet.
pub fn dns_server() -> Option<Ipv4Addr> {
    let guard = NET.lock();
    let state = guard.as_ref()?;
    if !state.configured {
        return None;
    }
    Some(state.dns_servers.first().copied().unwrap_or(QEMU_DNS))
}

// ─── DNS resolve (A + AAAA; family preference follows configuration) ───────

/// Resolve `hostname` to an [`IpAddr`] via a DNS query.
///
/// IPv4/IPv6 literals return directly. Otherwise queries are sent as single
/// UDP datagrams to [`dns_server`]`:53` over temporary sockets, pumped in
/// bounded locked steps: AAAA first when the interface has an IPv6 address,
/// then A; the other family is used as fallback when the preferred one has no
/// answer.
pub fn resolve(hostname: &str) -> Option<IpAddr> {
    // Literals first (v4 dotted-quad or any v6 text form).
    if hostname.contains(':') {
        return wire::parse_ipv6_literal(hostname).map(IpAddr::V6);
    }
    if let Some(o) = dns::parse_ipv4_literal(hostname) {
        return Some(IpAddr::V4(Ipv4Addr::new(o[0], o[1], o[2], o[3])));
    }

    let server = dns_server()?;

    // IPv4 FIRST. QEMU user-mode networking (slirp) advertises RAs and answers
    // AAAA queries but does NOT route outbound IPv6, so AAAA-first resolution
    // sent apt/TLS into a black hole (regression introduced with dual-stack).
    // AAAA remains the fallback when a name has no A record, and explicit
    // IPv6 endpoints connect fine.
    let order: [(u16, bool); 2] = [(dns::QTYPE_A, false), (dns::QTYPE_AAAA, true)];

    for (qtype, _) in order {
        if let Some(found) = dns_query_once(server, hostname, qtype) {
            return Some(found);
        }
    }
    None
}

/// One DNS exchange of `qtype` on a temporary socket; returns the first
/// matching address.
fn dns_query_once(server: Ipv4Addr, hostname: &str, qtype: u16) -> Option<IpAddr> {
    // Random transaction id: a tick-derived id is predictable and collides
    // between back-to-back lookups, letting an off-path reply be mistaken for
    // the answer.
    let id: u16 = random_u32() as u16;
    let mut query: Vec<u8> = Vec::new();
    if !dns::build_dns_query_typed(id, hostname, qtype, &mut query) {
        return None;
    }

    let handle = {
        let mut guard = NET.lock();
        let state = guard.as_mut()?;
        let port = ephemeral_port_locked(state);
        state.udp.open(port)
    };

    let server_ep = IpEndpoint::new(IpAddr::V4(server), 53);
    let now = scheduler::ticks();
    let deadline = now + DNS_TIMEOUT_TICKS;
    let mut next_send = now;
    let mut result: Option<IpAddr> = None;

    for _ in 0..DNS_MAX_STEPS {
        {
            let mut guard = NET.lock();
            let state = match guard.as_mut() {
                Some(s) => s,
                None => break,
            };
            // (Re-)send the query every DNS_RESEND_TICKS until an answer
            // arrives or the deadline passes; a single lost datagram no longer
            // burns the whole budget spinning.
            if scheduler::ticks() >= next_send {
                let port = state.udp.port_of(handle).unwrap_or(0);
                if udp::send_from(state, port, &query, server_ep).is_ok() {
                    next_send = scheduler::ticks() + DNS_RESEND_TICKS;
                }
            }
            state.step();

            let mut rbuf = [0u8; 1500];
            if let Some((n, _ep)) = state.udp.recv(handle, &mut rbuf) {
                result = match qtype {
                    dns::QTYPE_AAAA => dns::parse_dns_aaaa_response(&rbuf[..n], id, hostname)
                        .map(|o| IpAddr::V6(wire::Ipv6Addr(o))),
                    _ => dns::parse_dns_a_response(&rbuf[..n], id, hostname)
                        .map(|o| IpAddr::V4(Ipv4Addr::new(o[0], o[1], o[2], o[3]))),
                };
                if result.is_some() {
                    break;
                }
            }
        }

        if scheduler::ticks() >= deadline {
            break;
        }
        for _ in 0..DNS_STEP_SPIN {
            core::hint::spin_loop();
        }
    }

    if let Some(state) = NET.lock().as_mut() {
        state.udp.close(handle);
    }

    result
}

/// Drive a brief, self-contained TCP client exchange for the shell `nc`
/// command (R15.2, R15.3): connect, send once established, collect echoed
/// bytes within a bounded poll window, close, tear down.
pub fn nc_echo(remote: IpEndpoint, payload: &[u8]) -> NcResult {
    let handle = match tcp_connect(remote) {
        Ok(h) => h,
        Err(_) => return NcResult::Failed,
    };

    let mut established = false;
    let mut sent = false;
    let mut reply: Vec<u8> = Vec::new();
    let mut done = false;

    for _ in 0..4000 {
        {
            let mut guard = NET.lock();
            let Some(s) = guard.as_mut() else {
                return NcResult::Failed;
            };
            s.step();

            match s.tcp.get(handle) {
                None => {
                    if !established {
                        s.tcp.remove(handle);
                        return NcResult::Failed; // refused / reaped before establish
                    }
                }
                Some(sock) => {
                    if sock.state() == tcp::State::Established {
                        established = true;
                    }
                }
            }

            if established && !sent {
                let n = s
                    .tcp
                    .get_mut(handle)
                    .map(|k| k.send_slice(payload))
                    .unwrap_or(0);
                if n == payload.len() {
                    sent = true;
                }
            }

            // Drain any echoed bytes.
            loop {
                let mut buf = [0u8; 1024];
                let n = s
                    .tcp
                    .get_mut(handle)
                    .map(|k| k.recv_slice(&mut buf))
                    .unwrap_or(0);
                if n == 0 {
                    break;
                }
                reply.extend_from_slice(&buf[..n]);
            }

            if sent && reply.len() >= payload.len() {
                if let Some(k) = s.tcp.get_mut(handle) {
                    k.close();
                }
                done = true;
            }
        }

        for _ in 0..20_000 {
            core::hint::spin_loop();
        }
        if done {
            break;
        }
    }

    // Give the close handshake a few polls, then reclaim the slot.
    {
        let mut guard = NET.lock();
        if let Some(s) = guard.as_mut() {
            for _ in 0..4 {
                s.step();
            }
            s.tcp.remove(handle);
        }
    }

    if established {
        NcResult::Echoed(reply)
    } else {
        NcResult::Failed
    }
}

// ─── ICMP ping ───────────────────────────────────────────────────────────────

/// Send one ICMP echo request to `addr` (v4 or v6) and wait up to ~1 s for the
/// reply. Returns the measured round-trip time in ticks (ms at TICK_HZ).
pub fn ping(addr: IpAddr) -> Option<u64> {
    const PING_TIMEOUT_TICKS: u64 = crate::arch::x86_64::apic::ms_to_ticks(1000);

    let ident = random_u32() as u16;
    let seq = 1u16;
    let started = scheduler::ticks();
    let payload = b"pagh-ping";

    // Register the waiter BEFORE sending so a fast reply cannot be missed.
    {
        let mut guard = NET.lock();
        let state = guard.as_mut()?;
        if state.pings.len() >= 8 {
            state.pings.remove(0);
        }
        state.pings.push(ip::PingWaiter {
            ident,
            seq,
            started,
            seq_any: true,
            done: false,
            rtt: None,
        });

        let now = scheduler::ticks();
        match addr {
            IpAddr::V4(v4) => {
                let msg = wire::icmp_echo_build(wire::ICMP_ECHO_REQUEST, ident, seq, payload);
                ip::output(state, None, IpAddr::V4(v4), wire::PROTO_ICMP, &msg, now);
            }
            IpAddr::V6(v6) => {
                let mut body = alloc::vec::Vec::with_capacity(4 + payload.len());
                body.extend_from_slice(&ident.to_be_bytes());
                body.extend_from_slice(&seq.to_be_bytes());
                body.extend_from_slice(payload);
                let src = if v6.is_link_local() {
                    state.ll_addr6
                } else {
                    state.cidr6.map(|c| c.addr).unwrap_or(state.ll_addr6)
                };
                let msg = wire::icmpv6_build(src, v6, wire::ICMPV6_ECHO_REQUEST, 0, &body);
                ip::output(state, None, IpAddr::V6(v6), wire::PROTO_ICMPV6, &msg, now);
            }
        }
    }

    loop {
        {
            let mut guard = NET.lock();
            if let Some(state) = guard.as_mut() {
                if let Some(p) = state.pings.iter().find(|p| p.ident == ident && p.done) {
                    let rtt = p.rtt;
                    state.pings.retain(|p| p.ident != ident);
                    return rtt;
                }
            }
        }
        if scheduler::ticks() >= started + PING_TIMEOUT_TICKS {
            // Unregister.
            let mut guard = NET.lock();
            if let Some(state) = guard.as_mut() {
                state.pings.retain(|p| p.ident != ident);
            }
            return None;
        }
        poll();
        scheduler::sleep_ticks(1);
    }
}
