// arch/x86_64/apic.rs — APIC subsystem: LAPIC timer, I/O APIC, IRQ dispatch
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use core::ptr;
use x86_64::instructions::port::Port;
use crate::sync::spinlock::Spinlock;

const LAPIC_ID: u32        = 0x020;
const LAPIC_EOI: u32       = 0x0B0;
const LAPIC_SPURIOUS: u32  = 0x0F0;
const LAPIC_LVT_TIMER: u32 = 0x320;
const LAPIC_TIMER_INIT: u32  = 0x380;
const LAPIC_TIMER_CURRENT: u32 = 0x390;
const LAPIC_TIMER_DIV: u32    = 0x3E0;

const SPURIOUS_ENABLE: u32 = 0x100;
const SPURIOUS_VECTOR: u32 = 0xFF;

const IOAPIC_REG_SEL: u32   = 0x00;
const IOAPIC_REG_WIN: u32   = 0x10;
const IOAPIC_REDTBL_BASE: u32 = 0x10;

static mut LAPIC_BASE: u64 = 0;
static mut IOAPIC_BASE: u64 = 0;

static IRQ_HANDLERS: Spinlock<[Option<fn()>; 224]> = Spinlock::new([None; 224]);

fn disable_pic() {
    let mut cmd_master: Port<u8> = Port::new(0x20);
    let mut data_master: Port<u8> = Port::new(0x21);
    let mut cmd_slave: Port<u8> = Port::new(0xA0);
    let mut data_slave: Port<u8> = Port::new(0xA1);

    // SAFETY: Port I/O to standard PIC addresses.
    unsafe {
        cmd_master.write(0x11);
        cmd_slave.write(0x11);
        data_master.write(0x20);
        data_slave.write(0x28);
        data_master.write(0x04);
        data_slave.write(0x02);
        data_master.write(0x01);
        data_slave.write(0x01);
        data_master.write(0xFF);
        data_slave.write(0xFF);
    }
    crate::debug!("PIC 8259 disabled");
}

unsafe fn lapic_write(reg: u32, value: u32) {
    // LAPIC_BASE is already mapped to HHDM in lib.rs, so we don't add HHDM again
    let addr = (LAPIC_BASE + reg as u64) as *mut u32;
    ptr::write_volatile(addr, value);
}

unsafe fn lapic_read(reg: u32) -> u32 {
    // LAPIC_BASE is already mapped to HHDM in lib.rs, so we don't add HHDM again
    let addr = (LAPIC_BASE + reg as u64) as *const u32;
    ptr::read_volatile(addr)
}

unsafe fn ioapic_write(reg: u32, value: u32) {
    // IOAPIC_BASE should be virtual (HHDM-mapped) if non-zero
    let sel = (IOAPIC_BASE + IOAPIC_REG_SEL as u64) as *mut u32;
    let win = (IOAPIC_BASE + IOAPIC_REG_WIN as u64) as *mut u32;
    ptr::write_volatile(sel, reg);
    ptr::write_volatile(win, value);
}

unsafe fn ioapic_read(reg: u32) -> u32 {
    // IOAPIC_BASE should be virtual (HHDM-mapped) if non-zero
    let sel = (IOAPIC_BASE + IOAPIC_REG_SEL as u64) as *mut u32;
    let win = (IOAPIC_BASE + IOAPIC_REG_WIN as u64) as *mut u32;
    ptr::write_volatile(sel, reg);
    ptr::read_volatile(win)
}

