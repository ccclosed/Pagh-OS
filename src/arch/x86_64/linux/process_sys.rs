//! Linux process/thread synchronization syscalls.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr;

use crate::sync::spinlock::Spinlock;

use super::check_user_ptr;
use super::errno::Errno;
use super::regs::SavedRegs;

const FUTEX_CMD_MASK: u64 = 0x7f;
const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const FUTEX_WAIT_BITSET: u64 = 9;
const FUTEX_WAKE_BITSET: u64 = 10;
const FUTEX_BITSET_MATCH_ANY: u32 = u32::MAX;
const TICK_HZ: u64 = 100;

/// Cooperative waiter registry. Tickets make FUTEX_WAKE return and release an
/// exact number of waiters even though the current scheduler has no blocked TCB
/// state yet. The key includes pid so identical virtual addresses in unrelated
/// address spaces never share a queue.
#[derive(Default)]
struct WaitQueue {
    next_ticket: u64,
    waiting: BTreeMap<u64, u32>,
    woken: BTreeMap<u64, ()>,
}

static FUTEX_QUEUES: Spinlock<BTreeMap<(u64, u64), WaitQueue>> = Spinlock::new(BTreeMap::new());

#[inline]
fn key(uaddr: u64) -> (u64, u64) {
    (crate::memory::vmm::current_pml4_phys(), uaddr)
}

#[inline]
fn load_word(addr: u64) -> Result<u32, Errno> {
    if addr & 3 != 0 {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(addr, 4)?;
    // SAFETY: the address is aligned and the mapped user range was validated.
    Ok(unsafe { ptr::read_volatile(addr as *const u32) })
}

fn timeout_deadline(timeout: u64) -> Result<Option<u64>, Errno> {
    if timeout == 0 {
        return Ok(None);
    }
    check_user_ptr(timeout, 16)?;
    // SAFETY: the complete timespec was validated above.
    let sec = unsafe { ptr::read_unaligned(timeout as *const i64) };
    let nsec = unsafe { ptr::read_unaligned((timeout + 8) as *const i64) };
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return Err(Errno::EINVAL);
    }
    let ticks = (sec as u64)
        .saturating_mul(TICK_HZ)
        .saturating_add((nsec as u64).saturating_add(9_999_999) / 10_000_000);
    Ok(Some(crate::task::scheduler::ticks().saturating_add(ticks)))
}

fn register_waiter(queue_key: (u64, u64), bitset: u32) -> u64 {
    let mut queues = FUTEX_QUEUES.lock();
    let queue = queues.entry(queue_key).or_default();
    let ticket = queue.next_ticket;
    queue.next_ticket = queue.next_ticket.wrapping_add(1);
    queue.waiting.insert(ticket, bitset);
    ticket
}

/// Remove a waiter or consume its wake token. Returns true only when it was
/// selected by FUTEX_WAKE.
fn finish_wait(queue_key: (u64, u64), ticket: u64) -> bool {
    let mut queues = FUTEX_QUEUES.lock();
    let mut was_woken = false;
    let mut empty = false;
    if let Some(queue) = queues.get_mut(&queue_key) {
        queue.waiting.remove(&ticket);
        was_woken = queue.woken.remove(&ticket).is_some();
        empty = queue.waiting.is_empty() && queue.woken.is_empty();
    }
    if empty {
        queues.remove(&queue_key);
    }
    was_woken
}

fn waiter_woken(queue_key: (u64, u64), ticket: u64) -> bool {
    FUTEX_QUEUES
        .lock()
        .get(&queue_key)
        .is_some_and(|queue| queue.woken.contains_key(&ticket))
}

