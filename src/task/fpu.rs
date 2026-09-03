// task/fpu.rs — Per-task FPU/SSE state (FXSAVE/FXRSTOR).
//
// The context-switch frame (see `task::switch`) carries only GPRs and RFLAGS;
// the x87/SSE register file (ST/MM, XMM, MXCSR, FCW/FSW) is architecturally
// separate and is NOT saved by pushfq/iretq paths. User code (glibc, cpython)
// uses SSE constantly, so two FP-using tasks rotating through the CPU would
// otherwise corrupt each other's XMM registers on every preemption.
//
// The kernel is compiled without FP codegen (-soft-float target feature
// subset) and never executes FP/SSE instructions, so kernel→kernel switches
// need no save/restore. The eager policy used here:
//
//   * every task gets a 512-byte 64-byte-aligned FXSAVE area at spawn;
//   * when the scheduler stops running a task whose saved frame is a ring-3
//     frame OR which owns Linux-compat state (a compat task may be preempted
//     or yield inside a kernel syscall with its user FP state still live), the
//     CPU's FP state is FXSAVE'd into that task's area;
//   * when the scheduler starts such a task, the state is FXRSTOR'd back (a
//     never-yet-run task's area is pre-filled with the FINIT reset image, so
//     the first restore yields a clean FPU).
//
// CR0/CR4 are configured once at boot by `arch::cpu::enable_sse` (EM=0, MP=1,
// OSFXSR=1, OSXMMEXCPT=1); no TS/lazy-switch machinery is needed.

use crate::sync::spinlock::Spinlock;
use alloc::collections::BTreeMap;

/// Size of the FXSAVE/FXRSTOR memory image (x87 + SSE, 64-bit mode).
const FXSAVE_SIZE: usize = 512;
/// FXSAVE images must be 16-byte aligned; use 64 to be conservative (and
/// cache-line friendly).
const FXSAVE_ALIGN: usize = 64;

/// x87 control word reset value: all exceptions masked, extended precision.
const FCW_INIT: u16 = 0x037F;
/// MXCSR reset value: all exceptions masked, round-to-nearest.
const MXCSR_INIT: u32 = 0x1F80;

/// A per-task FXSAVE area. `#[repr(align(64))]` keeps the interior buffer
/// aligned no matter where the value (or a pointer to it) is stored.
#[repr(C, align(64))]
pub struct FxArea([u8; FXSAVE_SIZE]);

impl FxArea {
    /// Fill the area with the FPU reset image (what FINIT + LDMXCSR leave).
    /// FXRSTOR from this image yields a clean, all-exceptions-masked FPU.
    fn init_image(&mut self) {
        self.0 = [0u8; FXSAVE_SIZE];
        self.0[0..2].copy_from_slice(&FCW_INIT.to_le_bytes());
        self.0[24..28].copy_from_slice(&MXCSR_INIT.to_le_bytes());
    }
}

/// Allocate a zero-initialized-then-reset-image FX area directly from the
/// global allocator (not `Box`), so the 64-byte alignment is explicit.
fn alloc_area() -> *mut FxArea {
    use alloc::alloc::{alloc_zeroed, handle_alloc_error, Layout};
    let layout = Layout::from_size_align(FXSAVE_SIZE, FXSAVE_ALIGN).expect("FxArea layout");
    // SAFETY: the layout has non-zero size; the returned pointer is aligned
    // and valid for reads/writes of `FXSAVE_SIZE` bytes for the program
    // lifetime (areas are freed only by the exit reaper, after the task can
    // no longer be scheduled).
    let ptr = unsafe { alloc_zeroed(layout) };
    if ptr.is_null() {
        handle_alloc_error(layout);
    }
    let area = unsafe { &mut *(ptr as *mut FxArea) };
    area.init_image();
    ptr as *mut FxArea
}

/// Free an area previously returned by [`alloc_area`] (same layout).
fn free_area(ptr: *mut FxArea) {
    use alloc::alloc::{dealloc, Layout};
    let layout = Layout::from_size_align(FXSAVE_SIZE, FXSAVE_ALIGN).expect("FxArea layout");
    // SAFETY: `ptr` was produced by `alloc_area` with this exact layout and
    // the caller guarantees no further use.
    unsafe { dealloc(ptr as *mut u8, layout) }
}

