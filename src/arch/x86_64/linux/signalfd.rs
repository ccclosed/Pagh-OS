//! `signalfd` (282) / `signalfd4` (289).
//!
//! Signalfd — читаемый fd над pending-набором сигналов вызывающего: подходящий
//! ожидающий сигнал потребляется из битмапа (не доставляется обычным путём
//! обработчика) и возвращается как 128-байтный `signalfd_siginfo`, первые u32
//! которого — `ssi_signo`. pagh хранит на ожидающий бит только номер сигнала,
//! поэтому остальные поля siginfo нулевые — это документировано, а не выдумано.

use alloc::sync::Arc;

use crate::sync::spinlock::Spinlock;
use crate::task::compat;
use crate::task::fd::{OpenObject, O_NONBLOCK, O_RDWR};

use super::check_user_ptr;
use super::errno::Errno;
use super::io_sys::{copy_in_pub, copy_out_checked};

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
            .alloc_with_flags(OpenObject::Signalfd { mask }, O_RDWR | (flags & O_NONBLOCK))?;
        if flags & SFD_CLOEXEC != 0 {
            cs.fds.set_cloexec(fd, true);
        }
        Ok::<u64, Errno>(fd as u64)
    })
    .ok_or(Errno::EBADF)??;
    Ok(fd)
}

/// `signalfd` (282) — форма без флагов.
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

/// `read(2)` на signalfd: блокироваться до подходящего ожидающего сигнала, затем
/// вернуть одну 128-байтную запись `signalfd_siginfo`.
pub fn read_signalfd(mask: u64, buf: u64, count: u64) -> Result<u64, Errno> {
    if count < SIGNALFD_SIGINFO_SIZE as u64 {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(buf, SIGNALFD_SIGINFO_SIZE as u64)?;
    loop {
        if let Some(signal) = compat::take_signalfd_signal(mask) {
            let mut record = [0u8; SIGNALFD_SIGINFO_SIZE];
            record[..4].copy_from_slice(&(signal as u32).to_le_bytes());
            // Ожидание выше уступает CPU; перепроверить указатель на 128-байтную
            // запись перед публикацией. Поток-сосед по CLONE_VM мог снять её
            // отображение — EFAULT, а не #PF режима ядра с остановкой машины.
            copy_out_checked(buf, &record)?;
            return Ok(SIGNALFD_SIGINFO_SIZE as u64);
        }
        if super::signal::has_deliverable_current() {
            return Err(Errno::EINTR);
        }
        crate::task::scheduler::yield_current();
    }
}
