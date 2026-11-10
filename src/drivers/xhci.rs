//! xHCI (USB host controller) — polled, no interrupts, HID-oriented.
//!
//! WHY THIS EXISTS. Until now the kernel had no USB support at all: the only
//! keyboard was PS/2 (ports 0x60/0x64 + IRQ1) and the only mouse PS/2 aux. A
//! machine with a USB keyboard therefore worked only if its firmware translated
//! USB HID into PS/2 scancodes in SMM ("USB Legacy Support") — which many boards
//! do and many others do not, and on the ones that do not pagh booted to a shell
//! nobody could type into. This driver removes that dependency: it drives the
//! xHCI controller itself and feeds the same input path the PS/2 driver feeds.
//!
//! DESIGN, and why it looks like `nvme.rs`. The controller is polled from one
//! kernel thread (`usb`, spawned in `boot.rs`): no interrupt routing, no handler
//! sharing the timer path, and the failure mode is "the poll thread stops"
//! rather than "an IRQ corrupts a ring". That is the same shape as the NVMe
//! driver, the only other driver here that owns a DMA protocol.
//!
//! WHAT THE HARDWARE ASKS FOR. xHCI is a set of rings in RAM the controller
//! walks by physical address: one command ring, one event ring per interrupter, a
//! device context per slot, and transfer rings per endpoint. All of it must be
//! physically contiguous and zeroed (the `DmaFrame`/`Ring` helpers), and every
//! action is "write a TRB, ring the doorbell, wait for the matching event".
//!
//! SCOPE. Enumeration brings a device to the point of a configured HID interface
//! with one interrupt-IN endpoint, which is exactly what a boot-protocol keyboard
//! or mouse needs; [`crate::drivers::usb_hid`] turns those reports into the
//! scancode stream the shell already consumes. Root ports only — a device behind
//! a hub is not reached (no hub class driver). Not implemented: isochronous
//! transfers, suspend/resume, hot-unplug teardown, USB 3 link power management.

use alloc::vec::Vec;

use core::ptr::{read_volatile, write_volatile};

use crate::drivers::pci::PciDevice;
use crate::memory::pmm;
use crate::sync::spinlock::Spinlock;

const PAGE_SIZE: usize = 4096;
const TRB_SIZE: usize = 16;

// ─── DMA frames ──────────────────────────────────────────────────────────────

/// One zeroed, page-aligned DMA frame, addressed both physically (for the
/// controller) and through the HHDM (for the CPU).
struct DmaFrame {
    phys: u64,
    virt: usize,
}

impl DmaFrame {
    fn new_zeroed(what: &str) -> Option<Self> {
        let phys = pmm::alloc_frame()?;
        let virt = crate::memory::vmm::phys_to_virt(phys) as usize;
        // SAFETY: the PMM just handed us this exclusive frame, mapped in the
        // HHDM, so zeroing the whole page is valid.
        unsafe { core::ptr::write_bytes(virt as *mut u8, 0, PAGE_SIZE) };
        crate::debug!("xhci: dma frame {} phys={:#x} virt={:#x}", what, phys, virt);
        Some(Self { phys, virt })
    }

    fn phys(&self) -> u64 {
        self.phys
    }

    fn virt(&self) -> usize {
        self.virt
    }
}

// ─── TRBs ────────────────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Trb {
    dw0: u32,
    dw1: u32,
    dw2: u32,
    dw3: u32,
}

const _: () = assert!(core::mem::size_of::<Trb>() == TRB_SIZE);

impl Trb {
    fn new(dw0: u32, dw1: u32, dw2: u32, dw3: u32) -> Self {
        Self { dw0, dw1, dw2, dw3 }
    }

    fn trb_type(&self) -> u8 {
        ((self.dw3 >> 10) & 0x3F) as u8
    }

    fn completion_code(&self) -> u8 {
        ((self.dw2 >> 24) & 0xFF) as u8
    }

    /// Residual transfer length reported by a transfer event.
    fn residual(&self) -> u32 {
        self.dw2 & 0x00FF_FFFF
    }

    fn slot_id(&self) -> u8 {
        ((self.dw3 >> 24) & 0xFF) as u8
    }

    fn endpoint_id(&self) -> u8 {
        ((self.dw3 >> 16) & 0x1F) as u8
    }
}

// TRB types (xHCI 1.2 table 6-91).
const TRB_NORMAL: u8 = 1;
const TRB_SETUP: u8 = 2;
const TRB_DATA: u8 = 3;
const TRB_STATUS: u8 = 4;
const TRB_LINK: u8 = 6;
const TRB_ENABLE_SLOT: u8 = 9;
const TRB_ADDRESS_DEVICE: u8 = 11;
const TRB_CONFIGURE_ENDPOINT: u8 = 12;
const TRB_TRANSFER_EVENT: u8 = 32;
const TRB_COMMAND_COMPLETION: u8 = 33;
const TRB_PORT_STATUS_CHANGE: u8 = 34;

// Completion codes (table 6-93).
const CC_SUCCESS: u8 = 1;
const CC_SHORT_PACKET: u8 = 13;

const TRB_CYCLE: u32 = 1 << 0;
const TRB_IOC: u32 = 1 << 5;
const TRB_IDT: u32 = 1 << 6;
const TRB_DIR_IN: u32 = 1 << 16;
/// Transfer Type field of a Setup TRB: 0 = no data, 2 = IN, 3 = OUT.
const TRT_NO_DATA: u32 = 0 << 16;
const TRT_IN: u32 = 2 << 16;
const TRT_OUT: u32 = 3 << 16;

/// Ring capacity in TRBs (one page holds 256, so 64 leaves room for the link).
const RING_TRBS: usize = 64;
const EVENT_TRBS: usize = 256;

/// A ring of TRBs walked by the controller.
struct Ring {
    frame: DmaFrame,
    enqueue: usize,
    dequeue: usize,
    cycle: u32,
    /// Interrupt-IN only: a normal TRB is outstanding until its transfer event
    /// arrives. Re-arming without waiting would queue a second read into the
    /// same buffer and the reports would race each other.
    outstanding: bool,
}

impl Ring {
    fn new(what: &str) -> Option<Self> {
        Some(Self {
            frame: DmaFrame::new_zeroed(what)?,
            enqueue: 0,
            dequeue: 0,
            cycle: TRB_CYCLE,
            outstanding: false,
        })
    }

    fn ptr(&self, index: usize) -> *mut Trb {
        (self.frame.virt() + index * TRB_SIZE) as *mut Trb
    }

