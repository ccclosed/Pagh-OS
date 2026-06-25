// pagh OS kernel — 64-bit kernel in Rust
//
// ┌─────────────────────────────────────────────────────────────────────────┐
// │  ONE CRATE, TWO KERNELS — selected by the build target (cfg(target_arch)) │
// ├─────────────────────────────────────────────────────────────────────────┤
// │  • x86_64  — the original pagh kernel (Limine/UEFI). Modules below are    │
// │    gated `#[cfg(target_arch = "x86_64")]`. Build/run: `run.cmd`.          │
// │  • riscv64 — the in-progress port (OpenSBI/QEMU virt). Its sources live   │
// │    in `src/arch/riscv64/` and are declared at the crate root here via     │
// │    `#[cfg(target_arch = "riscv64")] #[path = "arch/riscv64/X.rs"] mod X;`  │
// │    so the ported code's `crate::pmm`/`crate::timer`/`kprintln!` paths      │
// │    resolve unchanged. Build/run: `run_rv.cmd`.                            │
// │                                                                           │
// │  The two arches are MUTUALLY EXCLUSIVE: only one set of modules ever      │
// │  compiles in a given build, so a few module names (e.g. `net`, `shell`)   │
// │  intentionally appear once per arch. NOTE: today the riscv side keeps its │
// │  own copies of some upper-half modules (net/shell/ramfs); the planned     │
// │  end-state shares those and leaves only the true arch layer (cpu/trap/    │
// │  paging/timer/plic/sbi/uart) under arch/riscv64. See .kiro/specs/         │
// │  riscv-port/ and CONTRIBUTING.                                            │
// └─────────────────────────────────────────────────────────────────────────┘

#![no_std]
#![no_main]
#![cfg_attr(target_arch = "x86_64", feature(abi_x86_interrupt))]
#![feature(allocator_api)]
#![feature(custom_test_frameworks)]
#![feature(sync_unsafe_cell)]
#![test_runner(crate::test_runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;
#[cfg(target_arch = "x86_64")]
use core::panic::PanicInfo;
#[cfg(target_arch = "x86_64")]
use core::sync::atomic::AtomicU64;

// ── x86_64 kernel modules (the original pagh kernel) ──
#[cfg(target_arch = "x86_64")]
mod arch;
#[cfg(target_arch = "x86_64")]
mod boot;
#[cfg(target_arch = "x86_64")]
mod debug;
#[cfg(target_arch = "x86_64")]
mod drivers;
#[cfg(target_arch = "x86_64")]
mod fs;
#[cfg(target_arch = "x86_64")]
mod log;
#[cfg(target_arch = "x86_64")]
mod memory;
#[cfg(target_arch = "x86_64")]
mod net;
#[cfg(target_arch = "x86_64")]
mod pkg;
/// Boot-time Linux-compat self-test harness, compiled only under the
/// `lx_selftest`, `lx_livetest`, or `lx_bigindex` cargo features so the default
/// build/boot is unchanged.
#[cfg(all(target_arch = "x86_64", any(feature = "lx_selftest", feature = "lx_livetest", feature = "lx_bigindex")))]
mod selftest_lx;
#[cfg(target_arch = "x86_64")]
mod shell;
#[cfg(target_arch = "x86_64")]
mod sync;
#[cfg(target_arch = "x86_64")]
mod task;
#[cfg(target_arch = "x86_64")]
mod test;
#[cfg(target_arch = "x86_64")]
mod vfs;