/// Owned pointer to a per-task FXSAVE area (Send wrapper so it can live in a
/// global map; the kernel is single-core and all access is lock-guarded).
struct AreaPtr(*mut FxArea);
unsafe impl Send for AreaPtr {}

/// Per-pid FXSAVE areas, allocated by `scheduler::spawn` and released by the
/// exit reaper. The lock is held only for the map lookup/insert/remove.
static FP_AREAS: Spinlock<BTreeMap<u64, AreaPtr>> = Spinlock::new(BTreeMap::new());

/// Ensure `pid` has an FXSAVE area. Called from `scheduler::spawn` (task
/// context, possibly with interrupts disabled — never from an IRQ).
pub fn area_for(pid: u64) {
    let mut areas = FP_AREAS.lock();
    if !areas.contains_key(&pid) {
        areas.insert(pid, AreaPtr(alloc_area()));
    }
}

/// Release `pid`'s FXSAVE area. Called by the exit reaper after the task is
/// gone from rotation.
pub fn free(pid: u64) {
    if let Some(area) = FP_AREAS.lock().remove(&pid) {
        free_area(area.0);
    }
}

/// Does the saved frame at `rsp` (canonical layout, CS at +136) belong to a
/// task that may hold user FP state in the CPU?
///
/// Ring-3 frames (CS RPL=3) are direct. Yield frames always carry a kernel CS
/// even when the yielding task is a Linux-compat process interrupted inside a
/// syscall — for those the compat registry decides. Kernel threads have no
/// compat state and kernel CS: false.
fn frame_uses_fp(pid: u64, rsp: u64) -> bool {
    if rsp == 0 || (rsp as i64) < 0 || rsp < 0xffff_8000_0000_0000 {
        return false;
    }
    // SAFETY: the frame lives in the canonical higher half; the read mirrors
    // `scheduler::check_frame`'s probe of the same location.
    let cs = unsafe { core::ptr::read_volatile((rsp + 136) as *const u64) };
    (cs & 3) == 3 || crate::task::compat::compat_exists(pid)
}

/// Save the CPU's FP state into `pid`'s area if the task can hold user FP
/// state (see [`frame_uses_fp`]). Called by the scheduler when a task is
/// stopped (timer tick or cooperative yield).
pub fn save_if_user(pid: u64, rsp: u64) {
    if !frame_uses_fp(pid, rsp) {
        return;
    }
    let ptr = {
        let areas = FP_AREAS.lock();
        match areas.get(&pid) {
            Some(area) => area.0,
            None => {
                crate::warn!("[FPU] no FXSAVE area for user pid={} (state lost)", pid);
                return;
            }
        }
    };
    // SAFETY: `ptr` points to the task's 64-byte-aligned 512-byte area (the
    // map's entry keeps it alive); FXSAVE writes only that memory and reads
    // only the CPU's FP register file.
    unsafe {
        core::arch::asm!("fxsave [{0}]", in(reg) ptr, options(nostack));
    }
}

/// Restore `pid`'s FP state into the CPU if the task can hold user FP state.
/// Called by the scheduler right before resuming a task (its frame at `rsp`).
pub fn restore_if_user(pid: u64, rsp: u64) {
    if !frame_uses_fp(pid, rsp) {
        return;
    }
    let ptr = {
        let areas = FP_AREAS.lock();
        match areas.get(&pid) {
            Some(area) => area.0,
            None => {
                crate::warn!(
                    "[FPU] no FXSAVE area for user pid={} (restore skipped)",
                    pid
                );
                return;
            }
        }
    };
    // SAFETY: `ptr` points to the task's area holding a previously FXSAVE'd
    // image (or the reset image written at allocation) — a valid FXRSTOR
    // input; the load writes only the CPU's FP register file and reads only
    // the 512-byte buffer.
    unsafe {
        core::arch::asm!("fxrstor [{0}]", in(reg) ptr, options(nostack));
    }
}