    fn write(&self, index: usize, trb: Trb) {
        // SAFETY: `index < RING_TRBS` and one page holds 256 TRBs, so the write
        // stays inside our own DMA frame; volatile because the controller reads
        // these bytes without the CPU's knowledge.
        unsafe { write_volatile(self.ptr(index), trb) }
    }

    fn read(&self, index: usize) -> Trb {
        // SAFETY: as in `write`.
        unsafe { read_volatile(self.ptr(index)) }
    }

    fn base_phys(&self) -> u64 {
        self.frame.phys()
    }

    /// Append one TRB with the producer's cycle bit and advance the enqueue
    /// pointer, installing the wrap-around Link TRB at the end of the ring.
    fn push(&mut self, dw0: u32, dw1: u32, dw2: u32, dw3: u32) {
        let index = self.enqueue;
        self.write(index, Trb::new(dw0, dw1, dw2, dw3 | self.cycle));
        self.enqueue += 1;
        if self.enqueue >= RING_TRBS - 1 {
            // The link TRB carries the *next* cycle state: the controller reads
            // the ring again with the toggled bit after following it.
            let next_cycle = if self.cycle == TRB_CYCLE {
                0
            } else {
                TRB_CYCLE
            };
            self.write(
                RING_TRBS - 1,
                Trb::new(
                    self.base_phys() as u32,
                    (self.base_phys() >> 32) as u32,
                    0,
                    ((TRB_LINK as u32) << 10) | TRB_IOC | next_cycle,
                ),
            );
            self.enqueue = 0;
            self.cycle = next_cycle;
        }
    }

    /// Event-ring side: has the controller published an event the CPU has not
    /// consumed? The cycle bit is what distinguishes a fresh TRB from the stale
    /// contents of the ring.
    fn event_pending(&self) -> bool {
        let trb = self.read(self.dequeue);
        (trb.dw3 & TRB_CYCLE) == self.cycle && trb.trb_type() != 0
    }

    fn next_event(&mut self) -> Trb {
        let trb = self.read(self.dequeue);
        self.dequeue += 1;
        if self.dequeue >= EVENT_TRBS {
            self.dequeue = 0;
            self.cycle = if self.cycle == TRB_CYCLE {
                0
            } else {
                TRB_CYCLE
            };
        }
        trb
    }
}

// ─── Registers ───────────────────────────────────────────────────────────────

const CAP_CAPLENGTH: usize = 0x00;
const CAP_HCSPARAMS1: usize = 0x04;
const CAP_HCSPARAMS2: usize = 0x08;
const CAP_HCCPARAMS1: usize = 0x10;
const CAP_DBOFF: usize = 0x14;
const CAP_RTSOFF: usize = 0x18;

const OP_USBCMD: usize = 0x00;
const OP_USBSTS: usize = 0x04;
const OP_CRCR: usize = 0x18;
const OP_DCBAAP: usize = 0x30;
const OP_CONFIG: usize = 0x38;
const OP_PORTS: usize = 0x400;

/// Bytes between neighbouring port register sets.
///
/// Each port has four 32-bit registers (PORTSC, PORTPMSC, PORTLI, PORTHLPMC), so
/// the stride is 16 — this used to be 4, which made ports 2..=8 read and write
/// their neighbours' registers (the "5th port" was really the 2nd port's PORTSC).
/// The consequence was that a device on a USB2 port was never seen: QEMU puts its
/// emulated keyboard on port 5 of 8, so the whole enumeration path never ran.
const OP_PORT_STRIDE: usize = 0x10;

/// PORTSC register offset for a 1-based port number.
///
/// The arithmetic is pinned by a const assertion below, because this is exactly the
/// kind of constant that is invisible until hardware shows up.
const fn portsc(port: u8) -> usize {
    OP_PORTS + (port as usize - 1) * OP_PORT_STRIDE
}

// Port 1 is at 0x400 and each port takes 16 bytes: port 5 (the first USB2 port on
// a 4+4 controller) is at 0x440.
const _: () = assert!(portsc(1) == 0x400);
const _: () = assert!(portsc(5) == 0x440);

const USBCMD_RS: u32 = 1 << 0;
const USBCMD_HCRST: u32 = 1 << 1;
const USBSTS_HCH: u32 = 1 << 0;
const USBSTS_CNR: u32 = 1 << 11;

const PORTSC_CCS: u32 = 1 << 0;
const PORTSC_PED: u32 = 1 << 1;
const PORTSC_PR: u32 = 1 << 4;
const PORTSC_PLS_MASK: u32 = 0xF << 5;
const PORTSC_PP: u32 = 1 << 9;
const PORTSC_SPEED_MASK: u32 = 0xF << 10;
const PORTSC_SPEED_SHIFT: u32 = 10;
const PORTSC_CSC: u32 = 1 << 17;
const PORTSC_WRC: u32 = 1 << 19;
const PORTSC_PRC: u32 = 1 << 21;
const PORTSC_WPR: u32 = 1 << 31;
/// Write-1-to-clear bits: every PORTSC write must preserve the rest of the
/// register and set only the bits being acknowledged, or the controller keeps
/// re-reporting the same change (and, worse, can drop a real one).
const PORTSC_RW1C: u32 =
    PORTSC_CSC | (1 << 18) | PORTSC_WRC | (1 << 20) | PORTSC_PRC | (1 << 22) | (1 << 23);

const RTS_IR0: usize = 0x20;
const IR_ERSTSZ: usize = 0x08;
const IR_ERSTBA: usize = 0x10;
const IR_ERDP: usize = 0x18;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ErstEntry {
    base: u64,
    size: u32,
    _reserved: u32,
}

// Context field positions (xHCI 1.2 §6.2.2).
const SLOT_CTX_ENTRIES_SHIFT: u32 = 27;
const SLOT_CTX_SPEED_SHIFT: u32 = 20;
const SLOT_CTX_PORT_SHIFT: u32 = 16;
const EP_CTX_CERR_SHIFT: u32 = 1;
const EP_CTX_EP_TYPE_SHIFT: u32 = 3;
const EP_CTX_MAX_PACKET_SHIFT: u32 = 16;
const EP_CTX_INTERVAL_SHIFT: u32 = 16;
const EP_TYPE_CONTROL: u32 = 4;
const EP_TYPE_INTERRUPT_IN: u32 = 7;

/// Speed ids from PORTSC (table 5-27).
const SPEED_LOW: u8 = 2;
const SPEED_FULL: u8 = 1;
const SPEED_HIGH: u8 = 3;
const SPEED_SUPER: u8 = 4;

// ─── Slots ───────────────────────────────────────────────────────────────────

