// memory/pmm.rs — Physical Memory Manager (bitmap allocator)
// 64-bit x86_64 OS kernel in Rust (#![no_std])

use crate::sync::spinlock::Spinlock;
use limine::memmap;
use limine::request::MemmapResponse;

const FRAME_SIZE: u64 = 4096;

/// Physical Memory Manager state (bitmap allocator).
///
/// All PMM state lives here, behind the single [`PMM`] spinlock. The bitmap
/// is placed in usable RAM during [`init`] and accessed via the HHDM mapping,
/// so the `&'static mut [u64]` is valid for the lifetime of the kernel.
struct Pmm {
    /// Bitmap: one bit per 4KB frame. 1 = free, 0 = used.
    bitmap: &'static mut [u64],
    /// Total number of frames tracked.
    total_frames: usize,
    /// Number of free frames.
    free_count: usize,
    /// Base physical address the bitmap starts tracking from.
    base_addr: u64,
    /// Highest address tracked + 1.
    top_addr: u64,
}

/// Global PMM state, protected by a single spinlock.
///
/// `None` until [`init`] runs. The lock provides exclusive access, which is
/// what makes holding a `&'static mut [u64]` inside sound across callers.
static PMM: Spinlock<Option<Pmm>> = Spinlock::new(None);

/// Initialize the Physical Memory Manager from the Limine memory map.
pub fn init(memmap: &MemmapResponse) {
    let entries = memmap.entries();

    // Find the address range: lowest usable base and highest usable top.
    let mut base = u64::MAX;
    let mut top = 0u64;
    let mut total_usable = 0u64;

    // First pass: determine the total range and usable memory size.
    for entry in entries {
        if entry.type_ == memmap::MEMMAP_USABLE {
            if entry.base < base { base = entry.base; }
            let entry_top = entry.base + entry.length;
            if entry_top > top { top = entry_top; }
            total_usable += entry.length;
        }
    }

    if base == u64::MAX {
        crate::error!("[PMM] no usable memory found!");
        return;
    }

    // Align base down and top up to FRAME_SIZE.
    base &= !(FRAME_SIZE - 1);
    top = (top + FRAME_SIZE - 1) & !(FRAME_SIZE - 1);

    let total_frames = ((top - base) / FRAME_SIZE) as usize;
    // Bitmap size in bytes: total_frames / 8, rounded up to 8-byte alignment.
    let bitmap_words = (total_frames + 63) / 64;
    let bitmap_bytes = bitmap_words * 8;

    crate::debug!("Memory range: 0x{:x}..0x{:x} ({} MB)", base, top, (top - base) / (1024 * 1024));
    crate::debug!("Total frames: {}, bitmap: {} bytes", total_frames, bitmap_bytes);

    // Find a usable region large enough to hold the bitmap.
    // Place it in the largest usable region to avoid wasting kernel-adjacent memory.
    let mut bitmap_phys_addr: u64 = 0;
    for entry in entries {
        if entry.type_ == memmap::MEMMAP_USABLE {
            if entry.length >= bitmap_bytes as u64 {
                // Place the bitmap 2 MB into this region. The bitmap's own
                // frames are reserved explicitly below (the
                // bitmap-own-frames skip), so this offset is just a placement
                // choice — it no longer depends on any kernel-location
                // assumption now that kernel frames are reserved by their
                // non-usable memmap classification.
                let candidate = (entry.base + 0x200000) & !(FRAME_SIZE - 1); // 2MB offset
                if candidate + bitmap_bytes as u64 <= entry.base + entry.length {
                    bitmap_phys_addr = candidate;
                    break;
                }
                // Fallback: use start of region
                bitmap_phys_addr = entry.base;
                break;
            }
        }
    }

    if bitmap_phys_addr == 0 {
        // Last resort: truncate bitmap and use start of first usable region
        crate::warn!("[PMM] no region large enough for full bitmap, truncating");
        bitmap_phys_addr = base;
    }

    crate::debug!("Bitmap at physical: 0x{:x}", bitmap_phys_addr);

    // Map the bitmap into kernel address space via HHDM.
    let hhdm = crate::HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed);
    let bitmap_virt = (bitmap_phys_addr + hhdm) as *mut u64;
    let bitmap_slice: &'static mut [u64];

    // SAFETY: The bitmap region is in usable RAM, so accessing it via HHDM
    // is valid. We zero it first.
    unsafe {
        let slice_ptr = core::slice::from_raw_parts_mut(bitmap_virt, bitmap_words);
        slice_ptr.fill(0);
        bitmap_slice = slice_ptr;
    }

    // Mark all frames as used initially, then free usable regions.
    let mut pmm = Pmm {
        bitmap: bitmap_slice,
        total_frames,
        free_count: 0,
        base_addr: base,
        top_addr: top,
    };

    // ─── Reservation logic (frame free/reserve) ──────────────────────────
    // Precise reservation (task 6.2). There are only two *explicit*
    // reservations here:
    //   1. Below 1 MB (`addr < 0x100000`): low memory holds the real-mode IVT,
    //      BIOS data area, and assorted legacy/firmware structures. These must
    //      never be handed out (Property 2), so we skip every frame below 1 MB.
    //   2. The bitmap's own physical frames (skipped below and re-asserted as
    //      used in the follow-up pass).
    //
    // The kernel image itself is reserved *implicitly* by classification, not
    // by any magic address threshold: this loop only ever frees frames that
    // Limine reports as MEMMAP_USABLE, and the kernel's physical frames are
    // reported as MEMMAP_KERNEL_AND_MODULES (not usable). They are therefore
    // never freed, and remain marked used (0) — so no address within the
    // reserved kernel image range can ever be returned by `alloc_frame`
    // (Property 2). The same holds for MMIO/reserved/ACPI regions. This
    // replaces the old crude "first 8 MB" (`addr < 0x800000`) over-reservation,
    // which needlessly withheld usable RAM above the kernel.

    // Free frames in usable regions, BUT protect the bitmap itself.
    for entry in entries {
        if entry.type_ == memmap::MEMMAP_USABLE {
            let region_start = entry.base;
            let region_end = entry.base + entry.length;

            // Mark frames in this region as free, except those used by the bitmap.
            let frame_start = align_up(region_start, FRAME_SIZE);
            let frame_end = align_down(region_end, FRAME_SIZE);

            let mut addr = frame_start;
            while addr < frame_end {
                // (1) Reserve everything below 1 MB (legacy/BIOS/IVT).
                if addr < 0x100000 {
                    addr += FRAME_SIZE;
                    continue;
                }
                // (2) Skip the bitmap's own physical frames.
                if addr >= bitmap_phys_addr && addr < bitmap_phys_addr + bitmap_bytes as u64 {
                    addr += FRAME_SIZE;
                    continue;
                }

                let idx = ((addr - base) / FRAME_SIZE) as usize;
                let word = idx / 64;
                let bit = idx % 64;
                if word < pmm.bitmap.len() {
                    pmm.bitmap[word] |= 1u64 << bit;
                    pmm.free_count += 1;
                }
                addr += FRAME_SIZE;
            }
        }
    }

    // Also mark the bitmap's own frames as used.
    let mut bm_addr = bitmap_phys_addr;
    while bm_addr < bitmap_phys_addr + bitmap_bytes as u64 {
        let idx = ((bm_addr - base) / FRAME_SIZE) as usize;
        let word = idx / 64;
        let bit = idx % 64;
        if word < pmm.bitmap.len() {
            if pmm.bitmap[word] & (1u64 << bit) != 0 {
                pmm.bitmap[word] &= !(1u64 << bit);
                pmm.free_count -= 1;
            }
        }
        bm_addr += FRAME_SIZE;
    }

    let free_count = pmm.free_count;
    let total = pmm.total_frames;

    // Publish the fully-built state. Keep logging outside the locked section.
    *PMM.lock() = Some(pmm);

    crate::debug!("PMM Initialized: {} / {} frames free ({} MB / {} MB)",
        free_count, total,
        (free_count as u64 * FRAME_SIZE) / (1024 * 1024),
        (total as u64 * FRAME_SIZE) / (1024 * 1024),
    );

    let _ = total_usable;
}

