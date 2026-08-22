use crate::memory::{pmm, vmm};
use crate::sync::spinlock::Spinlock;
use crate::task::compat::CompatState;
use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::structures::paging::PageTableFlags;

/// Task control block, reduced to exactly the state the RSP-based context
/// switch restores from (Requirement 11.3). The vestigial `rip`/`rflags`/
/// `regs`/signal fields were removed: the entry point and initial register
/// values are encoded in the kernel stack frame `kernel_thread_spawn` builds,
/// not stored here.
///
/// A `Tcb` additionally carries an optional [`CompatState`] (task 11.2). It is
/// `None` for the existing pagh-native tasks (e.g. `spawn_test_user_process`)
/// so they are unaffected, and `Some` only once `run_linux_binary` (task 13.3)
/// populates the Linux-compatibility state for a `Compat_Process`. Because
/// `CompatState` owns heap-backed tables (`FdTable`/`VmRegionSet`/`BTreeSet`),
/// the `Tcb` is no longer `Copy`/`Clone`/`Debug`; it is moved through the
/// scheduler queues by value, which the existing call sites already do.
pub struct Tcb {
    pub pid: u64,
    /// The only state the switch restores from: the saved kernel stack pointer.
    pub kernel_rsp: u64,
    /// Physical address of this task's PML4 (reloaded into CR3 on switch).
    pub cr3: u64,
    /// Linux-compatibility state for a `Compat_Process`; `None` for native tasks.
    /// Read once `run_linux_binary` (task 13.3) populates it.
    #[allow(dead_code)]
    pub compat: Option<CompatState>,
}

impl Tcb {
    /// Construct a ready native task with no compat state. The entry point is
    /// not stored in the `Tcb`; it is baked into the constructed kernel stack
    /// frame pointed to by `kernel_rsp` (see `kernel_thread_spawn`).
    pub fn new(pid: u64, kernel_rsp: u64, cr3: u64) -> Self {
        Tcb {
            pid,
            kernel_rsp,
            cr3,
            compat: None,
        }
    }
}

static READY_QUEUE: Spinlock<VecDeque<Tcb>> = Spinlock::new(VecDeque::new());

// Frame ledger - catches stale/double restores at the moment
// they happen instead of at the fatal iretq (the apt #GP: iretq consumed a
// region of pid 1's stack that no longer held a saved frame). Every save
// stamps (pid -> rsp, live); every restore must find a matching live stamp
// and consumes it.
static FRAME_LEDGER: Spinlock<alloc::collections::BTreeMap<u64, (u64, bool)>> =
    Spinlock::new(alloc::collections::BTreeMap::new());
static KFRAME_ERRS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn stamp_save(pid: u64, rsp: u64) {
    FRAME_LEDGER.lock().insert(pid, (rsp, true));
}

fn stamp_restore(pid: u64, rsp: u64) {
    let prev = {
        let mut led = FRAME_LEDGER.lock();
        let prev = led.get(&pid).copied();
        led.insert(pid, (rsp, false));
        prev
    };
    if let Some((saved, live)) = prev {
        if saved != rsp {
            crate::error!("[SCHED] STALE RESTORE pid={} rsp=0x{:x} but last saved rsp=0x{:x} (live={})", pid, rsp, saved, live);
        } else if !live {
            crate::error!("[SCHED] DOUBLE RESTORE pid={} rsp=0x{:x} (frame already consumed once)", pid, rsp);
        }
    }
}
static NEXT_PID: AtomicU64 = AtomicU64::new(1);
static TICK_COUNT: AtomicU64 = AtomicU64::new(0);
static CURRENT_PID: Spinlock<u64> = Spinlock::new(0);

/// Sentinel meaning "no task is exiting" for [`EXITING_PID`]. `u64::MAX` is
/// never a real pid (pids start at 1 and the idle task is `IDLE_PID == 0`).
const NO_EXITING_PID: u64 = u64::MAX;

