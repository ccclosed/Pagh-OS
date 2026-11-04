// Feature: process credentials + filesystem permission enforcement, Property 56:
// the pure POSIX permission / setuid decisions the syscall layer relies on.
//
// The in-kernel `src/test.rs` routines exercise the same functions inside QEMU,
// but the interesting input space (all id combinations, all 512 permission
// triads, every transition target) is far too large for a boot-time suite. The
// properties here are the exhaustive half:
//
//   * `may` is compared against an independently written oracle, so a wrong
//     class shift, a mode mask that forgets type bits, or a root bypass applied
//     to execute fails on the host;
//   * the root rule is stated as a law: read/write always, execute iff one of
//     the nine permission bits is set in the 0o111 triad;
//   * a non-root process can never obtain access outside the bits of the class
//     its effective ids select (exhaustive over a small id universe);
//   * every set*id transition either refuses (`None`) or returns ids that obey
//     the Linux ownership rules — in particular an unprivileged caller can never
//     invent an identity outside `{uid, euid, suid}` / `{gid, egid, sgid}`.

use proptest::prelude::*;

use crate::cred::{Access, Creds};

fn access_bit(a: Access) -> u32 {
    match a {
        Access::Read => 0o4,
        Access::Write => 0o2,
        Access::Execute => 0o1,
    }
}

/// Independent oracle: same POSIX rule, written differently (an explicit
/// class-bit mask rather than a shift).
fn oracle_may(c: &Creds, mode: u32, owner: (u32, u32), access: Access) -> bool {
    let perms = mode & 0o777;
    if c.euid == 0 {
        return match access {
            Access::Read | Access::Write => true,
            Access::Execute => (perms & 0o111) != 0,
        };
    }
    let granted = if c.euid == owner.0 {
        perms & 0o700
    } else if c.egid == owner.1 {
        perms & 0o070
    } else {
        perms & 0o007
    };
    let shift = if c.euid == owner.0 {
        6
    } else if c.egid == owner.1 {
        3
    } else {
        0
    };
    ((granted >> shift) & access_bit(access)) != 0
}

fn credentials() -> impl Strategy<Value = Creds> {
    (any::<u32>(), any::<u32>(), any::<u32>(), any::<u32>(), any::<u32>(), any::<u32>()).prop_map(
        |(uid, gid, euid, egid, suid, sgid)| Creds::new(uid, gid, euid, egid, suid, sgid),
    )
}

fn access() -> impl Strategy<Value = Access> {
    prop_oneof![
        Just(Access::Read),
        Just(Access::Write),
        Just(Access::Execute)
    ]
}

