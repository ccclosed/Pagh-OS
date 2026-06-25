// arch/cpu.rs — Safe wrappers around privileged x86_64 CPU instructions.
//
// This module is the single safe boundary for privileged instructions that
// were previously written as ad-hoc inline `asm!` scattered across the tree
// (`hlt`, `cli`, `sti`, `pushfq`, `rdmsr`, `wrmsr`). The module name is
// architecture-neutral (`arch::cpu`) per the design; the implementation here
// is x86_64-specific.
//
// Design goal (Requirements 2.1, 2.2, 7.3): expose operations that cannot
// violate Rust's memory model as *safe* functions, keep operations whose
// soundness depends on caller context (`write_msr`) as `unsafe`, and document
// the soundness invariant on every remaining `unsafe` block.

/// RFLAGS interrupt-enable flag (IF) bit position.
const RFLAGS_IF: u64 = 1 << 9;

// CR0 bits relevant to FPU/SSE.
const CR0_EM: u64 = 1 << 2; // Emulation: must be CLEAR to use SSE.
const CR0_MP: u64 = 1 << 1; // Monitor coprocessor: set alongside clearing EM.

// CR4 bits relevant to SSE.
const CR4_OSFXSR: u64 = 1 << 9; // OS supports FXSAVE/FXRSTOR and SSE.
const CR4_OSXMMEXCPT: u64 = 1 << 10; // OS supports unmasked SSE exceptions (#XM).

/// Enable SSE so XMM/`movaps`-style instructions execute instead of faulting
/// with #UD.
///
/// The kernel is built for `x86_64-unknown-none`, whose default codegen
/// features include SSE/SSE2. In particular, Rust's `extern "x86-interrupt"`
/// handlers emit `movaps` to save/restore the XMM registers in their prologue.
/// Limine hands control over with SSE *not* OS-enabled (CR4.OSFXSR clear), so
/// the first such instruction raises #UD — and because the #UD handler itself
/// has a `movaps` prologue, it recurses until the stack overflows and the CPU
/// triple-faults. Enabling SSE once, before any interrupts are enabled, avoids
/// this entirely.
///
/// This must run as the very first boot step, before any code that may touch
/// XMM registers and before interrupts are enabled.
///
/// Sequence (Intel SDM Vol. 3A, §13.1.4 "Initialization of SSE Extensions"):
///   CR0.EM = 0, CR0.MP = 1, CR4.OSFXSR = 1, CR4.OSXMMEXCPT = 1.
pub fn enable_sse() {
    // SAFETY: Reading and writing CR0/CR4 with the standard SSE-enable bit
    // pattern is the architecturally defined way to turn on SSE. We only clear
    // EM and set MP/OSFXSR/OSXMMEXCPT, leaving all other control bits (paging,
    // PAE, etc. established by Limine) untouched. This touches no Rust-visible
    // memory and is performed once during early boot before interrupts.
    unsafe {
        let mut cr0: u64;
        core::arch::asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
        cr0 &= !CR0_EM;
        cr0 |= CR0_MP;
        core::arch::asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack));

        let mut cr4: u64;
        core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        cr4 |= CR4_OSFXSR | CR4_OSXMMEXCPT;
        core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack));
    }
}

/// Halt the CPU forever in a low-power wait loop.
///
/// This never returns. It is the canonical "we are done / fatal error" sink
/// for the boot path and panic handler.
pub fn halt_loop() -> ! {
    loop {
        // SAFETY: `hlt` only places the CPU in a halt state until the next
        // interrupt. It has no effect on memory and cannot violate Rust's
        // memory model. We never leave this loop, so control flow is sound.
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack));
        }
    }
}