/// PID of the task that has requested exit via [`exit_current`], or
/// [`NO_EXITING_PID`] when none.
///
/// The scheduler stores the running task only as `CURRENT_PID` and rebuilds its
/// `Tcb` from `current_rsp` on each tick, so "killing" a task is expressed as a
/// single pid the tick handler must NOT requeue. When the timer tick is about
/// to requeue the current task, it checks this flag: if it matches, the task is
/// dropped from rotation (never scheduled again) and the flag is cleared
/// (Requirement 12.4).
static EXITING_PID: AtomicU64 = AtomicU64::new(NO_EXITING_PID);

/// PID reserved for the idle task.
///
/// The idle task is the boot/main thread: it runs `kernel_main` and ends in a
/// halt loop, and is always runnable. The scheduler treats it as an explicit
/// task (Requirement 11.4) rather than scattering `CURRENT_PID == 0` checks
/// through the tick handler. It is scheduled whenever the ready queue is empty.
pub const IDLE_PID: u64 = 0;

/// The idle task, represented explicitly as a real `Tcb`. Only `kernel_rsp` is
/// updated at runtime (saved whenever the idle task is preempted by the timer
/// tick); `pid`/`cr3` are fixed. Accessed only through the helpers below so the
/// "idle task" concept has a single owner.
static IDLE_TASK: Spinlock<Tcb> = Spinlock::new(Tcb {
    pid: IDLE_PID,
    kernel_rsp: 0,
    cr3: 0,
    compat: None,
});

/// Returns true when `pid` is the idle task.
#[inline]
pub fn is_idle(pid: u64) -> bool {
    pid == IDLE_PID
}

/// Save the idle task's stack pointer (called when the idle task is preempted).
#[inline]
fn save_idle_rsp(rsp: u64) { stamp_save(IDLE_PID, rsp); IDLE_TASK.lock().kernel_rsp = rsp; }

/// The idle task's saved stack pointer (scheduled when nothing else is ready).
#[inline]
fn idle_rsp() -> u64 {
    IDLE_TASK.lock().kernel_rsp
}

pub fn init() {
    crate::debug!("Scheduler initialized (Round Robin)");
}
pub fn tick() {
    TICK_COUNT.fetch_add(1, Ordering::Relaxed);
}
pub fn ticks() -> u64 {
    TICK_COUNT.load(Ordering::Relaxed)
}

