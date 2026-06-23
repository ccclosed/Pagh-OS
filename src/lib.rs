// pagh OS kernel — 64-bit hybrid kernel in Rust
// lib.rs: entry point, panic handler, test infrastructure

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![feature(allocator_api)]
#![feature(custom_test_frameworks)]
#![test_runner(crate::test_runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;
use core::panic::PanicInfo;
use core::sync::atomic::AtomicU64;

mod arch;
mod boot;
mod debug;
mod drivers;
mod fs;
mod log;
mod memory;
mod net;
mod shell;
mod sync;
mod task;
mod test;
mod vfs;

use limine::request::{ExecutableAddressRequest, FramebufferRequest, HhdmRequest, MemmapRequest, RsdpRequest};
use limine::BaseRevision;

#[used]
#[no_mangle]
#[link_section = ".requests"]
pub static BASE_REVISION: BaseRevision = BaseRevision::with_revision(2);

#[used]
#[no_mangle]
#[link_section = ".requests"]
pub static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

#[used]
#[no_mangle]
#[link_section = ".requests"]
pub static MEMMAP_REQUEST: MemmapRequest = MemmapRequest::new();

#[used]
#[no_mangle]
#[link_section = ".requests"]
pub static KERNEL_ADDR_REQUEST: ExecutableAddressRequest = ExecutableAddressRequest::new();

#[used]
#[no_mangle]
#[link_section = ".requests"]
pub static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

#[used]
#[no_mangle]
#[link_section = ".requests"]
pub static RSDP_REQUEST: RsdpRequest = RsdpRequest::new();

pub(crate) static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);
pub(crate) static KERNEL_BASE: AtomicU64 = AtomicU64::new(0);
pub(crate) static KERNEL_SIZE: AtomicU64 = AtomicU64::new(0);

#[no_mangle]
pub fn _start() -> ! {
    crate::boot::start()
}

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
