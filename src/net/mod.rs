//! Networking subsystem (Component 5 of the networking-and-storage design).
//!
//! Owns the smoltcp `Interface`, `SocketSet`, address configuration, the poll
//! loop, and the demo echo services, layered over the `virtio-net` NIC through
//! the [`phy::SmolDevice`] adapter. The poll loop runs in a dedicated kernel
//! thread (`net_thread`) so heavy stack work stays out of IRQ context; the
//! 100 Hz timer tick only advances the monotonic clock ([`now`]) consumed here.
//!
//! Bring-up sequence:
//!   1. [`init`] enumerates PCI, attaches the NIC ([`phy::attach`]), builds the
//!      `Interface` (Ethernet medium, MAC from the device) and a `SocketSet`
//!      with a DHCPv4 client socket, and stores everything in [`NET`].
//!   2. The net thread repeatedly calls [`poll`], which advances smoltcp,
//!      applies any acquired DHCP lease to the interface, and — if DHCP has not
//!      completed within a tick-based timeout — falls back to the static QEMU
//!      user-net address `10.0.2.15/24` gw `10.0.2.2` (R13.3).
//!   3. ICMP echo is answered natively by smoltcp once the interface has an
//!      address; [`udp_echo_enable`] adds a UDP echo socket (R14.2).
//!
//! If no NIC is present, [`init`] returns `Err(NetError::NoDevice)`; the caller
//! logs a warning and boot continues (R17.3).

pub mod phy;

use alloc::vec;
use alloc::vec::Vec;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::socket::{dhcpv4, tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpCidr, IpEndpoint, Ipv4Address, Ipv4Cidr,
};

use crate::sync::spinlock::Spinlock;
use crate::task::scheduler;
use crate::{info, warn};

/// Errors produced by the networking subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    /// No virtio-net device was discovered, so networking is unavailable.
    NoDevice,
    /// A virtio-net device was found but the transport/device handshake failed.
    DeviceInit,
}

/// Static fallback address used when DHCP does not complete (R13.3). These are
/// the well-known QEMU user-mode networking values.
const FALLBACK_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
const FALLBACK_PREFIX: u8 = 24;
const FALLBACK_GW: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);

/// DHCP timeout in 100 Hz ticks (~5 s). If no lease is acquired within this many
/// ticks of the first poll, [`poll`] applies the static fallback once.
const DHCP_TIMEOUT_TICKS: u64 = 500;

/// Current IP configuration, reported by `ifconfig` (see [`ip_config`]).
#[derive(Debug, Clone, Copy)]
pub struct IpConfig {
    /// Assigned address + prefix (e.g. `10.0.2.15/24`).
    pub addr: IpCidr,
    /// Default gateway.
    pub gateway: Ipv4Address,
    /// The NIC's hardware (MAC) address.
    pub mac: EthernetAddress,
}

/// All mutable networking state, owned behind a single lock.
struct NetState {
    iface: Interface,
    sockets: SocketSet<'static>,
    device: phy::SmolDevice,
    dhcp_handle: SocketHandle,
    udp_handle: Option<SocketHandle>,
    udp_port: u16,
    /// TCP echo listener socket handle and its port (R14.3). The listener
    /// re-issues `listen(port)` after each connection closes so the next client
    /// can connect (a single-connection-at-a-time listener for v1).
    tcp_echo_handle: Option<SocketHandle>,
    tcp_echo_port: u16,
    /// Next ephemeral local port handed to outbound `tcp_connect` clients.
    next_eph: u16,
    mac: EthernetAddress,
    /// Set once an address has been assigned (via DHCP or the static fallback).
    configured: bool,
    /// Current default gateway, once configured.
    gateway: Option<Ipv4Address>,
    /// Tick at which the DHCP timeout expires (set on the first poll).
    deadline_tick: Option<u64>,
}

static NET: Spinlock<Option<NetState>> = Spinlock::new(None);

