// net/phy.rs — virtio-net attach + smoltcp `phy::Device` adapter
// 64-bit x86_64 OS kernel in Rust (#![no_std])
//
// Component 4 of the networking-and-storage design. This module attaches the
// QEMU `virtio-net` NIC discovered by `pci::enumerate()` using the
// `virtio-drivers` 0.11 crate's PCI transport (the exact same transport build
// pattern used by `drivers/virtio/blk.rs`), wraps the resulting `VirtIONet`
// behind a global `Spinlock<Option<..>>` owned by the `net` module, and exposes
// [`SmolDevice`], a thin `smoltcp::phy::Device` adapter over it.
//
// ## virtio-drivers 0.11 net API used
//
// `virtio_drivers::device::net::VirtIONet<H, T, const QUEUE_SIZE: usize>`:
//   * `new(transport, buf_len) -> Result<Self>` — pre-allocates `QUEUE_SIZE`
//     receive buffers of `buf_len` bytes and arms the RX queue.
//   * `mac_address() -> [u8; 6]`
//   * `can_recv() -> bool` / `can_send() -> bool`
//   * `receive() -> Result<RxBuffer>` — pops one completed RX buffer (the frame
//     is `RxBuffer::packet()`); ownership transfers to the caller.
//   * `recycle_rx_buffer(RxBuffer) -> Result` — returns a buffer to the RX queue.
//   * `new_tx_buffer` / `send(TxBuffer) -> Result` — `TxBuffer::from(&[u8])`
//     builds a TX buffer; `send` blocks until the frame is enqueued+used.
//
// ## smoltcp token handling (RX delivered once, TX enqueued once, no aliasing)
//
// `SmolDevice` is a zero-sized handle; the NIC lives behind the module-global
// [`NIC`] lock. `receive()` locks the NIC, pops exactly one `RxBuffer`, and
// hands ownership to a [`SmolRxToken`]; the lock is released before the token
// is consumed. `SmolRxToken::consume` runs smoltcp's parser on the frame bytes
// WITHOUT holding the NIC lock (so a reply built via the paired TX token does
// not deadlock), then re-locks only to `recycle_rx_buffer`, returning the
// buffer to the device exactly once. `SmolTxToken::consume` fills a fresh
// heap buffer and `send`s it exactly once. Because each RX buffer is popped
// once and recycled once, and each TX frame is built and sent once, no buffer
// is ever aliased between the device and the driver (Property 17 / R13.6/13.7).

use alloc::vec;

use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

use virtio_drivers::device::net::{RxBuffer, TxBuffer, VirtIONet};
use virtio_drivers::transport::pci::bus::{ConfigurationAccess, DeviceFunction, PciRoot};
use virtio_drivers::transport::pci::PciTransport;

use crate::drivers::pci::{self, PciAddress, PciDevice};
use crate::drivers::virtio::hal::PaghHal;
use crate::net::NetError;
use crate::sync::spinlock::Spinlock;
use crate::{info, warn};

/// PCI device IDs that identify a virtio network device.
///
/// `0x1000` is the transitional (legacy) virtio-net ID; `0x1041` is the modern
/// virtio-1.0 ID (`0x1040 + DeviceType::Network`). QEMU's `virtio-net-pci`
/// presents one of these depending on its (non-)transitional configuration.
const VIRTIO_NET_DEVICE_ID_LEGACY: u16 = 0x1000;
const VIRTIO_NET_DEVICE_ID_MODERN: u16 = 0x1041;

/// Number of RX/TX descriptors in each virtqueue (const generic of `VirtIONet`).
pub const QUEUE_SIZE: usize = 16;

/// Per-buffer length handed to `VirtIONet::new`. Must be at least the device's
/// minimum (1526 bytes: 1514-byte max Ethernet frame + 12-byte virtio-net
/// header, rounded up); 2048 gives comfortable headroom for a 1500-MTU frame.
const BUF_LEN: usize = 2048;

/// The maximum Ethernet frame MTU advertised to smoltcp (R13 / design: 1500).
const MTU: usize = 1500;

/// The attached virtio-net device, owned by the `net` module behind a single
/// lock. `None` until [`attach`] succeeds (and whenever no NIC is present).
static NIC: Spinlock<Option<VirtIONet<PaghHal, PciTransport, QUEUE_SIZE>>> = Spinlock::new(None);

/// A `ConfigurationAccess` adapter bridging the `virtio-drivers` PCI transport
/// to pagh's legacy-port PCI config-space helpers (mirrors the one in
/// `drivers/virtio/blk.rs`; kept module-local to avoid a cross-module dependency
/// on a private type).
struct PaghPciAccess;

impl ConfigurationAccess for PaghPciAccess {
    fn read_word(&self, device_function: DeviceFunction, register_offset: u8) -> u32 {
        pci::config_read_u32(df_to_addr(device_function), register_offset)
    }

    fn write_word(&mut self, device_function: DeviceFunction, register_offset: u8, data: u32) {
        pci::config_write_u32(df_to_addr(device_function), register_offset, data);
    }

    unsafe fn unsafe_clone(&self) -> Self {
        PaghPciAccess
    }
}