/// Allocate a single physical frame (4KB).
/// Returns the physical address of the frame, or None if out of memory.
pub fn alloc_frame() -> Option<u64> {
    let mut guard = PMM.lock();
    let pmm = match *guard {
        Some(ref mut pmm) => pmm,
        None => return None, // Not initialized
    };

    let base_addr = pmm.base_addr;

    // Simple linear scan for a free frame.
    for word_idx in 0..pmm.bitmap.len() {
        let word = pmm.bitmap[word_idx];
        if word != 0 {
            let bit = word.trailing_zeros() as usize;
            pmm.bitmap[word_idx] &= !(1u64 << bit);
            pmm.free_count -= 1;

            let frame_idx = word_idx * 64 + bit;
            let addr = base_addr + (frame_idx as u64) * FRAME_SIZE;
            return Some(addr);
        }
    }

    None
}

/// Allocate `count` physically-contiguous 4 KiB frames.
///
/// Scans the bitmap for the first run of `count` consecutive *free* frames
/// (bit == 1), marks them all used (bit == 0), and returns the physical base
/// address of the run. Returns `None` if `count == 0`, the PMM is
/// uninitialized, or no contiguous run of `count` free frames exists.
///
/// Guarantees (mirroring [`alloc_frame`]):
/// - the returned base is 4096-byte page-aligned,
/// - the base is `>= 0x100000` — frames below 1 MB (and every other reserved
///   region: the kernel image, the bitmap's own frames, MMIO/ACPI) are marked
///   used at [`init`] time, so they can never appear inside a free run and are
///   never returned here,
/// - all `count` frames were previously free and are now used, so the run does
///   not overlap any other live allocation.
///
/// `count == 1` behaves exactly like [`alloc_frame`] (returns the first free
/// frame). Operates under the single [`PMM`] spinlock, leaving the existing
/// single-frame API untouched. Pair every successful call with exactly one
/// [`free_frames_contiguous`] of the same `base`/`count` to restore the free
/// count.
pub fn alloc_frames_contiguous(count: usize) -> Option<u64> {
    if count == 0 {
        return None;
    }

    let mut guard = PMM.lock();
    let pmm = match *guard {
        Some(ref mut pmm) => pmm,
        None => return None, // Not initialized
    };

    let total = pmm.total_frames;
    if count > total {
        return None;
    }

    // Linear scan for the first run of `count` consecutive free frames.
    //
    // Loop invariant: when `run_len > 0`, the frames in
    // `[run_start, run_start + run_len)` are all free for the current `run_len`.
    // Bits beyond `total_frames` in the final bitmap word are zero (used) and
    // are never scanned, so out-of-range frames cannot form part of a run.
    let mut run_start: usize = 0;
    let mut run_len: usize = 0;
    let mut found: Option<usize> = None;

    let mut idx = 0usize;
    while idx < total {
        let word = idx / 64;
        let bit = idx % 64;
        let is_free = (pmm.bitmap[word] >> bit) & 1 == 1;

        if is_free {
            if run_len == 0 {
                run_start = idx;
            }
            run_len += 1;
            if run_len == count {
                found = Some(run_start);
                break;
            }
        } else {
            run_len = 0;
        }
        idx += 1;
    }

    let start = found?;

    // Mark the whole run used.
    for i in start..start + count {
        let word = i / 64;
        let bit = i % 64;
        pmm.bitmap[word] &= !(1u64 << bit);
    }
    pmm.free_count -= count;

    let addr = pmm.base_addr + (start as u64) * FRAME_SIZE;
    Some(addr)
}