/// Halt the CPU once until the next interrupt arrives (`hlt`).
///
/// Unlike [`halt_loop`], this executes a *single* `hlt` and returns after the
/// CPU wakes (e.g. from a timer or device IRQ). It is the one-shot primitive
/// for IRQ-wait spin loops (the shell read loop, keyboard wait paths) that need
/// to sleep until an interrupt then continue, rather than halt forever.
pub fn halt() {
    // SAFETY: `hlt` only places the CPU in a halt state until the next
    // interrupt. It accesses no memory and cannot violate Rust's memory model.
    unsafe {
        core::arch::asm!("hlt", options(nomem, nostack));
    }
}

/// Enable maskable interrupts on the current CPU (`sti`).
///
/// Exposed as safe per the design: `sti` only changes interrupt *delivery*;
/// it does not read or write memory and cannot by itself violate Rust's
/// memory model. Callers relying on interrupts being disabled for mutual
/// exclusion should use [`without_interrupts`] instead.
pub fn enable_interrupts() {
    // SAFETY: `sti` sets RFLAGS.IF to enable interrupt delivery. It touches no
    // memory and does not invalidate any Rust invariant.
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack));
    }
}

/// Disable maskable interrupts on the current CPU (`cli`).
///
/// Exposed as safe per the design: `cli` only changes interrupt delivery and
/// cannot violate Rust's memory model.
pub fn disable_interrupts() {
    // SAFETY: `cli` clears RFLAGS.IF to mask interrupt delivery. It touches no
    // memory and does not invalidate any Rust invariant.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }
}

/// Returns `true` if maskable interrupts are currently enabled (RFLAGS.IF set).
pub fn interrupts_enabled() -> bool {
    let flags: u64;
    // SAFETY: `pushfq` pushes RFLAGS and we immediately pop it into a register.
    // This has no observable side effects and preserves all flags; the stack is
    // balanced by the matching push/pop pair.
    unsafe {
        core::arch::asm!(
            "pushfq",
            "pop {}",
            out(reg) flags,
            options(preserves_flags),
        );
    }
    (flags & RFLAGS_IF) != 0
}

/// Run `f` with interrupts disabled, restoring the prior interrupt state after.
///
/// Reads the current IF, disables interrupts, runs `f`, then re-enables
/// interrupts only if they were enabled on entry. This is the single primitive
/// other modules (e.g. `sync::spinlock`) build interrupt-safe critical sections
/// on top of.
///
/// Note: `panic = "abort"` for this kernel, so there is no unwinding path to
/// worry about; the restore simply runs after `f` returns.
pub fn without_interrupts<R>(f: impl FnOnce() -> R) -> R {
    let were_enabled = interrupts_enabled();
    disable_interrupts();

    let result = f();

    if were_enabled {
        enable_interrupts();
    }

    result
}

/// Read a model-specific register via `rdmsr`.
///
/// Safe: reading an MSR has no effect on Rust-visible memory. Reading an
/// unimplemented MSR can `#GP`, but that is a hardware fault, not memory
/// unsafety; callers pass well-known MSR numbers.
pub fn read_msr(msr: u32) -> u64 {
    let (high, low): (u32, u32);
    // SAFETY: `rdmsr` reads the MSR named by ECX into EDX:EAX. It performs no
    // memory access and cannot violate Rust's memory model.
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((high as u64) << 32) | (low as u64)
}

/// Write a model-specific register via `wrmsr`.
///
/// Stays `unsafe`: writing an MSR can reconfigure the CPU (e.g. enabling
/// `SYSCALL`/`SYSRET`, relocating the LAPIC base, toggling features) in ways
/// whose soundness depends entirely on caller context.
///
/// # Safety
/// The caller must ensure that writing `value` to `msr` keeps the system in a
/// well-defined state — for example, that any addresses programmed into the MSR
/// are valid and that dependent subsystems expect the new configuration.
pub unsafe fn write_msr(msr: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    // SAFETY: The caller guarantees (per this fn's `# Safety` contract) that the
    // write leaves the CPU in a sound state. `wrmsr` writes EDX:EAX to the MSR
    // named by ECX and accesses no Rust-visible memory.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") low,
            in("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
}
