//! pagh OS — RISC-V (riscv64gc) kernel (branch `riscv-port`).
//!
//! Boot path under QEMU `virt` + OpenSBI: firmware (M-mode) jumps to `_start`
//! at 0x8020_0000 in S-mode with `a0`=hartid, `a1`=DTB pointer. `_start` sets up
//! the boot stack and calls [`kmain`], which brings the kernel up step by step.
//!
//! Milestone A: SBI console.
//! Milestone B: DTB memory discovery, bitmap PMM, Sv39 identity paging, heap.
#![no_std]
#![no_main]

extern crate alloc;

mod blk;
mod cpu;
mod dtb;
mod heap;
mod net;
mod paging;
mod pmm;
mod sbi;
mod sched;
mod shell;
mod timer;
mod trap;
mod uart;
mod umode;

use alloc::vec::Vec;
use core::panic::PanicInfo;

// Entry trampoline (first bytes at 0x8020_0000). Set sp to the top of the
// linker-provided boot stack, then call Rust. `a0`/`a1` (hartid, DTB) are
// untouched before the call, so they arrive as kmain's C arguments.
core::arch::global_asm!(
    r#"
    .section .text.entry
    .globl _start
_start:
    la      sp, _stack_top
    call    kmain
.hang:
    wfi
    j       .hang
"#
);

extern "C" {
    static _kernel_end: u8;
}

/// First free physical address after the loaded kernel image (+ boot stack).
fn kernel_end() -> usize {
    core::ptr::addr_of!(_kernel_end) as usize
}

/// Fixed kernel-heap size carved from the front of usable RAM.
const HEAP_SIZE: usize = 32 * 1024 * 1024;

/// Default RAM assumption if the DTB has no sized memory region.
const DEFAULT_RAM_END: usize = 0x8000_0000 + 128 * 1024 * 1024;

#[no_mangle]
pub extern "C" fn kmain(hartid: usize, dtb: usize) -> ! {
    kprintln!();
    kprintln!("========================================");
    kprintln!("  pagh OS  --  riscv64 (S-mode, OpenSBI)");
    kprintln!("========================================");
    kprintln!("rv: boot hart {}, dtb @ {:#x}", hartid, dtb);

    // 1. Discover RAM from the device tree (fall back conservatively).
    let mem = dtb::memory(dtb).unwrap_or(dtb::MemInfo {
        start: 0x8000_0000,
        end: DEFAULT_RAM_END,
    });
    kprintln!(
        "rv: RAM {:#x}..{:#x} ({} MiB)",
        mem.start,
        mem.end,
        (mem.end - mem.start) / (1024 * 1024)
    );

    // 2. Carve [kernel_end .. +HEAP_SIZE] for the heap, give the rest to the PMM.
    let kend = (kernel_end() + 0xfff) & !0xfff;
    let heap_start = kend;
    let pmm_start = heap_start + HEAP_SIZE;
    kprintln!(
        "rv: kernel_end {:#x}; heap {:#x}..{:#x} ({} MiB)",
        kend,
        heap_start,
        pmm_start,
        HEAP_SIZE / (1024 * 1024)
    );

    pmm::init(pmm_start, mem.end);
    let (free, total) = pmm::stats();
    kprintln!(
        "rv: PMM up -- {} / {} frames free ({} MiB usable)",
        free,
        total,
        (free * pmm::FRAME_SIZE) / (1024 * 1024)
    );

    // 3. Enable Sv39 identity paging (root table from the PMM).
    // SAFETY: boot hart, PMM is up, mapping is identity so execution continues.
    unsafe { paging::init_identity() };
    kprintln!("rv: Sv39 identity paging enabled (satp set)");

    // 4. Bring up the heap over the carved, identity-mapped region.
    // SAFETY: region is owned by the heap and identity-mapped readable/writable.
    unsafe { heap::init(heap_start, HEAP_SIZE) };
    kprintln!("rv: heap up ({} MiB, galloc)", HEAP_SIZE / (1024 * 1024));

    // 4b. Bring up the real ns16550 MMIO UART and move the console onto it.
    if let Some(ub) = dtb::uart(dtb) {
        uart::init(ub);
        kprintln!("rv: ns16550 UART @ {:#x} online -- console now on MMIO", ub);
    } else {
        kprintln!("rv: ns16550 UART not found in DTB; staying on SBI console");
    }

    // 5. Smoke-test the allocator: build a Vec on the heap and use it.
    let mut v: Vec<u64> = Vec::new();
    for i in 0..1000u64 {
        v.push(i * i);
    }
    let sum: u64 = v.iter().sum();
    kprintln!("rv: heap test -- sum of squares 0..1000 = {} (len {})", sum, v.len());

    kprintln!("rv: Milestone B OK -- DTB + PMM + Sv39 + heap.");

    // 6. Traps + a periodic ~100 Hz timer interrupt (Milestone C).
    trap::init();
    timer::init();
    // SAFETY: the trap vector and timer are armed before enabling interrupts.
    unsafe { cpu::enable_interrupts() };
    kprintln!("rv: traps + 100 Hz timer armed; counting ticks...");

    let mut last_sec = 0u64;
    loop {
        // SAFETY: wait for the next interrupt (the timer fires at 100 Hz).
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
        let t = timer::ticks();
        let sec = t / 100;
        if sec > last_sec {
            last_sec = sec;
            kprintln!("rv: ~{}s elapsed, {} timer ticks", sec, t);
            if sec >= 2 {
                break;
            }
        }
    }

    kprintln!("rv: Milestone C OK -- trap vector + 100 Hz timer interrupts.");

    // 7. Cooperative scheduler + context switch over kernel threads.
    sched::init();
    sched::spawn(thread_a);
    sched::spawn(thread_b);
    kprintln!("rv: scheduler up; cooperative round-robin over 2 kernel threads:");
    for _ in 0..3 {
        sched::yield_now();
    }
    kprintln!("rv: back in main; context switch + scheduler OK.");

    kprintln!("rv: Milestone C.2 OK -- context switch + cooperative scheduler.");

    // 8. virtio-blk over virtio-mmio: probe + read/write round-trip (Milestone E).
    blk::test(dtb);
    kprintln!("rv: Milestone E (blk) OK -- virtio-mmio + virtio-blk.");

    // 8b. virtio-net + smoltcp: acquire a DHCPv4 lease (Milestone E).
    net::demo(dtb);
    kprintln!("rv: Milestone E (net) OK -- virtio-net + smoltcp + DHCP.");

    // 9. Drop to U-mode and run a tiny program that makes ecall syscalls.
    let (entry, user_sp) = umode::setup();
    kprintln!("rv: entering U-mode at {:#x} (user sp {:#x})...", entry, user_sp);
    // SAFETY: entry/stack are mapped user-accessible; this does not return
    // (the user program exits via the SYS_EXIT syscall, which parks).
    unsafe { umode::enter(entry, user_sp) };
}

/// Demo kernel thread A: print and cooperatively yield.
extern "C" fn thread_a() -> ! {
    let mut i = 0u64;
    loop {
        kprintln!("    [thread A] iteration {}", i);
        i += 1;
        sched::yield_now();
    }
}

/// Demo kernel thread B: print and cooperatively yield.
extern "C" fn thread_b() -> ! {
    let mut i = 0u64;
    loop {
        kprintln!("    [thread B] iteration {}", i);
        i += 1;
        sched::yield_now();
    }
}

/// Park the current hart until the next interrupt, forever.
fn park() -> ! {
    cpu::park()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("\nrv: PANIC: {}", info);
    park();
}