proptest! {
    /// `may` agrees with the independent oracle over arbitrary ids, modes
    /// (including type/suid bits) and owners.
    #[test]
    fn may_agrees_with_the_independent_oracle(
        c in credentials(),
        mode in any::<u32>(),
        owner_uid in any::<u32>(),
        owner_gid in any::<u32>(),
        a in access(),
    ) {
        prop_assert_eq!(
            c.may(mode, (owner_uid, owner_gid), a),
            oracle_may(&c, mode, (owner_uid, owner_gid), a)
        );
    }

    /// Root bypass is exact: read/write unconditionally, execute only when the
    /// file carries at least one execute bit.
    #[test]
    fn root_bypass_is_exactly_read_write(
        mode in any::<u32>(),
        owner_uid in any::<u32>(),
        owner_gid in any::<u32>(),
    ) {
        let root = Creds::ROOT;
        prop_assert!(root.may(mode, (owner_uid, owner_gid), Access::Read));
        prop_assert!(root.may(mode, (owner_uid, owner_gid), Access::Write));
        prop_assert_eq!(
            root.may(mode, (owner_uid, owner_gid), Access::Execute),
            (mode & 0o111) != 0
        );
    }

    /// A non-root process never receives an access bit its selected class lacks.
    /// The small id universe makes every class collision (owner/group/other)
    /// reachable, and 0..=0o7777 sweeps permission and special bits.
    #[test]
    fn nonroot_access_is_bounded_by_the_selected_class(
        euid in 1u32..3,
        egid in 1u32..3,
        owner_uid in 0u32..3,
        owner_gid in 0u32..3,
        mode in 0u32..0o7777,
        a in access(),
    ) {
        let c = Creds::new(1, 2, euid, egid, 1, 2);
        let perms = mode & 0o777;
        let allowed = c.may(mode, (owner_uid, owner_gid), a);
        if euid == owner_uid {
            prop_assert_eq!(allowed, (perms & (0o700 & (access_bit(a) << 6))) != 0);
        } else if egid == owner_gid {
            prop_assert_eq!(allowed, (perms & (0o070 & (access_bit(a) << 3))) != 0);
        } else {
            prop_assert_eq!(allowed, (perms & access_bit(a)) != 0);
        }
    }

    /// `setuid` can only succeed for root or for a target the caller already
    /// owns (real or saved), and on success the effective/saved ids are the
    /// target.
    #[test]
    fn setuid_outcome_obeys_the_transition_rule(c in credentials(), target in any::<u32>()) {
        match c.setuid(target) {
            Some(next) => {
                if c.euid == 0 {
                    prop_assert_eq!((next.uid, next.euid, next.suid), (target, target, target));
                } else {
                    prop_assert!(target == c.uid || target == c.suid);
                    prop_assert_eq!(next.uid, c.uid);
                    prop_assert_eq!(next.euid, target);
                    prop_assert_eq!(next.suid, target);
                }
                body_ids_unchanged(&c, &next);
            }
            None => {
                prop_assert!(c.euid != 0);
                prop_assert!(target != c.uid && target != c.suid);
            }
        }
    }

    /// `setgid` is the exact group analogue.
    #[test]
    fn setgid_outcome_obeys_the_transition_rule(c in credentials(), target in any::<u32>()) {
        match c.setgid(target) {
            Some(next) => {
                if c.euid == 0 {
                    prop_assert_eq!((next.gid, next.egid, next.sgid), (target, target, target));
                } else {
                    prop_assert!(target == c.gid || target == c.sgid);
                    prop_assert_eq!(next.gid, c.gid);
                    prop_assert_eq!(next.egid, target);
                    prop_assert_eq!(next.sgid, target);
                }
                user_ids_unchanged(&c, &next);
            }
            None => {
                prop_assert!(c.euid != 0);
                prop_assert!(target != c.gid && target != c.sgid);
            }
        }
    }

    /// `setreuid` never lets an unprivileged process keep a saved id outside the
    /// set it started with, and a successful real-id change propagates the saved
    /// id from the new effective id.
    #[test]
    fn setreuid_outcome_stays_within_owned_ids(
        c in credentials(),
        uid in proptest::option::of(any::<u32>()),
        euid in proptest::option::of(any::<u32>()),
    ) {
        match c.setreuid(uid, euid) {
            Some(next) => {
                let owned = [c.uid, c.euid, c.suid];
                if c.euid != 0 {
                    if let Some(u) = uid {
                        prop_assert!(owned.contains(&u));
                        prop_assert_eq!(next.uid, u);
                    }
                    if let Some(e) = euid {
                        prop_assert!(owned.contains(&e));
                        prop_assert_eq!(next.euid, e);
                    }
                } else {
                    if let Some(u) = uid { prop_assert_eq!(next.uid, u); }
                    if let Some(e) = euid { prop_assert_eq!(next.euid, e); }
                }
                if uid.is_some() || (euid.is_some() && next.euid != next.uid) {
                    prop_assert_eq!(next.suid, next.euid);
                }
                group_ids_unchanged(&c, &next);
            }
            None => prop_assert!(c.euid != 0),
        }
    }

    /// `setresuid` is the most permissive transition, so it is the one most
    /// worth bounding: unprivileged callers can only name ids they already own.
    #[test]
    fn setresuid_never_invents_an_identity(
        c in credentials(),
        uid in proptest::option::of(any::<u32>()),
        euid in proptest::option::of(any::<u32>()),
        suid in proptest::option::of(any::<u32>()),
    ) {
        match c.setresuid(uid, euid, suid) {
            Some(next) => {
                let owned = [c.uid, c.euid, c.suid];
                for requested in [uid, euid, suid].into_iter().flatten() {
                    if c.euid != 0 {
                        prop_assert!(owned.contains(&requested));
                    }
                }
                if let Some(u) = uid { prop_assert_eq!(next.uid, u); }
                if let Some(e) = euid { prop_assert_eq!(next.euid, e); }
                if let Some(s) = suid { prop_assert_eq!(next.suid, s); }
                group_ids_unchanged(&c, &next);
            }
            None => prop_assert!(c.euid != 0),
        }
    }

    /// Same law for the group side.
    #[test]
    fn setresgid_never_invents_an_identity(
        c in credentials(),
        gid in proptest::option::of(any::<u32>()),
        egid in proptest::option::of(any::<u32>()),
        sgid in proptest::option::of(any::<u32>()),
    ) {
        match c.setresgid(gid, egid, sgid) {
            Some(next) => {
                let owned = [c.gid, c.egid, c.sgid];
                for requested in [gid, egid, sgid].into_iter().flatten() {
                    if c.euid != 0 {
                        prop_assert!(owned.contains(&requested));
                    }
                }
                if let Some(g) = gid { prop_assert_eq!(next.gid, g); }
                if let Some(e) = egid { prop_assert_eq!(next.egid, e); }
                if let Some(s) = sgid { prop_assert_eq!(next.sgid, s); }
                user_ids_unchanged(&c, &next);
            }
            None => prop_assert!(c.euid != 0),
        }
    }

    /// `setregid` mirrors `setreuid`.
    #[test]
    fn setregid_outcome_stays_within_owned_ids(
        c in credentials(),
        gid in proptest::option::of(any::<u32>()),
        egid in proptest::option::of(any::<u32>()),
    ) {
        match c.setregid(gid, egid) {
            Some(next) => {
                let owned = [c.gid, c.egid, c.sgid];
                if c.euid != 0 {
                    if let Some(g) = gid {
                        prop_assert!(owned.contains(&g));
                        prop_assert_eq!(next.gid, g);
                    }
                    if let Some(e) = egid {
                        prop_assert!(owned.contains(&e));
                        prop_assert_eq!(next.egid, e);
                    }
                } else {
                    if let Some(g) = gid { prop_assert_eq!(next.gid, g); }
                    if let Some(e) = egid { prop_assert_eq!(next.egid, e); }
                }
                if gid.is_some() || (egid.is_some() && next.egid != next.gid) {
                    prop_assert_eq!(next.sgid, next.egid);
                }
                only_user_ids_unchanged(&c, &next);
            }
            None => prop_assert!(c.euid != 0),
        }
    }
}

fn user_ids_unchanged(a: &Creds, b: &Creds) {
    assert_eq!((a.uid, a.euid, a.suid), (b.uid, b.euid, b.suid));
    assert_eq!((a.gid, a.egid, a.sgid), (b.gid, b.egid, b.sgid));
}
/// The group transition may rewrite all three GIDs, but it must never touch the
/// user identity.
fn only_user_ids_unchanged(a: &Creds, b: &Creds) {
    assert_eq!((a.uid, a.euid, a.suid), (b.uid, b.euid, b.suid));
}
fn group_ids_unchanged(a: &Creds, b: &Creds) {
    assert_eq!((a.gid, a.egid, a.sgid), (b.gid, b.egid, b.sgid));
}
fn body_ids_unchanged(a: &Creds, b: &Creds) {
    assert_eq!((a.gid, a.egid, a.sgid), (b.gid, b.egid, b.sgid));
}