//! smoltcp `phy::Device` adapter, generic over a [`NetDevice`] HAL trait.
//!
//! This is the seam from the design's HAL table: the shared smoltcp glue speaks
//! only to the arch-neutral [`NetDevice`] trait (send a frame, receive a frame,
//! MAC, capabilities), while the riscv [`VirtioNet`] wrapper implements that
//! trait over `virtio-drivers` 0.11's `VirtIONet` on the MMIO transport. Making
//! the [`SmolDevice`] adapter generic over `NetDevice` is what lets the same net
//! stack run on either arch's NIC (x86 PCI vs riscv MMIO) — only the `NetDevice`
//! impl differs.
//!
//! The adapter owns its device (no global NIC cell): [`SmolDevice`] holds the
//! `NetDevice` directly, the RX token owns one popped frame (copied out and the
//! virtio buffer recycled immediately), and the TX token borrows the device to
//! send exactly one frame.

use core::ptr::NonNull;

use alloc::vec::Vec;

use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

use virtio_drivers::device::net::{TxBuffer, VirtIONet};
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};
use virtio_drivers::transport::{DeviceType, Transport};

use crate::blk::HalImpl;

const QUEUE_SIZE: usize = 16;
const BUF_LEN: usize = 2048;
const MTU: usize = 1500;

type Nic = VirtIONet<HalImpl, MmioTransport<'static>, QUEUE_SIZE>;

/// Hardware-abstraction trait the smoltcp [`SmolDevice`] adapter drives.
///
/// Each arch supplies an implementation over its NIC (riscv virtio-mmio here,
/// x86 virtio-pci on `main`); the smoltcp glue stays arch-blind.
pub trait NetDevice {
    /// The NIC's 6-byte Ethernet MAC address.
    fn mac_address(&self) -> [u8; 6];

    /// Link capabilities (medium, MTU, burst) reported to smoltcp.
    fn capabilities(&self) -> DeviceCapabilities;

    /// `true` if at least one received frame is ready to pop.
    fn can_receive(&self) -> bool;

    /// `true` if the TX queue can accept another frame.
    fn can_transmit(&self) -> bool;

    /// Pop one received frame into an owned buffer, recycling the underlying
    /// driver RX buffer. Returns `None` when no frame is ready.
    fn receive_frame(&mut self) -> Option<Vec<u8>>;

    /// Transmit a single Ethernet frame (best effort; send errors are dropped).
    fn transmit_frame(&mut self, frame: &[u8]);
}

/// riscv virtio-net `NetDevice`: wraps a `virtio-drivers` `VirtIONet` attached
/// over the MMIO transport, reusing the identity-map [`crate::blk::HalImpl`].
pub struct VirtioNet {
    nic: Nic,
}

impl NetDevice for VirtioNet {
    fn mac_address(&self) -> [u8; 6] {
        self.nic.mac_address()
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MTU;
        caps.max_burst_size = Some(QUEUE_SIZE);
        caps
    }

    fn can_receive(&self) -> bool {
        self.nic.can_recv()
    }

    fn can_transmit(&self) -> bool {
        self.nic.can_send()
    }

    fn receive_frame(&mut self) -> Option<Vec<u8>> {
        if !self.nic.can_recv() {
            return None;
        }
        match self.nic.receive() {
            Ok(buf) => {
                // Copy the frame out so the (driver-owned) RX buffer can be
                // recycled immediately; smoltcp parses the owned copy.
                let frame = buf.packet().to_vec();
                let _ = self.nic.recycle_rx_buffer(buf);
                Some(frame)
            }
            Err(_) => None,
        }
    }

    fn transmit_frame(&mut self, frame: &[u8]) {
        let _ = self.nic.send(TxBuffer::from(frame));
    }
}

/// Scan the DTB virtio-mmio nodes, attach the first network device, and return
/// it as a [`VirtioNet`]. Returns `None` if no virtio-net device is present.
pub fn attach(dtb: usize) -> Option<VirtioNet> {
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
            Ok(nic) => return Some(VirtioNet { nic }),
            Err(_) => continue,
        }
    }
    None
}

/// smoltcp `phy::Device` adapter generic over any [`NetDevice`]. Owns the device
/// directly; tokens borrow it (TX) or own a copied frame (RX).
pub struct SmolDevice<D: NetDevice> {
    dev: D,
}

impl<D: NetDevice> SmolDevice<D> {
    /// Wrap a `NetDevice` for use as a smoltcp `phy::Device`.
    pub fn new(dev: D) -> Self {
        SmolDevice { dev }
    }

    /// The wrapped device's MAC address.
    pub fn mac_address(&self) -> [u8; 6] {
        self.dev.mac_address()
    }
}

impl<D: NetDevice> Device for SmolDevice<D> {
    type RxToken<'a>
        = SmolRxToken
    where
        D: 'a;
    type TxToken<'a>
        = SmolTxToken<'a, D>
    where
        D: 'a;

    fn capabilities(&self) -> DeviceCapabilities {
        self.dev.capabilities()
    }

    fn receive(&mut self, _ts: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Pop a frame (ending the &mut borrow), then hand out a paired TX token.
        let frame = self.dev.receive_frame()?;
        Some((SmolRxToken { frame }, SmolTxToken { dev: &mut self.dev }))
    }

    fn transmit(&mut self, _ts: Instant) -> Option<Self::TxToken<'_>> {
        if self.dev.can_transmit() {
            Some(SmolTxToken { dev: &mut self.dev })
        } else {
            None
        }
    }
}

/// RX token owning one received frame; delivered to smoltcp exactly once.
pub struct SmolRxToken {
    frame: Vec<u8>,
}

impl phy::RxToken for SmolRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.frame)
    }
}

/// TX token borrowing the device: build one frame and send it once.
pub struct SmolTxToken<'a, D: NetDevice> {
    dev: &'a mut D,
}

impl<'a, D: NetDevice> phy::TxToken for SmolTxToken<'a, D> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut frame = alloc::vec![0u8; len];
        let result = f(&mut frame);
        self.dev.transmit_frame(&frame);
        result
    }
}
