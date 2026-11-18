// Feature: `shutdown(2)` at nr 48 (task-81), property `p60`: the pure
// direction-closing policy in `src/arch/x86_64/linux/sock_shutdown.rs`.
//
// Why this exists: `shutdown` is the half-close that `close` cannot express. A
// client that finished writing had only `close`, which also destroys the
// reading half; the peer therefore never saw a clean `SHUT_WR` EOF while the
// connection was still usable in the other direction. The kernel now sets a
// per-direction flag on the AF_UNIX queue (or orders a FIN in the TCP stack)
// and every read/write/poll path asks this module what that flag means, so a
// blocked peer wakes up instead of spinning in `yield_current()`.
//
// The errno order is the part that silently rots: Linux finds the descriptor
// first (`EBADF`), then its kind (`ENOTSOCK`), then validates `how` (`EINVAL`),
// and only then looks at the connection (`ENOTCONN`, TCP/UDP only). It was
// verified on the host with a live kernel probe (`pipe` + `how=99` is
// `ENOTSOCK`, unconnected TCP + `how=99` is `EINVAL`, an unconnected AF_UNIX
// socket is a successful no-op), and these tests pin that table.

use crate::errno::Errno;
use crate::sock_shutdown::{
    plan, read_at_eof, read_ready, write_alive, write_ready, Action, FdKind, How, SHUT_RD,
    SHUT_RDWR, SHUT_WR,
};
use proptest::prelude::*;

fn kinds() -> impl Strategy<Value = FdKind> {
    prop_oneof![
        Just(FdKind::Absent),
        Just(FdKind::NotSocket),
        Just(FdKind::UnixConnected),
        Just(FdKind::UnixUnconnected),
        Just(FdKind::TcpConnected),
        Just(FdKind::TcpUnconnected),
        Just(FdKind::Udp),
    ]
}

proptest! {
    /// Descriptor-less always wins, whatever `how` says — `sockfd_lookup` runs
    /// before any protocol code.
    #[test]
    fn absent_fd_is_ebadf(how in any::<u64>()) {
        prop_assert_eq!(plan(FdKind::Absent, how), Err(Errno::EBADF));
    }

    /// A non-socket descriptor is `ENOTSOCK`, even with a garbage `how`
    /// (host probe: pipe + how=99 -> ENOTSOCK).
    #[test]
    fn non_socket_is_enotsock(how in any::<u64>()) {
        prop_assert_eq!(plan(FdKind::NotSocket, how), Err(Errno::ENOTSOCK));
    }

    /// `how` outside 0/1/2 is `EINVAL` for every socket kind, including the
    /// unconnected ones (host probe: unconnected TCP + how=99 -> EINVAL).
    #[test]
    fn bad_how_is_einval(kind in kinds(), how in any::<u64>()) {
        prop_assume!(how > SHUT_RDWR);
        if kind == FdKind::Absent || kind == FdKind::NotSocket {
            return Ok(());
        }
        prop_assert_eq!(plan(kind, how), Err(Errno::EINVAL));
    }

    /// An unconnected AF_UNIX socket has nothing to close and Linux succeeds
    /// (it just checks `how`), while TCP/UDP report `ENOTCONN`.
    #[test]
    fn connection_state_after_how(how in 0u64..=SHUT_RDWR) {
        prop_assert_eq!(plan(FdKind::UnixUnconnected, how), Ok(Action::Noop));
        prop_assert_eq!(plan(FdKind::TcpUnconnected, how), Err(Errno::ENOTCONN));
        prop_assert_eq!(plan(FdKind::Udp, how), Err(Errno::ENOTCONN));
    }

    /// A connected socket maps `how` onto the two direction bits, and only
    /// those bits: `SHUT_RD` never touches the write half and vice versa.
    #[test]
    fn connected_sockets_map_how_to_directions(how in 0u64..=SHUT_RDWR) {
        let parsed = How::parse(how).unwrap();
        let unix = plan(FdKind::UnixConnected, how).unwrap();
        let tcp = plan(FdKind::TcpConnected, how).unwrap();
        match unix {
            Action::Unix { read, write } => {
                prop_assert_eq!(read, parsed.shuts_read());
                prop_assert_eq!(write, parsed.shuts_write());
            }
            _ => prop_assert!(false, "AF_UNIX chose a non-Unix action"),
        }
        match tcp {
            Action::Tcp { read, write } => {
                prop_assert_eq!(read, parsed.shuts_read());
                prop_assert_eq!(write, parsed.shuts_write());
            }
            _ => prop_assert!(false, "TCP chose a non-TCP action"),
        }
    }
}

