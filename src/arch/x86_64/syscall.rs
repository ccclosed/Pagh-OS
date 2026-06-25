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

        // LStar points at the naked `syscall_entry` stub (global_asm below) so the
        // `syscall` instruction and `int 0x80` both funnel into `linux_dispatch`.
        // SFMASK clears IF on `syscall`, so the kernel runs the syscall window with
        // interrupts masked — this is what makes the single-slot user-RSP scratch in
        // `syscall_entry` non-reentrant-safe under the current single-task model.
        LStar::write(VirtAddr::new(syscall_entry as *const () as u64));
        SFMask::write(RFlags::INTERRUPT_FLAG);
    }
    // NOTE: the boot orchestrator's `init_syscalls` step emits the single
    // concise `info!("syscalls")` line; we deliberately avoid double-logging here.
}

// ─── `syscall`-instruction kernel-stack mirror ───────────────────────────────
//
// Unlike `int 0x80`, the `syscall` instruction performs NO stack switch: on entry
// the kernel is still running on the user's `rsp`. We must switch to a kernel
// stack manually before pushing the saved-register frame. The kernel uses the
// same per-task kernel stack the CPU loads from TSS `RSP0` for the `int 0x80`
// path; since there is no per-CPU `GS` base set up (no `swapgs`), the `syscall`
// stub cannot read `RSP0` out of the TSS cheaply, so we mirror the current RSP0
// into this global whenever it is (re)programmed (`set_syscall_kernel_stack`,
// called alongside `gdt::set_kernel_stack` in `task::process`).
//
// Single-task model: there is exactly one RSP0 slot today (see
// `gdt::set_kernel_stack`), so a single global mirror is exact. The `syscall`
// window runs with IF masked (SFMASK), so this slot is not re-entered.
#[no_mangle]
static mut SYSCALL_KERNEL_RSP: u64 = 0;

/// Mirror the current ring-3 → ring-0 kernel stack top (TSS `RSP0`) into the
/// location the `syscall_entry` stub loads. Call this with the same value passed
/// to `gdt::set_kernel_stack`.
///
/// # Safety / invariants
/// Single-task, init-time/per-task-spawn use only; written with interrupts
/// effectively quiescent for the spawning window. The `syscall` stub reads it with
/// IF masked, so there is no concurrent reader.
pub fn set_syscall_kernel_stack(rsp0: u64) {
    // SAFETY: single-writer, single-task bring-up; the only reader is the
    // `syscall_entry` stub which runs with interrupts masked (SFMASK clears IF).
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!(SYSCALL_KERNEL_RSP), rsp0);
    }
}

// ─── int 0x80 system-call trampoline ─────────────────────────────────────────
//
// The legacy pagh-native test program invokes syscalls via `int 0x80`. `int 0x80`
// is the simpler of the two entries: the CPU automatically switches to the kernel
// stack (TSS RSP0) on the ring-3 → ring-0 transition and `iretq` cleanly returns
// to ring 3, so we avoid the manual stack switch the `syscall` path needs. The IDT
// entry for vector 0x80 is installed with DPL=3 so ring-3 code may raise it (see
// `idt::init`); that wiring is unchanged (R11.2).
//
// Widened entry (task 10.1): the stub now saves ALL 15 general-purpose registers
// into a `SavedRegs` frame (see `linux::regs::SavedRegs`) and passes a single
// pointer to that frame to `linux_dispatch`. Funnelling through one
// `*mut SavedRegs` lets the dispatcher read the full Linux argument set
// (number=rax, args=rdi,rsi,rdx,r10,r8,r9) and modify saved registers, while
// sidestepping the SysV six-register argument limit. After the call the result is
// written into the saved `rax` slot; every other GPR is restored unchanged so the
// caller observes them preserved (R1.7), then `iretq` returns to ring 3.
//
// On `int 0x80` the CPU pushes [SS, RSP, RFLAGS, CS, RIP] (5 qwords = 40 bytes,
// no error code) onto RSP0. RSP0 is 16-byte aligned, so after those 40 bytes plus
// our 15×8 = 120 bytes the stack is 160 bytes below RSP0 → 16-byte aligned exactly
// at the `call` (the SysV requirement; no extra padding needed on this path).
//
// SavedRegs layout contract: pushed first = rax = highest address = offset 112;
// pushed last = r15 = lowest address = offset 0 = where rsp/the pointer points.
// The `mov [rsp + 112], rax` therefore targets the saved `rax` slot.
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
    // rsp now points at the SavedRegs frame (r15 at offset 0). Pass it as the sole
    // (rdi) argument to linux_dispatch. rsp is 16-byte aligned here (see header).
    "    mov rdi, rsp",
    "    call linux_dispatch",
    // Write the dispatcher's return value into the saved rax slot (offset 112).
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

