use alloc::collections::VecDeque;
use crate::sync::spinlock::Spinlock;
use crate::memory::{pmm, vmm};
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::structures::paging::PageTableFlags;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState { Ready, Running, Blocked, Dead }

/// Task control block, reduced to exactly the state the RSP-based context
/// switch restores from (Requirement 11.3). The vestigial `rip`/`rflags`/
/// `regs`/signal fields were removed: the entry point and initial register
/// values are encoded in the kernel stack frame `kernel_thread_spawn` builds,
/// not stored here. With only `u64`/enum fields the `Tcb` is now `Copy`.
#[derive(Debug, Clone, Copy)]
pub struct Tcb {
    pub pid: u64,
    pub state: TaskState,
    /// The only state the switch restores from: the saved kernel stack pointer.
    pub kernel_rsp: u64,
    /// Physical address of this task's PML4 (reloaded into CR3 on switch).
    pub cr3: u64,
}

impl Tcb {
    /// Construct a ready task. The entry point is not stored in the `Tcb`; it
    /// is baked into the constructed kernel stack frame pointed to by
    /// `kernel_rsp` (see `kernel_thread_spawn`).
    pub fn new(pid: u64, kernel_rsp: u64, cr3: u64) -> Self {
        Tcb { pid, state: TaskState::Ready, kernel_rsp, cr3 }
    }
}

static READY_QUEUE: Spinlock<VecDeque<Tcb>> = Spinlock::new(VecDeque::new());
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
    state: TaskState::Running,
    kernel_rsp: 0,
    cr3: 0,
});

/// Returns true when `pid` is the idle task.
#[inline]
pub fn is_idle(pid: u64) -> bool { pid == IDLE_PID }

/// Save the idle task's stack pointer (called when the idle task is preempted).
#[inline]
fn save_idle_rsp(rsp: u64) { IDLE_TASK.lock().kernel_rsp = rsp; }

/// The idle task's saved stack pointer (scheduled when nothing else is ready).
#[inline]
fn idle_rsp() -> u64 { IDLE_TASK.lock().kernel_rsp }

pub fn init() { crate::debug!("Scheduler initialized (Round Robin)"); }
pub fn tick() { TICK_COUNT.fetch_add(1, Ordering::Relaxed); }
pub fn ticks() -> u64 { TICK_COUNT.load(Ordering::Relaxed) }
pub fn spawn(tcb: Tcb) -> u64 { let pid = tcb.pid; READY_QUEUE.lock().push_back(tcb); pid }
pub fn schedule() -> Option<Tcb> { READY_QUEUE.lock().pop_front() }
pub fn requeue(tcb: Tcb) { READY_QUEUE.lock().push_back(tcb); }
pub fn next_pid() -> u64 { NEXT_PID.fetch_add(1, Ordering::Relaxed) }
pub fn set_current_pid(pid: u64) { *CURRENT_PID.lock() = pid; }
pub fn current_pid() -> u64 { *CURRENT_PID.lock() }

