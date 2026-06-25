//! virtio-net over virtio-mmio + a smoltcp `phy::Device` adapter, driven far
//! enough to acquire a DHCPv4 lease (the verifiable network milestone).
//!
//! Mirrors the x86_64 kernel's `net::phy` adapter (same `virtio-drivers` 0.11
//! net API and smoltcp 0.12 token discipline), but attaches over the MMIO
//! transport and reuses the identity-map [`crate::blk::HalImpl`]. The NIC lives
//! behind a spinlock in a small `unsafe impl Send` cell (single-hart, serialized
//! access), so the zero-sized [`SmolDevice`] tokens re-lock to pop/recycle RX
//! buffers and to send TX frames exactly once.

use core::ptr::NonNull;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::dhcpv4;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, Ipv4Address, Ipv4Cidr};

use virtio_drivers::device::net::{RxBuffer, TxBuffer, VirtIONet};
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};
use virtio_drivers::transport::{DeviceType, Transport};

use crate::blk::HalImpl;

const QUEUE_SIZE: usize = 16;
const BUF_LEN: usize = 2048;
const MTU: usize = 1500;

type Nic = VirtIONet<HalImpl, MmioTransport<'static>, QUEUE_SIZE>;

/// `Send` wrapper so the NIC can live in a `static` Mutex (single-hart, all
/// access serialized through the lock).
struct NicCell(Option<Nic>);
// SAFETY: every access is under NIC.lock() on the single boot hart.
unsafe impl Send for NicCell {}

static NIC: spin::Mutex<NicCell> = spin::Mutex::new(NicCell(None));

/// The acquired DHCPv4 lease `(address, gateway)`, for the shell `net` command.
static IP_INFO: spin::Mutex<Option<(Ipv4Cidr, Option<Ipv4Address>)>> = spin::Mutex::new(None);

/// The current lease, if any.
pub fn ip_info() -> Option<(Ipv4Cidr, Option<Ipv4Address>)> {
    *IP_INFO.lock()
}

/// Scan the DTB virtio-mmio nodes, attach the first network device, and store it
/// in [`NIC`]. Returns the MAC address on success.
pub fn attach(dtb: usize) -> Option<[u8; 6]> {
    // SAFETY: valid DTB pointer from OpenSBI.
    let fdt = unsafe { fdt::Fdt::from_ptr(dtb as *const u8) }.ok()?;
    for node in fdt.all_nodes() {
        let is_vmmio = node
            .compatible()
            .map(|c| c.all().any(|s| s == "virtio,mmio"))
            .unwrap_or(false);
        if !is_vmmio {
            continue;
        }
        let mut regs = match node.reg() {
            Some(r) => r,
            None => continue,
        };
        let (base, size) = match regs.next() {
            Some(r) => (r.starting_address as usize, r.size.unwrap_or(0x1000)),
            None => continue,
        };
        // Identify by the MMIO DeviceID (1 = network) without constructing (and
        // dropping) a transport over non-matching slots — a dropped MmioTransport
        // resets its device, which would clobber the already-initialized blk.
        // SAFETY: identity-mapped MMIO reads.
        let (magic, dev_id) = unsafe {
            (
                core::ptr::read_volatile(base as *const u32),
                core::ptr::read_volatile((base + 8) as *const u32),
            )
        };
        if magic != 0x7472_6976 || dev_id != 1 {
            continue;
        }
        let header = match NonNull::new(base as *mut VirtIOHeader) {
            Some(h) => h,
            None => continue,
        };
        // SAFETY: confirmed virtio-mmio network device at this identity-mapped window.
        let transport = match unsafe { MmioTransport::new(header, size) } {
            Ok(t) => t,
            Err(_) => continue,
        };
        if transport.device_type() != DeviceType::Network {
            continue;
        }
        match Nic::new(transport, BUF_LEN) {
            Ok(nic) => {
                let mac = nic.mac_address();
                NIC.lock().0 = Some(nic);
                return Some(mac);
            }
            Err(_) => continue,
        }
    }
    None
}

/// Zero-sized smoltcp device over the global [`NIC`].
pub struct SmolDevice;

impl Device for SmolDevice {
    type RxToken<'a> = SmolRxToken;
    type TxToken<'a> = SmolTxToken;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MTU;
        caps.max_burst_size = Some(QUEUE_SIZE);
        caps
    }

    fn receive(&mut self, _ts: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut guard = NIC.lock();
        let nic = guard.0.as_mut()?;
        if !nic.can_recv() {
            return None;
        }
        match nic.receive() {
            Ok(buf) => Some((SmolRxToken { buf: Some(buf) }, SmolTxToken)),
            Err(_) => None,
        }
    }

    fn transmit(&mut self, _ts: Instant) -> Option<Self::TxToken<'_>> {
        let mut guard = NIC.lock();
        let nic = guard.0.as_mut()?;
        if nic.can_send() {
            Some(SmolTxToken)
        } else {
            None
        }
    }
}

/// RX token owning one popped buffer; delivered once and recycled once.
pub struct SmolRxToken {
    buf: Option<RxBuffer>,
}

impl phy::RxToken for SmolRxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let buf = self.buf.take().expect("rx token without buffer");
        // Parse without holding the lock so a paired TX reply cannot deadlock.
        let result = f(buf.packet());
        if let Some(nic) = NIC.lock().0.as_mut() {
            let _ = nic.recycle_rx_buffer(buf);
        }
        result
    }
}

/// TX token: build one frame and send it once.
pub struct SmolTxToken;

impl phy::TxToken for SmolTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut frame = alloc::vec![0u8; len];
        let result = f(&mut frame);
        if let Some(nic) = NIC.lock().0.as_mut() {
            let _ = nic.send(TxBuffer::from(frame.as_slice()));
        }
        result
    }
}

/// smoltcp `Instant` from the 100 Hz tick counter.
fn now() -> Instant {
    Instant::from_millis((crate::timer::ticks() * 10) as i64)
}

/// Probe for a virtio-net device and run DHCPv4 to acquire a lease, printing the
/// result. Returns once a lease is obtained or the timeout elapses.
pub fn demo(dtb: usize) {
    let mac = match attach(dtb) {
        Some(m) => m,
        None => {
            crate::kprintln!("rv: no virtio-net device found on virtio-mmio");
            return;
        }
    };
    crate::kprintln!(
        "rv: virtio-net MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    let mut device = SmolDevice;
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