// ─── `syscall`-instruction fast-path entry (LStar target) ────────────────────
//
// x86_64 static Linux binaries normally use the `syscall` instruction rather than
// `int 0x80`. This stub is the `LStar` target programmed in `init()`. It funnels
// into the SAME `linux_dispatch` as `int80_stub`, building an identical
// `SavedRegs` frame, so the supported-set/validation/errno logic exists once.
//
// `syscall` semantics the stub must honor:
//   * The CPU saves user RIP into rcx and user RFLAGS into r11 (so we must keep
//     rcx/r11 intact through to `sysretq`, which reloads RIP from rcx and RFLAGS
//     from r11). They are saved in the frame and restored by the symmetric pops.
//   * SFMASK clears IF, so the syscall window runs with interrupts masked.
//   * The CPU performs NO stack switch: on entry rsp is still the USER stack. We
//     must switch to the kernel stack before touching it. We stash the user rsp in
//     a global scratch and load the mirrored kernel RSP0 (single-task model;
//     non-reentrant under the masked-IF window — see `set_syscall_kernel_stack`).
//   * CS/SS are loaded from STAR on both `syscall` (kernel selectors) and
//     `sysretq` (user selectors), consistent with the GDT layout in `gdt.rs`.
//
// Stack alignment: after switching to the (16-byte aligned) kernel stack top, the
// 15 pushes leave rsp at kernel_top-120 ≡ 8 (mod 16). We therefore `sub rsp, 8`
// to reach 16-byte alignment before `call`, and `add rsp, 8` after to recover the
// SavedRegs pointer for the `mov [rsp + 112], rax` write-back.
core::arch::global_asm!(
    ".global syscall_entry",
    "syscall_entry:",
    // Switch from the user stack to the kernel stack. rip-relative access to the
    // module-global scratch/mirror; no GS/swapgs is required.
    "    mov [rip + SYSCALL_USER_RSP], rsp",
    "    mov rsp, [rip + SYSCALL_KERNEL_RSP]",
    // Build the SavedRegs frame in the SAME order as int80_stub (rcx=user RIP and
    // r11=user RFLAGS are captured here and restored before sysretq).
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
    "    mov rdi, rsp",   // &SavedRegs (sole arg)
    "    sub rsp, 8",     // 16-byte align for the SysV call
    "    call linux_dispatch",
    "    add rsp, 8",     // recover the SavedRegs pointer
    "    mov [rsp + 112], rax",
    "    pop r15",
    "    pop r14",
    "    pop r13",
    "    pop r12",
    "    pop r11",        // user RFLAGS -> r11 (consumed by sysretq)
    "    pop r10",
    "    pop r9",
    "    pop r8",
    "    pop rbp",
    "    pop rdi",
    "    pop rsi",
    "    pop rdx",
    "    pop rcx",        // user RIP -> rcx (consumed by sysretq)
    "    pop rbx",
    "    pop rax",
    "    mov rsp, [rip + SYSCALL_USER_RSP]",  // restore user stack
    "    sysretq",
    // Single-slot scratch for the user rsp across the syscall window. Safe under
    // the current single-task model with IF masked (SFMASK) — no nesting/reentry.
    ".section .bss",
    ".align 8",
    "SYSCALL_USER_RSP: .skip 8",
    ".section .text",
);

extern "C" {
    /// Naked `int 0x80` entry stub (see the `global_asm!` above). Installed as
    /// the IDT vector-0x80 handler with DPL=3 by `idt::init`.
    pub fn int80_stub();

    /// Naked `syscall`-instruction entry stub (see the `global_asm!` above).
    /// Installed as the `LStar` MSR target by `init`.
    pub fn syscall_entry();
}

/// Legacy pagh-native syscall dispatcher (`SYS_WRITE`/`SYS_EXIT`/`SYS_YIELD`).
///
/// Retained as the boot-path compatibility target: `linux_dispatch` delegates the
/// three native numbers here so the existing ring-3 test process keeps working
/// until task 12.7 installs real Linux routing. `num` is the syscall number and
/// `a1`..`a3` are the user arguments (originally in rdi/rsi/rdx).
pub(crate) fn legacy_dispatch(num: u64, a1: u64, a2: u64, a3: u64) -> u64 {
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