fn wake_waiters(queue_key: (u64, u64), maximum: u64, bitset: u32) -> u64 {
    if maximum == 0 {
        return 0;
    }
    let mut queues = FUTEX_QUEUES.lock();
    let Some(queue) = queues.get_mut(&queue_key) else {
        return 0;
    };

    let selected: Vec<u64> = queue
        .waiting
        .iter()
        .filter(|(_, waiter_mask)| **waiter_mask & bitset != 0)
        .take(core::cmp::min(maximum, usize::MAX as u64) as usize)
        .map(|(ticket, _)| *ticket)
        .collect();
    for ticket in &selected {
        queue.waiting.remove(ticket);
        queue.woken.insert(*ticket, ());
    }
    selected.len() as u64
}

/// Cooperative futex implementation with an exact ticketed waiter registry.
/// WAIT still yields cooperatively, but WAKE now selects at most `val` matching
/// waiters, reports the exact count, and honors WAIT/WAKE_BITSET masks.
pub fn sys_futex(
    uaddr: u64,
    op: u64,
    val: u64,
    timeout: u64,
    _uaddr2: u64,
    val3: u64,
) -> Result<u64, Errno> {
    let cmd = op & FUTEX_CMD_MASK;
    match cmd {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let bitset = if cmd == FUTEX_WAIT_BITSET {
                let mask = val3 as u32;
                if mask == 0 {
                    return Err(Errno::EINVAL);
                }
                mask
            } else {
                FUTEX_BITSET_MATCH_ANY
            };
            if load_word(uaddr)? != val as u32 {
                return Err(Errno::EAGAIN);
            }
            let deadline = timeout_deadline(timeout)?;
            let queue_key = key(uaddr);
            let ticket = register_waiter(queue_key, bitset);

            loop {
                if waiter_woken(queue_key, ticket) {
                    finish_wait(queue_key, ticket);
                    return Ok(0);
                }
                // A changed word permits a spurious successful return; callers
                // must always re-check the userspace condition, as on Linux.
                if load_word(uaddr)? != val as u32 {
                    finish_wait(queue_key, ticket);
                    return Ok(0);
                }
                if deadline.is_some_and(|end| crate::task::scheduler::ticks() >= end) {
                    // A concurrent wake wins over timeout if it selected us first.
                    if finish_wait(queue_key, ticket) {
                        return Ok(0);
                    }
                    return Err(Errno::ETIMEDOUT);
                }
                crate::task::scheduler::yield_current();
            }
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            let bitset = if cmd == FUTEX_WAKE_BITSET {
                let mask = val3 as u32;
                if mask == 0 {
                    return Err(Errno::EINVAL);
                }
                mask
            } else {
                FUTEX_BITSET_MATCH_ANY
            };
            let _ = load_word(uaddr)?;
            Ok(wake_waiters(key(uaddr), val, bitset))
        }
        _ => Err(Errno::ENOSYS),
    }
}

const WNOHANG: u64 = 1;
const RUSAGE_SIZE: u64 = 18 * 8;
pub fn sys_wait4(pid: u64, status: u64, options: u64, rusage: u64) -> Result<u64, Errno> {
    if options & !WNOHANG != 0 {
        return Err(Errno::EINVAL);
    }
    let wanted = pid as i64;
    if wanted != -1 && wanted <= 0 {
        return Err(Errno::ECHILD);
    }
    if status != 0 {
        check_user_ptr(status, 4)?
    }
    if rusage != 0 {
        check_user_ptr(rusage, RUSAGE_SIZE)?
    }
    let parent = crate::task::scheduler::current_pid();
    loop {
        if let Some((child, code)) = crate::task::compat::reap_child(parent, wanted) {
            if status != 0 {
                unsafe { ptr::write_unaligned(status as *mut u32, (code as u32) << 8) }
            }
            if rusage != 0 {
                unsafe { ptr::write_bytes(rusage as *mut u8, 0, RUSAGE_SIZE as usize) }
            }
            return Ok(child);
        }
        if !crate::task::compat::has_child(parent, wanted) {
            return Err(Errno::ECHILD);
        }
        if options & WNOHANG != 0 {
            return Ok(0);
        }
        crate::task::scheduler::yield_current();
    }
}

