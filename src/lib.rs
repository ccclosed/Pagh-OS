// pagh OS kernel — 64-bit hybrid kernel in Rust
// lib.rs: entry point, panic handler, test infrastructure

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![feature(allocator_api)]
#![feature(custom_test_frameworks)]
#![feature(sync_unsafe_cell)]
#![test_runner(crate::test_runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;
use core::panic::PanicInfo;
use core::sync::atomic::AtomicU64;

mod arch;
mod boot;
mod boot_menu;
mod debug;
mod drivers;
mod fs;
mod log;
mod logfile;
mod memory;
mod net;
mod pkg;
mod provision;
mod security;
/// Boot-time Linux-compat self-test harness, compiled only under the
/// `lx_selftest`, `lx_livetest`, `lx_bigindex`, or `lx_tlsbig` cargo features so
/// the default build/boot is unchanged.
#[cfg(any(
    feature = "lx_selftest",
    feature = "lx_livetest",
    feature = "lx_bigindex",
    feature = "lx_tlsbig"
))]
mod selftest_lx;
mod shell;
mod sync;
mod task;
mod test;
mod vfs;

use limine::request::{
    ExecutableAddressRequest, FramebufferRequest, HhdmRequest, MemmapRequest, ModulesRequest,
    RsdpRequest,
};
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

/// Boot-loaded files (`module_path:` entries in `limine.conf`).
///
/// The storage path uses this to find a disk image travelling next to the
/// kernel — the boot medium itself becomes the system's disk, which is what
/// makes a machine with no supported drive usable at all (see
/// `drivers::memdisk`). The request is unconditional and costs one response
/// lookup; with no modules configured the response is simply empty.
#[used]
#[no_mangle]
#[link_section = ".requests"]
pub static MODULES_REQUEST: ModulesRequest = ModulesRequest::new();

pub(crate) static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);
pub(crate) static KERNEL_BASE: AtomicU64 = AtomicU64::new(0);
pub(crate) static KERNEL_SIZE: AtomicU64 = AtomicU64::new(0);

/// The kernel's own boot stack.
///
/// WHY THE KERNEL DOES NOT USE THE BOOTLOADER'S STACK. Limine hands over with a
/// stack it sized for its own purposes, and every call the kernel makes before
/// the scheduler installs a per-task stack runs on it. That works until it does
/// not: on a BIOS boot, adding one call to the boot path pushed a legitimate
/// depth over the limit, the overflow landed on memory Limine had put below the
/// stack, and the kernel died fetching an instruction from a page that was no
/// longer mapped — a crash that appears and disappears with unrelated code
/// changes, which is the signature of a stack overflow rather than a logic bug.
/// Owning the stack makes the boot path independent of the loader's sizing.
///
/// 128 KiB matches half of a kernel task stack (`KERNEL_STACK_PAGES`), which is
/// far more than the boot path has ever used, and it is `.bss`: it costs no
/// space in the image.
#[repr(align(16))]
struct BootStack([u8; BOOT_STACK_BYTES]);

/// Size of the kernel's boot stack.
const BOOT_STACK_BYTES: usize = 128 * 1024;

static mut BOOT_STACK: BootStack = BootStack([0; BOOT_STACK_BYTES]);

#[no_mangle]
pub fn _start() -> ! {
    // SAFETY: this runs once, before any other code in the kernel, and the only
    // reference to `BOOT_STACK` is the address taken here (the stack is used by
    // value, never read through the static). Setting RSP to its top does not
    // invalidate anything the compiler holds in registers: the `asm!` block
    // diverges, so no stack slot of this function is live across it.
    unsafe {
        let top = core::ptr::addr_of!(BOOT_STACK) as u64;
        let top = (top + BOOT_STACK_BYTES as u64) & !0xF;
        core::arch::asm!(
            "mov rsp, {stack}",
            "call {entry}",
            stack = in(reg) top,
            entry = sym crate::boot::start,
            options(noreturn)
        );
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    arch::cpu::disable_interrupts();
    if let Some(loc) = info.location() {
        kprint!("[PANIC] {}:{} — ", loc.file(), loc.line());
        // A panic is fatal (`panic = "abort"`): on a machine with no serial port
        // the screen is the only witness, so say it there too.
        drivers::earlyfb::errorln(format_args!("PANIC at {}:{}", loc.file(), loc.line()));
    } else {
        kprint!("[PANIC] ");
        drivers::earlyfb::errorln(format_args!("PANIC (no location)"));
    }
    kprintln!("{}", info.message());
    drivers::earlyfb::errorln(format_args!("{}", info.message()));
    debug::unwind::stack_trace();
    arch::cpu::halt_loop();
}

#[cfg(test)]
fn test_runner(_tests: &[&dyn Fn()]) {}