/// The smoltcp monotonic instant, derived from the 100 Hz tick counter as
/// `Instant::from_millis(ticks * 10)` (R13.5).
pub fn now() -> Instant {
    Instant::from_millis((scheduler::ticks() * 10) as i64)
}

/// Initialize the networking subsystem (R13.1).
///
/// Enumerates PCI, attaches the virtio-net NIC, builds the smoltcp `Interface`
/// (Ethernet medium, hardware address = the NIC's MAC), a `SocketSet`, and a
/// DHCPv4 client socket, and stores the resulting [`NetState`] in [`NET`].
/// Address acquisition (DHCP, with static fallback) happens in [`poll`] once
/// the net thread is running and the timer tick is advancing [`now`].
///
/// Returns `Err(NetError::NoDevice)` if no NIC is present; the caller logs and
/// continues booting (R17.3).
pub fn init() -> Result<(), NetError> {
    let devices = crate::drivers::pci::enumerate();
    let mac_bytes = phy::attach(&devices)?;
    let mac = EthernetAddress(mac_bytes);

    let mut device = phy::SmolDevice;
    let config = Config::new(HardwareAddress::Ethernet(mac));
    let iface = Interface::new(config, &mut device, now());

    let mut sockets = SocketSet::new(Vec::new());
    let dhcp_handle = sockets.add(dhcpv4::Socket::new());

    let state = NetState {
        iface,
        sockets,
        device,
        dhcp_handle,
        udp_handle: None,
        udp_port: 0,
        tcp_echo_handle: None,
        tcp_echo_port: 0,
        next_eph: 49152,
        mac,
        configured: false,
        gateway: None,
        deadline_tick: None,
    };

    *NET.lock() = Some(state);
    info!("net: interface built (awaiting DHCP, static fallback after timeout)");
    Ok(())
}

/// Apply an IPv4 address + optional gateway to the interface, replacing any
/// existing configuration.
fn apply_ipv4(state: &mut NetState, cidr: Ipv4Cidr, gateway: Option<Ipv4Address>) {
    state.iface.update_ip_addrs(|addrs| {
        addrs.clear();
        let _ = addrs.push(IpCidr::Ipv4(cidr));
    });
    if let Some(gw) = gateway {
        let _ = state.iface.routes_mut().add_default_ipv4_route(gw);
    } else {
        state.iface.routes_mut().remove_default_ipv4_route();
    }
    state.configured = true;
    state.gateway = gateway;
}