// ── riscv64 kernel modules (folded in from the former rv/ seed). They live in
// src/arch/riscv64/ but are declared at the crate root via `#[path]` so the
// ported code's `crate::pmm`, `crate::timer`, `crate::kprintln!`, ... resolve
// unchanged. `sbi` is declared first with `#[macro_use]` so its `kprint!`/
// `kprintln!` macros are in textual scope for every following riscv module. ──
#[cfg(target_arch = "riscv64")]
#[macro_use]
#[path = "arch/riscv64/sbi.rs"]
mod sbi;
#[cfg(target_arch = "riscv64")]
#[path = "arch/riscv64/dtb.rs"]
mod dtb;
#[cfg(target_arch = "riscv64")]
#[path = "arch/riscv64/cpu.rs"]
mod cpu;
#[cfg(target_arch = "riscv64")]
#[path = "arch/riscv64/pmm.rs"]
mod pmm;
#[cfg(target_arch = "riscv64")]
#[path = "arch/riscv64/paging.rs"]
mod paging;
#[cfg(target_arch = "riscv64")]
#[path = "arch/riscv64/heap.rs"]
mod heap;
#[cfg(target_arch = "riscv64")]
#[path = "arch/riscv64/timer.rs"]
mod timer;
#[cfg(target_arch = "riscv64")]
#[path = "arch/riscv64/trap.rs"]
mod trap;
#[cfg(target_arch = "riscv64")]
#[path = "arch/riscv64/plic.rs"]
mod plic;
#[cfg(target_arch = "riscv64")]
#[path = "arch/riscv64/sched.rs"]
mod sched;
#[cfg(target_arch = "riscv64")]
#[path = "arch/riscv64/umode.rs"]
mod umode;
#[cfg(target_arch = "riscv64")]
#[path = "arch/riscv64/elf.rs"]
mod elf;
#[cfg(target_arch = "riscv64")]
#[path = "arch/riscv64/uart.rs"]
mod uart;
#[cfg(target_arch = "riscv64")]
#[path = "arch/riscv64/blk.rs"]
mod blk;
#[cfg(target_arch = "riscv64")]
#[path = "arch/riscv64/net.rs"]
mod net;
#[cfg(target_arch = "riscv64")]
#[path = "arch/riscv64/ramfs.rs"]
mod ramfs;
#[cfg(target_arch = "riscv64")]
#[path = "arch/riscv64/shell.rs"]
mod shell;
#[cfg(target_arch = "riscv64")]
#[path = "arch/riscv64/boot.rs"]
mod rv_boot;

#[cfg(target_arch = "x86_64")]
use limine::request::{ExecutableAddressRequest, FramebufferRequest, HhdmRequest, MemmapRequest, RsdpRequest};
#[cfg(target_arch = "x86_64")]
use limine::BaseRevision;

#[cfg(target_arch = "x86_64")]
#[used]
#[no_mangle]
#[link_section = ".requests"]
pub static BASE_REVISION: BaseRevision = BaseRevision::with_revision(2);

#[cfg(target_arch = "x86_64")]
#[used]
#[no_mangle]
#[link_section = ".requests"]
pub static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

#[cfg(target_arch = "x86_64")]
#[used]
#[no_mangle]
#[link_section = ".requests"]
pub static MEMMAP_REQUEST: MemmapRequest = MemmapRequest::new();

#[cfg(target_arch = "x86_64")]
#[used]
#[no_mangle]
#[link_section = ".requests"]
pub static KERNEL_ADDR_REQUEST: ExecutableAddressRequest = ExecutableAddressRequest::new();

#[cfg(target_arch = "x86_64")]
#[used]
#[no_mangle]
#[link_section = ".requests"]
pub static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

#[cfg(target_arch = "x86_64")]
#[used]
#[no_mangle]
#[link_section = ".requests"]
pub static RSDP_REQUEST: RsdpRequest = RsdpRequest::new();

#[cfg(target_arch = "x86_64")]
pub(crate) static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);
#[cfg(target_arch = "x86_64")]
pub(crate) static KERNEL_BASE: AtomicU64 = AtomicU64::new(0);
#[cfg(target_arch = "x86_64")]
pub(crate) static KERNEL_SIZE: AtomicU64 = AtomicU64::new(0);

#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub fn _start() -> ! {
    crate::boot::start()
}

#[cfg(target_arch = "x86_64")]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    arch::cpu::disable_interrupts();
    if let Some(loc) = info.location() {
        kprint!("[PANIC] {}:{} — ", loc.file(), loc.line());
    } else { kprint!("[PANIC] "); }
    kprintln!("{}", info.message());
    debug::unwind::stack_trace();
    arch::cpu::halt_loop();
}

#[cfg(test)]
fn test_runner(_tests: &[&dyn Fn()]) {}