/// Convert a `virtio-drivers` `DeviceFunction` into a pagh `PciAddress`.
fn df_to_addr(df: DeviceFunction) -> PciAddress {
    PciAddress {
        bus: df.bus,
        device: df.device,
        function: df.function,
    }
}

/// True if a discovered PCI device is a virtio network device.
fn is_virtio_net(dev: &PciDevice) -> bool {
    dev.is_virtio()
        && (dev.device_id == VIRTIO_NET_DEVICE_ID_LEGACY
            || dev.device_id == VIRTIO_NET_DEVICE_ID_MODERN)
}

/// Discover the first virtio-net device among `devices`, attach it, and store
/// it in the module-global [`NIC`]. Returns the device's MAC address on success.
///
/// Returns `Err(NetError::NoDevice)` if no virtio-net device is present, and
/// `Err(NetError::DeviceInit)` if the transport/handshake fails — in both cases
/// the caller logs and continues booting (R17.3), leaving [`NIC`] as `None`.
pub fn attach(devices: &[PciDevice]) -> Result<[u8; 6], NetError> {
    let dev = match devices.iter().find(|d| is_virtio_net(d)) {
        Some(d) => d,
        None => return Err(NetError::NoDevice),
    };

    let addr = dev.address;
    info!(
        "virtio-net: found device {:02x}:{:02x}.{} (id {:#06x})",
        addr.bus, addr.device, addr.function, dev.device_id
    );

    // Enable bus-mastering (and memory-space decoding) so the device can drive
    // virtqueue DMA and respond to MMIO BAR accesses from the transport.
    pci::enable_bus_master(addr);

    let mut root = PciRoot::new(PaghPciAccess);
    let device_function = DeviceFunction {
        bus: addr.bus,
        device: addr.device,
        function: addr.function,
    };

    let transport = match PciTransport::new::<PaghHal, _>(&mut root, device_function) {
        Ok(t) => t,
        Err(e) => {
            warn!("virtio-net: PCI transport init failed: {:?}", e);
            return Err(NetError::DeviceInit);
        }
    };

    let nic = match VirtIONet::<PaghHal, PciTransport, QUEUE_SIZE>::new(transport, BUF_LEN) {
        Ok(n) => n,
        Err(e) => {
            warn!("virtio-net: device init failed: {:?}", e);
            return Err(NetError::DeviceInit);
        }
    };

    let mac = nic.mac_address();
    info!(
        "virtio-net: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    *NIC.lock() = Some(nic);
    Ok(mac)
}

/// A smoltcp `phy::Device` over the attached `virtio-net` NIC.
///
/// Zero-sized: all state lives in the module-global [`NIC`]. Cheap to construct
/// and to hand to `Interface::poll`.
pub struct SmolDevice;

impl Device for SmolDevice {
    type RxToken<'a> = SmolRxToken;
    type TxToken<'a> = SmolTxToken;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MTU;
        // Bound smoltcp's per-poll burst to the queue depth so it never tries to
        // hold more buffers in flight than the device has descriptors for.
        caps.max_burst_size = Some(QUEUE_SIZE);
        caps
    }

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut guard = NIC.lock();
        let nic = guard.as_mut()?;
        if !nic.can_recv() {
            return None;
        }
        // Pop exactly one completed RX buffer; ownership moves into the token,
        // so the frame is delivered to smoltcp exactly once (R13.6).
        match nic.receive() {
            Ok(rx_buf) => Some((SmolRxToken { buf: Some(rx_buf) }, SmolTxToken)),
            // `NotReady` etc.: nothing to deliver this poll.
            Err(_) => None,
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        let mut guard = NIC.lock();
        let nic = guard.as_mut()?;
        if nic.can_send() {
            Some(SmolTxToken)
        } else {
            None
        }
    }
}

/// RX token owning a single popped `RxBuffer`. The buffer is delivered to
/// smoltcp once (in `consume`) and recycled to the device exactly once.
pub struct SmolRxToken {
    buf: Option<RxBuffer>,
}

impl phy::RxToken for SmolRxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        // `receive()` always populates `buf`, so this take cannot be `None`.
        let rx_buf = self.buf.take().expect("SmolRxToken consumed without a buffer");

        // Run smoltcp's parser on the frame bytes WITHOUT holding the NIC lock,
        // so a reply built through the paired TX token (which re-locks the NIC)
        // cannot deadlock against us.
        let result = f(rx_buf.packet());

        // Return the buffer to the RX queue exactly once.
        if let Some(nic) = NIC.lock().as_mut() {
            let _ = nic.recycle_rx_buffer(rx_buf);
        }

        result
    }
}

/// TX token. `consume` builds one frame and sends it exactly once (R13.7).
pub struct SmolTxToken;

impl phy::TxToken for SmolTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // Build the frame in a fresh heap buffer (no aliasing with any device
        // or other token buffer), let smoltcp fill it, then enqueue it once.
        let mut frame = vec![0u8; len];
        let result = f(&mut frame);

        if let Some(nic) = NIC.lock().as_mut() {
            let _ = nic.send(TxBuffer::from(frame.as_slice()));
        }

        result
    }
}
