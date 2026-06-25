//! Networking subsystem.
//!
//! Owns the smoltcp DHCPv4 bring-up demo (the verifiable network milestone) over
//! the riscv virtio-net NIC, layered through the generic [`phy::SmolDevice`]
//! adapter (which is parameterized over the [`phy::NetDevice`] HAL trait).
//!
//! Submodules:
//!   * [`phy`]  — the `NetDevice` HAL trait + the generic smoltcp `phy::Device`
//!                adapter + the riscv virtio-net `NetDevice` implementation.
//!   * [`dns`]  — pure DNS query construction + A-record response parsing.
//!   * [`http`] — pure HTTP/1.1 request building + response-head parsing.
//!
//! The `dns`/`http` parsers are `core`+`alloc` only (no sockets, no globals);
//! they are shared verbatim with the x86 kernel and are wired into the socket
//! pump (resolve / fetch) in the next milestone task. TLS/HTTPS is deferred to
//! that task as well (the TLS crates are not yet in `Cargo.toml`).
//
// TODO(task 8): wire dns/http into a real socket pump (resolve/fetch_deb),
// add UDP/TCP echo + `nc`, and port the TLS/HTTPS path (VARIANT-A) once the TLS
// dependencies are added to Cargo.toml.

pub mod dns;
pub mod http;
pub mod phy;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::socket::dhcpv4;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, Ipv4Address, Ipv4Cidr};

use phy::{NetDevice, SmolDevice};

/// The acquired DHCPv4 lease `(address, gateway)`, for the shell `net` command.
static IP_INFO: spin::Mutex<Option<(Ipv4Cidr, Option<Ipv4Address>)>> = spin::Mutex::new(None);

/// The current lease, if any.
pub fn ip_info() -> Option<(Ipv4Cidr, Option<Ipv4Address>)> {
    *IP_INFO.lock()
}

/// smoltcp `Instant` from the 100 Hz tick counter.
fn now() -> Instant {
    Instant::from_millis((crate::timer::ticks() * 10) as i64)
}

/// Probe for a virtio-net device and run DHCPv4 to acquire a lease, printing the
/// result. Returns once a lease is obtained or the timeout elapses.
pub fn demo(dtb: usize) {
    let dev = match phy::attach(dtb) {
        Some(d) => d,
        None => {
            crate::kprintln!("rv: no virtio-net device found on virtio-mmio");
            return;
        }
    };
    let mac = dev.mac_address();
    crate::kprintln!(
        "rv: virtio-net MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    let mut device = SmolDevice::new(dev);
    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    config.random_seed = crate::timer::ticks() ^ 0x9e37_79b9;
    let mut iface = Interface::new(config, &mut device, now());

    let mut sockets = SocketSet::new(alloc::vec::Vec::new());
    let dhcp_handle = sockets.add(dhcpv4::Socket::new());

    crate::kprintln!("rv: net up; requesting DHCP lease...");
    let deadline = crate::timer::ticks() + 1500; // ~15 s

    loop {
        iface.poll(now(), &mut device, &mut sockets);

        match sockets.get_mut::<dhcpv4::Socket>(dhcp_handle).poll() {
            Some(dhcpv4::Event::Configured(cfg)) => {
                iface.update_ip_addrs(|addrs| {
                    let _ = addrs.push(IpCidr::Ipv4(cfg.address));
                });
                if let Some(router) = cfg.router {
                    let _ = iface.routes_mut().add_default_ipv4_route(router);
                }
                crate::kprintln!("rv: DHCP lease acquired: {}", cfg.address);
                if let Some(router) = cfg.router {
                    crate::kprintln!("rv: default gateway: {}", router);
                }
                *IP_INFO.lock() = Some((cfg.address, cfg.router));
                return;
            }
            Some(dhcpv4::Event::Deconfigured) => {}
            None => {}
        }

        if crate::timer::ticks() >= deadline {
            crate::kprintln!("rv: DHCP timed out (no lease in ~15 s)");
            return;
        }

        // SAFETY: wait for the next timer tick so time advances and we don't
        // busy-spin; RX buffers are filled by the device via DMA meanwhile.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}