fn exec_cstr(ptr: u64, budget: &mut usize) -> Result<Vec<u8>, Errno> {
    if ptr == 0 {
        return Err(Errno::EFAULT);
    }
    let mut out = Vec::new();
    for i in 0..4096u64 {
        check_user_ptr(ptr + i, 1)?;
        let b = unsafe { ptr::read((ptr + i) as *const u8) };
        if b == 0 {
            *budget = budget.checked_add(out.len() + 1).ok_or(Errno::E2BIG)?;
            if *budget > 4096 {
                return Err(Errno::E2BIG);
            }
            return Ok(out);
        }
        out.push(b);
    }
    Err(Errno::E2BIG)
}
fn exec_vec(base: u64, budget: &mut usize) -> Result<Vec<Vec<u8>>, Errno> {
    if base == 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for i in 0..256u64 {
        let slot = base + i * 8;
        check_user_ptr(slot, 8)?;
        let p = unsafe { ptr::read_unaligned(slot as *const u64) };
        if p == 0 {
            return Ok(out);
        };
        out.push(exec_cstr(p, budget)?);
    }
    Err(Errno::E2BIG)
}
fn runerr(e: crate::task::process::RunError) -> Errno {
    match e {
        crate::task::process::RunError::ArgsTooLarge => Errno::E2BIG,
        crate::task::process::RunError::NotFound => Errno::ENOENT,
        crate::task::process::RunError::LoadFailed(_) => Errno::ENOEXEC,
        crate::task::process::RunError::StackFailed => Errno::ENOMEM,
    }
}
/// execve(59), syscall-instruction path: replace CR3/image and return through
/// sysretq directly to the new ELF entry with a new System-V initial stack.
pub fn sys_execve(regs: &mut SavedRegs, path: u64, argv: u64, envp: u64) -> Result<u64, Errno> {
    let mut budget = 0usize;
    let pathb = exec_cstr(path, &mut budget)?;
    let path = String::from_utf8(pathb).map_err(|_| Errno::EINVAL)?;
    let av = exec_vec(argv, &mut budget)?;
    let ev = exec_vec(envp, &mut budget)?;
    let ar: Vec<&[u8]> = av.iter().map(|v| v.as_slice()).collect();
    let er: Vec<&[u8]> = ev.iter().map(|v| v.as_slice()).collect();
    let image = crate::task::process::exec_linux_image(&path, &ar, &er).map_err(runerr)?;
    let flags = regs.r11;
    *regs = SavedRegs::default();
    regs.r11 = flags | 2;
    regs.rcx = image.entry;
    // `syscall`-instruction path (musl): the exit stub restores the user RSP
    // from the per-task slot at +120 right above the SavedRegs frame, so
    // execve must rewrite THAT slot; the old global is never read on exit.
    unsafe { ((regs as *mut SavedRegs as *mut u64).add(15)).write(image.initial_rsp) };
    Ok(0)
}

const CLONE_VM: u64 = 0x100;
const CLONE_FS: u64 = 0x200;
const CLONE_FILES: u64 = 0x400;
const CLONE_SIGHAND: u64 = 0x800;
const CLONE_THREAD: u64 = 0x10000;
const CLONE_SYSVSEM: u64 = 0x40000;
const CLONE_SETTLS: u64 = 0x80000;
const CLONE_PARENT_SETTID: u64 = 0x100000;
const CLONE_CHILD_CLEARTID: u64 = 0x200000;
const CLONE_CHILD_SETTID: u64 = 0x01000000;
const CLONE_SUPPORTED: u64 = 0xff
    | CLONE_VM
    | CLONE_FS
    | CLONE_FILES
    | CLONE_SIGHAND
    | CLONE_THREAD
    | CLONE_SYSVSEM
    | CLONE_SETTLS
    | CLONE_PARENT_SETTID
    | CLONE_CHILD_CLEARTID
    | CLONE_CHILD_SETTID;
