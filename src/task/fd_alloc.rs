//! Pure, host-testable file-descriptor bookkeeping (R2.4, R2.6, R2.14).
//!
//! The "lowest free descriptor >= 3" allocation rule and the EBADF-on-absent-fd
//! semantics are pure index arithmetic over a slot vector. They are factored out
//! here — `core` + `alloc` only, with **no** dependency on `VfsNode`, `Errno`, the
//! VMM, or any global mutable state — so the kernel-facing [`super::fd::FdTable`]
//! (whose `OpenObject` embeds `Arc<dyn VfsNode>`) stays out of the host build while
//! Property 7 can still exercise this logic with a dummy stored type `T` on the
//! host (R11.6).
//!
//! This module is the host-testable seam wired into `host-tests/src/lib.rs`.
#![allow(dead_code)]

use alloc::vec::Vec;

/// Marker error for an fd that refers to no open slot. The kernel-facing wrapper
/// maps this to `Errno::EBADF` (R2.6, R2.14); keeping it dependency-free here is
/// what lets this module compile into the host-tests crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadFd;

/// Return the lowest index `>= min` whose slot is free.
///
/// A slot is free when it is `Some(None)` (an explicitly empty slot) or lies past
/// the end of `slots` (`None` from `slice::get`). Occupied slots (`Some(Some(_))`)
/// are skipped. The returned index may equal or exceed `slots.len()`, meaning the
/// caller must grow the vector to store there. Pure and total (R2.4).
pub fn lowest_free_index<T>(slots: &[Option<T>], min: usize) -> usize {
    let mut i = min;
    loop {
        match slots.get(i) {
            // Occupied: keep scanning.
            Some(Some(_)) => i += 1,
            // Explicitly empty, or past the end: this index is free.
            Some(None) | None => return i,
        }
    }
}

/// Generic descriptor table: a vector of optional slots with lowest-free-index
/// allocation and EBADF-style absence reporting.
///
/// Carries no kernel or VFS dependencies, so it is host-testable for Property 7
/// with any dummy stored type `T`. The kernel composes it as `FdSlots<OpenObject>`
/// inside [`super::fd::FdTable`].
#[derive(Debug, Default)]
pub struct FdSlots<T> {
    slots: Vec<Option<T>>,
}

impl<T> FdSlots<T> {
    /// An empty table with no slots.
    pub fn new() -> Self {
        Self { slots: Vec::new() }
    }

    /// A table pre-seeded with the given slots (e.g. standard streams at 0/1/2).
    pub fn from_slots(slots: Vec<Option<T>>) -> Self {
        Self { slots }
    }

    /// Allocate the lowest free index `>= min`, growing the vector with empty
    /// slots as needed, store `obj` there, and return the index (R2.4).
    pub fn alloc(&mut self, min: usize, obj: T) -> u32 {
        let idx = lowest_free_index(&self.slots, min);
        if idx < self.slots.len() {
            self.slots[idx] = Some(obj);
        } else {
            // Grow with empty slots up to `idx`, then place `obj` at `idx`.
            while self.slots.len() < idx {
                self.slots.push(None);
            }
            self.slots.push(Some(obj));
        }
        idx as u32
    }

    /// Place `obj` at the explicit index `idx`, growing the vector with empty
    /// slots as needed and replacing (dropping) any object already there. Returns
    /// the object that previously occupied the slot, if any. Used by `dup2`/`dup3`
    /// which name an explicit target descriptor.
    pub fn set(&mut self, idx: u32, obj: T) -> Option<T> {
        let idx = idx as usize;
        while self.slots.len() <= idx {
            self.slots.push(None);
        }
        self.slots[idx].replace(obj)
    }

    /// Borrow the object at `fd`, or `None` for an out-of-range/empty slot
    /// (caller maps `None` -> EBADF, R2.14).
    pub fn get(&self, fd: u32) -> Option<&T> {
        self.slots.get(fd as usize).and_then(|slot| slot.as_ref())
    }

    /// Mutably borrow the object at `fd`, or `None` for an out-of-range/empty slot
    /// (caller maps `None` -> EBADF, R2.14).
    pub fn get_mut(&mut self, fd: u32) -> Option<&mut T> {
        self.slots.get_mut(fd as usize).and_then(|slot| slot.as_mut())
    }

    /// Free the slot at `fd`. Returns [`BadFd`] when the slot is out of range or
    /// already empty, leaving the table unchanged; otherwise clears the slot and
    /// returns `Ok` (R2.6, R2.14).
    pub fn close(&mut self, fd: u32) -> Result<(), BadFd> {
        match self.slots.get_mut(fd as usize) {
            Some(slot) if slot.is_some() => {
                *slot = None;
                Ok(())
            }
            _ => Err(BadFd),
        }
    }

    /// Number of slots currently tracked (including empty ones).
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether the table tracks no slots at all.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}
