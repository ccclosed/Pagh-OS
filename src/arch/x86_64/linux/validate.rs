//! Pure user-pointer range validation for the Linux compatibility layer.
//!
//! Pure, allocation-free, `core`-only logic shared with the `host-tests` crate
//! (R11.6). This is the *range* half of the syscall pointer choke point: it rejects
//! buffers that start at or above the user/kernel boundary, that overflow when their
//! length is added, or whose end exceeds the boundary — all **without dereferencing**
//! anything (R1.5). The effectful *page-presence* walk (R1.6) lives in a later task
//! and enumerates the same page bases produced by [`spanned_pages`].
#![allow(dead_code)]

/// Page size used for the spanned-page enumeration (x86_64 4 KiB pages).
const PAGE_SIZE: u64 = 4096;

/// Exclusive upper bound of the lower-half canonical user address range
/// (`User_Addr_Max`). A buffer is in range only when it lies strictly below this.
///
/// NOTE: this is intentionally a standalone copy rather than a re-export of the
/// `const` in `src/vfs/elf.rs`. That module pulls in kernel-only dependencies
/// (the VMM/PMM and the `x86_64` paging crate), so importing it would make this
/// pure module — and therefore the `host-tests` crate that `#[path]`-includes it —
/// impossible to build on the host. The value is fixed by the architecture.
pub const USER_ADDR_MAX: u64 = 0x0000_8000_0000_0000;

/// Outcome of a pure user-pointer range check.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PtrCheck {
    /// The range is wholly within the user half (or empty) and may be walked.
    Ok,
    /// The range is out of bounds; the syscall must return `-EFAULT` (R1.5).
    Efault,
}

/// Pure range validation for a user buffer `[start, start + len)`.
///
/// Returns [`PtrCheck::Ok`] when `len == 0` (an empty buffer is always acceptable
/// and dereferences nothing). Otherwise returns [`PtrCheck::Efault`] when:
///   * `start >= USER_ADDR_MAX`,
///   * `start + len` overflows `u64`, or
///   * `start + len > USER_ADDR_MAX`.
/// In every other case it returns [`PtrCheck::Ok`]. This function never reads from
/// the supplied address (R1.5).
pub fn check_user_range(start: u64, len: u64) -> PtrCheck {
    if len == 0 {
        return PtrCheck::Ok;
    }
    if start >= USER_ADDR_MAX {
        return PtrCheck::Efault;
    }
    match start.checked_add(len) {
        Some(end) if end <= USER_ADDR_MAX => PtrCheck::Ok,
        _ => PtrCheck::Efault,
    }
}

/// Pure enumerator of the 4 KiB page bases spanned by `[start, start + len)`.
///
/// For `len == 0` the iterator is empty. For `len > 0` it yields every page base
/// from `first_page` (the page containing `start`) through `last_page` (the page
/// containing the final byte `start + len - 1`) inclusive, stepping by
/// [`PAGE_SIZE`]. The effectful page-presence walk and the property test use this
/// to confirm each spanned page is mapped (R1.6).
///
/// Inputs are expected to have already passed [`check_user_range`], so
/// `start + len - 1` does not overflow; the saturating arithmetic keeps the
/// enumerator total regardless.
pub fn spanned_pages(start: u64, len: u64) -> impl Iterator<Item = u64> {
    // Choose bounds so the inclusive range is empty when `len == 0`.
    let (first_page, last_page) = if len == 0 {
        // `1..=0` is an empty RangeInclusive, so no page bases are produced.
        (1, 0)
    } else {
        let last_addr = start.saturating_add(len - 1);
        (page_base(start), page_base(last_addr))
    };
    (first_page..=last_page).step_by(PAGE_SIZE as usize)
}

/// Round an address down to its containing page base.
#[inline]
fn page_base(addr: u64) -> u64 {
    addr & !(PAGE_SIZE - 1)
}