/// Block the calling thread for approximately `n` timer ticks, halting between
/// ticks instead of busy-spinning so other tasks (and the CPU) are not starved.
///
/// At the 100 Hz LAPIC timer this is ~`n * 10 ms`. Interrupts MUST be enabled
/// on the caller (they are on the shell thread); otherwise the tick count never
/// advances and this would block forever. The wait tolerates the (astronomical)
/// tick-counter wraparound via `wrapping`/elapsed comparison.
pub fn sleep_ticks(n: u64) {
    let start = ticks();
    while ticks().wrapping_sub(start) < n {
        // `hlt` with IF masked sleeps forever (only an NMI would wake it),
        // and the Linux syscall window ENTERS with IF cleared by SFMASK /
        // the interrupt gate. Force interrupts on before halting — the same
        // pattern `exit_current` uses. Being preempted here is fine.
        crate::arch::cpu::enable_interrupts();
        crate::arch::cpu::halt();
    }
}
/// Debug tripwire: validate that a saved context frame at `rsp` looks like
/// the canonical 21-word layout ([+128] RIP canonical, [+136] CS plausible,
/// [+144] RFLAGS has the always-one bit) and dump it if not. Called both when
/// a frame is enqueued and right before it is restored, so frame corruption
/// is caught at the moment it happens instead of at the fatal `iretq`.
pub fn check_frame(who: &str, pid: u64, rsp: u64) {
    if rsp == 0 {
        return;
    }
    // The tripwire must never fault on its own input: a corrupted frame can
    // carry an arbitrary rsp, and selftests enqueue synthetic Tcbs with
    // sentinel stack pointers (0x8000/0xDEAD). Only probe frames that live in
    // the canonical higher half; anything else is reported as-is.
    if (rsp as i64) >= 0 || rsp < 0xffff_8000_0000_0000 {
        crate::warn!(
            "[SCHED] BAD FRAME ({}) pid={} rsp=0x{:x}: non-canonical or user-space stack pointer, not probed",
            who,
            pid,
            rsp
        );
        return;
    }
    // Kernel threads (shell, idle, etc.) use a kernel-mode yield frame
    // whose layout differs from the ring-3 interrupt frame: there is no
    // user CS/SS/RSP triple at [+136..+160], so the value at [+136] is
    // just a kernel RSP, not a segment selector. Validating it against
    // ring-3 invariants always fires a false BAD FRAME and then the
    // iretq blows up on that RSP-as-CS (GP#err=rsp&~3). Only run the
    // check for tasks that actually have a Linux compat state.
    let is_compat = crate::task::compat::compat_exists(pid);
    let rd = |off: u64| unsafe { core::ptr::read_volatile((rsp + off) as *const u64) };
    let rip = rd(128);
    let cs = rd(136);
    let rf = rd(144);
    let rip_canonical = (((rip as i64) << 16) >> 16) as u64 == rip;
    let rf_ok = rf & 0x2 != 0;
    // Kernel threads must ALWAYS carry the kernel code selector at
    // [+136] and a higher-half RIP; a stack-pointer value in the CS slot is
    // exactly the apt iretq #GP signature (CS=0x...081238 -> #GP err=0x1238).
    let kcs = crate::arch::x86_64::gdt::Selectors::kernel_code().0 as u64;
    // A ring-3 frame (CS with RPL=3, plausible selector) is valid
    // even for tasks WITHOUT a Linux compat state -- the built-in ring-3 test
    // process (pid 3) is spawned raw, and 16.5.1 only whitelisted compat tasks,
    // so its perfectly healthy frame spammed BAD FRAME on every tick. The real
    // tripwire (a stack-pointer value in the CS slot) still fires: such values
    // are huge and fail cs < 0x40.
    let ring3 = cs & 3 == 3 && cs != 0 && cs < 0x40;
    let cs_ok = if is_compat { cs != 0 && cs < 0x40 } else { cs == kcs || ring3 };
    let rip_ok = if is_compat || ring3 { rip_canonical } else { rip_canonical && rip >= 0xffff_8000_0000_0000 };
    if !(rip_ok && cs_ok && rf_ok) {
        if !is_compat && KFRAME_ERRS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) >= 8 { return }
        crate::error!("[SCHED] BAD FRAME ({}) pid={} rsp=0x{:x} rip=0x{:x} cs=0x{:x} rflags=0x{:x}", who, pid, rsp, rip, cs, rf);
        let mut i = 0u64;
        while i < 21 {
            crate::error!("  frame[+0x{:02x}] 0x{:016x}", i * 8, rd(i * 8));
            i += 1;
        }
    }
}

pub fn spawn(tcb: Tcb) -> u64 {
    let pid = tcb.pid;
    check_frame("spawn", pid, tcb.kernel_rsp);
    stamp_save(pid, tcb.kernel_rsp);
    let mut q = READY_QUEUE.lock();
    if q.iter().any(|t| t.pid == pid) {
        crate::error!("[SCHED] DOUBLE ENQUEUE (spawn) pid={} rsp=0x{:x}", pid, tcb.kernel_rsp);
    }
    q.push_back(tcb);
    pid
}
pub fn schedule() -> Option<Tcb> { READY_QUEUE.lock().pop_front() }
pub fn requeue(tcb: Tcb) {
    check_frame("enqueue", tcb.pid, tcb.kernel_rsp);
    stamp_save(tcb.pid, tcb.kernel_rsp);
    let mut q = READY_QUEUE.lock();
    if q.iter().any(|t| t.pid == tcb.pid) {
        crate::error!(
            "[SCHED] DOUBLE ENQUEUE pid={} rsp=0x{:x}",
            tcb.pid,
            tcb.kernel_rsp
        );
    }
    q.push_back(tcb);
}
pub fn remove_ready_pids(pids: &[u64]) {
    READY_QUEUE.lock().retain(|t| !pids.contains(&t.pid));
}
pub fn next_pid() -> u64 {
    NEXT_PID.fetch_add(1, Ordering::Relaxed)
}
pub fn set_current_pid(pid: u64) {
    *CURRENT_PID.lock() = pid;
}
pub fn current_pid() -> u64 {
    *CURRENT_PID.lock()
}
fn activate_task(pid: u64) {
    if is_idle(pid) {
        return;
    }
    let top = crate::memory::layout::kernel_stack_for_pid(pid).2;
    crate::arch::x86_64::gdt::set_kernel_stack(top);
    crate::arch::x86_64::syscall::set_syscall_kernel_stack(top);
    if let Some(base) = crate::task::compat::fs_base_for(pid) {
        x86_64::registers::model_specific::FsBase::write(x86_64::VirtAddr::new(base));
    }
}

