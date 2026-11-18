// Feature: blocking AF_UNIX read wakeup (watchdog `nr=45 arg0=0x3 closed`),
// property `p58`: the `sock_wait` verdict the kernel takes on every yielded CPU
// slice of `recvfrom`/`recvmsg`/`read` on a socket queue.
//
// Why this exists: there are no wait queues here — a blocking read loops on
// `yield_current()` and re-reads the byte queue. `readers`/`writers` are counted
// per QUEUE (`src/task/fd.rs`), not per fd, and the syscall itself holds an
// `Arc` to the queue, so closing the READING fd from a sibling thread neither
// drops a counter nor produces EOF: `read` answers `WouldBlock` forever. Real
// serial logs show exactly that shape — two processes stuck in `recvfrom` on
// descriptors their own table already reports "closed" (`serial_shell.log`,
// `serial_desktop.log`). The kernel now asks two questions per yield and this
// property pins the answer:
//
//   * does any fd in the process still refer to this queue? (`holds_read_end`)
//     No  -> `EBADF`: nobody can consume bytes written there any more;
//   * are there live writing ends? (`writers`)
//     No  -> `Ok(0)`: EOF, the normal peer-close path.
//
// The interesting corner is exactly the one the old code got wrong: an orphaned
// queue may still have `writers > 0` (the peer process is alive), so ownership
// must dominate the writers count — `EBADF`, never a silent eternal wait.

use crate::sock_wait::{wait_verdict, write_verdict, WaitVerdict, WriteVerdict};
use proptest::prelude::*;

proptest! {
    /// The regression itself: a closed fd (no fd references the queue) is never
    /// "keep waiting", no matter how many writing ends the queue still counts.
    #[test]
    fn closed_fd_never_keeps_waiting(writers in any::<usize>()) {
        prop_assert_eq!(wait_verdict(false, writers), WaitVerdict::Ebadf);
    }

    /// A queue still owned by a descriptor with live writers still waits — the
    /// fix must not turn ordinary blocked reads into spurious EBADF.
    #[test]
    fn owned_queue_with_writers_keeps_waiting(writers in 1usize..=usize::MAX) {
        prop_assert_eq!(wait_verdict(true, writers), WaitVerdict::Continue);
    }

    /// Peer close (last writing end gone) on an owned queue is EOF, not EAGAIN
    /// and not a wait.
    #[test]
    fn peer_close_is_eof(referenced in any::<bool>()) {
        let v = wait_verdict(referenced, 0);
        prop_assert_ne!(v, WaitVerdict::Continue);
        if referenced {
            prop_assert_eq!(v, WaitVerdict::Eof);
        }
    }

    /// Ownership dominates: raising the writers count can never turn an
    /// orphaned queue into a wait, and lowering it can never turn an owned
    /// queue into EBADF. In other words the verdict is monotone in `writers`
    /// and depends on ownership before the writer count.
    #[test]
    fn verdict_is_monotone_in_writers(
        referenced in any::<bool>(),
        a in any::<usize>(),
        b in any::<usize>(),
    ) {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let v_lo = wait_verdict(referenced, lo);
        let v_hi = wait_verdict(referenced, hi);
        // Only `Continue`/`Eof` for an owned queue, only `Ebadf` for an
        // orphaned one — across the whole writers range.
        prop_assert_eq!(v_lo == WaitVerdict::Continue, v_hi == WaitVerdict::Continue);
        prop_assert_eq!(v_lo == WaitVerdict::Ebadf, v_hi == WaitVerdict::Ebadf);
        if !referenced {
            prop_assert_eq!(v_lo, WaitVerdict::Ebadf);
            prop_assert_eq!(v_hi, WaitVerdict::Ebadf);
        }
    }
}

/// The complete 2x2 decision table, so no future edit can quietly reorder the
/// ownership and writer checks (the natural mistake: `writers > 0` first, which
/// reintroduces the eternal sleep for an orphaned queue with a live peer).
#[test]
fn decision_table_is_exhaustive() {
    assert_eq!(wait_verdict(false, 0), WaitVerdict::Ebadf);
    assert_eq!(wait_verdict(false, 1), WaitVerdict::Ebadf);
    assert_eq!(wait_verdict(true, 0), WaitVerdict::Eof);
    assert_eq!(wait_verdict(true, 1), WaitVerdict::Continue);
    // Multi-writer queues (socketpair ends inherited by a child) stay waits.
    assert_eq!(wait_verdict(true, 3), WaitVerdict::Continue);
}
// ─── Пишущая сторона (SUBFIX 3): закрытие пишущего fd во время ожидания ───

proptest! {
    /// The write-side regression itself: a write fd closed from a sibling
    /// thread must end the wait with `EBADF`, never keep polling a queue whose
    /// counters cannot change (the syscall holds the queue's `Arc`).
    #[test]
    fn closed_write_fd_never_keeps_waiting(referenced in any::<bool>()) {
        if referenced {
            prop_assert_eq!(write_verdict(true), WriteVerdict::Continue);
        } else {
            prop_assert_eq!(write_verdict(false), WriteVerdict::Ebadf);
        }
    }
}

/// The two-row write table. Ownership is the ONLY question the policy asks:
/// the peer-gone case (`EPIPE`) is decided by the queue itself, so it must not
/// leak into this verdict.
#[test]
fn write_decision_table_is_exhaustive() {
    assert_eq!(write_verdict(true), WriteVerdict::Continue);
    assert_eq!(write_verdict(false), WriteVerdict::Ebadf);
}