/// One HID interrupt-IN endpoint: its ring, its report buffer and its DCI.
struct HidEndpoint {
    dci: u8,
    ring: Ring,
    buf: DmaFrame,
    /// Report size in bytes (8 for a boot keyboard, 3-4 for a boot mouse).
    len: usize,
    class: u8,
    protocol: u8,
}

/// One addressed device.
struct Slot {
    id: u8,
    port: u8,
    speed: u8,
    ep0: Ring,
    input_ctx: DmaFrame,
    device_ctx: DmaFrame,
    /// Data buffer for control transfers (one at a time, single-threaded poll).
    ctrl_buf: DmaFrame,
    hid: Option<HidEndpoint>,
}

struct Controller {
    op: usize,
    db: usize,
    rt: usize,
    ctx_size: usize,
    ports: u8,
    cmdr: Ring,
    event: Ring,
    erst: DmaFrame,
    /// Device context array base (DCBAAP) and its scratchpad pages.
    dcbaa_phys: u64,
    slots: Vec<Slot>,
    /// Reports delivered to `usb_hid` so far.
    reports: u64,
    /// Devices enumerated so far (for logging).
    devices: u64,
    /// Ports that have been powered and reset this boot. A port is brought up
    /// exactly once: connect status is reported from then on.
    brought_up: Vec<u8>,
    /// One-shot diagnostic: the port map after every port has been brought up.
    logged_ports: bool,
}

static CONTROLLER: Spinlock<Option<Controller>> = Spinlock::new(None);

// ─── MMIO ────────────────────────────────────────────────────────────────────

fn rd32(base: usize, off: usize) -> u32 {
    // SAFETY: `base + off` is inside the BAR0 window mapped by `map_bar0`
    // (64 KiB covers capability + operational + runtime + doorbells), and the
    // read is volatile so the register is re-read every time.
    unsafe { read_volatile((base + off) as *const u32) }
}

fn wr32(base: usize, off: usize, v: u32) {
    // SAFETY: as in `rd32`.
    unsafe { write_volatile((base + off) as *mut u32, v) }
}

fn rd64(base: usize, off: usize) -> u64 {
    (rd32(base, off) as u64) | ((rd32(base, off + 4) as u64) << 32)
}

fn wr64(base: usize, off: usize, v: u64) {
    wr32(base, off, v as u32);
    wr32(base, off + 4, (v >> 32) as u32);
}

// ─── Init ────────────────────────────────────────────────────────────────────

