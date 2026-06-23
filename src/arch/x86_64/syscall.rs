// arch/x86_64/syscall.rs — System call interface (SYSCALL/SYSRET)
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use x86_64::VirtAddr;
use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::segmentation::SegmentSelector;
use x86_64::registers::rflags::RFlags;

pub const SYS_WRITE: u64 = 1;
pub const SYS_EXIT: u64  = 2;
pub const SYS_YIELD: u64 = 3;

pub fn init() {
    unsafe {
        let mut efer = Efer::read();
        efer |= EferFlags::SYSTEM_CALL_EXTENSIONS;
        Efer::write(efer);

        let user_cs = crate::arch::x86_64::gdt::Selectors::user_code();
        let kernel_cs = crate::arch::x86_64::gdt::Selectors::kernel_code();
        Star::write(
            user_cs,
            SegmentSelector::new(0, x86_64::PrivilegeLevel::Ring0),
            SegmentSelector::new(0, x86_64::PrivilegeLevel::Ring0),
            kernel_cs,
        ).ok();

        LStar::write(VirtAddr::new(syscall_entry as *const () as u64));
        SFMask::write(RFlags::INTERRUPT_FLAG);
    }
    // NOTE: the boot orchestrator's `init_syscalls` step emits the single
    // concise `info!("syscalls")` line; we deliberately avoid double-logging here.
}

#[no_mangle]
extern "sysv64" fn syscall_entry(
    syscall_number: u64, arg1: u64, arg2: u64, arg3: u64,
    _arg4: u64, _arg5: u64, _arg6: u64,
) -> u64 {
    syscall_dispatch(syscall_number, arg1, arg2, arg3)
}

// ─── int 0x80 system-call trampoline ─────────────────────────────────────────
//
// The user test program invokes syscalls via `int 0x80` rather than the
// `syscall` instruction. `int 0x80` is markedly simpler to get right in a small
// kernel: the CPU already switches to the kernel stack (TSS RSP0) on the ring-3
// → ring-0 transition and `iretq` cleanly returns to ring 3, so we avoid the
// `syscall`/`sysretq` subtleties (manual stack switch, rcx/r11 preservation,
// SS/CS reload ordering). The IDT entry for vector 0x80 is installed with DPL=3
// so ring-3 code is permitted to raise it (see `idt::init`).
//
// User ABI (matches the existing dispatcher): syscall number in rax, arguments
// in rdi/rsi/rdx. On `int 0x80` the CPU pushes [SS, RSP, RFLAGS, CS, RIP] onto
// RSP0 (no error code). We save all GPRs (so the caller sees them preserved
// except rax, which receives the result), marshal the user registers into the
// SysV ABI `syscall_dispatch(num, a1, a2, a3)` expects, dispatch, write the
// result back into the saved rax slot, restore, and `iretq`.
//
// Stack alignment: RSP0 is 16-byte aligned. The CPU pushes 40 bytes, then we
// push 15×8 = 120 bytes → 160 bytes total, leaving RSP 16-byte aligned right
// before `call` (SysV requirement).
core::arch::global_asm!(
    ".global int80_stub",
    "int80_stub:",
    "    push rax",
    "    push rbx",
    "    push rcx",
    "    push rdx",
    "    push rsi",
    "    push rdi",
    "    push rbp",
    "    push r8",
    "    push r9",
    "    push r10",
    "    push r11",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    // Marshal user (rax=num, rdi=a1, rsi=a2, rdx=a3) into SysV arg registers
    // (rdi=num, rsi=a1, rdx=a2, rcx=a3). The original user values are still live
    // in the registers (push does not modify them); reorder bottom-up so no
    // source is clobbered before it is read.
    "    mov rcx, rdx",   // a3  -> 4th arg
    "    mov rdx, rsi",   // a2  -> 3rd arg
    "    mov rsi, rdi",   // a1  -> 2nd arg
    "    mov rdi, rax",   // num -> 1st arg
    "    call syscall_dispatch",
    // Store the return value into the saved rax slot (rax was pushed first, so
    // it sits 14 slots = 112 bytes above the current rsp).
    "    mov [rsp + 112], rax",
    "    pop r15",
    "    pop r14",
    "    pop r13",
    "    pop r12",
    "    pop r11",
    "    pop r10",
    "    pop r9",
    "    pop r8",
    "    pop rbp",
    "    pop rdi",
    "    pop rsi",
    "    pop rdx",
    "    pop rcx",
    "    pop rbx",
    "    pop rax",
    "    iretq",
);

extern "C" {
    /// Naked `int 0x80` entry stub (see the `global_asm!` above). Installed as
    /// the IDT vector-0x80 handler with DPL=3 by `idt::init`.
    pub fn int80_stub();
}