/// The documented decision table, spelled out so no future edit can reorder the
/// checks (the natural mistake: validating `how` first, which turns a non-socket
/// with a garbage `how` into `EINVAL` instead of `ENOTSOCK`).
#[test]
fn errno_order_table_is_exhaustive() {
    assert_eq!(plan(FdKind::Absent, SHUT_RD), Err(Errno::EBADF));
    assert_eq!(plan(FdKind::Absent, 7), Err(Errno::EBADF));
    assert_eq!(plan(FdKind::NotSocket, SHUT_RD), Err(Errno::ENOTSOCK));
    assert_eq!(plan(FdKind::NotSocket, 7), Err(Errno::ENOTSOCK));
    assert_eq!(plan(FdKind::UnixConnected, 7), Err(Errno::EINVAL));
    assert_eq!(plan(FdKind::TcpUnconnected, 7), Err(Errno::EINVAL));
    assert_eq!(plan(FdKind::TcpUnconnected, SHUT_WR), Err(Errno::ENOTCONN));
    assert_eq!(plan(FdKind::Udp, SHUT_RDWR), Err(Errno::ENOTCONN));
    assert_eq!(plan(FdKind::UnixUnconnected, SHUT_RDWR), Ok(Action::Noop));
}

/// Both halves independently: after `SHUT_WR` a reader sees EOF while the
/// writer keeps *its own* read side open; after `SHUT_RD` a writer sees EPIPE
/// while the reader keeps its own write side open.
#[test]
fn half_close_is_one_direction_only() {
    // SHUT_WR: reader at EOF, writer's own receive side untouched.
    assert!(read_at_eof(1, false, true));
    assert!(read_ready(false, 1, false, true));
    assert!(!write_alive(1, false, true));

    // SHUT_RD: writer sees EPIPE, reader's own send side untouched.
    assert!(!write_alive(1, true, false));
    assert!(!write_ready(1, true, false, true));
    assert!(read_at_eof(1, true, false));

    // Untouched queue: neither side is closed.
    assert!(!read_at_eof(1, false, false));
    assert!(write_alive(1, false, false));
    assert!(write_ready(1, false, false, true));
    assert!(!write_ready(1, false, false, false));
}

proptest! {
    /// Closing either direction is enough to break the opposite half; only a
    /// fully healthy queue stays writable. (`write_alive` must never be true
    /// when *any* shutdown flag is set — that is the flagged-write EPIPE.)
    #[test]
    fn write_alive_requires_both_halves(
        readers in 0usize..=4,
        read_shut in any::<bool>(),
        write_shut in any::<bool>(),
    ) {
        let alive = write_alive(readers, read_shut, write_shut);
        prop_assert_eq!(alive, !read_shut && !write_shut && readers > 0);
        if alive {
            prop_assert!(read_ready(true, readers, read_shut, write_shut));
        }
    }

    /// A reader at EOF must never be reported as "no data yet" — otherwise the
    /// blocked peer loops forever instead of returning `Ok(0)`.
    #[test]
    fn eof_is_read_ready(
        writers in 0usize..=4,
        read_shut in any::<bool>(),
        write_shut in any::<bool>(),
    ) {
        if read_at_eof(writers, read_shut, write_shut) {
            prop_assert!(read_ready(false, writers, read_shut, write_shut));
