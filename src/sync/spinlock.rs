// sync/spinlock.rs — Spinlock with interrupt-safe locking
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// A spinlock that disables interrupts on the current CPU while held.
///
/// This prevents deadlocks when an interrupt handler tries to acquire
/// the same lock — since interrupts are disabled, the handler cannot
/// preempt the lock holder on the same core.
pub struct Spinlock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

/// Guard returned by [`Spinlock::lock`]. Releases the lock on drop.
pub struct SpinlockGuard<'a, T> {
    lock: &'a Spinlock<T>,
    /// Whether interrupts were enabled before we acquired the lock.
    were_enabled: bool,
}

// SAFETY: Spinlock provides mutual exclusion. T is only accessible
// through `lock()` which guarantees exclusive access.
unsafe impl<T: Send> Send for Spinlock<T> {}
unsafe impl<T: Send> Sync for Spinlock<T> {}

impl<T> Spinlock<T> {
    /// Create a new unlocked spinlock.
    pub const fn new(data: T) -> Self {
        Spinlock {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    /// Acquire the spinlock, disabling interrupts.
    ///
    /// Spins until the lock is available. Returns a guard that
    /// automatically releases the lock and restores the interrupt state.
    pub fn lock(&self) -> SpinlockGuard<'_, T> {
        let were_enabled = crate::arch::cpu::interrupts_enabled();
        // Disabling interrupts prevents deadlocks when an interrupt handler
        // tries to acquire the same lock on this CPU.
        crate::arch::cpu::disable_interrupts();

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

    /// Try to acquire the spinlock without blocking.
    pub fn try_lock(&self) -> Option<SpinlockGuard<'_, T>> {
        let were_enabled = crate::arch::cpu::interrupts_enabled();
        // See lock() rationale.
        crate::arch::cpu::disable_interrupts();

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
            // Restore interrupt state on failure: we disabled them above,
            // so re-enable only if they were enabled before.
            if were_enabled {
                crate::arch::cpu::enable_interrupts();
            }
            None
        }
    }

    fn unlock(&self, were_enabled: bool) {
        self.locked.store(false, Ordering::Release);
        if were_enabled {
            // Interrupts were enabled before lock acquisition; restore them.
            crate::arch::cpu::enable_interrupts();
        }
    }
}

impl<T> Deref for SpinlockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        // SAFETY: Exclusive access guaranteed by the guard.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinlockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: Exclusive access guaranteed by the guard.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinlockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.unlock(self.were_enabled);
    }
}
