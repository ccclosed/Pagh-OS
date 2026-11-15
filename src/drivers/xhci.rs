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
//! SCOPE. Enumeration brings a device to the point of configured HID interfaces
//! with interrupt-IN endpoints, which is exactly what boot-protocol keyboards
//! and mice need; [`crate::drivers::usb_hid`] turns those reports into the
//! scancode stream the shell already consumes. Every boot-class keyboard/mouse
//! interface of a device is bound (a combo keyboard's consumer/system
//! collections are skipped: they have no layout this driver parses), all of them
//! are configured with one xHCI Configure Endpoint command and polled together.
//! Root ports only — a device behind a hub is not reached (no hub class driver).
//! Not implemented: isochronous transfers, suspend/resume, hot-unplug teardown,
//! USB 3 link power management, HID report-descriptor parsing (report-protocol
//! support is best effort; see `usb_hid`), keyboard LED output reports.

use alloc::vec;
use alloc::vec::Vec;

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::drivers::pci::PciDevice;
use crate::drivers::usb_desc::{self, HidIface};
use crate::drivers::usb_hid_proto::{self, RecoveryAction, ReportMode};
use crate::drivers::xhci_port::{self, FailReason, Phase, PortObservation, PortPolicy, Step};
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

impl Drop for DmaFrame {
    fn drop(&mut self) {
        crate::memory::pmm::free_frame(self.phys);
    }
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

// ─── Clock ───────────────────────────────────────────────────────────────────

/// TSC ticks per microsecond, measured once at boot. 0 means "no usable clock".
///
/// WHY A CLOCK AT ALL. Every wait in this driver used to be "N spins", which is a
/// different wall-clock duration on every machine. The field machine is a 26-port
/// PCH where a USB2 port needs tens of milliseconds to finish a reset, while a
/// spin budget that looked generous under QEMU can expire in the middle of one —
/// and the driver then reports the port as failed without ever having given the
/// link the time it asked for. The boot menu polls this driver with interrupts
/// disabled (so `scheduler::ticks` never advances), but `rdtsc` works in every
/// context; calibrating it against the PIT at boot turns every USB wait into a
/// real deadline.
static TSC_PER_US: AtomicU64 = AtomicU64::new(0);

fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: `rdtsc` only reads the time-stamp counter. It has no memory side
    // effects, so the compiler is told so (`nomem`) and nothing is pushed.
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    ((hi as u64) << 32) | lo as u64
}

/// Measure the TSC against a 10 ms PIT channel-2 one-shot.
///
/// Runs from `init_usb`, i.e. before the boot menu starts using PIT channel 2 for
/// its own countdown, so the two users never overlap.
fn calibrate_clock() -> bool {
    use x86_64::instructions::port::Port;
    const PIT_HZ: u64 = 1_193_182;
    // 10 ms one-shot on channel 2.
    let count = (PIT_HZ / 100) as u16 - 1;
    // SAFETY: port 0x61 gates PIT channel 2 and mirrors its output; 0x43/0x42 are
    // the PIT command and channel-2 data ports. The gate value is restored on
    // every return path, and the speaker is never enabled (bit 1 stays clear).
    unsafe {
        let mut gate: Port<u8> = Port::new(0x61);
        let mut cmd: Port<u8> = Port::new(0x43);
        let mut ch2: Port<u8> = Port::new(0x42);
        let saved = gate.read();
        gate.write((saved & !0x02) | 0x01);
        cmd.write(0b1011_0000);
        ch2.write((count & 0xFF) as u8);
        ch2.write((count >> 8) as u8);
        let start = rdtsc();
        let mut spins = 0u32;
        while gate.read() & 0x20 == 0 {
            spins += 1;
            if spins > 50_000_000 {
                gate.write(saved);
                crate::warn!("xhci: no clock (PIT channel 2 never timed out)");
                return false;
            }
            core::hint::spin_loop();
        }
        let end = rdtsc();
        gate.write(saved);
        // The window is 10 ms, so ticks/second is the delta times 100.
        let hz = end.wrapping_sub(start).saturating_mul(100);
        if !(1_000_000..=100_000_000_000).contains(&hz) {
            crate::warn!("xhci: no clock (implausible TSC rate {})", hz);
            return false;
        }
        TSC_PER_US.store((hz / 1_000_000).max(1), Ordering::Relaxed);
        crate::info!("xhci: clock: TSC ~{} MHz", hz / 1_000_000);
    }
    true
}

fn clock_ready() -> bool {
    TSC_PER_US.load(Ordering::Relaxed) != 0
}

/// Microseconds from an arbitrary epoch; only differences are meaningful.
fn now_us() -> u64 {
    let per_us = TSC_PER_US.load(Ordering::Relaxed);
    if per_us == 0 {
        return 0;
    }
    rdtsc() / per_us
}