pub fn kernel_thread_spawn(entry: fn()) -> u64 {
    let pid = next_pid();
    let (_guard_base, stack_base, stack_top) =
        crate::memory::layout::kernel_stack_for_pid(pid);

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
        //     iretq                       ; consume RIP, CS, RFLAGS, RSP, SS
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
        //   [+128] RIP = trampoline  [+136] CS  [+144] RFLAGS  [+152] RSP = stack_top
        //   [+160] SS
        let mut rsp = stack_top;

        // ── iretq frame (highest addresses), consumed by `iretq` ──────────
        rsp -= 8; (rsp as *mut u64).write(kernel_ss);   // [+160] SS
        rsp -= 8; (rsp as *mut u64).write(stack_top);   // [+152] RSP (clean top)
        rsp -= 8; (rsp as *mut u64).write(0x202u64);    // [+144] RFLAGS (IF set)
        rsp -= 8; (rsp as *mut u64).write(kernel_cs);   // [+136] CS

        let trampoline = crate::task::switch::kernel_thread_trampoline as u64;
        rsp -= 8; (rsp as *mut u64).write(trampoline);  // [+128] RIP -> trampoline

        // ── 15 GPR slots, written high→low to match the pop order ─────────
        // High→low addresses correspond to: rax (highest, popped last) down to
        // r15 (lowest, popped first). `entry` goes in the rdi slot.
        rsp -= 8; (rsp as *mut u64).write(0);            // [+120] rax
        rsp -= 8; (rsp as *mut u64).write(0);            // [+112] rbx
        rsp -= 8; (rsp as *mut u64).write(0);            // [+104] rcx
        rsp -= 8; (rsp as *mut u64).write(0);            // [+96]  rdx
        rsp -= 8; (rsp as *mut u64).write(0);            // [+88]  rsi
        rsp -= 8; (rsp as *mut u64).write(entry as u64); // [+80]  rdi = entry
        rsp -= 8; (rsp as *mut u64).write(0);            // [+72]  rbp
        rsp -= 8; (rsp as *mut u64).write(0);            // [+64]  r8
        rsp -= 8; (rsp as *mut u64).write(0);            // [+56]  r9
        rsp -= 8; (rsp as *mut u64).write(0);            // [+48]  r10
        rsp -= 8; (rsp as *mut u64).write(0);            // [+40]  r11
        rsp -= 8; (rsp as *mut u64).write(0);            // [+32]  r12
        rsp -= 8; (rsp as *mut u64).write(0);            // [+24]  r13
        rsp -= 8; (rsp as *mut u64).write(0);            // [+16]  r14
        rsp -= 8; (rsp as *mut u64).write(0);            // [+8]   r15

        // ── RFLAGS word consumed by `popfq` (lowest address = final rsp) ──
        rsp -= 8; (rsp as *mut u64).write(0x202u64);     // [+0] RFLAGS-for-popfq

        let tcb = Tcb {
            pid,
            state: TaskState::Ready,
            kernel_rsp: rsp,
            cr3: vmm::current_pml4_phys(),
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
            state: TaskState::Ready,
            kernel_rsp: current_rsp,
            cr3: vmm::current_pml4_phys(),
        });
    }

    crate::arch::x86_64::apic::send_eoi();

    let next = match schedule() {
        Some(tcb) => tcb,
        None => {
            // Nothing ready: run the idle task.
            set_current_pid(IDLE_PID);
            return idle_rsp();
        }
    };

    set_current_pid(next.pid);

    // Single centralized CR3 reload for the preemptive path (Requirement 11.5).
    // Delegates to `vmm::load_cr3`, the ONE place that writes CR3 on a switch.
    // The reload doubles as a TLB flush so the next task's stack pages are
    // reloaded. No other site in this path touches CR3.
    unsafe { vmm::load_cr3(next.cr3); }

    next.kernel_rsp
}

pub fn yield_current() {
    let current_rsp: u64;
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) current_rsp, options(nomem, nostack)); }

    let current_pid = current_pid();
    let mut old_rsp = current_rsp;
    let next_tcb = match schedule() {
        Some(tcb) => tcb,
        None => return,
    };
    let new_rsp = next_tcb.kernel_rsp;
    set_current_pid(next_tcb.pid);

    let current_cr3 = vmm::current_pml4_phys();
    // Centralized CR3 reload for the cooperative path (Requirement 11.5): the
    // cooperative yield reloads CR3 through the same `vmm::load_cr3` helper the
    // preemptive tick uses, so CR3 is written in exactly one place. CR3 is not
    // rewritten anywhere else in this path.
    unsafe { vmm::load_cr3(next_tcb.cr3); }

    unsafe { crate::task::switch::switch_context(&mut old_rsp, new_rsp, None); }

    requeue(Tcb {
        pid: current_pid,
        state: TaskState::Ready,
        kernel_rsp: old_rsp,
        cr3: current_cr3,
    });
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

    EXITING_PID.store(pid, Ordering::Release);

    // Wait to be preempted and dropped. Keep interrupts enabled so the timer
    // tick can fire; once the tick drops us we are never scheduled again.
    loop {
        crate::arch::cpu::enable_interrupts();
        crate::arch::cpu::halt();
    }
}