pub fn kernel_thread_spawn(entry: fn()) -> u64 {
    let pid = next_pid();
    let (_guard_base, stack_base, stack_top) = crate::memory::layout::kernel_stack_for_pid(pid);

    for page in 0..crate::memory::layout::KERNEL_STACK_PAGES {
        let vaddr = stack_base + page * crate::memory::layout::PAGE_SIZE;
        let frame = pmm::alloc_frame().expect("SCHED: PMM OOM");
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
        vmm::map(frame, vaddr, flags).expect("SCHED: VMM map fail");
    }

    let kernel_cs = crate::arch::x86_64::gdt::Selectors::kernel_code().0 as u64;
    let kernel_ss = crate::arch::x86_64::gdt::Selectors::kernel_data().0 as u64;

    unsafe {
        // Build the initial kernel-thread frame so its byte layout matches
        // EXACTLY the order in which `irq32_stub`/`scheduler_tick_irq` restore
        // registers (Requirement 11.1 / Property 7). The preemptive restore path
        // does, given the new RSP:
        //     mov rsp, new_rsp
        //     popfq                       ; consume the RFLAGS-for-popfq word
        //     pop r15; pop r14; ...; pop rax   ; 15 GPRs (r15 first, rax last)
        //     iretq                       ; RIP, CS, RFLAGS, RSP, SS (long mode: always 5)
        //
        // We construct the frame from the HIGHEST address downward, so the
        // final (lowest) RSP is what the restore path begins popping from.
        // The register `entry` is placed in the *rdi* slot: after the 15 GPR
        // pops, rdi == entry, and `iretq` sets RSP = stack_top. The trampoline
        // then runs with rdi = entry and a clean stack and simply `call rdi`
        // (no pop). See `kernel_thread_trampoline` in switch.rs.
        //
        // Frame layout (low address = final kernel_rsp → high address):
        //   [kernel_rsp+0]   RFLAGS-for-popfq
        //   [+8]   r15   [+16]  r14   [+24]  r13   [+32]  r12   [+40]  r11
        //   [+48]  r10   [+56]  r9    [+64]  r8    [+72]  rbp   [+80]  rdi = entry
        //   [+88]  rsi   [+96]  rdx   [+104] rcx   [+112] rbx   [+120] rax
        //   [+128] RIP = trampoline  [+136] CS  [+144] RFLAGS  [+152] RSP  [+160] SS
        let mut rsp = stack_top;

        // Long-mode IRETQ ALWAYS pops five words — RIP, CS, RFLAGS, RSP, SS —
        // even on a same-privilege (ring0 -> ring0) return. A synthetic ring0
        // frame must therefore provide the RSP/SS words too; a 3-word frame
        // made iretq read past stack_top into the unmapped guard area and
        // page-fault on the thread's very first restore.
        rsp -= 8;
        (rsp as *mut u64).write(kernel_ss); // [+160] SS
        rsp -= 8;
        (rsp as *mut u64).write(stack_top); // [+152] RSP after iretq
        rsp -= 8;
        (rsp as *mut u64).write(0x202u64); // [+144] RFLAGS (IF set)
        rsp -= 8;
        (rsp as *mut u64).write(kernel_cs); // [+136] CS

        let trampoline = crate::task::switch::kernel_thread_trampoline as *const () as u64;
        rsp -= 8;
        (rsp as *mut u64).write(trampoline); // [+128] RIP -> trampoline

        // ── 15 GPR slots, written high→low to match the pop order ─────────
        // High→low addresses correspond to: rax (highest, popped last) down to
        // r15 (lowest, popped first). `entry` goes in the rdi slot.
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+120] rax
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+112] rbx
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+104] rcx
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+96]  rdx
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+88]  rsi
        rsp -= 8;
        (rsp as *mut u64).write(entry as u64); // [+80]  rdi = entry
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+72]  rbp
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+64]  r8
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+56]  r9
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+48]  r10
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+40]  r11
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+32]  r12
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+24]  r13
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+16]  r14
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+8]   r15

        // ── RFLAGS word consumed by `popfq` (lowest address = final rsp) ──
        rsp -= 8;
        (rsp as *mut u64).write(0x002u64); // [+0] RFLAGS-for-popfq (IF=0 invariant)

        let tcb = Tcb {
            pid,
            kernel_rsp: rsp,
            cr3: vmm::current_pml4_phys(),
            compat: None,
        };
        spawn(tcb);
        pid
    }
}