/// Find and start an xHCI controller. `false` means "no controller, or it
/// refused to start" — USB input is simply unavailable then, and the PS/2 path
/// (plus any firmware translation) is all the machine has.
pub fn init(devices: &[PciDevice]) -> bool {
    let Some(dev) = devices
        .iter()
        .find(|d| d.class == 0x0C && d.subclass == 0x03)
    else {
        return false;
    };
    let lo = crate::drivers::pci::config_read_u32(dev.address, 0x10);
    let hi = crate::drivers::pci::config_read_u32(dev.address, 0x14);
    let bar = ((hi as u64) << 32) | (lo as u64 & 0xFFFF_FFF0);
    if bar == 0 || lo & 0x6 != 0x4 {
        crate::warn!("xhci: BAR0 is not a 64-bit memory BAR (lo={:#x})", lo);
        return false;
    }
    let Ok(virt) = crate::memory::vmm::map_mmio(bar, 64 * 1024) else {
        crate::warn!("xhci: cannot map BAR0 {:#x}", bar);
        return false;
    };
    let base = virt as usize;

    let caplength = (rd32(base, CAP_CAPLENGTH) & 0xFF) as usize;
    let op = base + caplength;
    let hcsparams1 = rd32(base, CAP_HCSPARAMS1);
    let hcsparams2 = rd32(base, CAP_HCSPARAMS2);
    let hccparams1 = rd32(base, CAP_HCCPARAMS1);
    let dboff = (rd32(base, CAP_DBOFF) & !0x3) as usize;
    let rtsoff = (rd32(base, CAP_RTSOFF) & !0x1F) as usize;
    let max_slots = (hcsparams1 & 0xFF) as usize;
    let ports = ((hcsparams1 >> 24) & 0xFF) as u8;
    let ctx_size = if hccparams1 & (1 << 2) != 0 { 64 } else { 32 };
    let scratchpads = (((hcsparams2 >> 21) & 0x1F) << 5) | ((hcsparams2 >> 27) & 0x1F);
    crate::info!(
        "xhci: {:04x}:{:04x} bar={:#x} slots={} ports={} ctx={}B scratchpad={}",
        dev.vendor_id,
        dev.device_id,
        bar,
        max_slots,
        ports,
        ctx_size,
        scratchpads
    );
    if max_slots == 0 || ports == 0 {
        crate::warn!("xhci: controller reports no slots/ports");
        return false;
    }

    // Port state BEFORE anything is touched: the firmware may already have
    // brought devices up, and comparing this with the post-reset state is what
    // told us whether a reset (rather than our port handling) was losing them.
    for port in 1..=ports {
        let sc = rd32(op, portsc(port));
        if sc != 0 {
            crate::debug!("xhci: pre-reset port {} PORTSC={:#010x}", port, sc);
        }
    }

    // Reset: the firmware may have left the controller running its own schedule,
    // and we are about to replace every ring it knows about.
    wr32(op, OP_USBCMD, rd32(op, OP_USBCMD) & !USBCMD_RS);
    spin_until(|| rd32(op, OP_USBSTS) & USBSTS_HCH != 0, 100_000);
    wr32(op, OP_USBCMD, USBCMD_HCRST);
    spin_until(|| rd32(op, OP_USBCMD) & USBCMD_HCRST == 0, 1_000_000);
    spin_until(|| rd32(op, OP_USBSTS) & USBSTS_CNR == 0, 1_000_000);

    let Some(dcbaa) = DmaFrame::new_zeroed("dcbaa") else {
        return false;
    };
    let mut scratchpad_pages: Vec<DmaFrame> = Vec::new();
    if scratchpads > 0 {
        let Some(array) = DmaFrame::new_zeroed("scratchpad array") else {
            return false;
        };
        for i in 0..scratchpads {
            let Some(page) = DmaFrame::new_zeroed("scratchpad page") else {
                return false;
            };
            // SAFETY: `array` is our own page and `i < scratchpads`, so the
            // 8-byte slot is inside it.
            unsafe { write_volatile((array.virt() as *mut u64).add(i as usize), page.phys()) };
            scratchpad_pages.push(page);
        }
        // SAFETY: DCBAA[0] is the first 8 bytes of our own zeroed frame.
        unsafe { write_volatile(dcbaa.virt() as *mut u64, array.phys()) };
    }

    let (Some(cmdr), Some(event), Some(erst)) = (
        Ring::new("command ring"),
        Ring::new("event ring"),
        DmaFrame::new_zeroed("erst"),
    ) else {
        return false;
    };
    // SAFETY: `erst` is our own zeroed page and the entry is its first 16 bytes.
    unsafe {
        write_volatile(
            erst.virt() as *mut ErstEntry,
            ErstEntry {
                base: event.base_phys(),
                size: EVENT_TRBS as u32,
                _reserved: 0,
            },
        );
    }

    let rt = base + rtsoff;
    wr32(rt + RTS_IR0, IR_ERSTSZ, 1);
    wr64(rt + RTS_IR0, IR_ERSTBA, erst.phys());
    wr64(rt + RTS_IR0, IR_ERDP, event.base_phys());

    wr32(op, OP_CONFIG, max_slots.min(u8::MAX as usize) as u32);
    wr64(op, OP_DCBAAP, dcbaa.phys());
    wr64(op, OP_CRCR, cmdr.base_phys() | TRB_CYCLE as u64);
    wr32(op, OP_USBCMD, USBCMD_RS);
    if !spin_until(|| rd32(op, OP_USBSTS) & USBSTS_HCH == 0, 100_000) {
        crate::warn!(
            "xhci: controller did not start (USBSTS={:#x})",
            rd32(op, OP_USBSTS)
        );
        return false;
    }
    crate::info!("xhci: running (ports={} cr={:#x})", ports, cmdr.base_phys());
    // Port state at bring-up, at debug level: which ports have a device attached,
    // whether they are powered and at what speed is the first thing to look at
    // when enumeration does not happen.
    for port in 1..=ports {
        let sc = rd32(op, portsc(port));
        crate::debug!(
            "xhci: port {} PORTSC={:#010x} ccs={} ped={} pp={} speed={} pls={}",
            port,
            sc,
            sc & PORTSC_CCS != 0,
            sc & PORTSC_PED != 0,
            sc & PORTSC_PP != 0,
            (sc & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT,
            (sc & PORTSC_PLS_MASK) >> 5
        );
    }

    *CONTROLLER.lock() = Some(Controller {
        op,
        db: base + dboff,
        rt,
        ctx_size,
        ports,
        cmdr,
        event,
        erst,
        dcbaa_phys: dcbaa.phys(),
        slots: Vec::new(),
        reports: 0,
        devices: 0,
        brought_up: Vec::new(),
        logged_ports: false,
    });
    // The scratchpad pages must outlive every command the controller runs.
    core::mem::forget(scratchpad_pages);
    true
}

/// Spin until `cond` holds or the budget runs out. Returns whether it held.
fn spin_until(cond: impl Fn() -> bool, budget: u32) -> bool {
    let mut i = 0;
    while i < budget {
        if cond() {
            return true;
        }
        core::hint::spin_loop();
        i += 1;
    }
    cond()
}

// ─── Commands and events ─────────────────────────────────────────────────────

fn ring_doorbell(ctrl: &Controller, slot: u8, target: u8) {
    wr32(ctrl.db, slot as usize * 4, target as u32);
}

/// Consume one event, moving ERDP past it. `None` when nothing is pending within
/// the budget.
fn next_event(ctrl: &mut Controller, budget: u32) -> Option<Trb> {
    if !spin_until(
        || {
            // The closure cannot borrow `ctrl` mutably, so the check is done on a
            // raw read of the event ring through the shared reference.
            ctrl.event.event_pending()
        },
        budget,
    ) {
        return None;
    }
    let ev = ctrl.event.next_event();
    let erdp = ctrl.event.base_phys() + (ctrl.event.dequeue * TRB_SIZE) as u64;
    wr64(ctrl.rt + RTS_IR0, IR_ERDP, erdp);
    Some(ev)
}

/// Submit a command TRB and return its completion event.
fn command(ctrl: &mut Controller, dw0: u32, dw1: u32, dw2: u32, dw3: u32) -> Option<Trb> {
    ctrl.cmdr.push(dw0, dw1, dw2, dw3);
    ring_doorbell(ctrl, 0, 0);
    for _ in 0..32 {
        let ev = next_event(ctrl, 500_000)?;
        match ev.trb_type() {
            TRB_COMMAND_COMPLETION => return Some(ev),
            TRB_PORT_STATUS_CHANGE => {
                let port = (ev.dw0 >> 24) & 0xFF;
                crate::debug!("xhci: port {} status change", port);
            }
            _ => {}
        }
    }
    None
}

/// Address of an input-context TRB argument: bits 4+ of dw0/dw1.
fn ctx_addr(phys: u64) -> (u32, u32) {
    ((phys & 0xFFFF_FFF0) as u32, (phys >> 32) as u32)
}

// ─── Enumeration ─────────────────────────────────────────────────────────────

/// Power `port` and run its reset sequence; return the speed of a device found.
///
/// The reset is what makes the connection visible. It is tempting to read
/// PORTSC first and only reset a port that already reports a device, but that is
/// precisely the order that does not work: a controller whose ports are in
/// RxDetect (QEMU's model, and real hardware after the firmware has handed over)
/// reports `CCS=0` until the reset sequence runs, so a "detect then reset" loop
/// never detects anything. Power, reset, then read.
fn reset_port(ctrl: &mut Controller, port: u8) -> Option<u8> {
    let off = portsc(port);
    let mut sc = rd32(ctrl.op, off);
    if sc & PORTSC_PP == 0 {
        wr32(ctrl.op, off, (sc & !PORTSC_RW1C) | PORTSC_PP);
        if !spin_until(|| rd32(ctrl.op, off) & PORTSC_PP != 0, 100_000) {
            return None;
        }
        sc = rd32(ctrl.op, off);
    }
    // Look at the connect status BEFORE resetting. A port with nothing on it never
    // reports a reset completion, so the old order — always reset, then spin for
    // PRC — made every empty port burn a full spin budget on every call. That is
    // what turned the menu's keyboard poll into a CPU hog once a controller was
    // present (each poll reset eight ports and waited on seven of them).
    if sc & PORTSC_CCS == 0 {
        if sc & PORTSC_CSC != 0 {
            wr32(ctrl.op, off, (sc & !PORTSC_RW1C) | PORTSC_CSC);
        }
        return None;
    }
    // Consume stale change bits (the controller sets CSC on every port at reset)
    // so the PRC below is the reset's own.
    wr32(
        ctrl.op,
        off,
        (sc & !PORTSC_RW1C) | PORTSC_CSC | PORTSC_PRC | PORTSC_WRC,
    );
    let sc = rd32(ctrl.op, off);
    wr32(ctrl.op, off, (sc & !PORTSC_RW1C) | PORTSC_PR);
    if !spin_until(
        || rd32(ctrl.op, off) & (PORTSC_PRC | PORTSC_WPR) != 0,
        200_000,
    ) {
        // A device that is connected but does not complete the reset (a broken
        // cable, a hub, a port we cannot drive) must not hang the caller.
        crate::debug!("xhci: port {} reset not completed", port);
        return None;
    }
    let cur = rd32(ctrl.op, off);
    wr32(
        ctrl.op,
        off,
        (cur & !PORTSC_RW1C) | PORTSC_PRC | PORTSC_WRC | PORTSC_CSC,
    );
    let cur = rd32(ctrl.op, off);
    if cur & PORTSC_CCS == 0 {
        return None;
    }
    Some(((cur & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT) as u8)
}

/// EP0 max packet size before the device descriptor is known, by speed.
fn ep0_max_packet(speed: u8) -> u32 {
    match speed {
        SPEED_HIGH => 64,
        SPEED_SUPER => 512,
        _ => 8,
    }
}

/// Run a control-IN transfer on the device's EP0 and return the bytes read.
fn control_in(
    ctrl: &mut Controller,
    slot_index: usize,
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    len: usize,
) -> Option<Vec<u8>> {
    if len > PAGE_SIZE {
        return None;
    }
    let slot_id = {
        let slot = ctrl.slots.get_mut(slot_index)?;
        // Setup stage: the 8-byte request packet goes in the TRB itself (IDT).
        let dw0 = request_type as u32 | ((request as u32) << 8) | ((value as u32) << 16);
        let dw1 = index as u32 | ((len as u32) << 16);
        let dw3 = ((TRB_SETUP as u32) << 10) | TRB_IDT | TRT_IN;
        slot.ep0.push(dw0, dw1, 8, dw3);
        // Data stage: one IN TRB covering the whole buffer.
        let buf_phys = slot.ctrl_buf.phys();
        slot.ep0.push(
            buf_phys as u32,
            (buf_phys >> 32) as u32,
            len as u32,
            ((TRB_DATA as u32) << 10) | TRB_DIR_IN,
        );
        // Status stage: OUT, with IOC so completion is signalled.
        slot.ep0
            .push(0, 0, 0, ((TRB_STATUS as u32) << 10) | TRB_IOC);
        slot.id
    };
    ring_doorbell(ctrl, slot_id, 1);
    // Two events can arrive: the data stage and the status stage. Wait for the
    // one carrying the status TRB's completion (endpoint 1, IOC set).
    let mut result = None;
    for _ in 0..8 {
        let ev = next_event(ctrl, 1_000_000)?;
        if ev.trb_type() != TRB_TRANSFER_EVENT || ev.endpoint_id() != 1 {
            continue;
        }
        let cc = ev.completion_code();
        if cc != CC_SUCCESS && cc != CC_SHORT_PACKET {
            crate::warn!("xhci: control-IN failed: cc={}", cc);
            return None;
        }
        let got = len.saturating_sub(ev.residual() as usize);
        let slot = ctrl.slots.get(slot_index)?;
        // SAFETY: `ctrl_buf` is our own page and `got <= len <= PAGE_SIZE`.
        let bytes =
            unsafe { core::slice::from_raw_parts(slot.ctrl_buf.virt() as *const u8, got) }.to_vec();
        result = Some(bytes);
        break;
    }
    result
}

/// Run a control transfer with NO data stage (a standard OUT request).
///
/// WHY THIS EXISTS: enumeration used to go straight from GET_DESCRIPTOR to the
/// xHCI Configure Endpoint command, never sending USB SET_CONFIGURATION. The
/// device therefore stayed unconfigured, its interrupt endpoint answered STALL,
/// and the report path read `cc=6` (stall) forever — a keyboard that enumerates
/// but never types.
fn control_no_data(
    ctrl: &mut Controller,
    slot_index: usize,
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
) -> bool {
    let slot_id = {
        let Some(slot) = ctrl.slots.get_mut(slot_index) else {
            return false;
        };
        let dw0 = request_type as u32 | ((request as u32) << 8) | ((value as u32) << 16);
        let dw1 = index as u32;
        let dw3 = ((TRB_SETUP as u32) << 10) | TRB_IDT | TRT_NO_DATA;
        // The Setup TRB's status field is the transfer length of the SETUP PACKET,
        // which is always 8 — even for a request with no data stage (the stage is
        // signalled by TRT=0 instead). Writing 0 here made the controller reject
        // the whole transfer before the device ever saw it.
        slot.ep0.push(dw0, dw1, 8, dw3);
        // Status stage: for a transfer without a data stage the status is IN.
        slot.ep0
            .push(0, 0, 0, ((TRB_STATUS as u32) << 10) | TRB_DIR_IN | TRB_IOC);
        slot.id
    };
    ring_doorbell(ctrl, slot_id, 1);
    for _ in 0..8 {
        let Some(ev) = next_event(ctrl, 1_000_000) else {
            return false;
        };
        if ev.trb_type() != TRB_TRANSFER_EVENT || ev.endpoint_id() != 1 {
            continue;
        }
        let cc = ev.completion_code();
        if cc == CC_SUCCESS || cc == CC_SHORT_PACKET {
            return true;
        }
        crate::warn!("xhci: control request {:#04x} failed: cc={}", request, cc);
        return false;
    }
    false
}

/// Read a descriptor: `kind` is the descriptor type, `index` its index.
fn get_descriptor(
    ctrl: &mut Controller,
    slot_index: usize,
    kind: u8,
    index: u8,
    len: usize,
) -> Option<Vec<u8>> {
    control_in(
        ctrl,
        slot_index,
        0x80,
        6,
        ((kind as u16) << 8) | index as u16,
        0,
        len,
    )
}

/// Enable a slot and return its index in `ctrl.slots`.
fn enable_slot(ctrl: &mut Controller) -> Option<usize> {
    let ev = command(ctrl, 0, 0, 0, (TRB_ENABLE_SLOT as u32) << 10)?;
    if ev.completion_code() != CC_SUCCESS {
        crate::warn!("xhci: enable slot failed: cc={}", ev.completion_code());
        return None;
    }
    let id = ev.slot_id();
    if id == 0 {
        return None;
    }
    // The device context array entry for this slot must point at the device
    // context before ADDRESS_DEVICE is issued.
    let ep0 = Ring::new("ep0 ring")?;
    let input_ctx = DmaFrame::new_zeroed("input context")?;
    let device_ctx = DmaFrame::new_zeroed("device context")?;
    let ctrl_buf = DmaFrame::new_zeroed("control buffer")?;
    // SAFETY: DCBAA is our own frame and `id` is a slot the controller just gave
    // us, so the index is inside the array.
    unsafe {
        let entry =
            (crate::memory::vmm::phys_to_virt(ctrl.dcbaa_phys) as *mut u64).add(id as usize);
        write_volatile(entry, device_ctx.phys());
    }
    ctrl.slots.push(Slot {
        id,
        port: 0,
        speed: 0,
        ep0,
        input_ctx,
        device_ctx,
        ctrl_buf,
        hid: None,
    });
    Some(ctrl.slots.len() - 1)
}

/// Build the input context for ADDRESS_DEVICE and issue the command.
fn address_device(ctrl: &mut Controller, slot_index: usize, port: u8, speed: u8) -> bool {
    let ctx_size = ctrl.ctx_size;
    let Some(slot) = ctrl.slots.get(slot_index) else {
        return false;
    };
    let input = slot.input_ctx.virt();
    let input_phys = slot.input_ctx.phys();
    let ep0_phys = slot.ep0.base_phys();
    let device_ctx_phys = slot.device_ctx.phys();
    let slot_id = slot.id;
    let max_packet = ep0_max_packet(speed);

    // SAFETY: `input` is our own zeroed page; the Input Control Context (32 B),
    // the slot context and the EP0 context all live inside it at the offsets the
    // spec defines for a controller with this context size.
    unsafe {
        let p = input as *mut u32;
        // ICC: add slot + EP0.
        write_volatile(p, 0); // drop flags
        write_volatile(p.add(1), 0b11); // add flags: A0 (slot) + A1 (EP0)
                                        // Slot context: speed + port + one context entry.
        let slot_ctx = p.add(8) as *mut u32; // 32-byte ICC
        write_volatile(
            slot_ctx,
            (1 << SLOT_CTX_ENTRIES_SHIFT) | ((speed as u32) << SLOT_CTX_SPEED_SHIFT),
        );
        write_volatile(slot_ctx.add(1), (port as u32) << SLOT_CTX_PORT_SHIFT);
        // EP0 context. DWORD 0 carries only the interval (0 for a control
        // endpoint); CErr, the endpoint type and the max packet size live in
        // DWORD 1. This layout matters: the first version put CErr and the max
        // packet in DWORD 0, which put the packet size in the interval field and
        // left EP info 2 empty — control transfers survived on the device's own
        // EP0 defaults, which is exactly why the mistake stayed invisible until
        // an interrupt endpoint had to be configured.
        let ep_ctx = (input + ctx_size * 2) as *mut u32;
        write_volatile(ep_ctx, 0);
        write_volatile(
            ep_ctx.add(1),
            (3 << EP_CTX_CERR_SHIFT)
                | ((EP_TYPE_CONTROL) << EP_CTX_EP_TYPE_SHIFT)
                | ((max_packet) << EP_CTX_MAX_PACKET_SHIFT),
        );
        write_volatile(ep_ctx.add(2), (ep0_phys | 1) as u32);
        write_volatile(ep_ctx.add(3), (ep0_phys >> 32) as u32);
    }
    let _ = device_ctx_phys;

    // The TRB parameter is a GUEST-PHYSICAL address; `input` is the HHDM virtual
    // one. Handing the controller the virtual address made it read unrelated
    // memory and reject the command with a TRB error (cc=5) — the second half of
    // why enumeration had never worked.
    let (a0, a1) = ctx_addr(input_phys);
    let ev = command(
        ctrl,
        a0,
        a1,
        0,
        ((TRB_ADDRESS_DEVICE as u32) << 10) | ((slot_id as u32) << 24),
    );
    match ev {
        Some(ev) if ev.completion_code() == CC_SUCCESS => true,
        Some(ev) => {
            crate::warn!("xhci: address device failed: cc={}", ev.completion_code());
            false
        }
        None => false,
    }
}

/// Parsed configuration descriptor: the HID interface and its IN endpoint.
struct HidIface {
    config_value: u8,
    interface: u8,
    subclass: u8,
    protocol: u8,
    ep_addr: u8,
    max_packet: u16,
    interval: u8,
}

/// Walk the configuration descriptor looking for a HID interface with an
/// interrupt-IN endpoint.
fn parse_config(conf: &[u8]) -> Option<HidIface> {
    // bConfigurationValue is what SET_CONFIGURATION takes (offset 5).
    let config_value = *conf.get(5)?;
    let mut i = 0usize;
    let mut current: Option<(u8, u8, u8)> = None;
    while i + 2 <= conf.len() {
        let len = conf[i] as usize;
        if len < 2 || i + len > conf.len() {
            break;
        }
        let kind = conf[i + 1];
        if kind == 4 && len >= 9 {
            // INTERFACE: class 3 (HID), and remember its subclass/protocol.
            let number = conf[i + 2];
            let class = conf[i + 5];
            let subclass = conf[i + 6];
            let protocol = conf[i + 7];
            current = if class == 3 {
                Some((number, subclass, protocol))
            } else {
                None
            };
        } else if kind == 5 && len >= 7 {
            // ENDPOINT: attributes bit 0-1 == 3 means interrupt.
            let (number, subclass, protocol) = current?;
            let addr = conf[i + 2];
            let attrs = conf[i + 3];
            if attrs & 0x03 == 3 && addr & 0x80 != 0 {
                return Some(HidIface {
                    config_value,
                    interface: number,
                    subclass,
                    protocol,
                    ep_addr: addr,
                    max_packet: u16::from_le_bytes([conf[i + 4], conf[i + 5]]),
                    interval: conf[i + 6],
                });
            }
        }
        i += len;
    }
    None
}

/// Configure the HID interrupt-IN endpoint (DCI = 2*number + 1 for IN).
fn configure_hid_endpoint(ctrl: &mut Controller, slot_index: usize, iface: &HidIface) -> bool {
    let dci = (iface.ep_addr & 0x0F) * 2 + 1;
    let ctx_size = ctrl.ctx_size;
    let Some(ring) = Ring::new("hid ring") else {
        return false;
    };
    let Some(buf) = DmaFrame::new_zeroed("hid buffer") else {
        return false;
    };
    let (input, input_phys, ring_phys, buf_phys, slot_id, speed, max_packet, interval) = {
        let slot = &mut ctrl.slots[slot_index];
        (
            slot.input_ctx.virt(),
            slot.input_ctx.phys(),
            ring.base_phys(),
            buf.phys(),
            slot.id,
            slot.speed,
            iface.max_packet as u32,
            iface.interval,
        )
    };
    // SAFETY: `input` is our own page, and the contexts are written at the
    // offsets the controller's context size dictates.
    unsafe {
        let p = input as *mut u32;
        // Clear the previous input context, then declare the new endpoint.
        core::ptr::write_bytes(input as *mut u8, 0, PAGE_SIZE);
        write_volatile(p.add(1), (1 << 0) | (1u32 << dci)); // A0 + A(dci)
        let slot_ctx = p.add(8) as *mut u32;
        write_volatile(slot_ctx, (dci as u32) << SLOT_CTX_ENTRIES_SHIFT);
        let ep_ctx = (input + ctx_size * (1 + dci as usize)) as *mut u32;
        // xHCI stores the polling interval as log2(microframes) for high/super
        // speed devices (so USB bInterval=1 is field 0) and as the frame count for
        // full/low speed. Writing bInterval raw gave an interval of 2^bInterval.
        let interval_field: u32 = if speed == SPEED_HIGH || speed == SPEED_SUPER {
            (interval.saturating_sub(1) & 0x0F) as u32
        } else {
            (interval & 0x0F) as u32
        };
        write_volatile(ep_ctx, interval_field << EP_CTX_INTERVAL_SHIFT);
        write_volatile(
            ep_ctx.add(1),
            (3 << EP_CTX_CERR_SHIFT)
                | (EP_TYPE_INTERRUPT_IN << EP_CTX_EP_TYPE_SHIFT)
                | (max_packet << EP_CTX_MAX_PACKET_SHIFT),
        );
        write_volatile(ep_ctx.add(2), (ring_phys | 1) as u32);
        write_volatile(ep_ctx.add(3), (ring_phys >> 32) as u32);
    }
    // Physical address: see the same note in `address_device`.
    let (a0, a1) = ctx_addr(input_phys);
    let ev = command(
        ctrl,
        a0,
        a1,
        0,
        ((TRB_CONFIGURE_ENDPOINT as u32) << 10) | ((slot_id as u32) << 24),
    );
    match ev {
        Some(ev) if ev.completion_code() == CC_SUCCESS => {}
        other => {
            crate::warn!(
                "xhci: configure endpoint failed (cc={:?})",
                other.map(|e| e.completion_code())
            );
            return false;
        }
    }
    let _ = buf_phys;
    let report_len = if iface.protocol == 1 { 8 } else { 4 };
    ctrl.slots[slot_index].hid = Some(HidEndpoint {
        dci,
        ring,
        buf,
        len: report_len,
        class: iface.subclass,
        protocol: iface.protocol,
    });
    true
}

/// Enumerate the device on `port`, returning true when a HID interface was
/// configured. Logs what it found either way.
fn enumerate_with_speed(ctrl: &mut Controller, port: u8, speed: u8) -> bool {
    let speed_name = match speed {
        SPEED_LOW => "low",
        SPEED_FULL => "full",
        SPEED_HIGH => "high",
        SPEED_SUPER => "super",
        _ => "?",
    };
    let Some(slot_index) = enable_slot(ctrl) else {
        return false;
    };
    ctrl.slots[slot_index].port = port;
    ctrl.slots[slot_index].speed = speed;
    if !address_device(ctrl, slot_index, port, speed) {
        return false;
    }
    // Device descriptor: 8 bytes first (a full-speed device may only have an
    // 8-byte EP0 at this point), then the whole 18.
    let Some(head) = get_descriptor(ctrl, slot_index, 1, 0, 8) else {
        crate::warn!("xhci: port {}: no device descriptor", port);
        return false;
    };
    if head.len() < 8 {
        return false;
    }
    let vendor = u16::from_le_bytes([head[8 - 8], head[9 - 8]]);
    let product = u16::from_le_bytes([head[10 - 8], head[11 - 8]]);
    let Some(dev) = get_descriptor(ctrl, slot_index, 1, 0, 18) else {
        return false;
    };
    if dev.len() < 18 {
        return false;
    }
    let max_packet0 = dev[7];
    crate::info!(
        "xhci: port {} {:04x}:{:04x} speed={} ep0={}B",
        port,
        vendor,
        product,
        speed_name,
        max_packet0
    );
    // Configuration descriptor: 9 bytes for the total length, then the whole blob.
    let Some(head) = get_descriptor(ctrl, slot_index, 2, 0, 9) else {
        return false;
    };
    if head.len() < 9 {
        return false;
    }
    let total = u16::from_le_bytes([head[2], head[3]]) as usize;
    if total < 9 || total > PAGE_SIZE {
        return false;
    }
    let Some(conf) = get_descriptor(ctrl, slot_index, 2, 0, total) else {
        return false;
    };
    let Some(iface) = parse_config(&conf) else {
        crate::info!(
            "xhci: port {}: device {:04x}:{:04x} has no HID interrupt-IN interface",
            port,
            vendor,
            product
        );
        return false;
    };
    // SET_CONFIGURATION activates the interface (without it the HID endpoint
    // stalls), SET_PROTOCOL(1) selects the boot report layout this driver parses,
    // and SET_IDLE(0) stops the device NAKing reports when nothing changes.
    if !control_no_data(ctrl, slot_index, 0x00, 9, iface.config_value as u16, 0) {
        crate::warn!("xhci: port {}: SET_CONFIGURATION failed", port);
        return false;
    }
    if !control_no_data(ctrl, slot_index, 0x21, 0x0B, 1, iface.interface as u16) {
        crate::warn!("xhci: port {}: SET_PROTOCOL(boot) failed", port);
    }
    if !control_no_data(ctrl, slot_index, 0x21, 0x0A, 0, iface.interface as u16) {
        crate::warn!("xhci: port {}: SET_IDLE failed", port);
    }
    if !configure_hid_endpoint(ctrl, slot_index, &iface) {
        return false;
    }
    // Boot protocol: 0 = none, 1 = keyboard, 2 = mouse.
    crate::info!(
        "xhci: HID device on port {}: iface={} subclass={} protocol={} ep={:#04x} report={}B",
        port,
        iface.interface,
        iface.subclass,
        iface.protocol,
        iface.ep_addr,
        if (iface.protocol == 1) { 8 } else { 4 }
    );
    true
}

/// One poll pass: enumerate anything newly attached, then collect HID reports
/// and hand them to [`crate::drivers::usb_hid`].
pub fn poll() {
    let mut guard = CONTROLLER.lock();
    let Some(ctrl) = guard.as_mut() else {
        return;
    };
    // 1. Bring up every port once (power, connect check, reset), then enumerate
    //    what appeared. `reset_port` reads the connect status before resetting, so
    //    an empty port costs one register read instead of a reset timeout. Ports
    //    are brought up exactly once, so the cost is per boot, not per poll.
    for port in 1..=ctrl.ports {
        if ctrl.slots.iter().any(|s| s.port == port) {
            continue;
        }
        if !ctrl.brought_up.contains(&port) {
            ctrl.brought_up.push(port);
            match reset_port(ctrl, port) {
                Some(speed) => {
                    if enumerate_with_speed(ctrl, port, speed) {
                        ctrl.devices += 1;
                    }
                }
                None => crate::debug!("xhci: port {} has no device", port),
            }
            continue;
        }
        // A device plugged in later shows up as a change on an already reset
        // port: reset it again to bring the new device up.
        let off = portsc(port);
        let sc = rd32(ctrl.op, off);
        if sc & PORTSC_CSC != 0 {
            wr32(ctrl.op, off, (sc & !PORTSC_RW1C) | PORTSC_CSC);
            if let Some(speed) = reset_port(ctrl, port) {
                if enumerate_with_speed(ctrl, port, speed) {
                    ctrl.devices += 1;
                }
            }
        }
    }
    // One-shot port map once every port has been through bring-up: on a machine
    // where nothing enumerates, this says whether the controller reports a device
    // at all, which separates "our reset/enumeration is wrong" from "the device
    // is not on this controller".
    if !ctrl.logged_ports && ctrl.brought_up.len() >= ctrl.ports as usize {
        ctrl.logged_ports = true;
        for port in 1..=ctrl.ports {
            let sc = rd32(ctrl.op, portsc(port));
            if sc != 0 {
                crate::info!(
                    "xhci: port {} after bring-up PORTSC={:#010x} ccs={} ped={} pp={} speed={} pls={}",
                    port,
                    sc,
                    sc & PORTSC_CCS != 0,
                    sc & PORTSC_PED != 0,
                    sc & PORTSC_PP != 0,
                    (sc & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT,
                    (sc & PORTSC_PLS_MASK) >> 5
                );
            }
        }
    }

    // 2. Drain reports from every configured HID endpoint.
    for index in 0..ctrl.slots.len() {
        let Some(hid) = ctrl.slots[index].hid.as_ref() else {
            continue;
        };
        let dci = hid.dci;
        let len = hid.len;
        let slot_id = ctrl.slots[index].id;
        // Re-arm a single interrupt-IN transfer if none is outstanding.
        let armed = {
            let hid = ctrl.slots[index].hid.as_mut().unwrap();
            if hid.ring.outstanding {
                true
            } else {
                let buf_phys = hid.buf.phys();
                hid.ring.push(
                    buf_phys as u32,
                    (buf_phys >> 32) as u32,
                    len as u32,
                    ((TRB_NORMAL as u32) << 10) | TRB_IOC,
                );
                hid.ring.outstanding = true;
                false
            }
        };
        if !armed {
            ring_doorbell(ctrl, slot_id, dci);
        }
        // Collect a completion if one is waiting.
        let mut events = 0;
        while events < 4 {
            let Some(ev) = next_event(ctrl, 0) else {
                break;
            };
            if ev.trb_type() != TRB_TRANSFER_EVENT || ev.endpoint_id() != dci {
                continue;
            }
            events += 1;
            let slot = &mut ctrl.slots[index];
            let Some(hid) = slot.hid.as_mut() else { break };
            hid.ring.outstanding = false;
            let cc = ev.completion_code();
            if cc == CC_SUCCESS || cc == CC_SHORT_PACKET {
                let got = len.saturating_sub(ev.residual() as usize);
                if got > 0 {
                    // SAFETY: the buffer is our own page and `got <= len`.
                    let report =
                        unsafe { core::slice::from_raw_parts(hid.buf.virt() as *const u8, got) };
                    crate::drivers::usb_hid::feed(report, hid.protocol);
                    ctrl.reports += 1;
                }
            } else {
                // A stalled or overrun endpoint cannot be fixed by re-arming it;
                // re-arming is what turns one bad context into a thousand events a
                // second. Say so once and stop listening on this device.
                crate::warn!(
                    "xhci: HID endpoint (DCI {}) gave up: cc={}; input from it is off until reboot",
                    dci,
                    cc
                );
                ctrl.slots[index].hid = None;
                break;
            }
        }
    }
}

/// Poll loop, run as the kernel's `usb` thread.
///
/// One pass: enumerate anything newly attached, then collect whatever reports the
/// configured HID endpoints have produced. Sleeping a millisecond between passes
/// costs nothing measurable (the transfers are re-armed, not restarted) and keeps
/// the thread out of the way of the shell and the network pump.
pub fn usb_thread() {
    if status().0 {
        crate::info!("usb: poll thread started");
    }
    loop {
        poll();
        crate::task::scheduler::sleep_ticks(1);
    }
}

/// Every port's raw PORTSC, for the shell's `usb` command.
///
/// This is the diagnostic that matters on hardware: if a keyboard is plugged in
/// and its port still reads `ccs=false`, the problem is below this driver (cable,
/// port, or the device never reporting a speed); if a port reads `ccs=true` but
/// no `HID device on port N` line followed, the failure is in the enumeration
/// steps, and the log names the one that stopped.
pub fn port_status() -> Vec<(u8, u32)> {
    let guard = CONTROLLER.lock();
    let Some(ctrl) = guard.as_ref() else {
        return Vec::new();
    };
    (1..=ctrl.ports)
        .map(|port| {
            let sc = rd32(ctrl.op, portsc(port));
            (port, sc)
        })
        .collect()
}

/// Decode a PORTSC word for display: `(ccs, ped, pp, speed, pls)`.
pub fn decode_portsc(sc: u32) -> (bool, bool, bool, u8, u8) {
    (
        sc & PORTSC_CCS != 0,
        sc & PORTSC_PED != 0,
        sc & PORTSC_PP != 0,
        ((sc & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT) as u8,
        ((sc & PORTSC_PLS_MASK) >> 5) as u8,
    )
}

/// `(present, ports, reports)` for the shell's diagnostics.
pub fn status() -> (bool, u8, u64) {
    match CONTROLLER.lock().as_ref() {
        Some(c) => (true, c.ports, c.reports),
        None => (false, 0, 0),
    }
}