/// Advance the network stack once: poll smoltcp, apply any DHCP lease, handle
/// the DHCP timeout fallback, and service the UDP echo socket. Called by
/// [`net_thread`] (never from IRQ context, so locking is deadlock-free).
pub fn poll() {
    let mut guard = NET.lock();
    let state = match guard.as_mut() {
        Some(s) => s,
        None => return,
    };

    let timestamp = now();

    // Arm the DHCP fallback deadline on the first poll.
    if state.deadline_tick.is_none() {
        state.deadline_tick = Some(scheduler::ticks() + DHCP_TIMEOUT_TICKS);
    }

    // Advance smoltcp (RX ingress + TX egress, ARP/IP/ICMP/UDP/TCP/DHCP).
    let _ = state.iface.poll(timestamp, &mut state.device, &mut state.sockets);

    // Service the DHCP client: apply a newly-acquired lease to the interface.
    // Copy the (Copy) config fields out first so the socket borrow ends before
    // we mutate the interface.
    let dhcp_event = state
        .sockets
        .get_mut::<dhcpv4::Socket>(state.dhcp_handle)
        .poll();
    match dhcp_event {
        Some(dhcpv4::Event::Configured(cfg)) => {
            let cidr = cfg.address;
            let router = cfg.router;
            apply_ipv4(state, cidr, router);
            let gw = router.unwrap_or(Ipv4Address::UNSPECIFIED);
            info!(
                "net: DHCP lease acquired: {} gw {}",
                cidr, gw
            );
        }
        Some(dhcpv4::Event::Deconfigured) => {
            // Lease lost: clear the address (keep the interface up for a renew).
            // smoltcp emits one Deconfigured on the very first poll as it resets
            // into the Discovering state; that initial event is not an error, so
            // only warn when we were actually configured before.
            let was_configured = state.configured;
            state.iface.update_ip_addrs(|addrs| addrs.clear());
            state.iface.routes_mut().remove_default_ipv4_route();
            state.configured = false;
            state.gateway = None;
            if was_configured {
                warn!("net: DHCP lease lost");
            }
        }
        None => {}
    }

    // DHCP timeout -> static fallback, applied exactly once (R13.3).
    if !state.configured {
        if let Some(deadline) = state.deadline_tick {
            if scheduler::ticks() >= deadline {
                let cidr = Ipv4Cidr::new(FALLBACK_IP, FALLBACK_PREFIX);
                apply_ipv4(state, cidr, Some(FALLBACK_GW));
                info!(
                    "net: DHCP timed out, static fallback {} gw {}",
                    cidr, FALLBACK_GW
                );
            }
        }
    }

    // Service the UDP echo socket: reflect each datagram back to its sender.
    if let Some(handle) = state.udp_handle {
        let sock = state.sockets.get_mut::<udp::Socket>(handle);
        let mut buf = [0u8; 2048];
        loop {
            match sock.recv_slice(&mut buf) {
                Ok((n, meta)) => {
                    // Echo the same payload back to the sender (R14.2). Ignore
                    // send errors (e.g. tx buffer momentarily full).
                    let _ = sock.send_slice(&buf[..n], meta.endpoint);
                }
                Err(_) => break,
            }
        }
    }

    // Service the TCP echo listener (R14.3). A single listening socket accepts
    // one connection at a time and re-listens once each connection closes:
    //
    //   * Closed                   -> (re-)issue listen(port) to accept the next
    //                                  client. smoltcp moves Listen -> SynRcvd ->
    //                                  Established as the handshake completes.
    //   * data ready & tx writable -> recv the bytes and send them straight back
    //                                  (echo) on the same connection.
    //   * CloseWait (peer FIN +    -> close our half so the socket walks back to
    //     rx drained)                 Closed, where the next poll re-listens.
    //
    // smoltcp answers everything else (SYN/ACK, retransmits, the FIN handshake)
    // internally during `iface.poll` above.
    if let Some(handle) = state.tcp_echo_handle {
        let port = state.tcp_echo_port;
        let sock = state.sockets.get_mut::<tcp::Socket>(handle);

        // Re-listen only once a previous connection has fully closed. Matching
        // on the concrete state (not may_recv/can_recv) is important: a socket
        // in the Listen state also reports may_recv()==false, so the old
        // "close when !may_recv && !can_recv" check would have closed the
        // listener on every poll before any client could connect.
        if sock.state() == tcp::State::Closed {
            let _ = sock.listen(port);
        }

        // Echo received bytes back on the connection.
        if sock.can_recv() && sock.can_send() {
            let mut buf = [0u8; 1024];
            if let Ok(n) = sock.recv_slice(&mut buf) {
                if n > 0 {
                    let _ = sock.send_slice(&buf[..n]);
                }
            }
        }

        // Peer closed its side and we have drained the rx buffer: close ours so
        // the socket returns to Closed for the next re-listen.
        if sock.state() == tcp::State::CloseWait && !sock.can_recv() {
            sock.close();
        }
    }
}