#[no_mangle]
pub extern "C" fn scheduler_tick_irq(current_rsp: u64) -> u64 {
    let tick = TICK_COUNT.fetch_add(1, Ordering::Relaxed);

    if tick % 100 == 0 {
        crate::trace!("Tick {} RSP=0x{:x}", tick, current_rsp);
    }

    let cur = current_pid();

    if (current_rsp & (1 << 47)) == 0 {
        crate::error!("[SCHED] RSP in user space! RSP=0x{:x}", current_rsp);
        crate::arch::cpu::halt_loop();
    }

    if is_idle(cur) {
        // The idle task was preempted: save its stack pointer in the explicit
        // idle task rather than treating pid 0 as a magic special case.
        save_idle_rsp(current_rsp);
    } else if EXITING_PID.load(Ordering::Acquire) == cur {
        // The current task called `exit_current` (Requirement 12.4): do NOT
        // requeue it, so it is dropped from rotation and never scheduled again.
        // Clear the flag now that the exit has been honoured; its kernel stack
        // is simply abandoned (no reaping in this minimal design).
        EXITING_PID.store(NO_EXITING_PID, Ordering::Release);
        crate::trace!("[SCHED] task {} exited", cur);
    } else {
        requeue(Tcb {
            pid: cur,
            kernel_rsp: current_rsp,
            cr3: vmm::current_pml4_phys(),
            compat: None,
        });
    }

    crate::arch::x86_64::apic::send_eoi();

    let next = match schedule() {
        Some(tcb) => tcb,
        None => {
            // Nothing ready: run the idle task.
            set_current_pid(IDLE_PID);
            let rsp = idle_rsp();
            check_frame("restore-idle", IDLE_PID, rsp);
            stamp_restore(IDLE_PID, rsp);
            return rsp;
        }
    };

    set_current_pid(next.pid);
    activate_task(next.pid);

    // Single centralized CR3 reload for the preemptive path (Requirement 11.5).
    // Delegates to `vmm::load_cr3`, the ONE place that writes CR3 on a switch.
    // The reload doubles as a TLB flush so the next task's stack pages are
    // reloaded. No other site in this path touches CR3.
    unsafe {
        vmm::load_cr3(next.cr3);
    }

    check_frame("restore-tick", next.pid, next.kernel_rsp);
    stamp_restore(next.pid, next.kernel_rsp);
    next.kernel_rsp
}

/// Cooperatively yield the CPU to the next ready task (if any).
///
/// The frame save/restore lives in `switch::yield_switch`; the queue rotation
/// lives in [`scheduler_yield_switch`]. See both for the critical
/// requeue-before-restore ordering (the stage-13.6 fix).
pub fn yield_current() {
    // Every in-kernel blocking loop funnels through here, so the
    // yielding tasks themselves drive the stuck-syscall watchdog scan.
    crate::arch::x86_64::linux::watchdog_tick();
    // SAFETY: yield_switch saves this task's full context in the canonical
    // saved-frame layout, requeues it via `scheduler_yield_switch`, and only
    // then switches stacks; the task is resumed later by any restore path.
    unsafe {
        crate::task::switch::yield_switch();
    }
}