fn init_lapic() {
    // Enable LAPIC via MSR if needed (IA32_APIC_BASE, MSR 0x1B).
    // read_msr is safe (no memory effect); the write stays unsafe below.
    let mut apic_base_msr = crate::arch::cpu::read_msr(0x1B);
    crate::debug!("LAPIC MSR before: 0x{:016x}", apic_base_msr);

    // Set bit 11 (global enable)
    apic_base_msr |= 1 << 11;

    // SAFETY: Setting the IA32_APIC_BASE global-enable bit (bit 11) while
    // preserving the existing base address is the intended, sound LAPIC
    // reconfiguration; we read-modify-write the same MSR and program no new
    // address, so the LAPIC stays at its current (already-mapped) location.
    unsafe {
        crate::arch::cpu::write_msr(0x1B, apic_base_msr);
    }

    crate::debug!("LAPIC MSR after: 0x{:016x} (enabled)", apic_base_msr);

    // SAFETY: LAPIC_BASE points at the HHDM-mapped LAPIC MMIO region; the
    // volatile register reads/writes below target valid mapped device memory.
    unsafe {
        // Configure LAPIC registers
        let spurious = SPURIOUS_VECTOR | SPURIOUS_ENABLE;
        lapic_write(LAPIC_SPURIOUS, spurious);
        let spurious_read = lapic_read(LAPIC_SPURIOUS);
        crate::debug!("Spurious reg: wrote=0x{:x} read=0x{:x}", spurious, spurious_read);
        
        lapic_write(LAPIC_TIMER_DIV, 3);  // Divide by 16
        lapic_write(LAPIC_TIMER_INIT, 625_000);  // Initial count for ~100Hz
        
        let timer_cfg = 32 | (1 << 17);  // Vector 32, periodic mode
        lapic_write(LAPIC_LVT_TIMER, timer_cfg);
        let timer_read = lapic_read(LAPIC_LVT_TIMER);
        crate::debug!("Timer LVT: wrote=0x{:x} read=0x{:x}", timer_cfg, timer_read);
        
        // Check if timer is masked
        if (timer_read & (1 << 16)) != 0 {
            crate::warn!("Timer is MASKED! Unmasking...");
            lapic_write(LAPIC_LVT_TIMER, timer_cfg & !(1 << 16));
        }
        
        lapic_write(LAPIC_EOI, 0);
    }
    crate::debug!("LAPIC fully configured (vector 32, 100 Hz)");
}

fn init_ioapic() {
    if unsafe { IOAPIC_BASE } == 0 { return; }
    unsafe {
        let ver = ioapic_read(0x01);
        let max_redir = ((ver >> 16) & 0xFF) + 1;
        crate::debug!("I/O APIC: {} redir entries", max_redir);
        for i in 0..max_redir {
            ioapic_write(IOAPIC_REDTBL_BASE + i * 2 + 1, 1 << 16);
        }
    }
}

pub fn init() {
    crate::debug!("Initializing...");
    disable_pic();
    let addrs = crate::arch::x86_64::acpi::apic_addresses();
    let lapic_phys = addrs.lapic_phys;
    let ioapic_phys = addrs.ioapic_phys;
    
    // The APIC owns its own MMIO mapping (Requirement 7.4): establish the
    // LAPIC/IOAPIC mappings here via the VMM's `map_mmio` helper before any
    // register access. `map_mmio` maps NO_CACHE|NO_EXECUTE|PRESENT|WRITABLE
    // through the HHDM window and returns the virtual base, which equals
    // `phys + hhdm` — the same value previously computed by hand — so
    // LAPIC_BASE/IOAPIC_BASE are unchanged.
    let lapic_virt =
        crate::memory::vmm::map_mmio(lapic_phys, 0x1000).expect("map LAPIC MMIO");
    let ioapic_virt = if ioapic_phys != 0 {
        crate::memory::vmm::map_mmio(ioapic_phys, 0x1000).expect("map IOAPIC MMIO")
    } else {
        0
    };
    unsafe {
        LAPIC_BASE = lapic_virt;
        IOAPIC_BASE = ioapic_virt;
    }

    crate::debug!("LAPIC virt=0x{:x}, IOAPIC virt=0x{:x}", 
        unsafe { LAPIC_BASE }, unsafe { IOAPIC_BASE });
    
    init_lapic();
    init_ioapic();
}

pub fn register_irq(vector: u8, handler: fn()) {
    IRQ_HANDLERS.lock()[(vector - 32) as usize] = Some(handler);
}

pub fn irq_dispatch(vector: u8) {
    if vector < 32 { return; }
    if let Some(h) = IRQ_HANDLERS.lock()[(vector - 32) as usize] {
        h();
    }
}

pub fn send_eoi() {
    unsafe { lapic_write(LAPIC_EOI, 0); }
}

static mut TIMER_HANDLER: Option<fn()> = None;

pub fn set_timer_handler(handler: fn()) {
    unsafe { TIMER_HANDLER = Some(handler); }
    crate::debug!("Timer handler registered");
}

pub fn timer_tick() {
    unsafe {
        if let Some(h) = TIMER_HANDLER { h(); }
    }
}

pub fn route_irq(isa_irq: u8, vector: u8) {
    if unsafe { IOAPIC_BASE } == 0 { return; }
    unsafe {
        let reg = IOAPIC_REDTBL_BASE + (isa_irq as u32) * 2;
        ioapic_write(reg, vector as u32);
        ioapic_write(reg + 1, 0);
    }
    crate::debug!("IRQ {} routed to vector {}", isa_irq, vector);
}