/// Enable a UDP echo service bound to `port` (R14.2). Idempotent-ish: a second
/// call replaces the stored handle (the old socket stays in the set, harmless).
pub fn udp_echo_enable(port: u16) {
    let mut guard = NET.lock();
    let state = match guard.as_mut() {
        Some(s) => s,
        None => {
            warn!("net: udp_echo_enable: no interface");
            return;
        }
    };

    let rx = udp::PacketBuffer::new(
        vec![udp::PacketMetadata::EMPTY; 8],
        vec![0u8; 8 * 1024],
    );
    let tx = udp::PacketBuffer::new(
        vec![udp::PacketMetadata::EMPTY; 8],
        vec![0u8; 8 * 1024],
    );
    let mut sock = udp::Socket::new(rx, tx);
    if sock.bind(port).is_err() {
        warn!("net: udp_echo_enable: bind {} failed", port);
        return;
    }
    let handle = state.sockets.add(sock);
    state.udp_handle = Some(handle);
    state.udp_port = port;
    info!("net: UDP echo enabled on port {}", port);
}

/// Start a TCP echo listener bound to `port` (R14.3).
///
/// Adds a single `tcp::Socket` (4 KiB rx/tx buffers) to the set, puts it in the
/// `Listen` state, and records its handle. [`poll`] services it: it echoes
/// received bytes back on the accepted connection and re-issues `listen(port)`
/// once a connection closes, so a sequence of clients can connect one after
/// another (a single concurrent connection is sufficient for the v1 echo demo).
pub fn tcp_echo_listen(port: u16) {
    let mut guard = NET.lock();
    let state = match guard.as_mut() {
        Some(s) => s,
        None => {
            warn!("net: tcp_echo_listen: no interface");
            return;
        }
    };

    let rx = tcp::SocketBuffer::new(vec![0u8; 4096]);
    let tx = tcp::SocketBuffer::new(vec![0u8; 4096]);
    let mut sock = tcp::Socket::new(rx, tx);
    if sock.listen(port).is_err() {
        warn!("net: tcp_echo_listen: listen {} failed", port);
        return;
    }
    let handle = state.sockets.add(sock);
    state.tcp_echo_handle = Some(handle);
    state.tcp_echo_port = port;
    info!("net: TCP echo listening on port {}", port);
}