/// Scheduler half of the cooperative yield. Called from the `yield_switch`
/// asm with interrupts masked; `current_rsp` is the caller's freshly saved
/// frame (canonical layout, lowest word = popfq RFLAGS).
///
/// CRITICAL ORDERING (the stage-13.6 hang): the yielding task must be
/// requeued HERE — between the frame save and the stack switch — because any
/// code placed after the switch only runs once the task has already been
/// rescheduled, which can never happen if it was never enqueued.
///
/// Returns the saved RSP of the next task to run, or `current_rsp` unchanged
/// when the ready queue is empty (the caller's own frame is then restored and
/// the yield is a no-op).
pub extern "C" fn scheduler_yield_switch(current_rsp: u64) -> u64 {
    let cur = current_pid();

    let next = match schedule() {
        Some(tcb) => tcb,
        // Nothing else ready: resume the caller's own frame.
        None => return current_rsp,
    };

    // Requeue the yielding task BEFORE the stack switch (see doc above). The
    // idle task is never queued; it parks its frame in the dedicated slot,
    // mirroring the preemptive path.
    if is_idle(cur) {
        save_idle_rsp(current_rsp);
    } else {
        requeue(Tcb {
            pid: cur,
            kernel_rsp: current_rsp,
            cr3: vmm::current_pml4_phys(),
            compat: None,
        });
    }

    set_current_pid(next.pid);
    activate_task(next.pid);

    // Centralized CR3 reload for the cooperative path (Requirement 11.5): the
    // cooperative yield reloads CR3 through the same `vmm::load_cr3` helper the
    // preemptive tick uses, so CR3 is written in exactly one place. CR3 is not
    // rewritten anywhere else in this path.
    unsafe {
        vmm::load_cr3(next.cr3);
    }

    check_frame("restore-yield", next.pid, next.kernel_rsp);
    stamp_restore(next.pid, next.kernel_rsp);
    next.kernel_rsp
}

/// Terminate the calling task and yield to the scheduler forever.
///
/// Requirement 12.4: `SYS_EXIT` must end the *calling task* while the scheduler
/// keeps running other tasks — it must NOT halt the whole CPU. Given the
/// RSP-based scheduler (which keeps no persistent `Tcb` for the running task),
/// the minimal robust mechanism is:
///
///   1. Record the current pid in [`EXITING_PID`].
///   2. Spin in a halt loop with interrupts **enabled** so the periodic timer
///      tick can preempt us.
///   3. On the next tick, `scheduler_tick_irq` sees `cur == EXITING_PID`, drops
///      the task instead of requeuing it, clears the flag, and switches to the
///      next ready task. Because this task is never requeued, control never
///      returns here — hence the `-> !` return type.
///
/// Interrupts MUST stay enabled in the loop, otherwise the timer could never
/// fire and the task (and CPU) would deadlock.
///
/// The idle task (`IDLE_PID`) is never a real, exitable task; if `exit_current`
/// is somehow reached on it we fall back to a full halt loop rather than
/// removing the always-runnable idle task from rotation.
pub fn exit_current() -> ! {
    let pid = current_pid();
    if is_idle(pid) {
        crate::arch::cpu::halt_loop();
    }

    // Drop any Linux compat state owned by this process. For native tasks (no
    // registered state) this is a no-op; for a Compat_Process it releases the
    // FdTable/VmRegionSet/nosys-set so the registry does not leak across exits.
    crate::task::compat::finish_compat_exit(pid);

    EXITING_PID.store(pid, Ordering::Release);

    // Wait to be preempted and dropped. Keep interrupts enabled so the timer
    // tick can fire; once the tick drops us we are never scheduled again.
    loop {
        crate::arch::cpu::enable_interrupts();
        crate::arch::cpu::halt();
    }
}
