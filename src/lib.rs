//! pagh OS — RISC-V (riscv64gc) kernel.
//!
//! This is the `riscv-port` branch: a standalone RISC-V kernel for the QEMU
//! `virt` machine, booted by OpenSBI in S-mode. The x86_64 kernel lives
//! separately on the `main` branch — the two arches are kept on separate
//! branches by design.
//!
//! Boot: OpenSBI jumps to `_start` (in [`boot`]) at 0x8020_0000; `_start` sets up
//! the stack and calls `kmain`, which brings up the SBI/UART console, DTB-driven
//! memory discovery, the bitmap PMM, Sv39 paging, the heap, traps + a 100 Hz
//! timer, a preemptive scheduler, U-mode + `ecall` syscalls, an ELF loader,
//! virtio-blk and virtio-net (smoltcp + DHCP), a ramfs, and an interactive shell.
#![no_std]
#![no_main]

extern crate alloc;

// `sbi` defines the `kprint!`/`kprintln!` console macros and must be declared
// first with `#[macro_use]` so they are in scope for every following module.
#[macro_use]
mod sbi;

mod blk;
mod boot;
mod cpu;
mod dtb;
mod elf;
mod heap;
mod net;
mod paging;
mod plic;
mod pmm;
mod ramfs;
mod sched;
mod shell;
mod timer;
mod trap;
mod uart;
mod umode;
