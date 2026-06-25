//! pagh OS — RISC-V (riscv64gc) boot/orchestration (folded into the main crate
//! from the former standalone `rv/` seed; branch `riscv-port`).
//!
//! Boot path under QEMU `virt` + OpenSBI: firmware (M-mode) jumps to `_start`
//! at 0x8020_0000 in S-mode with `a0`=hartid, `a1`=DTB pointer. `_start` sets up
//! the boot stack and calls [`kmain`], which brings the kernel up step by step.
//!
//! The riscv64 modules are declared at the crate root via `#[path]` in `lib.rs`
//! (gated `#[cfg(target_arch = "riscv64")]`), so this code's `crate::pmm`,
//! `crate::timer`, `crate::kprintln!`, ... paths resolve unchanged.

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
    let mem = crate::dtb::memory(dtb).unwrap_or(crate::dtb::MemInfo {
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

    crate::pmm::init(pmm_start, mem.end);
    let (free, total) = crate::pmm::stats();
    kprintln!(
        "rv: PMM up -- {} / {} frames free ({} MiB usable)",
        free,
        total,
        (free * crate::pmm::FRAME_SIZE) / (1024 * 1024)
    );

    // 3. Enable Sv39 identity paging (root table from the PMM).
    // SAFETY: boot hart, PMM is up, mapping is identity so execution continues.
    unsafe { crate::paging::init_identity() };
    kprintln!("rv: Sv39 identity paging enabled (satp set)");

    // 4. Bring up the heap over the carved, identity-mapped region.
    // SAFETY: region is owned by the heap and identity-mapped readable/writable.
    unsafe { crate::heap::init(heap_start, HEAP_SIZE) };
    kprintln!("rv: heap up ({} MiB, galloc)", HEAP_SIZE / (1024 * 1024));

    // 4b. Bring up the real ns16550 MMIO UART and move the console onto it.
    if let Some(ub) = crate::dtb::uart(dtb) {
        crate::uart::init(ub);
        kprintln!("rv: ns16550 UART @ {:#x} online -- console now on MMIO", ub);
        // Arm interrupt-driven RX: PLIC routes the UART IRQ to S-mode, the UART
        // raises RDA interrupts, and SEIE lets them be delivered (once SIE is on).
        crate::plic::init();
        crate::uart::enable_rx_interrupt();
        // SAFETY: arming SEIE; the trap vector is installed before interrupts are
        // globally enabled below.
        unsafe { crate::cpu::sie_set(1 << 9) };
        kprintln!("rv: PLIC + UART RX interrupt armed (IRQ {})", crate::plic::UART_IRQ);
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
    crate::trap::init();
    crate::timer::init();
    // SAFETY: the trap vector and timer are armed before enabling interrupts.
    unsafe { crate::cpu::enable_interrupts() };
    kprintln!("rv: traps + 100 Hz timer armed; counting ticks...");

    let mut last_sec = 0u64;
    loop {
        // SAFETY: wait for the next interrupt (the timer fires at 100 Hz).
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
        let t = crate::timer::ticks();
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

    // 7. Preemptive scheduler: two CPU-bound threads that NEVER yield; only the
    //    timer interrupt switches between them. Bounded: each prints a few times
    //    then exits, after which only `main` remains runnable.
    crate::sched::init();
    crate::sched::spawn(worker_a);
    crate::sched::spawn(worker_b);
    kprintln!("rv: preemptive scheduler up; 2 non-yielding threads, timer-driven:");
    while crate::sched::running_count() > 1 {
        core::hint::spin_loop();
    }
    kprintln!("rv: workers finished; back in main (preemption now a no-op).");
    kprintln!("rv: Milestone C.2 OK -- preemptive context switch + scheduler.");

    // 8. virtio-blk over virtio-mmio: attach + read/write round-trip.
    crate::blk::init(dtb);
    crate::blk::selftest();
    kprintln!("rv: Milestone E (blk) OK -- virtio-mmio + virtio-blk.");

    // 8a. Storage: register blk as a BlockDevice, bring up the VFS, and mount a
    //     journaled ext2 filesystem at /mnt (ported from the x86 kernel).
    crate::blk::register();
    crate::vfs::init();
    mount_ext2();

    // 8b. virtio-net + smoltcp: acquire a DHCPv4 lease.
    crate::net::demo(dtb);
    kprintln!("rv: Milestone E (net) OK -- virtio-net + smoltcp + DHCP.");

    // 8c. ramfs self-test + persistence to virtio-blk.
    crate::ramfs::selftest();
    crate::ramfs::persist_selftest();

    // 9. Load and run a real static riscv64 ELF in U-mode.
    let image = crate::elf::build_test_elf();
    crate::elf::load_and_run(&image);
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
    let mut next = crate::timer::ticks() + 30;
    loop {
        if crate::timer::ticks() >= next {
            kprintln!("    [preempt {}] print {}", name, printed);
            printed += 1;
            next = crate::timer::ticks() + 30;
            if printed >= 3 {
                crate::sched::exit();
            }
        }
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("\nrv: PANIC: {}", info);
    crate::cpu::park()
}

/// Bring up the journaled ext2 filesystem on the virtio-blk disk and mount it at
/// `/mnt` (ported from the x86 boot path). On first boot (no valid superblock)
/// the disk is formatted, then mounted; a one-shot journaled write/read demo
/// proves the real-disk path.
fn mount_ext2() {
    use crate::fs::ext2::Ext2Fs;

    let blk = match crate::drivers::get_block("virtio-blk0") {
        Some(b) => b,
        None => {
            crate::warn!("fs: no virtio-blk device; /mnt not mounted");
            return;
        }
    };

    // First boot: if there is no valid ext2 superblock yet, format the disk.
    if Ext2Fs::mount(blk.clone()).is_err() {
        kprintln!("rv: fs: no ext2 filesystem found, formatting disk...");
        if let Err(e) = Ext2Fs::format(blk.clone()) {
            crate::error!("fs: format failed: {:?}; /mnt not mounted", e);
            return;
        }
    }

    match Ext2Fs::mount(blk) {
        Ok(root) => {
            if let Err(e) = crate::vfs::mount_at("/mnt", root) {
                crate::error!("fs: mount_at(/mnt) failed: {:?}", e);
                return;
            }
            kprintln!("rv: ext2 mounted at /mnt");
            fs_demo();
        }
        Err(e) => crate::warn!("fs: ext2 mount failed: {:?}", e),
    }
}

/// One-shot journaled write/read self-demo against the real ext2 disk at /mnt.
fn fs_demo() {
    const NAME: &str = "rvfs.txt";
    const CONTENT: &[u8] = b"pagh-riscv-ext2-journaled-write-OK";

    let mnt = match crate::vfs::lookup_path("/mnt") {
        Ok(n) => n,
        Err(_) => return,
    };
    let f = match mnt.create_file(NAME) {
        Ok(f) => f,
        Err(e) => {
            crate::warn!("fs demo: create failed: {:?}", e);
            return;
        }
    };
    if f.write(0, CONTENT).is_err() {
        crate::warn!("fs demo: write failed");
        return;
    }
    mnt.sync();

    let f2 = match mnt.lookup(NAME) {
        Ok(f) => f,
        Err(_) => {
            crate::warn!("fs demo: lookup-after-write failed");
            return;
        }
    };
    let mut buf = [0u8; 64];
    let n = f2.read(0, &mut buf).unwrap_or(0);
    if &buf[..n] == CONTENT {
        kprintln!(
            "rv: fs demo: /mnt/{} journaled write+read round-trip PASS ({} bytes)",
            NAME,
            n
        );
    } else {
        kprintln!("rv: fs demo: /mnt/{} MISMATCH (n={})", NAME, n);
    }
}
