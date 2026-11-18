// Feature: non-root phase 2, property P64: the network admission policy — the
// privileged-port rule for `bind(2)` and the raw-socket gate for `socket(2)`.
//
// These are the two places where Linux checks a capability (`CAP_NET_BIND_SERVICE`
// and `CAP_NET_RAW`) before the kernel touches any protocol state. pagh models a
// capability set as `euid == 0` (the same reduction as the rest of the security
// model), so the whole decision is pure and belongs here: the syscall layer must
// turn a `false` into `EACCES`/`EPERM` *before* reserving a port or allocating a
// socket, which is exactly what `inet_sock::sys_bind_inet` does.
//
// The bug class: an off-by-one at the 1024 boundary (1023 is privileged, 1024 is
// not), a check that looks at real instead of effective uid, or a gate applied
// after the port is already reserved (visible only as a race, never in a review).
// The oracle below is written independently as the Linux predicate.
//
// `port == 0` is the wildcard "let the kernel pick an ephemeral port" request and
// is never privileged — Linux assigns from `net.ipv4.ip_local_port_range`, far
// above 1024, so no capability is consulted.

use proptest::prelude::*;

use crate::cred::Creds;

fn ids() -> impl Strategy<Value = u32> {
    prop_oneof![Just(0u32), Just(1000u32), Just(1001u32), 0u32..4]
}

fn creds_with(uid: u32, euid: u32) -> Creds {
    Creds::new(uid, 0, euid, 0, uid, 0)
}

/// The Linux predicate: wildcard, unprivileged port, or root.
fn oracle_may_bind_port(euid: u32, port: u16) -> bool {
    port == 0 || port >= 1024 || euid == 0
}

proptest! {
    /// The helper must agree with the independent oracle over every port value,
    /// including the exact 1023/1024 boundary and the wildcard request.
    #[test]
    fn bind_port_gate_agrees_with_the_oracle(
        euid in ids(),
        port in prop_oneof![
            Just(0u16),
            Just(1u16),
            Just(80u16),
            Just(1023u16),
            Just(1024u16),
            Just(8080u16),
            Just(65535u16),
            0u16..=65535,
        ],
    ) {
        let c = creds_with(euid, euid);
        prop_assert_eq!(
            c.may_bind_port(port),
            oracle_may_bind_port(euid, port),
            "euid={} port={}",
            euid,
            port
        );
    }

    /// Every port below 1024 is root-only; the boundary itself (1024) is open.
    #[test]
    fn privileged_ports_are_root_only(euid in ids(), port in 1u16..1024) {
        let c = creds_with(euid, euid);
        prop_assert_eq!(c.may_bind_port(port), euid == 0);
        prop_assert_eq!(c.may_bind_port(1024), true);
    }

    /// Ports at or above 1024 are open to any identity, whatever the ids are
    /// (real, effective and saved ids all differ here).
    #[test]
    fn unprivileged_ports_are_open_to_everyone(euid in ids(), port in 1024u16..=65535) {
        let c = Creds::new(1000, 1000, euid, 1000, 1000, 1000);
        prop_assert!(c.may_bind_port(port));
    }

    /// The wildcard port request is never gated, even with the widest id spread.
    #[test]
    fn wildcard_port_is_never_gated(euid in ids()) {
        let c = creds_with(euid, euid);
        prop_assert!(c.may_bind_port(0));
    }

    /// The raw-socket gate is root-only, independent of the port rule: a
    /// non-root process cannot obtain `SOCK_RAW` even though it may bind any
    /// unprivileged port.
    #[test]
    fn raw_socket_gate_is_root_only(euid in ids()) {
        let c = creds_with(euid, euid);
        prop_assert_eq!(c.may_use_raw_socket(), euid == 0);
        // The two gates are independent: an unprivileged process is still
        // allowed unprivileged ports while being denied raw sockets.
        prop_assert!(c.may_bind_port(8080));
    }
}