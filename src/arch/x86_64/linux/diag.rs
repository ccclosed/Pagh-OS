//! Diagnostics pure logic for the x86_64 binary-compatibility layer.
//!
//! Pure, `core`+`alloc`-only logic shared with the `host-tests` crate (R11.6).
//! Covers two observability rules:
//!   * nosys de-duplication (R12.2 / Property 27): a per-process record of which
//!     unsupported syscall numbers have already been logged, so the `-ENOSYS`
//!     diagnostic is emitted at most once per distinct number per process.
//!   * exit-code normalization (R12.3 / Property 28): the recorded/logged exit
//!     code is the low byte of the requested code, hence always in `0..=255`.
//!
//! Both functions are free of hardware access, privileged instructions, and global
//! mutable state — all inputs arrive as parameters (the caller owns the per-process
//! `BTreeSet`), so the `host-tests` crate can assert their behavior on the host.
#![allow(dead_code)]

use alloc::collections::BTreeSet;

/// Decide whether the `-ENOSYS` diagnostic should be logged for syscall number
/// `nr`, recording the decision in the per-process `seen` set (R12.2 / P27).
///
/// Returns `true` exactly once per distinct `nr` — on its first occurrence — and
/// `false` for every subsequent occurrence of that same number. This is precisely
/// [`BTreeSet::insert`]'s contract (`true` when the value was newly inserted), so
/// the caller emits the log entry iff this returns `true`.
pub fn should_log_nosys(seen: &mut BTreeSet<u64>, nr: u64) -> bool {
    seen.insert(nr)
}

/// Normalize a requested process exit code to the value Linux reports to a waiter
/// (R12.3 / P28).
///
/// Linux passes the exit status through the low 8 bits, so the recorded exit code
/// is `code & 0xFF`, which always lies in `0..=255`. The requested code is taken as
/// `u64` (the raw syscall argument register width); only the low byte is
/// significant, so any wider bits are discarded.
pub fn exit_code_byte(code: u64) -> u8 {
    (code & 0xFF) as u8
}