/// Free a `count`-frame physically-contiguous run previously returned by
/// [`alloc_frames_contiguous`].
///
/// Clears (marks free) `count` consecutive frames starting at `base`,
/// restoring the free-frame count by exactly the number of frames that were
/// actually used (idempotent per-frame, like [`free_frame`]). Out-of-range
/// frames are skipped. `base` must be the page-aligned base returned by a
/// matching `alloc_frames_contiguous(count)`.
pub fn free_frames_contiguous(base: u64, count: usize) {
    if count == 0 {
        return;
    }

    let mut guard = PMM.lock();
    let pmm = match *guard {
        Some(ref mut pmm) => pmm,
        None => return, // Not initialized
    };

    if base < pmm.base_addr || base >= pmm.top_addr {
        return; // Not in our range
    }

    let start = ((base - pmm.base_addr) / FRAME_SIZE) as usize;
    for i in start..start + count {
        if i >= pmm.total_frames {
            break;
        }
        let word = i / 64;
        let bit = i % 64;
        if pmm.bitmap[word] & (1u64 << bit) == 0 {
            pmm.bitmap[word] |= 1u64 << bit;
            pmm.free_count += 1;
        }
    }
}

/// Free a previously allocated physical frame.
pub fn free_frame(addr: u64) {
    let mut guard = PMM.lock();
    let pmm = match *guard {
        Some(ref mut pmm) => pmm,
        None => return, // Not initialized
    };

    if addr < pmm.base_addr || addr >= pmm.top_addr {
        return; // Not in our range
    }

    let idx = ((addr - pmm.base_addr) / FRAME_SIZE) as usize;
    let word = idx / 64;
    let bit = idx % 64;

    if word < pmm.bitmap.len() {
        if pmm.bitmap[word] & (1u64 << bit) == 0 {
            pmm.bitmap[word] |= 1u64 << bit;
            pmm.free_count += 1;
        }
    }
}

/// Total number of frames tracked.
pub fn total_frames() -> usize {
    match *PMM.lock() {
        Some(ref pmm) => pmm.total_frames,
        None => 0,
    }
}

/// Number of free frames.
pub fn free_frames() -> usize {
    match *PMM.lock() {
        Some(ref pmm) => pmm.free_count,
        None => 0,
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────

fn align_up(addr: u64, align: u64) -> u64 {
    (addr + align - 1) & !(align - 1)
}

fn align_down(addr: u64, align: u64) -> u64 {
    addr & !(align - 1)
}