/// Open an outbound TCP connection to `remote` and return its socket handle for
/// client use (e.g. the shell `nc` command).
///
/// Adds a `tcp::Socket` (4 KiB rx/tx buffers) to the set and initiates the
/// connection from an ephemeral local port. The connection completes
/// asynchronously across subsequent [`poll`] calls; the caller drives it (and
/// ultimately removes the socket) via [`nc_echo`] or directly.
pub fn tcp_connect(remote: IpEndpoint) -> Result<SocketHandle, NetError> {
    let mut guard = NET.lock();
    let state = guard.as_mut().ok_or(NetError::NoDevice)?;

    // Pick an ephemeral local port (wraps within the 49152..=65535 range).
    let local_port = state.next_eph;
    state.next_eph = if state.next_eph >= 65535 {
        49152
    } else {
        state.next_eph + 1
    };

    let rx = tcp::SocketBuffer::new(vec![0u8; 4096]);
    let tx = tcp::SocketBuffer::new(vec![0u8; 4096]);
    let sock = tcp::Socket::new(rx, tx);
    let handle = state.sockets.add(sock);

    // `iface.context()` and `sockets.get_mut(..)` borrow disjoint fields of
    // `state`, so both mutable borrows can coexist across the connect call.
    let cx = state.iface.context();
    let res = state
        .sockets
        .get_mut::<tcp::Socket>(handle)
        .connect(cx, remote, local_port);

    if res.is_err() {
        state.sockets.remove(handle);
        return Err(NetError::DeviceInit);
    }
    Ok(handle)
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

/// Drive a brief, self-contained TCP client exchange for the shell `nc` command
/// (R15.2, R15.3): connect to `remote`, send `payload` once the connection is
/// established, collect the bytes echoed back within a bounded poll window, then
/// close and tear the socket down.
///
/// Simplification (documented for v1): the shell is line-based and the main
/// poll loop runs in the net thread, so rather than a fully interactive session
/// this pumps the stack in short, individually-locked steps here. Each step
/// takes the `NET` lock, advances smoltcp once, drives the client socket's state
/// machine (send-once / drain-rx / close), then releases the lock and spins
/// briefly so the timer tick advances and QEMU can deliver the peer's frames.
/// It stops as soon as it has received at least `payload.len()` bytes back, the
/// peer closes, or the bounded iteration budget is exhausted.
pub fn nc_echo(remote: IpEndpoint, payload: &[u8]) -> NcResult {
    let handle = match tcp_connect(remote) {
        Ok(h) => h,
        Err(_) => return NcResult::Failed,
    };

    let mut sent = false;
    let mut established = false;
    let mut reply: Vec<u8> = Vec::new();
    let mut done = false;

    // ~ up to a few thousand short steps; each step polls once and spins a
    // little, which (with the 100 Hz tick) comfortably covers connection setup
    // and a round-trip echo over QEMU user-net.
    for _ in 0..4000 {
        {
            let mut guard = NET.lock();
            let state = match guard.as_mut() {
                Some(s) => s,
                None => return NcResult::Failed,
            };

            let ts = now();
            let _ = state.iface.poll(ts, &mut state.device, &mut state.sockets);

            let sock = state.sockets.get_mut::<tcp::Socket>(handle);

            if sock.is_active() {
                established = true;
            }

            // If the socket reached Closed before we ever got established and
            // sent data, the connect was refused / unreachable.
            if !established && sock.state() == tcp::State::Closed {
                state.sockets.remove(handle);
                return NcResult::Failed;
            }

            // Send the payload once, as soon as the tx half is writable.
            if !sent && sock.can_send() {
                let _ = sock.send_slice(payload);
                sent = true;
            }

            // Drain any echoed bytes.
            if sock.can_recv() {
                let mut buf = [0u8; 1024];
                if let Ok(n) = sock.recv_slice(&mut buf) {
                    reply.extend_from_slice(&buf[..n]);
                }
            }

            // Done once we've echoed back at least what we sent, or the peer
            // closed its side after we sent.
            if sent && reply.len() >= payload.len() {
                sock.close();
                done = true;
            } else if sent && !sock.may_recv() && !sock.can_recv() {
                sock.close();
                done = true;
            }
        }

        // Release the lock and let time / QEMU advance between steps.
        for _ in 0..20_000 {
            core::hint::spin_loop();
        }

        if done {
            break;
        }
    }

    // Give the close handshake a couple of polls, then remove the client socket
    // so its buffers are reclaimed.
    {
        let mut guard = NET.lock();
        if let Some(state) = guard.as_mut() {
            for _ in 0..4 {
                let ts = now();
                let _ = state.iface.poll(ts, &mut state.device, &mut state.sockets);
            }
            state.sockets.remove(handle);
        }
    }

    if established {
        NcResult::Echoed(reply)
    } else {
        NcResult::Failed
    }
}

/// Current IP configuration for `ifconfig`, or `None` if no NIC is present or
/// no address has been assigned yet.
pub fn ip_config() -> Option<IpConfig> {
    let guard = NET.lock();
    let state = guard.as_ref()?;
    let addr = state.iface.ip_addrs().first().copied()?;
    Some(IpConfig {
        addr,
        gateway: state.gateway.unwrap_or(Ipv4Address::UNSPECIFIED),
        mac: state.mac,
    })
}

/// The networking kernel thread entry point (R13.4). Loops forever: advance the
/// stack, then yield CPU via a short busy delay (the 100 Hz tick both preempts
/// this thread and advances [`now`], so a coarse cadence is sufficient).
pub fn net_thread() {
    info!("net: poll thread started");
    loop {
        poll();
        for _ in 0..50_000 {
            core::hint::spin_loop();
        }
    }
}
