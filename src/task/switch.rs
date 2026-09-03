// task/switch.rs — Context switch, kernel thread trampoline, timer IRQ stub
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use core::arch::asm;

/// Cooperative context switch for an *already-running* thread (the
/// `yield_current` path).
///
/// # Single saved-frame invariant (Requirement 5.1)
///
/// All three context paths — `kernel_thread_spawn` (initial frame),
/// `yield_switch` (this cooperative/yield path), and `irq32_stub` (the
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
///   [+144]     RFLAGS     (for iretq — resumed task keeps the caller's IF state)
///   [+152]     RSP        (ALL frames: long-mode iretq always pops SS:RSP)
///   [+160]     SS         (ALL frames: long-mode iretq always pops SS:RSP)
/// ```
///
/// # Requeue-before-restore (the stage-13.6 fix)
///
/// This path now mirrors `irq32_stub` *structurally*, not just in frame
/// layout: after saving the frame it CALLS into the scheduler
/// (`scheduler_yield_switch`) with the saved RSP; the scheduler requeues the
/// yielding task *before* the stack switch, exactly like `scheduler_tick_irq`
/// on the preemptive path, then returns the next task's saved RSP for the
/// shared restore tail.
///
/// The previous implementation (`switch_context`) requeued the yielding task
/// in Rust code placed *after* the inline-asm stack switch. That code only
/// runs once the task has already been rescheduled — which could never
/// happen, because the task was never enqueued (its saved RSP lived only in a
/// local on its own, now-suspended stack). The first cooperative yield that
/// found the ready queue non-empty therefore silently dropped the yielding
/// task from rotation forever (the clone-thread FUTEX_WAIT hang).
///
/// # Long-mode IRETQ always pops five words (the stage-13.6 boot fix)
///
/// In 64-bit mode `iretq` unconditionally consumes RIP, CS, RFLAGS, RSP and
/// SS — even when CS stays ring0 (no privilege change); likewise interrupt
/// delivery always pushes all five. Synthetic ring0 frames must therefore
/// include the RSP/SS words: a 3-word frame makes the restoring `iretq` read
/// past the top of the kernel stack into the unmapped guard area (page fault
/// at [stack_top] on the task's first restore). An earlier comment here
/// claimed the opposite ("same-privilege iretq consumes only 3 words") — that
/// conclusion was wrong; the preemptive tick path only works because the CPU
/// itself pushes the full 5-word frame on EVERY interrupt in long mode.
pub unsafe fn yield_switch() {
    // Kernel selectors for the synthesized iretq frame. Same values the CPU
    // pushes on a same-privilege interrupt and the same ones
    // `kernel_thread_spawn` bakes into a fresh thread's initial frame.
    let kernel_cs = crate::arch::x86_64::gdt::Selectors::kernel_code().0 as u64;
    let kernel_ss = crate::arch::x86_64::gdt::Selectors::kernel_data().0 as u64;

    asm!(
        // Long-mode iretq ALWAYS pops SS:RSP (even ring0 -> ring0), so the
        // synthesized frame must include them. rax is safe as scratch here
        // and below: clobber_abi("C") declares it clobbered. Neither lea nor
        // push touches RFLAGS, so the pushfq below still captures the
        // caller's flags.
        "push r11",              // [+160] SS  (kernel data selector, pinned in("r11"))
        "lea rax, [rsp + 8]",    // caller's RSP at entry (above the SS push)
        "push rax",              // [+152] RSP (iretq restores the caller stack)
        // IRET-RFLAGS capture: records the caller's IF state (kernel threads
        // yield with IF=1; Linux syscall handlers may yield with IF in either
        // state), so `iretq` resumes the task with its own interrupt state.
        // This pushfq MUST stay BEFORE the `cli` below.
        "pushfq",                // [+144] RFLAGS (for iretq)
        // popfq-slot IF=0 invariant (mirrors irq32_stub): clear IF *after* the
        // iret-RFLAGS capture but *before* the GPR pushes and the final
        // pushfq, so the restore tail of ANY path resuming this frame runs
        // with interrupts OFF until `iretq`. It also makes the whole
        // save → scheduler → restore critical region atomic on this
        // single-CPU design. No `sti` is needed: `iretq` restores IF from the
        // iret-frame RFLAGS slot captured above.
        "cli",
        "push r10",              // [+136] CS  (kernel code selector, pinned in("r10"))
        // rax is free as scratch here: clobber_abi("C") below already declares
        // it clobbered (a generic `out(reg) _` operand is not allowed together
        // with clobber_abi). The GPR push below then stores the label address
        // in the frame's rax slot, which is fine for the same reason.
        "lea rax, [rip + 2f]",
        "push rax",              // [+128] RIP  = resume label below
        // ── 15 GPRs, rax first … r15 last (mirror of the restore pops) ──────
        "push rax", "push rbx", "push rcx", "push rdx",
        "push rsi", "push rdi", "push rbp",
        "push r8", "push r9", "push r10", "push r11",
        "push r12", "push r13", "push r14", "push r15",
        "pushfq",                // [+0] RFLAGS (for popfq, IF=0) = lowest = saved RSP
        // ── Requeue-before-restore: hand the saved frame to the scheduler ───
        // scheduler_yield_switch(current_rsp) requeues this task, picks the
        // next one (activating its RSP0/FS-base and reloading CR3), and
        // returns its saved RSP — or returns `current_rsp` unchanged when
        // nothing else is ready (the yield is then a no-op restore of our own
        // frame).
        "mov rdi, rsp",
        "and rsp, -16",          // SysV 16-byte alignment for the call
        "call {switch_fn}",
        "mov rsp, rax",
        // ── Restore: identical to irq32_stub's tail ─────────────────────
        "popfq",
        "pop r15", "pop r14", "pop r13", "pop r12",
        "pop r11", "pop r10", "pop r9", "pop r8",
        "pop rbp", "pop rdi", "pop rsi", "pop rdx",
        "pop rcx", "pop rbx", "pop rax",
        "iretq",
        // Resume point: a task saved by this (or any) path lands here via
        // iretq with RSP restored to its entry value; control then leaves the
        // asm block and the function returns to its caller.
        "2:",
        // Foreground-wait #GP fix: these MUST be pinned
        // registers. With a generic `in(reg)` the allocator may pick RAX —
        // clobber_abi("C") does NOT exclude ABI-clobbered registers from
        // operand allocation, it merely skips adding clobbers for registers
        // already used by operands. When kcs landed in RAX, the
        // `lea rax, [rsp + 8]` scratch above overwrote it before its push:
        // the CS slot received a copy of the caller-RSP and the restoring
        // `iretq` faulted with #GP(err = rsp & ~3). R10/R11 are never
        // touched by this asm before their pushes. The GPR block later
        // stores the selector values in the r10/r11 frame slots — fine:
        // both are caller-saved and already declared clobbered.
        in("r10") kernel_cs,
        in("r11") kernel_ss,
        switch_fn = sym crate::task::scheduler::scheduler_yield_switch,
        clobber_abi("C"),
    );
}

