//! Bitmap physical frame allocator (4 KiB frames).
//!
//! The bitmap is stored *in-place* at the start of the managed region (the
//! classic bootstrap trick that avoids needing a heap before the allocator
//! exists): the frames the bitmap itself occupies are marked used, the rest are
//! free. State is all-`usize` (the bitmap is addressed by its integer address),
//! so it lives behind a `spin::Mutex` without any `Send`/`Sync` gymnastics.

use spin::Mutex;

/// Frame size (4 KiB).
pub const FRAME_SIZE: usize = 4096;

struct Pmm {
    /// First managed physical address (frame 0), 4 KiB aligned.
    base: usize,
    /// Number of frames under management.
    frames: usize,
    /// Address of the in-place bitmap (1 bit per frame; 1 = used).
    bitmap: usize,
    /// Count of currently-free frames.
    free: usize,
}

impl Pmm {
    #[inline]
    fn test(&self, i: usize) -> bool {
        // SAFETY: `i < self.frames`, so the byte index is within the bitmap that
        // was reserved inside the managed region at init.
        let byte = unsafe { ((self.bitmap + i / 8) as *const u8).read_volatile() };
        (byte >> (i % 8)) & 1 != 0
    }

    #[inline]
    fn set_used(&mut self, i: usize) {
        let p = (self.bitmap + i / 8) as *mut u8;
        // SAFETY: as above; single-byte RMW of an owned bitmap byte.
        unsafe {
            let v = p.read_volatile() | (1u8 << (i % 8));
            p.write_volatile(v);
        }
    }

    #[inline]
    fn set_free(&mut self, i: usize) {
        let p = (self.bitmap + i / 8) as *mut u8;
        // SAFETY: as above.
        unsafe {
            let v = p.read_volatile() & !(1u8 << (i % 8));
            p.write_volatile(v);
        }
    }
}

static PMM: Mutex<Option<Pmm>> = Mutex::new(None);

/// Initialize the allocator over the physical range `[start, end)`. `start` is
/// rounded up to a frame boundary; the bitmap is placed at the (rounded) start
/// and the frames it covers are pre-marked used.
pub fn init(start: usize, end: usize) {
    let base = (start + FRAME_SIZE - 1) & !(FRAME_SIZE - 1);
    let end = end & !(FRAME_SIZE - 1);
    assert!(end > base, "pmm: empty region");

    let frames = (end - base) / FRAME_SIZE;
    let bitmap_bytes = frames.div_ceil(8);
    let bitmap = base;

    // Zero the bitmap (all free), then reserve the frames the bitmap occupies.
    // SAFETY: the bitmap lives within the managed region we own exclusively.
    unsafe {
        core::ptr::write_bytes(bitmap as *mut u8, 0, bitmap_bytes);
    }

    let mut pmm = Pmm {
        base,
        frames,
        bitmap,
        free: frames,
    };

    let bitmap_frames = (bitmap_bytes + FRAME_SIZE - 1) / FRAME_SIZE;
    for i in 0..bitmap_frames {
        pmm.set_used(i);
        pmm.free -= 1;
    }

    *PMM.lock() = Some(pmm);
}

/// Allocate one physical frame, returning its base address, or `None` when out
/// of memory. The returned frame is **not** zeroed.
pub fn alloc_frame() -> Option<usize> {
    let mut guard = PMM.lock();
    let pmm = guard.as_mut()?;
    for i in 0..pmm.frames {
        if !pmm.test(i) {
            pmm.set_used(i);
            pmm.free -= 1;
            return Some(pmm.base + i * FRAME_SIZE);
        }
    }
    None
}

/// Free a previously-allocated frame.
pub fn free_frame(addr: usize) {
    let mut guard = PMM.lock();
    if let Some(pmm) = guard.as_mut() {
        if addr >= pmm.base {
            let i = (addr - pmm.base) / FRAME_SIZE;
            if i < pmm.frames && pmm.test(i) {
                pmm.set_free(i);
                pmm.free += 1;
            }
        }
    }
}

/// `(free, total)` frame counts.
pub fn stats() -> (usize, usize) {
    let guard = PMM.lock();
    match guard.as_ref() {
        Some(p) => (p.free, p.frames),
        None => (0, 0),
    }
}