/// Busy-wait for at least `us` microseconds, where the spec mandates a delay
/// (USB2 reset recovery, port power settle, firmware handoff timeout).
fn settle_us(us: u64) {
    if clock_ready() {
        let deadline = now_us() + us;
        while now_us() < deadline {
            core::hint::spin_loop();
        }
    } else {
        // No calibrated clock: fall back to the old spin-count behaviour.
        spin_settle((us.saturating_mul(3)).min(u32::MAX as u64) as u32);
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
const TRB_DISABLE_SLOT: u8 = 10;
const TRB_ADDRESS_DEVICE: u8 = 11;
const TRB_CONFIGURE_ENDPOINT: u8 = 12;
const TRB_EVALUATE_CONTEXT: u8 = 13;
/// Reset Endpoint (xHCI 1.2 table 6-91): recovers a halted endpoint so its
/// transfer ring can be re-armed after a babble/stall instead of disabling input
/// until reboot.
const TRB_RESET_ENDPOINT: u8 = 14;
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
const USBSTS_HSE: u32 = 1 << 1;
const USBSTS_HCE: u32 = 1 << 11;
const USBSTS_CNR: u32 = 1 << 12;

/// Port Link State values this driver acts on (xHCI 1.2 table 5-27).
const PLS_U0: u32 = 0;

const PORTSC_CCS: u32 = 1 << 0;
const PORTSC_PED: u32 = 1 << 1;
const PORTSC_PR: u32 = 1 << 4;
const PORTSC_PLS_MASK: u32 = 0xF << 5;
const PORTSC_PLS_SHIFT: u32 = 5;
const PORTSC_PP: u32 = 1 << 9;
/// Port Link State is writable only in the same write that sets this bit; a
/// write of the field alone is ignored by the controller.
const PORTSC_LWS: u32 = 1 << 16;
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

/// One HID interrupt-IN endpoint: its ring, its report buffer, its DCI and what
/// SET_PROTOCOL left the interface in.
struct HidEndpoint {
    /// Interface number, for the diagnostic lines.
    iface: u8,
    /// `bInterfaceSubClass` (1 = boot) and `bInterfaceProtocol` (1 keyboard,
    /// 2 mouse): what the report parser needs to know.
    subclass: u8,
    protocol: u8,
    /// Boot or report protocol as SET_PROTOCOL's result left it.
    mode: ReportMode,
    /// Device endpoint context index (2 * endpoint number + direction).
    dci: u8,
    ring: Ring,
    buf: DmaFrame,
    /// Transfer buffer size: at least the endpoint's wMaxPacketSize, so a report
    /// longer than the old hard-coded 4 bytes cannot babble the endpoint.
    len: usize,
    /// Reset Endpoint attempts made since configuration (bounded by
    /// `usb_hid_proto::MAX_RECOVERIES`).
    recoveries: u8,
    /// Reports shorter than the layout's minimum, counted in diagnostics.
    bad_reports: u64,
    /// Whether the wrong-size report was already logged (one line, not one per
    /// poll: a broken device must not flood the serial log).
    bad_report_logged: bool,
    /// Set once the recovery budget is spent; the endpoint is no longer polled.
    disabled: bool,
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
    /// Every bound HID interrupt-IN endpoint of this device. A combo device can
    /// have several (keyboard + consumer/system collections); the old driver kept
    /// only the first.
    hid: Vec<HidEndpoint>,
}

/// What the driver got out of one port. The boot menu prints this: on a machine
/// with a dead keyboard and no serial port it is the entire bug report.
///
/// The timing decisions live in [`PortPolicy`] (host-tested, see
/// `src/drivers/xhci_port.rs`); this struct is the display copy plus the per-port
/// link snapshot the policy cannot print by itself.
#[derive(Clone, Copy)]
struct PortDiag {
    /// Last stage the port reached (`""` before the first attempt).
    stage: &'static str,
    /// Completion code (or PORTSC value) that ended it.
    code: u32,
    /// Speed reported at the last successful adoption/reset.
    speed: u8,
    /// Reset attempts so far (`try=`; the policy retries forever with backoff).
    attempts: u8,
    /// PORTSC just before the last reset and just after it. These two values are
    /// the difference between "the port never enabled" and "the device did not
    /// answer after it was enabled".
    pre: u32,
    post: u32,
    /// Whether PRC / PED were observed during the last reset wait. `seen_prc`
    /// false means the controller never even acknowledged the reset request.
    seen_prc: bool,
    seen_ped: bool,
    /// PED was seen together with PLS=U0 and a valid speed. `ped_sc` alone does
    /// not say that: a reset-in-progress glimpse has PLS=Polling (or no speed),
    /// so this bit distinguishes a U0 sample from a transient PED read.
    seen_ped_u0: bool,
    /// USB protocol generation of this port: 2, 3, or 0 when the controller did not
    /// describe it (the Supported Protocol capabilities say which port ranges are
    /// USB2 and which are USB3 — a keyboard is USB2 even when the port looks like a
    /// USB3 one).
    proto: u8,
    /// PORTSC at the LAST PED+U0 sighting; while the port never reached U0, the
    /// LAST transient PED sighting. Per-port by construction: each `PortDiag`
    /// owns its `PortPolicy`, there is no shared value and no shared index.
    ped_sc: u32,
    /// Wall-clock duration of the last attempt in milliseconds. Zero when the
    /// clock is unavailable. This is what separates "the port needs more time"
    /// from "the controller refuses" in a photo.
    ms: u16,
    /// The host-tested state machine: phase, deadlines, backoff, power-cycle
    /// gate, and the per-attempt PRC/PED snapshots.
    policy: PortPolicy,
}

struct Controller {
    op: usize,
    db: usize,
    rt: usize,
    ctx_size: usize,
    /// xHCI controllers found on the PCI bus (this one is the chosen one).
    controllers: usize,
    ports: u8,
    cmdr: Ring,
    event: Ring,
    erst: DmaFrame,
    /// Device Context Base Address Array. The controller writes a device-context
    /// pointer into this frame on Enable Slot and fetches it again for every later
    /// command on that slot, so the frame belongs to the controller, not to this
    /// function's stack: `dcbaa` used to be a local whose page was returned to the
    /// PMM as soon as `start_controller` returned. Under QEMU the page is rarely
    /// reused and the driver "works"; on hardware the first allocation to reuse it
    /// makes the controller fetch a garbage device context — a Host Controller
    /// Error, which halts the controller mid-boot and freezes every port with it.
    dcbaa: DmaFrame,
    /// Device context array base address (DCBAAP), kept for the release path.
    dcbaa_phys: u64,
    /// Scratchpad buffer array (pointed at by DCBAA[0]) and the pages it points
    /// to. Both must outlive the controller for the same reason as `dcbaa`.
    #[allow(dead_code)] // kept alive: DMA state the controller reads
    scratchpad_array: Option<DmaFrame>,
    #[allow(dead_code)] // kept alive: DMA state the controller reads
    scratchpad_pages: Vec<DmaFrame>,
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
    /// Per-port enumeration state for the on-screen diagnostic.
    diag: Vec<PortDiag>,
    /// The last thing that went wrong anywhere, for the one-line status.
    last_fail: Option<(u8, &'static str, u32)>,
    /// Attached-port mask as the scan saw it BEFORE the controller reset. Ports
    /// present then and absent now are the ones the reset cost us.
    probe_mask: u32,
    /// Round-robin cursor: at most ONE port is (re)initialised per poll so that a
    /// slow reset attempt cannot stall the boot menu's countdown.
    next_port: u8,
    /// Monotonic fallback clock (1 ms per poll) for the port policy when the TSC
    /// calibration failed. The policy's deadlines are microseconds; without a
    /// real clock the poll count is the best available time base and is far
    /// better than treating every deadline as already expired.
    fallback_us: u64,
    /// Per-port USB protocol (2 or 3) derived from the Supported Protocol
    /// capabilities, for the on-screen diagnostic. Mirrored into `PortDiag.proto`
    /// (which is what the screens print); kept here for the shell `usb` command.
    #[allow(dead_code)]
    protos: Vec<u8>,
    /// Port masks ([usb2, usb3]) as the Supported Protocol capabilities described
    /// them. Masks, not (first, count): the field machine's controller reports two
    /// entries whose ranges overlap, and a mask shows that honestly instead of
    /// hiding it behind a pretty range.
    proto_masks: [u32; 2],
    /// Raw extended-capability entries, for the screen dump.
    xcaps: Vec<XCap>,
    /// CAPLENGTH as read at init (the dump shows whether the operational base is
    /// where the driver thinks it is).
    caplength: u32,
    hciversion: u32,
    hcsparams1: u32,
    hccparams1: u32,
    /// What the BIOS/USB-legacy handoff found: `handoff`, `bios-owned`,
    /// `absent`, or `bad-xecp`.
    legacy: &'static str,
    /// A harmless register was written and read back at init: this separates "our
    /// writes are ignored" from "the controller ignores this particular write".
    writes_ok: bool,
    /// PCI command register at init (bit 1 = memory space, bit 2 = bus mastering).
    pci_cmd: u32,
    /// Times the controller was found halted (USBSTS.HCH), and the values that
    /// proved it. A halted controller ignores every port write, which looks
    /// exactly like "the ports never enable": this counter is the difference.
    halts: u32,
    halt_cmd: u32,
    halt_sts: u32,
    /// Transfer events that arrived while a command (Reset Endpoint,
    /// Configure Endpoint, …) was waiting for its completion. `command()` cannot
    /// drop them: the owning endpoint would stay "outstanding" forever and never
    /// be re-armed. The poll loop drains this queue before the event ring.
    deferred_events: Vec<Trb>,
    /// Successful Reset Endpoint recoveries since boot, for diagnostics.
    hid_recoveries: u64,
    /// Reports that were shorter than their layout's minimum, for diagnostics.
    hid_bad_reports: u64,
}

/// One extended-capability entry, as the walk saw it: offset in the BAR, id,
/// next-capability dword offset, and the two dwords a Supported Protocol entry
/// keeps its port range in. Printed on screen when the ranges look wrong, which
/// is exactly what the field machine did.
#[derive(Clone, Copy)]
pub struct XCap {
    pub offset: u32,
    pub id: u8,
    pub next: u8,
    /// Major revision from DW0 bits 31:24 (2 = USB2 protocol, 3 = USB3).
    pub major: u8,
    /// Minor revision from DW0 bits 23:16.
    pub minor: u8,
    pub dw1: u32,
    pub dw2: u32,
}

static CONTROLLER: Spinlock<Option<Controller>> = Spinlock::new(None);

/// How many xHCI controllers PCI enumeration saw, kept even when none started:
/// the screen-only diagnostic needs to tell "no controller" from "controller but
/// no device".
static CONTROLLERS_SEEN: AtomicUsize = AtomicUsize::new(0);

/// One PCI xHCI controller as the scan saw it, for the on-screen diagnostic. On a
/// board with two controllers this is how the operator (and the developer reading
/// a photo) sees which one has the keyboard.
#[derive(Clone, Copy)]
pub struct ControllerInfo {
    pub vendor: u16,
    pub device: u16,
    pub ports: u8,
    pub attached: u32,
    pub chosen: bool,
}

static CONTROLLER_SCAN: Spinlock<Vec<ControllerInfo>> = Spinlock::new(Vec::new());

/// What the controller scan found, in PCI order.
pub fn scan() -> Vec<ControllerInfo> {
    CONTROLLER_SCAN.lock().clone()
}

/// Raw register dump for the on-screen diagnostic: (`caplength`, `usbcmd`,
/// `usbsts`, `config`, `write-ok`, first four dwords of the selected port's
/// register set). This exists because "the port does not reset" has no other
/// explanation channel on a machine without serial.
pub struct RawDump {
    pub caplength: u32,
    pub hciversion: u32,
    pub hcsparams1: u32,
    pub hccparams1: u32,
    pub usbcmd: u32,
    pub usbsts: u32,
    pub config: u32,
    pub writes_ok: bool,
    pub legacy: &'static str,
    pub port: u8,
    pub port_regs: [u32; 4],
    /// PCI command register (bit 2 = bus mastering; without it the controller's
    /// ring DMA is aborted and it halts).
    pub pci_cmd: u32,
    /// Halts seen since boot, and the register values at the first one.
    pub halts: u32,
    pub halt_cmd: u32,
    pub halt_sts: u32,
}

/// Per-port protocol masks `[usb2, usb3]` (bit N = port N+1), for the diagnostic.
pub fn proto_masks() -> [u32; 2] {
    CONTROLLER
        .lock()
        .as_ref()
        .map(|c| c.proto_masks)
        .unwrap_or([0, 0])
}

/// Raw extended-capability walk, for the diagnostic.
pub fn xcaps() -> Vec<XCap> {
    CONTROLLER
        .lock()
        .as_ref()
        .map(|c| c.xcaps.clone())
        .unwrap_or_default()
}

pub fn raw_dump() -> Option<RawDump> {
    let guard = CONTROLLER.lock();
    let c = guard.as_ref()?;
    let port = (1..=c.ports).find(|p| rd32(c.op, portsc(*p)) & PORTSC_CCS != 0)?;
    Some(RawDump {
        caplength: c.caplength,
        hciversion: c.hciversion,
        hcsparams1: c.hcsparams1,
        hccparams1: c.hccparams1,
        usbcmd: rd32(c.op, OP_USBCMD),
        usbsts: rd32(c.op, OP_USBSTS),
        config: rd32(c.op, OP_CONFIG),
        writes_ok: c.writes_ok,
        legacy: c.legacy,
        pci_cmd: c.pci_cmd,
        halts: c.halts,
        halt_cmd: c.halt_cmd,
        halt_sts: c.halt_sts,
        port,
        port_regs: [
            rd32(c.op, portsc(port)),
            rd32(c.op, portsc(port) + 4),
            rd32(c.op, portsc(port) + 8),
            rd32(c.op, portsc(port) + 12),
        ],
    })
}

/// What the USB driver can say about itself, for the boot menu, the shell banner
/// and the `usb` command. On a machine with no serial port this struct IS the
/// field report.
pub struct Summary {
    /// xHCI controllers found on the PCI bus.
    pub controllers: usize,
    /// Ports on the controller this driver runs (0 when none started).
    pub ports: u8,
    /// Bit N set = port N+1 reports a connected device right now.
    pub ccs_mask: u32,
    /// Devices that made it all the way to a configured HID interface.
    pub devices: u64,
    /// HID reports handed to the input translator.
    pub reports: u64,
    /// Attached-port mask before the controller reset (what firmware left) versus
    /// `ccs_mask` (what the driver sees now).
    pub probe_mask: u32,
    /// Last enumeration failure: port, stage, code. `None` when everything that
    /// was attempted worked.
    pub last_fail: Option<(u8, &'static str, u32)>,
    /// HID endpoints reset after a transfer error (babble/stall/…) instead of
    /// being disabled until reboot.
    pub hid_recoveries: u64,
    /// Reports shorter than their layout's minimum (counted, logged once per
    /// endpoint).
    pub hid_bad_reports: u64,
}

impl Summary {
    /// True when some port reports a connected device — the first thing to look
    /// at when a keyboard does not type.
    pub fn any_attached(&self) -> bool {
        self.ccs_mask != 0
    }
}

/// Snapshot of the driver's state, cheap enough to call from a redraw loop.
pub fn summary() -> Summary {
    let guard = CONTROLLER.lock();
    match guard.as_ref() {
        Some(c) => {
            let mut mask = 0u32;
            for port in 1..=c.ports {
                if rd32(c.op, portsc(port)) & PORTSC_CCS != 0 {
                    mask |= 1 << (port - 1);
                }
            }
            Summary {
                controllers: c.controllers,
                ports: c.ports,
                ccs_mask: mask,
                devices: c.devices,
                reports: c.reports,
                probe_mask: c.probe_mask,
                last_fail: c.last_fail,
                hid_recoveries: c.hid_recoveries,
                hid_bad_reports: c.hid_bad_reports,
            }
        }
        None => Summary {
            controllers: CONTROLLERS_SEEN.load(Ordering::Relaxed),
            ports: 0,
            ccs_mask: 0,
            devices: 0,
            reports: 0,
            probe_mask: 0,
            last_fail: None,
            hid_recoveries: 0,
            hid_bad_reports: 0,
        },
    }
}

/// Monotonic microseconds for the port policy.
///
/// The policy's deadlines are real time (USB2 debounce/training/reset are tens to
/// hundreds of milliseconds by specification). `rdtsc` is calibrated at boot;
/// when calibration failed, `fallback_us` advances one millisecond per poll — a
/// poll thread sleeps one tick and the boot-menu loop is not slower than that,
/// so the fallback is a conservative time base rather than "all deadlines have
/// already expired" (which would restore the old reset-every-poll behaviour).
fn fsm_now(ctrl: &mut Controller) -> u64 {
    if clock_ready() {
        now_us()
    } else {
        ctrl.fallback_us = ctrl.fallback_us.saturating_add(1_000);
        ctrl.fallback_us
    }
}

/// Copy the policy's per-attempt link snapshot into the display row.
///
/// Every field comes from `diag[port-1].policy`: the values are per port by
/// construction. The field report's identical `ped_sc` on every USB2 port was
/// not a shared/index bug — it is the same reset-time transient (CCS+PED+PP+
/// speed=high+PRC) the controller reports on each port — so `seen_ped_u0` is
/// printed next to it to say whether the snapshot was a PED+U0 sample or only a
/// PED read during reset/Polling.
fn sync_diag(ctrl: &mut Controller, port: u8) {
    let Some(d) = ctrl.diag.get_mut(port as usize - 1) else {
        return;
    };
    let p = d.policy;
    d.pre = p.pre();
    d.post = p.post();
    d.seen_prc = p.seen_prc();
    d.seen_ped = p.seen_ped();
    d.seen_ped_u0 = p.seen_ped_u0();
    d.ped_sc = p.ped_sc();
    d.attempts = p.resets().min(u8::MAX as u16) as u8;
}

/// Record how long the last attempt took, in milliseconds (0 without a clock).
fn record_attempt_ms(ctrl: &mut Controller, port: u8) {
    if !clock_ready() {
        return;
    }
    let started = ctrl
        .diag
        .get(port as usize - 1)
        .map(|d| d.policy.attempt_started_us())
        .unwrap_or(0);
    if started == 0 {
        return;
    }
    let ms = now_us().saturating_sub(started) / 1000;
    if let Some(d) = ctrl.diag.get_mut(port as usize - 1) {
        d.ms = ms.min(u16::MAX as u64) as u16;
    }
}

/// Stage string shown while the policy is waiting (the failure stages are set
/// by [`port_failure`] and enumeration by `ctrl.fail`/`ctrl.stage`).
fn waiting_stage(phase: Phase) -> &'static str {
    match phase {
        Phase::Debounce => "debounce",
        Phase::ResetWait => "reset",
        Phase::Stabilize => "stabilize",
        Phase::PowerSettle => "power-settle",
        Phase::PowerCycle => "power-cycle",
        Phase::ResumeWait => "resume",
        Phase::Empty => "no-device",
        Phase::Backoff | Phase::Idle | Phase::Enumerating => "",
    }
}

/// Crude busy delay used only where a wrong-ordered reset costs a whole boot.
///
/// No PIT here on purpose: the boot menu programs PIT channel 2 itself and calls
/// into this driver while it counts, so a second user of that channel would break
/// the countdown.
fn spin_settle(iterations: u32) {
    let mut i = 0u32;
    while i < iterations {
        core::hint::spin_loop();
        i += 1;
    }
}

impl Controller {
    /// Record what happened on a port for the on-screen diagnostic.
    fn stage(&mut self, port: u8, stage: &'static str, code: u32) {
        if port == 0 {
            return;
        }
        if let Some(d) = self.diag.get_mut(port as usize - 1) {
            d.stage = stage;
            d.code = code;
        }
    }

    /// Record a FAILURE stage (and remember it as the last failure, which is what
    /// the one-line status shows when no HID device came up).
    fn fail(&mut self, port: u8, stage: &'static str, code: u32) {
        self.stage(port, stage, code);
        self.last_fail = Some((port, stage, code));
    }
}

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

/// Find, choose and start an xHCI controller.
///
/// A machine may expose MORE THAN ONE controller — desktop boards often pair the
/// chipset xHCI with a discrete one for the front-panel ports — and the keyboard
/// is on whichever one it was plugged into, so the driver probes every controller,
/// counts the ports that already report a connected device, and starts the one
/// with the most. Ties go to the first (stable PCI order).
///
/// `false` means "no controller, or none could be started" — USB input is simply
/// unavailable then, and PS/2 (plus any firmware translation) is all the machine
/// has.
pub fn init(devices: &[PciDevice]) -> bool {
    let candidates: Vec<&PciDevice> = devices
        .iter()
        .filter(|d| d.class == 0x0C && d.subclass == 0x03)
        .collect();
    CONTROLLERS_SEEN.store(candidates.len(), Ordering::Relaxed);
    if candidates.is_empty() {
        crate::warn!("xhci: no xHCI controller on the PCI bus");
        return false;
    }
    // The clock first: every wait below is expressed in microseconds, and the PIT
    // channel-2 window this calibration uses is free only until the boot menu
    // takes that channel for its own countdown.
    if !clock_ready() {
        calibrate_clock();
    }
    crate::info!(
        "xhci: {} xHCI controller(s) on the PCI bus",
        candidates.len()
    );
    let mut best: Option<(usize, u32)> = None; // (index, attached ports)
    let mut scan_infos: Vec<ControllerInfo> = Vec::new();
    for (index, dev) in candidates.iter().enumerate() {
        match probe_controller(dev) {
            Some((ports, mask)) => {
                let attached = mask.count_ones();
                crate::info!(
                    "xhci: controller {}/{} {:04x}:{:04x} ports={} attached_mask={:#010x}",
                    index + 1,
                    candidates.len(),
                    dev.vendor_id,
                    dev.device_id,
                    ports,
                    mask
                );
                scan_infos.push(ControllerInfo {
                    vendor: dev.vendor_id,
                    device: dev.device_id,
                    ports,
                    attached: mask,
                    chosen: false,
                });
                if best.map_or(true, |(_, n)| attached > n) {
                    best = Some((index, attached));
                }
            }
            None => crate::warn!(
                "xhci: controller {}/{} is unusable",
                index + 1,
                candidates.len()
            ),
        }
    }
    let Some((index, _)) = best else {
        crate::warn!("xhci: no xHCI controller could be mapped");
        return false;
    };
    if candidates.len() > 1 {
        crate::info!("xhci: using controller {}/{}", index + 1, candidates.len());
    }
    // Remember which one was chosen for the on-screen scan list.
    let chosen_pair = (candidates[index].vendor_id, candidates[index].device_id);
    for info in scan_infos.iter_mut() {
        info.chosen = info.vendor == chosen_pair.0 && info.device == chosen_pair.1;
    }
    *CONTROLLER_SCAN.lock() = scan_infos;
    start_controller(candidates[index], candidates.len())
}

/// Map BAR0 of a controller, or say why not.
fn map_bar0(dev: &PciDevice) -> Option<(usize, u64)> {
    let lo = crate::drivers::pci::config_read_u32(dev.address, 0x10);
    let hi = crate::drivers::pci::config_read_u32(dev.address, 0x14);
    let bar = ((hi as u64) << 32) | (lo as u64 & 0xFFFF_FFF0);
    if bar == 0 || lo & 0x6 != 0x4 {
        crate::warn!("xhci: BAR0 is not a 64-bit memory BAR (lo={:#x})", lo);
        return None;
    }
    match crate::memory::vmm::map_mmio(bar, 64 * 1024) {
        Ok(virt) => Some((virt as usize, bar)),
        Err(_) => {
            crate::warn!("xhci: cannot map BAR0 {:#x}", bar);
            None
        }
    }
}

/// Read the attached-device mask without disturbing the controller.
///
/// Runs before the BIOS handoff and the hardware reset, so on a real machine it
/// shows what firmware left behind; ports that are still in RxDetect simply read
/// as not attached and are retried after the reset by the port bring-up loop.
fn probe_controller(dev: &PciDevice) -> Option<(u8, u32)> {
    let (base, _bar) = map_bar0(dev)?;
    let caplength = (rd32(base, CAP_CAPLENGTH) & 0xFF) as usize;
    let op = base + caplength;
    let hcsparams1 = rd32(base, CAP_HCSPARAMS1);
    let ports = ((hcsparams1 >> 24) & 0xFF) as u8;
    let hccparams1 = rd32(base, CAP_HCCPARAMS1);
    bios_handoff(base, hccparams1);
    let mut mask = 0u32;
    for port in 1..=ports {
        if rd32(op, portsc(port)) & PORTSC_CCS != 0 {
            mask |= 1 << (port - 1);
        }
    }
    Some((ports, mask))
}

/// Take ownership of the controller from the firmware's USB legacy support.
///
/// WHY: xHCI carries a "USB Legacy Support" extended capability. While the BIOS
/// owns the semaphore it may keep SMI handlers trapping port accesses, and some
/// firmware leaves the controller in a state where the OS sees no port state at
/// all. The standard handoff is: ask for ownership (OS-owned bit), wait for the
/// BIOS-owned bit to clear, and if the firmware never answers, clear it yourself
/// after a bounded wait so boot continues.
fn bios_handoff(base: usize, hccparams1: u32) -> &'static str {
    const CAP_ID_USB_LEGACY: u32 = 1;
    const LEGSUP_BIOS_OWNED: u32 = 1 << 16;
    const LEGSUP_OS_OWNED: u32 = 1 << 24;
    let xecp = ((hccparams1 >> 16) & 0xFFFF) as usize;
    // The spec says extended capabilities start at xECP*4; firmware that reports
    // xECP=0 may still expose them where QEMU puts them (0x20), so that area is
    // scanned as a fallback. Getting this wrong means skipping the handoff, and a
    // BIOS-owned controller can keep the USB ports under SMM control — one of the
    // ways a port can enable and then be yanked back to Polling.
    let start = if xecp != 0 { xecp * 4 } else { 0x20 };
    let mut off = start;
    let mut guard = 0;
    // xECP is a dword offset and real controllers do use the far end of the BAR
    // (this Intel PCH reports 0x2000 dwords = 32 KiB). The BAR is mapped for
    // 64 KiB, so the walk may go there; the old 0x1000 limit aborted before the
    // capabilities and silently skipped the BIOS handoff.
    while off + 4 <= 0x10000 && guard < 64 {
        guard += 1;
        let cap = rd32(base, off);
        let id = cap & 0xFF;
        if id == CAP_ID_USB_LEGACY {
            let mut value = cap;
            if value & LEGSUP_BIOS_OWNED != 0 {
                wr32(base, off, value | LEGSUP_OS_OWNED);
                // xHCI 1.2 §7.1.1: the OS gives the firmware up to one second, then
                // takes ownership by force. The old two-million-spin budget was a
                // guess; on a machine whose firmware really is slow it could expire
                // in a few milliseconds and the "forced" path would run while the
                // firmware was still in the middle of releasing the ports.
                let clock = clock_ready();
                let start_us = if clock { now_us() } else { 0 };
                let mut spins = 0u32;
                loop {
                    value = rd32(base, off);
                    if value & LEGSUP_BIOS_OWNED == 0 {
                        crate::info!("xhci: BIOS handed the controller over");
                        return "handoff";
                    }
                    spins += 1;
                    let expired = if clock {
                        now_us().wrapping_sub(start_us) > 1_000_000
                    } else {
                        spins > 2_000_000
                    };
                    if expired {
                        wr32(base, off, (value & !LEGSUP_BIOS_OWNED) | LEGSUP_OS_OWNED);
                        crate::warn!("xhci: firmware did not release the controller; forced");
                        return "forced";
                    }
                    core::hint::spin_loop();
                }
            }
            wr32(base, off, value | LEGSUP_OS_OWNED);
            return "os-owned";
        }
        let next = ((cap >> 8) & 0xFF) as usize;
        if next == 0 || next >= 0x100 {
            break;
        }
        off += next * 4;
    }
    "absent"
}

/// Bring one controller all the way up: reset, rings, DCBAA, running.
fn start_controller(dev: &PciDevice, controllers: usize) -> bool {
    // Memory space AND bus mastering, before anything else: the controller's
    // command/event rings, DCBAA and scratchpads are all DMA, and a device with
    // bus mastering off cannot touch any of them. Every other DMA driver in this
    // kernel calls this; this one relied on firmware having left the bit set,
    // which is exactly the kind of state QEMU does not enforce and hardware does.
    crate::drivers::pci::enable_bus_master(dev.address);
    let pci_cmd = crate::drivers::pci::config_read_u32(dev.address, 0x04) & 0xFFFF;
    crate::info!(
        "xhci: PCI command={:#06x} (memory={} bus-master={})",
        pci_cmd,
        if pci_cmd & 0x2 != 0 { "on" } else { "OFF" },
        if pci_cmd & 0x4 != 0 { "on" } else { "OFF" }
    );
    // A real-time base for the port waits: USB2 reset/recovery/settle are tens of
    // milliseconds by specification, and a spin count is not a millisecond.
    if !clock_ready() {
        calibrate_clock();
    }
    let Some((base, bar)) = map_bar0(dev) else {
        return false;
    };
    // Attached ports before the reset, counted here so the diagnostic can show
    // exactly which connections the reset lost (firmware had them, we do not).
    let mut ccs_mask_probe = 0u32;

    let caplength = (rd32(base, CAP_CAPLENGTH) & 0xFF) as usize;
    let op = base + caplength;
    let hcsparams1 = rd32(base, CAP_HCSPARAMS1);
    let hcsparams2 = rd32(base, CAP_HCSPARAMS2);
    let hccparams1 = rd32(base, CAP_HCCPARAMS1);
    let hciversion = rd32(base, CAP_CAPLENGTH) >> 16;
    let legacy = bios_handoff(base, hccparams1);
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
        if sc & PORTSC_CCS != 0 {
            ccs_mask_probe |= 1 << (port as u32 - 1);
        }
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
    let dcbaa_phys_value = dcbaa.phys();
    let mut scratchpad_array: Option<DmaFrame> = None;
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
        // The array page is DMA state too: DCBAA[0] points at it and the
        // controller reads it whenever it needs a scratchpad. It travels into the
        // Controller with the frame above, for the same lifetime reason.
        scratchpad_array = Some(array);
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
    // Write-read-back on a harmless register: if this fails, every port write in
    // this driver is being dropped and no amount of reset logic can help.
    let before = rd32(op, OP_CONFIG);
    wr32(op, OP_CONFIG, before);
    let writes_ok = rd32(op, OP_CONFIG) == before;
    if !writes_ok {
        crate::warn!(
            "xhci: register write-readback failed (config {:#x})",
            before
        );
    }
    crate::info!("xhci: running (ports={} cr={:#x})", ports, cmdr.base_phys());
    // Give the PHYs a moment to settle before the first port reset: the firmware
    // left these links in some state and USB2 wants ~100 ms after a controller
    // reset (xHCI 1.2 §4.19.1). QEMU is instant, hardware is not.
    settle_us(100_000);
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

    // Which ports are USB2 and which are USB3, from the Supported Protocol
    // capabilities. This is DIAGNOSTIC, not policy: the reset path learns the
    // protocol from the port's own reported speed, because the field machine
    // shows how fragile this data can be (its controller reports two entries whose
    // ranges overlap). The raw entries are kept for the screen so the next photo
    // can be compared against the spec instead of against our parser — the old
    // display printed `usb2=17-41 usb3=17-26`, which is what a parser that reads
    // the wrong byte looks like.
    let mut protos: Vec<u8> = vec![0u8; ports as usize];
    let mut proto_masks: [u32; 2] = [0, 0];
    let mut xcaps: Vec<XCap> = Vec::new();
    let xecp = ((hccparams1 >> 16) & 0xFFFF) as usize;
    {
        let mut off = if xecp != 0 { xecp * 4 } else { 0x20 };
        let mut guard = 0;
        while off + 16 <= 0x10000 && guard < 64 {
            guard += 1;
            let dw0 = rd32(base, off);
            let id = (dw0 & 0xFF) as u8;
            let next = ((dw0 >> 8) & 0xFF) as u8;
            let dw1 = rd32(base, off + 4);
            let dw2 = rd32(base, off + 8);
            if xcaps.len() < 12 && (id == 1 || id == 2 || id == 3) {
                xcaps.push(XCap {
                    offset: off as u32,
                    id,
                    next,
                    // DW0 is the capability header for every extended capability,
                    // not only the protocol ones.
                    major: ((dw0 >> 24) & 0xFF) as u8,
                    minor: ((dw0 >> 16) & 0xFF) as u8,
                    dw1,
                    dw2,
                });
            }
            if id == 2 {
                // Supported Protocol capability, xHCI 1.2 section 7.2 / table 7-4:
                // DW0 is the header (major revision bits 31:24, minor 23:16), DW1
                // is the name string, DW2 is the PORT INFO word:
                //     bits 31:28 PSIC (speed-ID count), 15:8 port count, 7:0 port
                //     offset — and the offset is 1-BASED (Linux: XHCI_EXT_PORT_OFF
                //     / XHCI_EXT_PORT_COUNT, "port count includes the current port
                //     offset").
                // This is the bug the 2.18.12 field photo caught: the driver read
                // the count from bits 31:16 and the offset from 15:8 plus one, so
                // the machine's two clean entries (USB2 dw2=0x30191001 -> offset 1,
                // count 16; USB3 dw2=0x40000a11 -> offset 17, count 10) printed as
                // the impossible `usb2=17-41 usb3=17-26` and every real USB2 port
                // came out as proto=usb0. QEMU writes the same field layout
                // (`hcd-xhci.c` 0x28/0x38), so no emulator special case is needed.
                let major = ((dw0 >> 24) & 0xFF) as u8;
                let minor = ((dw0 >> 16) & 0xFF) as u8;
                let first = (dw2 & 0xFF) as usize;
                let count = ((dw2 >> 8) & 0xFF) as usize;
                let psic = (dw2 >> 28) & 0xF;
                crate::info!(
                    "xhci: xcap {:#06x} id={} next={:#04x} major={} minor={} offset={} count={} psic={} dw1={:#010x} dw2={:#010x}",
                    off,
                    id,
                    next,
                    major,
                    minor,
                    first,
                    count,
                    psic,
                    dw1,
                    dw2
                );
                let slot = match major {
                    2 => Some(0usize),
                    3 => Some(1usize),
                    _ => None,
                };
                if let Some(slot) = slot {
                    if count != 0 {
                        for port in first..first.saturating_add(count) {
                            if port >= 1 && port <= ports as usize {
                                proto_masks[slot] |= 1 << (port - 1);
                            }
                        }
                    }
                }
            }
            if next == 0 {
                break;
            }
            off += next as usize * 4;
        }
    }
    // A port belongs to one protocol at most. When the reported ranges overlap,
    // USB3 wins: that is the port whose reset differs, so mislabelling it as USB2
    // is the expensive direction to be wrong in.
    let covered = proto_masks[0] | proto_masks[1];
    proto_masks[0] &= !proto_masks[1];
    for port in 1..=ports as usize {
        let bit = 1u32 << (port - 1);
        protos[port - 1] = if proto_masks[1] & bit != 0 {
            3
        } else if proto_masks[0] & bit != 0 {
            2
        } else {
            0
        };
    }
    let all_ports = if ports >= 32 {
        u32::MAX
    } else {
        (1u32 << ports) - 1
    };
    if covered != all_ports {
        crate::warn!(
            "xhci: protocol caps cover {:#010x} of {:#010x} ports (usb2={:#010x} usb3={:#010x})",
            covered,
            all_ports,
            proto_masks[0],
            proto_masks[1]
        );
    }

    let diag: Vec<PortDiag> = (0..ports as usize)
        .map(|index| {
            let proto = protos.get(index).copied().unwrap_or(0);
            PortDiag {
                stage: "",
                code: 0,
                speed: 0,
                attempts: 0,
                pre: 0,
                post: 0,
                seen_prc: false,
                seen_ped: false,
                seen_ped_u0: false,
                ped_sc: 0,
                ms: 0,
                proto,
                // USB3 ports use WPR (warm reset), USB2 ports PR. An unknown
                // protocol (0) is treated as USB2: PR is the safe request there.
                policy: PortPolicy::new(proto == 3),
            }
        })
        .collect();
    *CONTROLLER.lock() = Some(Controller {
        op,
        db: base + dboff,
        rt,
        ctx_size,
        controllers,
        ports,
        cmdr,
        event,
        erst,
        dcbaa,
        dcbaa_phys: dcbaa_phys_value,
        scratchpad_array,
        scratchpad_pages,
        slots: Vec::new(),
        reports: 0,
        devices: 0,
        brought_up: Vec::new(),
        logged_ports: false,
        probe_mask: ccs_mask_probe,
        next_port: 1,
        fallback_us: 0,
        protos,
        proto_masks,
        xcaps,
        caplength: caplength as u32,
        hciversion,
        hcsparams1,
        hccparams1,
        legacy,
        writes_ok,
        pci_cmd,
        halts: 0,
        halt_cmd: 0,
        halt_sts: 0,
        diag,
        last_fail: None,
        deferred_events: Vec::new(),
        hid_recoveries: 0,
        hid_bad_reports: 0,
    });
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

/// Park a transfer event that arrived while a command was waiting, so the poll
/// loop can still deliver it. Dropping it would leave its endpoint marked
/// "outstanding" forever: it would never be re-armed and its input would die
/// silently — the exact class of bug this driver exists to avoid.
fn defer_event(ctrl: &mut Controller, ev: Trb) {
    // Bounded: the queue is drained every poll, and a command waits for one
    // completion. A full queue means something is very wrong; dropping a report
    // is still better than growing kernel heap without a bound.
    if ctrl.deferred_events.len() < 64 {
        ctrl.deferred_events.push(ev);
    }
}

/// Next event for the poll loop: deferred transfer events first (they are older
/// than whatever is still on the event ring), then the ring itself.
fn next_poll_event(ctrl: &mut Controller, budget: u32) -> Option<Trb> {
    if let Some(ev) = ctrl.deferred_events.pop() {
        return Some(ev);
    }
    next_event(ctrl, budget)
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
            // Not ours: a HID interrupt-IN completion for another endpoint.
            TRB_TRANSFER_EVENT => defer_event(ctrl, ev),
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

// ─── Port bring-up: executor for the pure policy ─────────────────────────────
//
// The decisions themselves live in `xhci_port.rs` and are host-tested against
// simulated ports. This section only performs the one side effect the policy
// asks for and keeps the display copy in sync. Nothing here waits out a USB2
// debounce or reset window: those are `PortPolicy` deadlines checked once per
// poll, so the boot menu's countdown keeps running and one stuck port costs a
// register read, not a blocked second.

/// Decode a raw PORTSC word into the policy's observation type.
fn observe_portsc(raw: u32) -> PortObservation {
    PortObservation::new(
        raw & PORTSC_CCS != 0,
        raw & PORTSC_PED != 0,
        raw & PORTSC_PP != 0,
        ((raw & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT) as u8,
        ((raw & PORTSC_PLS_MASK) >> PORTSC_PLS_SHIFT) as u8,
        raw & PORTSC_PRC != 0,
        raw,
    )
}

/// Execute one step of the port policy for `port`.
///
/// Called round-robin (one port per [`poll`]), so every port's state machine gets
/// one bounded step per pass. This is the cooperative waiting the boot menu
/// needs: a port that is debouncing or waiting for PRC is touched once, its
/// deadline is compared, and the poll returns.
fn poll_port(ctrl: &mut Controller, port: u8) {
    if ctrl.slots.iter().any(|s| s.port == port) {
        return;
    }
    let off = portsc(port);
    let raw = rd32(ctrl.op, off);
    let obs = observe_portsc(raw);
    let now = fsm_now(ctrl);
    let Some(step) = ctrl
        .diag
        .get_mut(port as usize - 1)
        .map(|d| d.policy.step(obs, now))
    else {
        return;
    };
    match step {
        Step::Idle => {
            let phase = ctrl.diag[port as usize - 1].policy.phase();
            let stage = waiting_stage(phase);
            if !stage.is_empty() {
                ctrl.stage(port, stage, raw);
            }
            sync_diag(ctrl, port);
        }
        Step::PowerOn => {
            wr32(ctrl.op, off, (raw & !PORTSC_RW1C) | PORTSC_PP);
            if !spin_until(|| rd32(ctrl.op, off) & PORTSC_PP != 0, 100_000) {
                policy_fail(ctrl, port, obs, now, FailReason::NoPower);
                port_failure(ctrl, port, FailReason::NoPower, raw);
                return;
            }
            crate::debug!("xhci: port {}: power on", port);
            ctrl.stage(port, "power-settle", raw);
            sync_diag(ctrl, port);
        }
        Step::PowerOff => {
            wr32(ctrl.op, off, raw & !PORTSC_RW1C & !PORTSC_PP);
            if !spin_until(|| rd32(ctrl.op, off) & PORTSC_PP == 0, 100_000) {
                policy_fail(ctrl, port, obs, now, FailReason::NoPower);
                port_failure(ctrl, port, FailReason::NoPower, raw);
                return;
            }
            let failures = ctrl.diag[port as usize - 1].policy.failures();
            crate::info!(
                "xhci: port {}: power cycle after {} failed attempt(s): PP off for {} ms, then a fresh debounce",
                port,
                failures,
                xhci_port::POWER_CYCLE_OFF_US / 1_000
            );
            ctrl.stage(port, "power-cycle", raw);
            sync_diag(ctrl, port);
        }
        Step::Reset { warm } => start_port_reset(ctrl, port, warm, raw),
        Step::Resume => {
            ctrl.stage(port, "resume", raw);
            match resume_port(ctrl, port) {
                Some(_speed) => {
                    // The resume waited its 20 ms; the next policy step sees U0
                    // and returns Enumerate. Do not write a reset here.
                    sync_diag(ctrl, port);
                }
                None => {
                    policy_fail(ctrl, port, obs, now, FailReason::ResumeLost);
                    port_failure(ctrl, port, FailReason::ResumeLost, raw);
                }
            }
        }
        Step::Enumerate { speed } => enumerate_port(ctrl, port, speed, now),
        Step::Failed(reason) => port_failure(ctrl, port, reason, raw),
    }
}

/// Clear stale change bits and write PR (USB2) or WPR (USB3).
///
/// The policy has already moved to `ResetWait`; `pre` is the raw PORTSC the
/// decision was made from, so a photo can separate "the port was in Polling
/// before we touched it" from "our reset changed it".
fn start_port_reset(ctrl: &mut Controller, port: u8, warm: bool, raw: u32) {
    let off = portsc(port);
    let attempt = ctrl.diag[port as usize - 1].policy.resets();
    // The PRC seen later must belong to THIS reset: acknowledge old ones first.
    wr32(
        ctrl.op,
        off,
        (raw & !PORTSC_RW1C) | PORTSC_CSC | PORTSC_PRC | PORTSC_WRC,
    );
    let sc = rd32(ctrl.op, off);
    let bit = if warm { PORTSC_WPR } else { PORTSC_PR };
    wr32(ctrl.op, off, (sc & !PORTSC_RW1C) | bit);
    ctrl.stage(port, "reset", raw);
    if attempt <= 2 || attempt % 8 == 0 {
        crate::info!(
            "xhci: port {}: reset #{} issued (usb{} pre={:#010x} ccs={} ped={} pp={} speed={} pls={} prc={})",
            port,
            attempt,
            if warm { 3 } else { 2 },
            raw,
            raw & PORTSC_CCS != 0,
            raw & PORTSC_PED != 0,
            raw & PORTSC_PP != 0,
            (raw & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT,
            (raw & PORTSC_PLS_MASK) >> PORTSC_PLS_SHIFT,
            raw & PORTSC_PRC != 0
        );
    } else {
        crate::debug!(
            "xhci: port {}: reset #{} issued (pre={:#010x})",
            port,
            attempt,
            raw
        );
    }
    sync_diag(ctrl, port);
}

/// The policy reported a stable PED+U0: acknowledge the change bits and hand the
/// port to enumeration.
///
/// The post-reset re-read is the last chance for the field failure to show up:
/// if PED disappeared between the policy's stability check and this read, the
/// attempt is recorded as `lost-after-reset` and the driver backs off (it does
/// NOT reset again immediately — the policy's next attempt observes first).
fn enumerate_port(ctrl: &mut Controller, port: u8, speed: u8, now: u64) {
    let off = portsc(port);
    let had_reset = ctrl.diag[port as usize - 1].policy.attempt_had_reset();
    let cur = rd32(ctrl.op, off);
    wr32(
        ctrl.op,
        off,
        (cur & !PORTSC_RW1C) | PORTSC_PRC | PORTSC_WRC | PORTSC_CSC,
    );
    let cur = rd32(ctrl.op, off);
    let still = observe_portsc(cur);
    if !still.enabled() {
        policy_fail(ctrl, port, still, now, FailReason::LostAfterReset);
        port_failure(ctrl, port, FailReason::LostAfterReset, cur);
        return;
    }
    {
        let d = &mut ctrl.diag[port as usize - 1];
        d.speed = speed;
        d.post = cur;
        d.stage = if had_reset { "reset-ok" } else { "adopted" };
        d.code = cur;
    }
    record_attempt_ms(ctrl, port);
    let attempts = ctrl.diag[port as usize - 1].policy.resets();
    let ms = ctrl.diag[port as usize - 1].ms;
    if had_reset {
        crate::info!(
            "xhci: port {}: reset-ok attempt={} ms={} speed={} post={:#010x} (enumerating)",
            port,
            attempts,
            ms,
            speed,
            cur
        );
    } else {
        crate::info!(
            "xhci: port {}: already enabled, adopting speed={} post={:#010x}",
            port,
            speed,
            cur
        );
    }
    sync_diag(ctrl, port);
    if enumerate_with_speed(ctrl, port, speed) {
        ctrl.devices += 1;
        ctrl.brought_up.push(port);
        if let Some(d) = ctrl.diag.get_mut(port as usize - 1) {
            d.policy.enumerated();
        }
    } else {
        // The specific enumeration stage (`device-descriptor`, `no-hid-interface`,
        // …) was already recorded by `enumerate_slot`; keep it and only update
        // the retry bookkeeping so the next attempt starts with a reset after
        // the backoff.
        // Enumeration can block on descriptor timeouts; start the backoff from
        // the moment it actually failed, not from the reset that preceded it.
        let failed_at = fsm_now(ctrl);
        let _ = ctrl
            .diag
            .get_mut(port as usize - 1)
            .map(|d| d.policy.enumeration_failed(still, failed_at));
        record_attempt_ms(ctrl, port);
        let failures = ctrl.diag[port as usize - 1].policy.failures();
        if failures <= 3 || failures % 8 == 0 {
            crate::warn!(
                "xhci: port {}: enumeration failed (attempt {}); retrying with a reset in {} ms",
                port,
                failures,
                xhci_port::backoff_us(failures) / 1_000
            );
        }
        sync_diag(ctrl, port);
    }
}

/// Record a failure from the policy/driver and log it with the evidence a photo
/// can carry: attempt count, elapsed ms, PRC/PED/ped_sc/post, and the retry gap.
fn port_failure(ctrl: &mut Controller, port: u8, reason: FailReason, raw: u32) {
    let stage = reason.stage();
    let was_last_fail = ctrl.last_fail.map_or(true, |(p, _, _)| p != port);
    ctrl.fail(port, stage, raw);
    record_attempt_ms(ctrl, port);
    sync_diag(ctrl, port);
    let d = &ctrl.diag[port as usize - 1];
    let failures = d.policy.failures();
    let prc = d.policy.seen_prc();
    let ped = d.policy.seen_ped();
    let ped_u0 = d.policy.seen_ped_u0();
    let ped_sc = d.policy.ped_sc();
    let post = d.policy.post();
    let ms = d.ms;
    let retry_ms = xhci_port::backoff_us(failures) / 1_000;
    // First few attempts and every 8th afterwards: enough for a field photo,
    // bounded so a permanently dead port cannot flood the serial log.
    if failures <= 3 || failures % 8 == 0 || was_last_fail {
        crate::warn!(
            "xhci: port {}: {} ms={} prc={} ped={} ped_u0={} ped_sc={:#010x} post={:#010x} (attempt {}, retry in {} ms)",
            port,
            stage,
            ms,
            prc as u8,
            ped as u8,
            ped_u0 as u8,
            ped_sc,
            post,
            failures,
            retry_ms
        );
    } else {
        crate::debug!(
            "xhci: port {}: {} ms={} (attempt {})",
            port,
            stage,
            ms,
            failures
        );
    }
}

/// Apply a policy failure decided outside `step` (PP/resume/post-reset checks).
fn policy_fail(
    ctrl: &mut Controller,
    port: u8,
    obs: PortObservation,
    now: u64,
    reason: FailReason,
) {
    let _ = ctrl
        .diag
        .get_mut(port as usize - 1)
        .map(|d| d.policy.fail(obs, now, reason));
}

/// Port link state from a PORTSC word.
fn port_pls(sc: u32) -> u32 {
    (sc & PORTSC_PLS_MASK) >> PORTSC_PLS_SHIFT
}

/// Port speed id from a PORTSC word.
fn port_speed(sc: u32) -> u8 {
    ((sc & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT) as u8
}

/// Software-resume a port whose link is suspended with the device enabled.
///
/// For a USB2 port, writing PLS=U0 with LWS=1 drives resume signalling (20 ms,
/// USB2 §7.1.7.7); for USB3 it starts the link's resume. Both then report U0.
fn resume_port(ctrl: &mut Controller, port: u8) -> Option<u8> {
    let off = portsc(port);
    let cur = rd32(ctrl.op, off);
    wr32(
        ctrl.op,
        off,
        (cur & !PORTSC_RW1C & !PORTSC_PLS_MASK) | PORTSC_LWS | (PLS_U0 << PORTSC_PLS_SHIFT),
    );
    let clock = clock_ready();
    let start = if clock { now_us() } else { 0 };
    let mut spins = 0u32;
    loop {
        let sc = rd32(ctrl.op, off);
        if port_pls(sc) == PLS_U0 && sc & PORTSC_PED != 0 {
            let speed = port_speed(sc);
            if speed != 0 {
                // Resume signalling needs 20 ms to settle before traffic.
                settle_us(20_000);
                return Some(adopt(ctrl, port, speed, "resumed", sc));
            }
        }
        if clock {
            if now_us().wrapping_sub(start) >= 40_000 {
                return None;
            }
        } else {
            spins += 1;
            if spins >= 400_000 {
                return None;
            }
        }
        core::hint::spin_loop();
    }
}

/// Take over an enabled port: acknowledge its change bits and record the speed.
fn adopt(ctrl: &mut Controller, port: u8, speed: u8, stage: &'static str, sc: u32) -> u8 {
    let off = portsc(port);
    let cur = rd32(ctrl.op, off);
    wr32(
        ctrl.op,
        off,
        (cur & !PORTSC_RW1C) | PORTSC_PRC | PORTSC_WRC | PORTSC_CSC,
    );
    let cur = rd32(ctrl.op, off);
    if let Some(d) = ctrl.diag.get_mut(port as usize - 1) {
        d.speed = speed;
        d.post = cur;
    }
    ctrl.stage(port, stage, sc);
    speed
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
        if ev.trb_type() != TRB_TRANSFER_EVENT {
            continue;
        }
        if ev.endpoint_id() != 1 {
            // A HID endpoint's transfer completed while we wait for EP0: keep it
            // for the poll loop instead of swallowing the report.
            defer_event(ctrl, ev);
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
        if ev.trb_type() != TRB_TRANSFER_EVENT {
            continue;
        }
        if ev.endpoint_id() != 1 {
            // A HID endpoint's transfer completed while ENUMERATION was sending a
            // control request; preserve it for the poll loop.
            defer_event(ctrl, ev);
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
    // A per-port stage is recorded by the caller, which knows the port; the slot
    // command itself only says whether the controller had a free slot.
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
        hid: Vec::new(),
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
        Some(ev) if ev.completion_code() == CC_SUCCESS => {
            ctrl.stage(port, "addressed", 0);
            true
        }
        Some(ev) => {
            ctrl.fail(port, "address", ev.completion_code() as u32);
            crate::warn!("xhci: address device failed: cc={}", ev.completion_code());
            false
        }
        None => {
            ctrl.fail(port, "address-noevent", 0);
            false
        }
    }
}

/// Update EP0's max packet size after the first descriptor read.
///
/// A full/low-speed device may start with an 8-byte EP0 and report its real
/// `bMaxPacketSize0` only in the device descriptor; reading the 18-byte descriptor
/// through an 8-byte EP0 then fails on real hardware (QEMU's keyboard is
/// high-speed, so its 64-byte EP0 hid this). Evaluate Context is the specified way
/// to change it: the input context carries the slot and EP0 contexts again.
fn evaluate_ep0_max_packet(ctrl: &mut Controller, slot_index: usize, max_packet: u32) -> bool {
    let ctx_size = ctrl.ctx_size;
    let (input, input_phys, ep0_phys, slot_id, port, speed) = {
        let Some(slot) = ctrl.slots.get(slot_index) else {
            return false;
        };
        (
            slot.input_ctx.virt(),
            slot.input_ctx.phys(),
            slot.ep0.base_phys(),
            slot.id,
            slot.port,
            slot.speed,
        )
    };
    // SAFETY: our own zeroed page; offsets are the spec's for this context size.
    unsafe {
        let p = input as *mut u32;
        core::ptr::write_bytes(input as *mut u8, 0, PAGE_SIZE);
        write_volatile(p, 0); // nothing to drop
        write_volatile(p.add(1), 0b11); // add slot + EP0
        let slot_ctx = p.add(8) as *mut u32;
        write_volatile(
            slot_ctx,
            (1 << SLOT_CTX_ENTRIES_SHIFT) | ((speed as u32) << SLOT_CTX_SPEED_SHIFT),
        );
        write_volatile(slot_ctx.add(1), (port as u32) << SLOT_CTX_PORT_SHIFT);
        let ep_ctx = (input + ctx_size * 2) as *mut u32;
        write_volatile(ep_ctx, 0);
        write_volatile(
            ep_ctx.add(1),
            (3 << EP_CTX_CERR_SHIFT)
                | (EP_TYPE_CONTROL << EP_CTX_EP_TYPE_SHIFT)
                | (max_packet << EP_CTX_MAX_PACKET_SHIFT),
        );
        write_volatile(ep_ctx.add(2), (ep0_phys | 1) as u32);
        write_volatile(ep_ctx.add(3), (ep0_phys >> 32) as u32);
    }
    let (a0, a1) = ctx_addr(input_phys);
    let ev = command(
        ctrl,
        a0,
        a1,
        0,
        ((TRB_EVALUATE_CONTEXT as u32) << 10) | ((slot_id as u32) << 24),
    );
    matches!(ev, Some(ev) if ev.completion_code() == CC_SUCCESS)
}

/// Configure every bindable HID interrupt-IN endpoint in ONE xHCI Configure
/// Endpoint command.
///
/// WHY ONE COMMAND: the slot context's "Context Entries" field must be the
/// highest DCI the device context carries, so adding endpoints one at a time
/// would re-write the slot context and issue N commands for N interfaces. The
/// device may have several (a combo keyboard's boot keyboard plus
/// consumer/system collections); configuring them together is also what makes
/// the xHCI endpoint state consistent with the single SET_CONFIGURATION.
///
/// The endpoint's `wMaxPacketSize` is the transfer buffer's lower bound: an
/// interrupt transfer may legitimately deliver a full packet, and a request
/// shorter than that is a babble (completion code 3) — the field mouse's 7-byte
/// report against the old 4-byte buffer.
fn configure_hid_endpoints(
    ctrl: &mut Controller,
    slot_index: usize,
    ifaces: &[HidIface],
    modes: &[ReportMode],
) -> bool {
    if ifaces.is_empty() || ifaces.len() != modes.len() {
        return false;
    }
    let ctx_size = ctrl.ctx_size;
    let (input, input_phys, slot_id, speed) = {
        let slot = &ctrl.slots[slot_index];
        (
            slot.input_ctx.virt(),
            slot.input_ctx.phys(),
            slot.id,
            slot.speed,
        )
    };
    // Rings and buffers first: their physical addresses go into the endpoint
    // contexts. Dropping this Vec on a failure returns the frames to the PMM, so
    // a failed Configure Endpoint leaves nothing behind.
    let mut eps: Vec<HidEndpoint> = Vec::with_capacity(ifaces.len());
    for (i, iface) in ifaces.iter().enumerate() {
        let Some(ring) = Ring::new("hid ring") else {
            return false;
        };
        let Some(buf) = DmaFrame::new_zeroed("hid buffer") else {
            return false;
        };
        let len = usb_hid_proto::transfer_len(iface.max_packet);
        if (iface.max_packet & 0x07FF) as usize > usb_hid_proto::MAX_REPORT {
            crate::warn!(
                "xhci: iface {}: wMaxPacketSize {} exceeds the {}B transfer buffer; clamped (a longer report would babble)",
                iface.interface,
                iface.max_packet,
                len
            );
        }
        eps.push(HidEndpoint {
            iface: iface.interface,
            subclass: iface.subclass,
            protocol: iface.protocol,
            mode: modes[i],
            dci: (iface.ep_addr & 0x0F) * 2 + 1,
            ring,
            buf,
            len,
            recoveries: 0,
            bad_reports: 0,
            bad_report_logged: false,
            disabled: false,
        });
    }
    // Highest DCI present decides the slot context's Context Entries field.
    let max_dci = eps.iter().map(|ep| ep.dci).max().unwrap_or(1);
    // SAFETY: `input` is our own zeroed page; the contexts are written at the
    // offsets the controller's context size dictates, and every DCI is <= 31
    // (device endpoint numbers are 4 bits), so the writes stay in the page.
    unsafe {
        let p = input as *mut u32;
        core::ptr::write_bytes(input as *mut u8, 0, PAGE_SIZE);
        // Input Control Context: A0 (slot context) plus one Add flag per DCI.
        let mut add = 1u32;
        for ep in &eps {
            add |= 1u32 << ep.dci;
        }
        write_volatile(p.add(1), add);
        let slot_ctx = p.add(8) as *mut u32;
        write_volatile(slot_ctx, (max_dci as u32) << SLOT_CTX_ENTRIES_SHIFT);
        for (ep, iface) in eps.iter().zip(ifaces.iter()) {
            let ep_ctx = (input + ctx_size * (1 + ep.dci as usize)) as *mut u32;
            // xHCI stores the polling interval as log2(microframes) for high/super
            // speed devices (so USB bInterval=1 is field 0) and as the frame count
            // for full/low speed. Writing bInterval raw gave 2^bInterval.
            let interval_field: u32 = if speed == SPEED_HIGH || speed == SPEED_SUPER {
                (iface.interval.saturating_sub(1) & 0x0F) as u32
            } else {
                (iface.interval & 0x0F) as u32
            };
            write_volatile(ep_ctx, interval_field << EP_CTX_INTERVAL_SHIFT);
            write_volatile(
                ep_ctx.add(1),
                (3 << EP_CTX_CERR_SHIFT)
                    | (EP_TYPE_INTERRUPT_IN << EP_CTX_EP_TYPE_SHIFT)
                    | ((iface.max_packet as u32) << EP_CTX_MAX_PACKET_SHIFT),
            );
            let ring_phys = ep.ring.base_phys();
            write_volatile(ep_ctx.add(2), (ring_phys | 1) as u32);
            write_volatile(ep_ctx.add(3), (ring_phys >> 32) as u32);
        }
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
                "xhci: configure endpoint ({} HID iface(s)) failed (cc={:?})",
                eps.len(),
                other.map(|e| e.completion_code())
            );
            return false;
        }
    }
    ctrl.slots[slot_index].hid = eps;
    true
}

/// Enumerate the device on `port`, returning true when a HID interface was
/// configured. Logs what it found either way.
fn enumerate_with_speed(ctrl: &mut Controller, port: u8, speed: u8) -> bool {
    let Some(slot_index) = enable_slot(ctrl) else {
        ctrl.fail(port, "enable-slot", 0);
        return false;
    };
    ctrl.slots[slot_index].port = port;
    ctrl.slots[slot_index].speed = speed;
    let ok = enumerate_slot(ctrl, slot_index, port, speed);
    if !ok {
        // A failed enumeration must NOT leave the slot behind: the poll loop skips
        // any port whose slot exists (so the retry would never run), and the
        // controller only has 64 slots in total.
        release_slot(ctrl, slot_index);
    }
    ok
}

/// Disable a slot the controller gave us and drop its bookkeeping.
fn release_slot(ctrl: &mut Controller, slot_index: usize) {
    let Some(slot) = ctrl.slots.get(slot_index) else {
        return;
    };
    let slot_id = slot.id;
    let _ = command(
        ctrl,
        0,
        0,
        0,
        ((TRB_DISABLE_SLOT as u32) << 10) | ((slot_id as u32) << 24),
    );
    // Clear the DCBAA entry so a later slot id cannot alias the old context.
    // SAFETY: DCBAA is our own frame and `slot_id` was handed out by the controller.
    unsafe {
        let entry =
            (crate::memory::vmm::phys_to_virt(ctrl.dcbaa_phys) as *mut u64).add(slot_id as usize);
        write_volatile(entry, 0);
    }
    ctrl.slots.remove(slot_index);
}

/// Everything after Enable Slot: address, descriptors, configuration, HID endpoint.
fn enumerate_slot(ctrl: &mut Controller, slot_index: usize, port: u8, speed: u8) -> bool {
    let speed_name = match speed {
        SPEED_LOW => "low",
        SPEED_FULL => "full",
        SPEED_HIGH => "high",
        SPEED_SUPER => "super",
        _ => "?",
    };
    if !address_device(ctrl, slot_index, port, speed) {
        return false;
    }
    // Device descriptor: 8 bytes first (a full-speed device may only have an
    // 8-byte EP0 at this point), then the whole 18.
    let Some(head) = get_descriptor(ctrl, slot_index, 1, 0, 8) else {
        ctrl.fail(port, "device-descriptor", 0);
        crate::warn!("xhci: port {}: no device descriptor", port);
        return false;
    };
    if head.len() < 8 {
        ctrl.fail(port, "device-descriptor-short", head.len() as u32);
        return false;
    }
    let vendor = u16::from_le_bytes([head[0], head[1]]);
    let product = u16::from_le_bytes([head[2], head[3]]);
    // The first 8 bytes carry bMaxPacketSize0 at offset 7. For full/low speed the
    // controller was told 8; if the device says otherwise, update EP0 before
    // asking for the 18-byte descriptor, or the long read fails on real hardware.
    let device_mps = head[7] as u32;
    let assumed_mps = ep0_max_packet(speed);
    if device_mps != assumed_mps && matches!(device_mps, 8 | 16 | 32 | 64) {
        if evaluate_ep0_max_packet(ctrl, slot_index, device_mps) {
            crate::debug!(
                "xhci: port {}: EP0 max packet {} -> {}",
                port,
                assumed_mps,
                device_mps
            );
        } else {
            crate::warn!(
                "xhci: port {}: EP0 max packet update to {} failed",
                port,
                device_mps
            );
        }
    }
    let Some(dev) = get_descriptor(ctrl, slot_index, 1, 0, 18) else {
        ctrl.fail(port, "device-descriptor-full", 0);
        return false;
    };
    if dev.len() < 18 {
        ctrl.fail(port, "device-descriptor-full-short", dev.len() as u32);
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
    match dev[4] {
        9 => {
            ctrl.fail(port, "hub-unsupported", 9);
            crate::warn!(
            "xhci: port {} holds a USB hub; hubs are not supported (keyboard behind it is invisible)",
            port
        )
        }
        0 => {}
        class => crate::info!("xhci: port {} device class {:#04x}", port, class),
    }
    // Configuration descriptor: 9 bytes for the total length, then the whole blob.
    let Some(head) = get_descriptor(ctrl, slot_index, 2, 0, 9) else {
        ctrl.fail(port, "config-descriptor", 0);
        return false;
    };
    if head.len() < 9 {
        ctrl.fail(port, "config-descriptor-short", head.len() as u32);
        return false;
    }
    let total = u16::from_le_bytes([head[2], head[3]]) as usize;
    if total < 9 || total > PAGE_SIZE {
        ctrl.fail(port, "config-length", total as u32);
        return false;
    }
    let Some(conf) = get_descriptor(ctrl, slot_index, 2, 0, total) else {
        ctrl.fail(port, "config-full", 0);
        return false;
    };
    let ifaces = usb_desc::parse_hid_interfaces(&conf);
    if ifaces.is_empty() {
        ctrl.fail(port, "no-hid-interface", dev[4] as u32);
        crate::info!(
            "xhci: port {}: device {:04x}:{:04x} has no HID interrupt-IN interface",
            port,
            vendor,
            product
        );
        return false;
    }
    // Bind only boot-class keyboard/mouse interfaces. A HID interface with
    // protocol 0 (consumer/system/vendor collections, even on a keyboard that
    // also exposes a boot keyboard) has no report layout this driver parses;
    // binding it would burn a ring and a buffer to feed reports `usb_hid` must
    // ignore. The skipped interfaces are logged so the decision is visible.
    let mut chosen: Vec<HidIface> = Vec::new();
    for iface in ifaces {
        if !usb_hid_proto::bindable(iface.subclass, iface.protocol) {
            crate::info!(
                "xhci: port {}: iface {} (subclass={} protocol={}) is not a boot keyboard/mouse; not bound",
                port,
                iface.interface,
                iface.subclass,
                iface.protocol
            );
            continue;
        }
        // One device endpoint number must not be configured twice: two HID
        // interfaces sharing an endpoint cannot both own the same DCI.
        if chosen
            .iter()
            .any(|c| c.ep_addr & 0x0F == iface.ep_addr & 0x0F)
        {
            crate::warn!(
                "xhci: port {}: iface {} reuses endpoint {:#04x}; keeping the first interface",
                port,
                iface.interface,
                iface.ep_addr
            );
            continue;
        }
        chosen.push(iface);
    }
    let Some(config_value) = chosen.first().map(|i| i.config_value) else {
        ctrl.fail(port, "no-boot-hid", dev[4] as u32);
        crate::info!(
            "xhci: port {}: device {:04x}:{:04x} has HID interfaces but none is a boot keyboard/mouse",
            port,
            vendor,
            product
        );
        return false;
    };
    // SET_CONFIGURATION activates the device (without it every endpoint stalls).
    if !control_no_data(ctrl, slot_index, 0x00, 9, config_value as u16, 0) {
        ctrl.fail(port, "set-configuration", config_value as u32);
        crate::warn!("xhci: port {}: SET_CONFIGURATION failed", port);
        return false;
    }
    // SET_PROTOCOL(0) = BOOT (HID 1.11 §7.2.6; the old code sent wValue=1, which
    // is REPORT protocol, while its comment claimed boot). Boot pins the report
    // layout to 8-byte keyboard / 3-byte mouse. A device may refuse (STALL): stay
    // in report protocol, record that so the parser does not pretend it is boot,
    // and keep the endpoint — `usb_hid`'s best-effort layouts are still better
    // than a dead keyboard. SET_IDLE(0) = report on change only.
    let mut modes: Vec<ReportMode> = Vec::with_capacity(chosen.len());
    for iface in &chosen {
        let mode = if control_no_data(ctrl, slot_index, 0x21, 0x0B, 0, iface.interface as u16) {
            ReportMode::Boot
        } else {
            crate::warn!(
                "xhci: port {}: iface {} rejected SET_PROTOCOL(boot); staying in report protocol (best-effort layout)",
                port,
                iface.interface
            );
            ReportMode::Report
        };
        if !control_no_data(ctrl, slot_index, 0x21, 0x0A, 0, iface.interface as u16) {
            crate::warn!(
                "xhci: port {}: iface {}: SET_IDLE failed",
                port,
                iface.interface
            );
        }
        modes.push(mode);
    }
    if !configure_hid_endpoints(ctrl, slot_index, &chosen, &modes) {
        ctrl.fail(port, "configure-endpoint", 0);
        return false;
    }
    ctrl.stage(port, "hid-ready", chosen.len() as u32);
    for (iface, ep) in chosen.iter().zip(ctrl.slots[slot_index].hid.iter()) {
        // Boot class: 0 = none, 1 = keyboard, 2 = mouse. `mode` is what
        // SET_PROTOCOL actually achieved; `report` is the transfer buffer size
        // derived from wMaxPacketSize (>= a whole packet, so no babble).
        crate::info!(
            "xhci: HID device on port {}: iface={} subclass={} protocol={} ep={:#04x} report={}B mode={} maxpkt={}",
            port,
            iface.interface,
            iface.subclass,
            iface.protocol,
            iface.ep_addr,
            ep.len,
            ep.mode.as_str(),
            iface.max_packet
        );
    }
    true
}

/// One poll pass: enumerate anything newly attached, then collect HID reports
/// and hand them to [`crate::drivers::usb_hid`].
pub fn poll() {
    let mut guard = CONTROLLER.lock();
    let Some(ctrl) = guard.as_mut() else {
        return;
    };
    // A halted controller is the worst failure mode this driver has: HCH=1 means
    // it stopped walking the rings, and every port register keeps the value it
    // had at the moment it died. The old driver never looked, so the diagnostic
    // screen presented a dead controller's frozen state as live ("all ports stuck
    // in Polling, tries exhausted") and kept retrying port writes the controller
    // could not see. Detect it, record the registers that prove it, and restart.
    if rd32(ctrl.op, OP_USBSTS) & USBSTS_HCH != 0 {
        ctrl.halts = ctrl.halts.saturating_add(1);
        let cmd = rd32(ctrl.op, OP_USBCMD);
        let sts = rd32(ctrl.op, OP_USBSTS);
        if ctrl.halts == 1 {
            ctrl.halt_cmd = cmd;
            ctrl.halt_sts = sts;
            crate::warn!(
                "xhci: controller halted (usbcmd={:#010x} usbsts={:#010x} hse={} hce={}): restarting",
                cmd,
                sts,
                (sts & USBSTS_HSE != 0) as u8,
                (sts & USBSTS_HCE != 0) as u8
            );
        }
        if !restart_controller(ctrl) {
            crate::warn!(
                "xhci: controller {} did not come back (usbcmd={:#010x} usbsts={:#010x})",
                ctrl.halts,
                rd32(ctrl.op, OP_USBCMD),
                rd32(ctrl.op, OP_USBSTS)
            );
            return;
        }
        crate::info!("xhci: controller restarted (halt #{})", ctrl.halts);
    }
    // 1. Advance ONE port's policy one step per poll, round-robin. The policy
    //    itself never blocks (a debounce/reset/backoff is a deadline checked on
    //    later polls), so a slow or dead port costs one PORTSC read per pass and
    //    the boot menu's countdown keeps running. One port per poll also keeps
    //    the enumeration work of a newly enabled device from delaying every
    //    other port's deadline by more than one pass.
    let port = ctrl.next_port;
    ctrl.next_port = if port >= ctrl.ports { 1 } else { port + 1 };
    poll_port(ctrl, port);

    // One-shot port map after one full round-robin pass: on a machine where
    // nothing enumerates, this says whether the controller reports a device at
    // all, which separates "our reset/enumeration is wrong" from "the device is
    // not on this controller". The wrap is the trigger, not `brought_up`:
    // ports with no device never enumerate by design, and the map is exactly
    // what a photo of such a machine needs.
    if !ctrl.logged_ports && ctrl.next_port == 1 {
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

    // 2. Arm every idle HID interrupt-IN endpoint. Completions are drained
    //    afterwards, not per endpoint, so one device's completion cannot be
    //    swallowed while another endpoint is being serviced.
    for si in 0..ctrl.slots.len() {
        let slot_id = ctrl.slots[si].id;
        for hi in 0..ctrl.slots[si].hid.len() {
            let dci = {
                let hid = &mut ctrl.slots[si].hid[hi];
                if hid.disabled || hid.ring.outstanding {
                    None
                } else {
                    let buf_phys = hid.buf.phys();
                    hid.ring.push(
                        buf_phys as u32,
                        (buf_phys >> 32) as u32,
                        hid.len as u32,
                        ((TRB_NORMAL as u32) << 10) | TRB_IOC,
                    );
                    hid.ring.outstanding = true;
                    Some(hid.dci)
                }
            };
            if let Some(dci) = dci {
                ring_doorbell(ctrl, slot_id, dci);
            }
        }
    }
    // 3. Drain completion events. Every transfer event names its slot and
    //    endpoint; route it to that endpoint. (The old loop matched only the
    //    single endpoint it was currently looking at and dropped the rest.)
    let mut events = 0usize;
    while events < 64 {
        let Some(ev) = next_poll_event(ctrl, 0) else {
            break;
        };
        if ev.trb_type() != TRB_TRANSFER_EVENT {
            continue;
        }
        events += 1;
        let slot_id = ev.slot_id();
        let dci = ev.endpoint_id();
        let Some(si) = ctrl.slots.iter().position(|s| s.id == slot_id) else {
            continue;
        };
        let Some(hi) = ctrl.slots[si]
            .hid
            .iter()
            .position(|h| h.dci == dci && !h.disabled)
        else {
            continue;
        };
        // Snapshot what the completion needs; keep every borrow of the slot short
        // because recovery issues a command through `ctrl`.
        let (len, mode, subclass, protocol, buf_virt) = {
            let hid = &ctrl.slots[si].hid[hi];
            (
                hid.len,
                hid.mode,
                hid.subclass,
                hid.protocol,
                hid.buf.virt(),
            )
        };
        ctrl.slots[si].hid[hi].ring.outstanding = false;
        let cc = ev.completion_code();
        if cc == CC_SUCCESS || cc == CC_SHORT_PACKET {
            let got = len.saturating_sub(ev.residual() as usize);
            if got == 0 {
                continue;
            }
            let mut report = [0u8; usb_hid_proto::MAX_REPORT];
            let n = got.min(report.len());
            // SAFETY: the buffer is our own DMA page, `n <= len <= MAX_REPORT`,
            // and the controller wrote at most `len` bytes into it before
            // publishing this transfer event.
            unsafe {
                core::ptr::copy_nonoverlapping(buf_virt as *const u8, report.as_mut_ptr(), n);
            }
            let report = &report[..n];
            let required = usb_hid_proto::minimum_report_len(subclass, protocol, mode);
            if n < required {
                ctrl.hid_bad_reports += 1;
                let hid = &mut ctrl.slots[si].hid[hi];
                hid.bad_reports += 1;
                if !hid.bad_report_logged {
                    hid.bad_report_logged = true;
                    crate::warn!(
                        "xhci: HID iface {} (DCI {}) got a {}B report, layout needs {}B (mode={}); report dropped (logging once)",
                        hid.iface,
                        dci,
                        n,
                        required,
                        mode.as_str()
                    );
                }
            } else {
                crate::drivers::usb_hid::feed_iface(report, subclass, protocol, mode);
                ctrl.reports += 1;
            }
            continue;
        }
        // Transfer error (babble, data-buffer error, stall, USB transaction
        // error, …): recover instead of disabling the endpoint on the first one.
        // The old "input off until reboot" is now the LAST resort, after
        // `MAX_RECOVERIES` failed resets.
        let (previous, iface) = {
            let hid = &ctrl.slots[si].hid[hi];
            (hid.recoveries, hid.iface)
        };
        match usb_hid_proto::recovery_action(previous) {
            RecoveryAction::ResetEndpoint => {
                ctrl.slots[si].hid[hi].recoveries += 1;
                let attempt = ctrl.slots[si].hid[hi].recoveries;
                let ok = reset_hid_endpoint(ctrl, slot_id, dci);
                if ok {
                    ctrl.hid_recoveries += 1;
                }
                crate::warn!(
                    "xhci: HID iface {} (DCI {}) transfer error cc={}: Reset Endpoint {} ({}/{})",
                    iface,
                    dci,
                    cc,
                    if ok { "ok, re-arming" } else { "failed" },
                    attempt,
                    usb_hid_proto::MAX_RECOVERIES
                );
                // The errored TRB produced this event, so the transfer ring has
                // no outstanding TRB: the next poll pushes a fresh Normal TRB.
                ctrl.slots[si].hid[hi].ring.outstanding = false;
            }
            RecoveryAction::Disable => {
                let hid = &mut ctrl.slots[si].hid[hi];
                hid.disabled = true;
                hid.ring.outstanding = false;
                crate::warn!(
                    "xhci: HID iface {} (DCI {}) still failing cc={} after {} Reset Endpoint attempt(s); input from it is off until reboot",
                    hid.iface,
                    dci,
                    cc,
                    usb_hid_proto::MAX_RECOVERIES
                );
            }
        }
    }
}

/// Recover one halted HID endpoint with the xHCI Reset Endpoint command (TRB
/// type 14: slot id in bits 24-31, DCI in bits 16-20).
///
/// WHY NOT JUST RE-ARM: a transfer error moves the endpoint to the Halted state,
/// and re-arming a halted endpoint only produces the same error event again. The
/// old driver read that as "this device is broken" and disabled input until
/// reboot — which is exactly what the field mouse looked like after its first
/// 7-byte report hit the 4-byte buffer. Reset Endpoint returns the endpoint to
/// Stopped/Running so the next poll's Normal TRB executes.
fn reset_hid_endpoint(ctrl: &mut Controller, slot_id: u8, dci: u8) -> bool {
    let ev = command(
        ctrl,
        0,
        0,
        0,
        ((TRB_RESET_ENDPOINT as u32) << 10) | ((dci as u32) << 16) | ((slot_id as u32) << 24),
    );
    matches!(ev, Some(ev) if ev.completion_code() == CC_SUCCESS)
}

/// Bring a halted controller back: reset it, re-point it at our rings, run it.
///
/// Everything this needs is owned by the `Controller` (rings, DCBAA, scratchpad
/// array), which is the reason those frames became fields: a reset makes the
/// controller re-read them, so they must still be ours.
fn restart_controller(ctrl: &mut Controller) -> bool {
    // Stop, then reset. The halt may be an HCE; a port or command write will not
    // clear it, a controller reset does.
    wr32(ctrl.op, OP_USBCMD, 0);
    let _ = spin_until(|| rd32(ctrl.op, OP_USBSTS) & USBSTS_HCH != 0, 200_000);
    wr32(ctrl.op, OP_USBCMD, USBCMD_HCRST);
    if !spin_until(|| rd32(ctrl.op, OP_USBCMD) & USBCMD_HCRST == 0, 2_000_000) {
        return false;
    }
    if !spin_until(|| rd32(ctrl.op, OP_USBSTS) & USBSTS_CNR == 0, 2_000_000) {
        return false;
    }
    let max_slots = (ctrl.hcsparams1 & 0xFF).min(u8::MAX as u32);
    wr32(ctrl.op, OP_CONFIG, max_slots);
    wr64(ctrl.op, OP_DCBAAP, ctrl.dcbaa.phys());
    wr64(ctrl.op, OP_CRCR, ctrl.cmdr.base_phys() | TRB_CYCLE as u64);
    wr32(ctrl.rt + RTS_IR0, IR_ERSTSZ, 1);
    wr64(ctrl.rt + RTS_IR0, IR_ERSTBA, ctrl.erst.phys());
    wr64(ctrl.rt + RTS_IR0, IR_ERDP, ctrl.event.base_phys());
    wr32(ctrl.op, OP_USBCMD, USBCMD_RS);
    if !spin_until(|| rd32(ctrl.op, OP_USBSTS) & USBSTS_HCH == 0, 2_000_000) {
        return false;
    }
    // A controller reset forgets every slot and every port; the rings start from
    // their first TRB with the cycle bit set again.
    ctrl.slots.clear();
    ctrl.deferred_events.clear();
    ctrl.brought_up.clear();
    ctrl.devices = 0;
    ctrl.cmdr.enqueue = 0;
    ctrl.cmdr.cycle = TRB_CYCLE;
    ctrl.cmdr.outstanding = false;
    ctrl.event.dequeue = 0;
    ctrl.event.cycle = TRB_CYCLE;
    for d in ctrl.diag.iter_mut() {
        d.stage = "";
        d.code = 0;
        d.speed = 0;
        d.attempts = 0;
        d.pre = 0;
        d.post = 0;
        d.seen_prc = false;
        d.seen_ped = false;
        d.seen_ped_u0 = false;
        d.ped_sc = 0;
        d.ms = 0;
        d.policy = PortPolicy::new(d.proto == 3);
    }
    true
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

/// One row per port that has (or had) a device on it, for the boot menu.
///
/// The stage is where enumeration stopped — `hid-ready` means the device types,
/// anything else is the answer to "why does the keyboard not work". The two
/// PORTSC snapshots separate "the port never enabled" from "the device did not
/// answer after it was enabled", `ped_sc` is the per-port PORTSC snapshot at the
/// last PED+U0 sighting (or the last transient PED sighting when `ped_u0=0`), and
/// `proto` says whether this is a USB2 port (where a keyboard lives) or a USB3
/// one.
pub struct PortRow {
    pub port: u8,
    pub portsc: u32,
    pub stage: &'static str,
    pub code: u32,
    pub speed: u8,
    pub pre: u32,
    pub post: u32,
    pub attempts: u8,
    pub seen_prc: bool,
    pub seen_ped: bool,
    /// Whether `ped_sc` is a real PED+U0 enable snapshot (true) or the last
    /// transient PED sighting during reset/training (false).
    pub seen_ped_u0: bool,
    pub ped_sc: u32,
    pub proto: u8,
    /// Duration of the last attempt in milliseconds (0 when no clock).
    pub ms: u16,
}

pub fn diagnostics() -> Vec<PortRow> {
    let guard = CONTROLLER.lock();
    let Some(c) = guard.as_ref() else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for port in 1..=c.ports {
        let sc = rd32(c.op, portsc(port));
        let Some(d) = c.diag.get(port as usize - 1) else {
            continue;
        };
        // Only ports that are interesting: a device is attached, or an attempt was
        // made (a device that vanished), or the stage is a real failure.
        if sc & PORTSC_CCS == 0 && d.stage.is_empty() {
            continue;
        }
        if sc & PORTSC_CCS == 0 && d.stage == "no-device" {
            continue;
        }
        rows.push(PortRow {
            port,
            portsc: sc,
            stage: d.stage,
            code: d.code,
            speed: d.speed,
            pre: d.pre,
            post: d.post,
            attempts: d.attempts,
            seen_prc: d.seen_prc,
            seen_ped: d.seen_ped,
            seen_ped_u0: d.seen_ped_u0,
            ped_sc: d.ped_sc,
            proto: d.proto,
            ms: d.ms,
        });
    }
    rows
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