// ─── Kernel thread trampoline ────────────────────────────────────────────

extern "C" {
    pub fn kernel_thread_trampoline() -> !;
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
    "    call rdi", // rdi = entry; pushes return addr within the stack
    "    jmp scheduler_exit_thread",
);

/// A finished kernel thread parks here after its entry point returns
/// (`kernel_thread_trampoline` jumps here by name). Marking the pid exiting
/// lets the next timer tick drop it from rotation instead of requeueing it —
/// the old `sti; hlt; jmp` spin requeued the dead frame every round forever.
#[no_mangle]
pub extern "C" fn scheduler_exit_thread() -> ! {
    crate::task::scheduler::kernel_thread_finished();
    // Park WITH INTERRUPTS ENABLED. The old `cli` + `hlt` sequence halted the
    // CPU with interrupts masked the moment any kernel thread returned (first
    // hit by the boot provisioner finishing/giving up) -- on a single core that
    // froze the entire machine: no timer, no scheduler, no shell. With `sti;
    // hlt` the timer keeps firing, and once the tick drops this task it is
    // never restored again.
    loop {
        crate::arch::cpu::enable_interrupts();
        crate::arch::cpu::halt();
    }
}

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
    "    sub rsp, 8", // align to 16 (pushfq made it 8-off)
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
