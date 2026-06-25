// task/switch.rs — Context switch, kernel thread trampoline, timer IRQ stub
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use core::arch::asm;

/// Cooperative context switch for an *already-running* thread (the
/// `yield_current` path).
///
/// # Single saved-frame invariant (Requirement 5.1)
///
/// All three context paths — `kernel_thread_spawn` (initial frame),
/// `switch_context` (this cooperative/yield path), and `irq32_stub` (the
/// preemptive/tick path) — produce and consume **one** saved-frame layout, so a
/// task suspended by ANY path can be resumed by ANY path with its exact
/// instruction pointer and stack pointer intact (Requirements 5.2, 5.3, 5.4).
///
/// Saved frame, low → high address (identical to what the timer IRQ leaves):
/// ```text
///   [rsp+0]    RFLAGS (for popfq, IF=0 — restore tail runs with interrupts off)
///   [+8..+120] r15,r14,r13,r12,r11,r10,r9,r8,rbp,rdi,rsi,rdx,rcx,rbx,rax
///   [+128]     RIP        (resume point)
///   [+136]     CS         (kernel code selector)
///   [+144]     RFLAGS     (for iretq, IF=1 — resumed task runs with interrupts on)
///   [+152]     RSP        (stack pointer to continue on after resume)
///   [+160]     SS         (kernel data selector)
/// ```
///
/// To match `irq32_stub` (whose iretq frame is pushed by the CPU on the timer
/// interrupt), the SAVE side here *synthesizes* the same iretq frame in
/// software: RIP = a resume label inside this function, CS = kernel CS,
/// RSP = the stack pointer at function entry (so execution continues exactly as
/// if `switch_context` had returned normally), SS = kernel SS. The RESTORE side
/// is then byte-for-byte identical to `irq32_stub`'s tail:
/// `mov rsp, new_rsp; popfq; pop r15..rax; iretq`.
///
/// Unlike the previous implementation, this path does NOT end in `ret`: it ends
/// in `iretq`, consuming the full 5-word `[RIP,CS,RFLAGS,RSP,SS]` tail, so the
/// yield and tick paths are interchangeable.
pub unsafe fn switch_context(old_rsp: &mut u64, new_rsp: u64, new_cr3: Option<u64>) {
    if let Some(cr3) = new_cr3 {
        asm!("mov cr3, {}", in(reg) cr3, options(nostack));
    }

    // Kernel selectors for the synthesized iretq frame. Same values the CPU
    // pushes on a same-privilege interrupt and the same ones `kernel_thread_spawn`
    // bakes into a fresh thread's initial frame.
    let kernel_cs = crate::arch::x86_64::gdt::Selectors::kernel_code().0 as u64;
    let kernel_ss = crate::arch::x86_64::gdt::Selectors::kernel_data().0 as u64;

    asm!(
        // ── Synthesize the iretq frame (high → low): SS, RSP, RFLAGS, CS, RIP.
        // `{scratch}` first captures the entry RSP, which becomes the iretq RSP
        // slot so that after resume RSP == entry RSP and the Rust epilogue/`ret`
        // returns to `yield_current` normally.
        "mov {scratch}, rsp",
        "push {kss}",            // [+160] SS
        "push {scratch}",        // [+152] RSP  = entry RSP
        // IRET-RFLAGS capture: this runs with IF=1 (the cooperative
        // `yield_current` caller has interrupts enabled), so the iret-frame
        // RFLAGS slot keeps IF=1 and the *resumed* task runs with interrupts
        // enabled after `iretq`. This pushfq MUST stay BEFORE the `cli` below.
        "pushfq",                // [+144] RFLAGS (for iretq), IF=1
        // popfq-slot IF=0 invariant (mirrors irq32_stub): clear IF *after* the
        // iret-RFLAGS capture but *before* the GPR pushes and the final pushfq.
        // Consequently the [+0] popfq-slot RFLAGS is saved with IF=0, so the
        // restore tail (`popfq; pop r15..rax; iretq`) of ANY path resuming a
        // frame we save runs with interrupts OFF until `iretq` — no timer IRQ
        // can arrive mid-restore and corrupt the frame/stack. It also makes
        // this switch's own critical region (synthesize-frame → save RSP →
        // load new RSP → restore) atomic. No `sti` is needed: `iretq` restores
        // IF from the iret-frame RFLAGS slot captured above.
        "cli",
        "push {kcs}",            // [+136] CS
        "lea {scratch}, [rip + 2f]",
        "push {scratch}",        // [+128] RIP  = resume label below
        // ── 15 GPRs, rax first … r15 last (mirror of the restore pops) ──────
        "push rax", "push rbx", "push rcx", "push rdx",
        "push rsi", "push rdi", "push rbp",
        "push r8", "push r9", "push r10", "push r11",
        "push r12", "push r13", "push r14", "push r15",
        "pushfq",                // [+0] RFLAGS (for popfq, IF=0) = lowest = saved RSP
        // ── Save this task's RSP, switch to the next task's RSP ─────────────
        "mov [{old}], rsp",
        "mov rsp, {new}",
        // ── Restore: identical to irq32_stub's tail ─────────────────────────
        "popfq",
        "pop r15", "pop r14", "pop r13", "pop r12",
        "pop r11", "pop r10", "pop r9", "pop r8",
        "pop rbp", "pop rdi", "pop rsi", "pop rdx",
        "pop rcx", "pop rbx", "pop rax",
        "iretq",
        // Resume point: a task saved by this (or any) path lands here via iretq
        // with RSP restored to its entry value; control then leaves the asm
        // block and the function returns to its caller.
        "2:",
        old = in(reg) old_rsp,
        new = in(reg) new_rsp,
        kcs = in(reg) kernel_cs,
        kss = in(reg) kernel_ss,
        scratch = out(reg) _,
    );
}

// ─── Kernel thread trampoline ────────────────────────────────────────────

extern "C" {
    pub fn kernel_thread_trampoline() -> !;
    // retained: not called from Rust — referenced by name from the
    // `kernel_thread_trampoline` global_asm block (`jmp scheduler_exit_thread`).
    // The extern decl keeps the symbol in scope for the inline asm linkage.
    #[allow(dead_code)]
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
    // retained: not called from Rust — invoked from the `irq32_stub` global_asm
    // block (`call scheduler_tick_irq`) which computes the next task's RSP.
    // The extern decl keeps the symbol in scope for the inline asm linkage.
    #[allow(dead_code)]
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
