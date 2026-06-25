// sync/spinlock.rs — IRQ-safe spinlock (ported from the x86_64 kernel).
//
// Disables supervisor interrupts on the current hart while held, so an interrupt
// handler that takes the same lock cannot deadlock against the holder. Adapted
// from the x86 version: it calls the riscv `crate::cpu` interrupt primitives
// (enable/disable are `unsafe` on riscv, so the calls are wrapped).
#![allow(dead_code)]

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// A spinlock that disables interrupts on the current hart while held.
pub struct Spinlock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

/// Guard returned by [`Spinlock::lock`]; releases the lock on drop.
pub struct SpinlockGuard<'a, T> {
    lock: &'a Spinlock<T>,
    were_enabled: bool,
}

// SAFETY: the lock provides mutual exclusion; `T` is only reachable through a guard.
unsafe impl<T: Send> Send for Spinlock<T> {}
unsafe impl<T: Send> Sync for Spinlock<T> {}

impl<T> Spinlock<T> {
    pub const fn new(data: T) -> Self {
        Spinlock {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    pub fn lock(&self) -> SpinlockGuard<'_, T> {
        let were_enabled = crate::cpu::interrupts_enabled();
        // SAFETY: masking interrupts on this hart while the lock is held prevents
        // a same-hart IRQ handler from deadlocking on the same lock.
        unsafe { crate::cpu::disable_interrupts() };

        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }

        SpinlockGuard {
            lock: self,
            were_enabled,
        }
    }

    pub fn try_lock(&self) -> Option<SpinlockGuard<'_, T>> {
        let were_enabled = crate::cpu::interrupts_enabled();
        // SAFETY: see `lock`.
        unsafe { crate::cpu::disable_interrupts() };

        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(SpinlockGuard {
                lock: self,
                were_enabled,
            })
        } else {
            if were_enabled {
                // SAFETY: restore the prior interrupt state on failed acquisition.
                unsafe { crate::cpu::enable_interrupts() };
            }
            None
        }
    }

    fn unlock(&self, were_enabled: bool) {
        self.locked.store(false, Ordering::Release);
        if were_enabled {
            // SAFETY: restore the interrupt state that held before `lock`.
            unsafe { crate::cpu::enable_interrupts() };
        }
    }
}

impl<T> Deref for SpinlockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        // SAFETY: exclusive access guaranteed by the guard.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinlockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: exclusive access guaranteed by the guard.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinlockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.unlock(self.were_enabled);
    }
}
