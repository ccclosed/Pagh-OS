//! `signalfd` (282) / `signalfd4` (289).
//!
//! A signalfd is a readable fd over the caller's pending-signal set: a matching
//! pending signal is consumed from the pending bitmap (not delivered through the
//! normal handler path) and returned as a 128-byte `signalfd_siginfo` whose first
//! u32 is `ssi_signo`. pagh stores only the signal number per pending bit, so the
//! remaining siginfo fields are zero — documented rather than invented.

use alloc::sync::Arc;

use crate::sync::spinlock::Spinlock;
use crate::task::compat;
use crate::task::fd::{OpenObject, O_NONBLOCK, O_RDWR};

use super::check_user_ptr;
use super::errno::Errno;
use super::io_sys::{copy_in_pub, copy_out_pub};

const SFD_NONBLOCK: u64 = 0o4000;
const SFD_CLOEXEC: u64 = 0o2000000;
const SIGNALFD_SIGINFO_SIZE: usize = 128;

fn read_mask(ptr: u64, size: u64) -> Result<u64, Errno> {
    if size != 8 || ptr == 0 {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(ptr, 8)?;
    let raw = copy_in_pub(ptr, 8);
    Ok(u64::from_le_bytes(raw[..8].try_into().unwrap_or([0; 8])))
}

fn create(mask: u64, flags: u64) -> Result<u64, Errno> {
    if flags & !(SFD_NONBLOCK | SFD_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    let mask = Arc::new(Spinlock::new(mask));
    let fd = compat::with_current_compat(|cs| {
        let fd = cs
            .fds
            .alloc_with_flags(OpenObject::Signalfd { mask }, O_RDWR | (flags & O_NONBLOCK));
        if flags & SFD_CLOEXEC != 0 {
            cs.fds.set_cloexec(fd, true);
        }
        fd
    })
    .ok_or(Errno::EBADF)?;
    Ok(fd as u64)
}

/// `signalfd` (282) — the flags-less form.
pub fn sys_signalfd(fd: u64, mask: u64, sizemask: u64) -> Result<u64, Errno> {
    let mask = read_mask(mask, sizemask)?;
    if fd == u64::MAX {
        return create(mask, 0);
    }
    update(fd, mask)
}

/// `signalfd4` (289).
pub fn sys_signalfd4(fd: u64, mask: u64, sizemask: u64, flags: u64) -> Result<u64, Errno> {
    let mask = read_mask(mask, sizemask)?;
    if fd == u64::MAX {
        return create(mask, flags);
    }
    if flags & !(SFD_NONBLOCK | SFD_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    update(fd, mask)
}

fn update(fd: u64, mask: u64) -> Result<u64, Errno> {
    compat::with_current_compat(|cs| match cs.fds.get(fd as u32) {
        Some(OpenObject::Signalfd { mask: state }) => {
            *state.lock() = mask;
            Some(fd)
        }
        Some(_) => None,
        None => None,
    })
    .flatten()
    .ok_or(Errno::EBADF)
}

/// `read(2)` on a signalfd: block until a matching pending signal, then return
/// one 128-byte `signalfd_siginfo` record.
pub fn read_signalfd(mask: u64, buf: u64, count: u64) -> Result<u64, Errno> {
    if count < SIGNALFD_SIGINFO_SIZE as u64 {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(buf, SIGNALFD_SIGINFO_SIZE as u64)?;
    loop {
        if let Some(signal) = compat::take_signalfd_signal(mask) {
            let mut record = [0u8; SIGNALFD_SIGINFO_SIZE];
            record[..4].copy_from_slice(&(signal as u32).to_le_bytes());
            copy_out_pub(buf, &record);
            return Ok(SIGNALFD_SIGINFO_SIZE as u64);
        }
        if super::signal::has_deliverable_current() {
            return Err(Errno::EINTR);
        }
        crate::task::scheduler::yield_current();
    }
}
