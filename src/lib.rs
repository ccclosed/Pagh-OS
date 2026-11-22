// ядро pagh OS — 64-битное гибридное ядро на Rust
// lib.rs: точка входа, panic-handler, тестовая инфраструктура

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
mod os_identity;
mod pkg;
mod provision;
mod security;
/// Boot-time стенд self-test Linux-совместимости, компилируется только с cargo-фичами
/// `lx_selftest`, `lx_livetest`, `lx_bigindex` или `lx_tlsbig`, так что
/// обычная сборка/загрузка не меняется.
#[cfg(any(
    feature = "lx_selftest",
    feature = "lx_livetest",
    feature = "lx_bigindex",
    feature = "lx_tlsbig",
    feature = "lx_apt_bench"
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

/// Загруженные загрузчиком файлы (записи `module_path:` в `limine.conf`).
///
/// Путь хранения использует это, чтобы найти образ диска, едущий рядом с
/// ядром — сам загрузочный носитель становится диском системы, что и делает
/// машину без поддерживаемого накопителя вообще пригодной (см.
/// `drivers::memdisk`). Запрос безусловный и стоит одного поиска ответа; без
/// настроенных модулей ответ просто пустой.
#[used]
#[no_mangle]
#[link_section = ".requests"]
pub static MODULES_REQUEST: ModulesRequest = ModulesRequest::new();

pub(crate) static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);
pub(crate) static KERNEL_BASE: AtomicU64 = AtomicU64::new(0);
pub(crate) static KERNEL_SIZE: AtomicU64 = AtomicU64::new(0);

/// Собственный boot-стек ядра.
///
/// ПОЧЕМУ ЯДРО НЕ ИСПОЛЬЗУЕТ СТЕК ЗАГРУЗЧИКА. Limine передаёт управление со
/// стеком, размер которого выбрал под свои нужды, и каждый вызов ядра до
/// установки стеков задач планировщиком идёт по нему. Это работает, пока не
/// перестаёт: при BIOS-загрузке добавление одного вызова в boot-путь увело
/// легитимную глубину за лимит, переполнение легло на память, которую Limine
/// положил под стек, и ядро умерло, выбирая инструкцию из уже не замапленной
/// страницы — краш, который появляется и исчезает от несвязанных изменений
/// кода, характерный признак переполнения стека, а не логической ошибки.
/// Собственный стек делает boot-путь независимым от размеров загрузчика.
///
/// 128 KiB — половина стека задачи ядра (`KERNEL_STACK_PAGES`), намного больше,
/// чем boot-путь когда-либо использовал, и это `.bss`: места в образе не стоит.
#[repr(align(16))]
struct BootStack([u8; BOOT_STACK_BYTES]);

/// Размер boot-стека ядра.
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
        // Паника фатальна (`panic = "abort"`): на машине без serial-порта экран —
        // единственный свидетель, так что скажите и там.
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
