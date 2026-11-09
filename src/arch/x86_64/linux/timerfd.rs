//! `timerfd_create` (283) / `timerfd_settime` (286) / `timerfd_gettime` (287).
//!
//! A timerfd is a thin state object in the per-process fd table; no timer IRQ
//! walks it. `read`/`poll`/`epoll` compare the deadline with the scheduler tick
//! clock and consume due expirations, which matches the observable Linux
//! behaviour for the single-CPU kernel without adding a second timer queue.

use alloc::sync::Arc;

use crate::sync::spinlock::Spinlock;
use crate::task::compat;
use crate::task::fd::{OpenObject, TimerfdState, O_NONBLOCK, O_RDWR};

use super::check_user_ptr;
use super::errno::Errno;
use super::io_sys::copy_in_pub;
use super::io_sys::copy_out_pub;

const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;
const TFD_NONBLOCK: u64 = 0o4000;
const TFD_CLOEXEC: u64 = 0o2000000;
const TFD_TIMER_ABSTIME: u64 = 1;
const NS_PER_SEC: u64 = 1_000_000_000;

fn ticks_for(sec: u64, nsec: u64) -> u64 {
    let hz = crate::arch::x86_64::apic::TICK_HZ;
    sec.saturating_mul(hz)
        .saturating_add(nsec.saturating_mul(hz) / NS_PER_SEC)
}

fn state_for(fd: u64) -> Result<Arc<Spinlock<TimerfdState>>, Errno> {
    compat::with_current_compat(|cs| match cs.fds.get(fd as u32) {
        Some(OpenObject::Timerfd(state)) => Some(Arc::clone(state)),
        _ => None,
    })
    .flatten()
    .ok_or(Errno::EBADF)
}

/// Read a Linux `struct itimerspec` (32 B).
fn read_itimerspec(ptr: u64) -> Result<(u64, u64, u64, u64), Errno> {
    check_user_ptr(ptr, 32)?;
    let raw = copy_in_pub(ptr, 32);
    let word = |i: usize| u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]));
    Ok((word(0), word(1), word(2), word(3)))
}

fn write_itimerspec(ptr: u64, interval_ticks: u64, remaining_ticks: u64) -> Result<(), Errno> {
    if ptr == 0 {
        return Ok(());
    }
    check_user_ptr(ptr, 32)?;
    let hz = crate::arch::x86_64::apic::TICK_HZ.max(1);
    let sec = |ticks: u64| ticks / hz;
    let nsec = |ticks: u64| (ticks % hz).saturating_mul(NS_PER_SEC / hz);
    let mut out = [0u8; 32];
    for (index, value) in [
        sec(interval_ticks),
        nsec(interval_ticks),
        sec(remaining_ticks),
        nsec(remaining_ticks),
    ]
    .into_iter()
    .enumerate()
    {
        out[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
    }
    copy_out_pub(ptr, &out);
    Ok(())
}

/// `timerfd_create` (283).
pub fn sys_timerfd_create(clockid: u64, flags: u64) -> Result<u64, Errno> {
    if clockid != CLOCK_REALTIME && clockid != CLOCK_MONOTONIC {
        return Err(Errno::EINVAL);
    }
    if flags & !(TFD_NONBLOCK | TFD_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    let state = Arc::new(Spinlock::new(TimerfdState {
        deadline: None,
        interval: 0,
        expirations: 0,
        nonblocking: flags & TFD_NONBLOCK != 0,
    }));
    let fd = compat::with_current_compat(|cs| {
        let fd = cs
            .fds
            .alloc_with_flags(OpenObject::Timerfd(state), O_RDWR | (flags & O_NONBLOCK));
        if flags & TFD_CLOEXEC != 0 {
            cs.fds.set_cloexec(fd, true);
        }
        fd
    })
    .ok_or(Errno::EBADF)?;
    Ok(fd as u64)
}

/// `timerfd_settime` (286).
pub fn sys_timerfd_settime(
    fd: u64,
    flags: u64,
    new_value: u64,
    old_value: u64,
) -> Result<u64, Errno> {
    if flags & !TFD_TIMER_ABSTIME != 0 || new_value == 0 {
        return Err(Errno::EINVAL);
    }
    let state = state_for(fd)?;
    let (int_sec, int_nsec, val_sec, val_nsec) = read_itimerspec(new_value)?;
    if int_nsec >= NS_PER_SEC || val_nsec >= NS_PER_SEC {
        return Err(Errno::EINVAL);
    }
    let interval = ticks_for(int_sec, int_nsec);
    let value = ticks_for(val_sec, val_nsec);
    let now = crate::task::scheduler::ticks();
    let mut guard = state.lock();
    let old_interval = guard.interval;
    let old_remaining = guard
        .deadline
        .map_or(0, |deadline| deadline.saturating_sub(now));
    guard.interval = interval;
    guard.expirations = 0;
    guard.deadline = if value == 0 {
        None
    } else if flags & TFD_TIMER_ABSTIME != 0 {
        Some(value)
    } else {
        Some(now.saturating_add(value))
    };
    drop(guard);
    write_itimerspec(old_value, old_interval, old_remaining)?;
    Ok(0)
}

/// `timerfd_gettime` (287).
pub fn sys_timerfd_gettime(fd: u64, curr_value: u64) -> Result<u64, Errno> {
    let state = state_for(fd)?;
    let guard = state.lock();
    let now = crate::task::scheduler::ticks();
    let remaining = guard
        .deadline
        .map_or(0, |deadline| deadline.saturating_sub(now));
    let interval = guard.interval;
    drop(guard);
    write_itimerspec(curr_value, interval, remaining)?;
    Ok(0)
}
