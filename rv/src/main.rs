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
mod elf;
mod heap;
mod net;
mod paging;
mod plic;
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
        // Arm interrupt-driven RX: PLIC routes the UART IRQ to S-mode, the UART
        // raises RDA interrupts, and SEIE lets them be delivered (once SIE is on).
        plic::init();
        uart::enable_rx_interrupt();
        // SAFETY: arming SEIE; the trap vector is installed in Milestone C before
        // interrupts are globally enabled.
        unsafe { cpu::sie_set(1 << 9) };
        kprintln!("rv: PLIC + UART RX interrupt armed (IRQ {})", plic::UART_IRQ);
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

    // 7. Preemptive scheduler: spawn two CPU-bound threads that NEVER yield;
    //    only the timer interrupt switches between them. Bounded: each prints a
    //    few times then exits, after which only `main` remains runnable.
    sched::init();
    sched::spawn(worker_a);
    sched::spawn(worker_b);
    kprintln!("rv: preemptive scheduler up; 2 non-yielding threads, timer-driven:");
    while sched::running_count() > 1 {
        // main is CPU-bound too; the timer preempts among main/A/B.
        core::hint::spin_loop();
    }
    kprintln!("rv: workers finished; back in main (preemption now a no-op).");
    kprintln!("rv: Milestone C.2 OK -- preemptive context switch + scheduler.");

    // 8. virtio-blk over virtio-mmio: attach + read/write round-trip (Milestone E).
    blk::init(dtb);
    blk::selftest();
    kprintln!("rv: Milestone E (blk) OK -- virtio-mmio + virtio-blk.");

    // 8b. virtio-net + smoltcp: acquire a DHCPv4 lease (Milestone E).
    net::demo(dtb);
    kprintln!("rv: Milestone E (net) OK -- virtio-net + smoltcp + DHCP.");

    // 9. Load and run a real static riscv64 ELF in U-mode (Milestone D/E).
    let image = elf::build_test_elf();
    elf::load_and_run(&image);
}

/// Demo worker A: CPU-bound, never yields; prints a few times (timer-paced) then
/// exits. Interleaving with B proves timer preemption (no cooperative yield).
extern "C" fn worker_a() -> ! {
    preempt_worker("A")
}

/// Demo worker B (see [`worker_a`]).
extern "C" fn worker_b() -> ! {
    preempt_worker("B")
}

/// Shared body: print `name` up to 3 times, ~300 ms apart (by global ticks),
/// busy-waiting in between (no yield/wfi), then exit.
fn preempt_worker(name: &str) -> ! {
    let mut printed = 0u32;
    let mut next = timer::ticks() + 30;
    loop {
        if timer::ticks() >= next {
            kprintln!("    [preempt {}] print {}", name, printed);
            printed += 1;
            next = timer::ticks() + 30;
            if printed >= 3 {
                sched::exit();
            }
        }
        core::hint::spin_loop();
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