/// Shared syscall dispatcher used by both the (unused) `syscall` fast-path entry
/// and the `int 0x80` trampoline. `num` is the syscall number and `a1`..`a3` are
/// the user arguments (originally in rdi/rsi/rdx).
#[no_mangle]
extern "C" fn syscall_dispatch(num: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    match num {
        SYS_WRITE => sys_write(a1, a2, a3),
        SYS_EXIT  => sys_exit(a1),
        SYS_YIELD => { crate::task::scheduler::yield_current(); 0 }
        _ => { crate::error!("[SYSCALL] Unknown: {}", num); u64::MAX }
    }
}

/// Exclusive upper bound of the lower-half canonical address range.
///
/// A canonical user pointer on x86_64 has bits 47..63 all zero, i.e. it must be
/// strictly below `0x0000_8000_0000_0000`. Both the START and the END of a user
/// buffer must satisfy this so the kernel never reads across into non-canonical
/// or kernel space (Requirement 12.3).
const USER_CANONICAL_LIMIT: u64 = 0x0000_8000_0000_0000;

fn sys_write(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    // ── Validation (Requirement 12.3): reject null / oversized / non-canonical
    // / unmapped user pointers WITHOUT ever dereferencing them. ──────────────
    //
    // Only stdout (fd 1) is writable.
    if fd != 1 { return u64::MAX; }
    // Non-null buffer and a bounded, non-zero length. `len <= 4096` bounds the
    // buffer to at most two pages, keeping the page-presence walk below cheap.
    if buf_ptr == 0 || len == 0 || len > 4096 { return u64::MAX; }
    // Start must be a lower-half canonical pointer (bits 47..63 zero).
    if buf_ptr >= USER_CANONICAL_LIMIT { return u64::MAX; }
    // The END of the buffer must also stay below the canonical boundary: reject
    // on arithmetic overflow (wrap) or if the last byte would land in
    // non-canonical / kernel space. `checked_add` catches the wrap case.
    match buf_ptr.checked_add(len) {
        Some(end) if end <= USER_CANONICAL_LIMIT => {}
        _ => return u64::MAX,
    }

    // Page-presence check: confirm every page the buffer spans is actually
    // mapped before reading, so the kernel never faults on an unmapped user
    // pointer ("never dereference an unvalidated user pointer", design §Safety).
    // The buffer spans at most two pages (len <= 4096); walk each page's base
    // from `buf_ptr` to the page containing the last byte (`buf_ptr+len-1`).
    let first_page = buf_ptr & !0xFFF;
    let last_page = (buf_ptr + len - 1) & !0xFFF;
    let mut page = first_page;
    loop {
        if crate::memory::vmm::virt_to_phys(page).is_none() {
            // Unmapped page in the buffer's span: refuse without dereferencing.
            return u64::MAX;
        }
        if page == last_page { break; }
        page += 0x1000;
    }

    // Route the write through the serial `Console` abstraction rather than the
    // old per-byte `format_args!` hack (Requirement 12.2).
    use crate::drivers::Console;
    let console = crate::drivers::serial::console();

    // SAFETY: `buf_ptr`/`len` were fully validated above: non-null, len <= 4096,
    // start AND end within the lower-half canonical range, and every page the
    // buffer spans confirmed mapped via `virt_to_phys`. The read therefore
    // cannot fault on an unmapped page or cross into kernel space.
    let slice = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, len as usize) };

    match core::str::from_utf8(slice) {
        // Valid UTF-8: write the whole string in one shot.
        Ok(s) => console.write_str(s),
        // Invalid UTF-8: never panic. Write the valid prefix as a `&str`, then
        // emit the remaining bytes individually as chars so output is robust.
        Err(e) => {
            let valid_up_to = e.valid_up_to();
            // SAFETY: `from_utf8` guarantees `slice[..valid_up_to]` is valid UTF-8.
            let prefix = unsafe { core::str::from_utf8_unchecked(&slice[..valid_up_to]) };
            console.write_str(prefix);
            for &byte in &slice[valid_up_to..] {
                let mut tmp = [0u8; 4];
                let s = (byte as char).encode_utf8(&mut tmp);
                console.write_str(s);
            }
        }
    }

    // Report the number of bytes consumed from the user buffer.
    len
}

fn sys_exit(code: u64) -> u64 {
    crate::debug!("sys_exit({})", code);
    // Requirement 12.4: terminate ONLY the calling task and let the scheduler
    // keep running other tasks. `exit_current` removes this task from the
    // rotation and never returns (the next timer tick switches away and never
    // schedules it again); it falls back to a full halt only if invoked on the
    // idle task. This replaces the old `halt_loop()` which froze the whole CPU.
    crate::task::scheduler::exit_current()
}
