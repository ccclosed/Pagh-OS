// task/switch.rs — Context switch, kernel thread trampoline, timer IRQ stub
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use core::arch::asm;

/// Cooperative context switch for an *already-running* thread (the
/// `yield_current` path). Saves the current thread's GPRs + RFLAGS and restores
/// the next thread's, using the SAME GPR + RFLAGS save/restore order as the
/// preemptive `irq32_stub` (rax..r15 push / popfq + r15..rax pop). The two paths
/// therefore share a single register save/restore layout (Requirement 11.2).
///
/// Note on tail handling: this cooperative path ends in `ret` (it returns to the
/// caller of `switch_context`), whereas the preemptive path ends in `iretq`.
/// Freshly-spawned kernel threads are entered EXCLUSIVELY via the preemptive
/// iretq path (`scheduler_tick_irq` -> `irq32_stub`), whose initial frame is
/// built by `kernel_thread_spawn`; `switch_context` is only ever used to switch
/// between threads that are already mid-execution (i.e. were previously saved by
/// one of these two paths), so the differing tail is safe.
pub unsafe fn switch_context(old_rsp: &mut u64, new_rsp: u64, new_cr3: Option<u64>) {
    if let Some(cr3) = new_cr3 {
        asm!("mov cr3, {}", in(reg) cr3, options(nostack));
    }
    asm!(
        "push rax", "push rbx", "push rcx", "push rdx",
        "push rsi", "push rdi", "push rbp",
        "push r8", "push r9", "push r10", "push r11",
        "push r12", "push r13", "push r14", "push r15",
        "pushfq",
        "mov [{}], rsp", "mov rsp, {}",
        "popfq",
        "pop r15", "pop r14", "pop r13", "pop r12",
        "pop r11", "pop r10", "pop r9", "pop r8",
        "pop rbp", "pop rdi", "pop rsi", "pop rdx",
        "pop rcx", "pop rbx", "pop rax",
        in(reg) old_rsp, in(reg) new_rsp,
        options(nostack, preserves_flags),
    );
}

pub unsafe fn jump_to_user(entry: u64, user_stack: u64, code_sel: u16, data_sel: u16) -> ! {
    asm!(
        "mov ds, {data_sel:x}", "mov es, {data_sel:x}",
        "mov fs, {data_sel:x}", "mov gs, {data_sel:x}",
        "push {data_sel}", "push {user_stack}",
        "push 0x3202", "push {code_sel}", "push {entry}",
        "iretq",
        data_sel = in(reg) data_sel as u64,
        user_stack = in(reg) user_stack,
        code_sel = in(reg) code_sel as u64,
        entry = in(reg) entry,
        options(noreturn, nostack),
    );
}

// ─── Kernel thread trampoline ────────────────────────────────────────────

extern "C" {
    pub fn kernel_thread_trampoline() -> !;
    fn scheduler_exit_thread() -> !;
}

core::arch::global_asm!(
    ".global kernel_thread_trampoline",
    "kernel_thread_trampoline:",
    // Entry point arrives in RDI via the GPR restore performed by irq32_stub
    // (the initial frame built by kernel_thread_spawn places `entry` in the
    // rdi slot). `iretq` has already set RSP = stack_top, so the stack is the
    // clean top of this thread's stack — no `pop` is needed (and popping here
    // would read garbage above the frame, the original bring-up bug).
    "    sti",
    "    call rdi",          // rdi = entry; pushes return addr within the stack
    "    cli",
    "    jmp scheduler_exit_thread",
);

core::arch::global_asm!(
    ".global scheduler_exit_thread",
    "scheduler_exit_thread:",
    "    hlt",
    "    jmp scheduler_exit_thread",
);

// ─── Timer IRQ stub (preemptive context switch) ──────────────────────────

extern "C" {
    pub fn irq32_stub();
    fn scheduler_tick_irq(current_rsp: u64) -> u64;
}

core::arch::global_asm!(
    ".global irq32_stub",
    "irq32_stub:",
    // ── Preemptive context switch (canonical switch path) ────────────────
    // On a timer IRQ the CPU has already pushed the iret frame
    // [RIP, CS, RFLAGS, RSP, SS] (high→low). We then push 15 GPRs and the
    // RFLAGS word for `popfq`. The save order below and the restore (pop)
    // order further down are exact mirrors, and they match — byte for byte —
    // the initial frame `kernel_thread_spawn` constructs (Requirement 11.1 /
    // Property 7).
    //
    // Slot ↔ register correspondence (write-order 1..=15 in spawn == push
    // order here; restore pops them in reverse, r15 first … rax last):
    //   1=rax  2=rbx  3=rcx  4=rdx  5=rsi  6=rdi  7=rbp  8=r8
    //   9=r9  10=r10 11=r11 12=r12 13=r13 14=r14 15=r15
    // `kernel_thread_spawn` places the thread `entry` in the **rdi** slot
    // (the 6th written slot), so after the 15 pops below rdi == entry.
    //
    // Save all GPRs in a fixed order (restored in reverse below)
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
    "    pushfq",
    // rdi = current RSP (arg1 for scheduler_tick_irq)
    // We must save it BEFORE aligning RSP
    "    mov rdi, rsp",
    "    sub rsp, 8",        // align to 16 (pushfq made it 8-off)
    "    call scheduler_tick_irq",
    // rax = new RSP to restore
    "    mov rsp, rax",
    "    popfq",
    // Restore the 15 GPRs in reverse of the push order (r15 first … rax last).
    // For a freshly-spawned thread this leaves rdi = entry; `iretq` then sets
    // RIP = kernel_thread_trampoline and RSP = stack_top.
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