pub fn sys_clone(
    regs: &SavedRegs,
    flags: u64,
    child_stack: u64,
    parent_tid: u64,
    child_tid: u64,
    tls: u64,
) -> Result<u64, Errno> {
    if flags & !CLONE_SUPPORTED != 0
        || flags & (CLONE_VM | CLONE_THREAD) != (CLONE_VM | CLONE_THREAD)
    {
        return Err(Errno::ENOSYS);
    }
    // Default stack = the caller's user RSP from the per-task slot at +120
    // above the SavedRegs frame (the global scratch may already belong to a
    // sibling thread by the time we run).
    let stack = if child_stack == 0 {
        unsafe { ((regs as *const SavedRegs as *const u64).add(15)).read() }
    } else {
        child_stack
    };
    check_user_ptr(stack.saturating_sub(8), 8)?;
    if flags & CLONE_PARENT_SETTID != 0 {
        check_user_ptr(parent_tid, 4)?
    }
    if flags & CLONE_CHILD_SETTID != 0 {
        check_user_ptr(child_tid, 4)?
    }
    let child = crate::task::process::spawn_linux_thread(regs, stack).map_err(|_| Errno::ENOMEM)?;
    let tls_value = if flags & CLONE_SETTLS != 0 {
        Some(tls)
    } else {
        None
    };
    let clear = if flags & CLONE_CHILD_CLEARTID != 0 {
        child_tid
    } else {
        0
    };
    if !crate::task::compat::clone_current_compat(child, tls_value, clear) {
        return Err(Errno::EINVAL);
    }
    if flags & CLONE_PARENT_SETTID != 0 {
        unsafe { ptr::write_unaligned(parent_tid as *mut u32, child as u32) }
    }
    if flags & CLONE_CHILD_SETTID != 0 {
        unsafe { ptr::write_unaligned(child_tid as *mut u32, child as u32) }
    }
    Ok(child)
}
const FUTEX_WAITERS: u32 = 0x8000_0000;
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
const FUTEX_TID_MASK: u32 = 0x3fff_ffff;
fn cleanup_robust(head: u64, len: u64, tid: u64) {
    if head == 0 || len != 24 || check_user_ptr(head, 24).is_err() {
        return;
    }
    let next = unsafe { ptr::read_unaligned(head as *const u64) };
    let offset = unsafe { ptr::read_unaligned((head + 8) as *const i64) };
    let pending = unsafe { ptr::read_unaligned((head + 16) as *const u64) };
    let mut node = next;
    let mut count = 0;
    while node != 0 && node != head && count < 2048 {
        let next_node = if check_user_ptr(node, 8).is_ok() {
            unsafe { ptr::read_unaligned(node as *const u64) }
        } else {
            break;
        };
        mark_owner_died(node, offset, tid);
        node = next_node;
        count += 1;
    }
    if pending != 0 {
        mark_owner_died(pending, offset, tid);
    }
}
fn mark_owner_died(node: u64, offset: i64, tid: u64) {
    let Some(addr) = node.checked_add_signed(offset) else {
        return;
    };
    if check_user_ptr(addr, 4).is_err() {
        return;
    }
    unsafe {
        let p = addr as *mut u32;
        let old = ptr::read_volatile(p);
        if old & FUTEX_TID_MASK == tid as u32 {
            ptr::write_volatile(p, (old & FUTEX_WAITERS) | FUTEX_OWNER_DIED);
            let _ = wake_waiters(key(addr), 1, FUTEX_BITSET_MATCH_ANY);
        }
    }
}
pub fn cleanup_thread_exit(pid: u64) {
    let clear = crate::task::compat::clear_tid_for(pid);
    let (head, len) = crate::task::compat::robust_for(pid);
    cleanup_robust(head, len, pid);
    if clear != 0 && check_user_ptr(clear, 4).is_ok() {
        unsafe { ptr::write_volatile(clear as *mut u32, 0) };
        let _ = wake_waiters(key(clear), 1, FUTEX_BITSET_MATCH_ANY);
    }
}
pub fn cleanup_current_thread_exit() {
    cleanup_thread_exit(crate::task::scheduler::current_pid())
}
