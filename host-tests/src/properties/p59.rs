// Feature: setuid/setgid-on-exec and login credentials, Property 59.
//
// `execve` models `S_ISUID`/`S_ISGID` on the target inode (Linux
// `bprm_fill_uid()`), which is the only way an unprivileged identity can change
// inside Pagh-OS. The interesting input space — every mode bit, every
// (caller, owner) pair — is far too large for a boot-time suite, so the
// exhaustive half lives here:
//
//   * `exec_setid` agrees with an independently written oracle: the suid bit
//     rewrites exactly `{euid, suid}`, the sgid bit exactly `{egid, sgid}`, and
//     real ids plus the supplementary group set survive any exec;
//   * an exec without the bits is the identity function (a normal binary never
//     changes credentials — Linux preserves them across exec);
//   * the privileged/secureexec flag is exactly "effective id differs from real
//     id after the exec", which is what the syscall layer logs;
//   * `for_login` maps all six ids to the target and carries the group set
//     verbatim, refusing (never truncating) an oversized set.

use proptest::prelude::*;

use crate::cred::{Access, Creds, GROUPS_CAP, S_ISGID, S_ISUID};

/// Independent oracle for `bprm_fill_uid()`, written as explicit field updates.
fn oracle_exec_setid(c: &Creds, mode: u32, owner: (u32, u32)) -> (Creds, bool) {
    let mut euid = c.euid;
    let mut suid = c.suid;
    let mut egid = c.egid;
    let mut sgid = c.sgid;
    let suid_bit = mode & 0o4000 != 0;
    let sgid_bit = mode & 0o2000 != 0;
    if suid_bit {
        euid = owner.0;
        suid = owner.0;
    }
    if sgid_bit {
        egid = owner.1;
        sgid = owner.1;
    }
    let next = Creds::new(c.uid, c.gid, euid, egid, suid, sgid)
        .with_groups(c.groups())
        .unwrap();
    let privileged = next.euid != next.uid || next.egid != next.gid;
    (next, privileged)
}

fn credentials() -> impl Strategy<Value = Creds> {
    (
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        prop::collection::vec(any::<u32>(), 0..=GROUPS_CAP),
    )
        .prop_map(|(uid, gid, euid, egid, suid, sgid, groups)| {
            Creds::new(uid, gid, euid, egid, suid, sgid)
                .with_groups(&groups)
                .expect("generator stays within the cap")
        })
}

proptest! {
    /// `exec_setid` matches the independent oracle for arbitrary modes, owners
    /// and caller identities, and keeps real ids plus supplementary groups.
    #[test]
    fn exec_setid_agrees_with_the_independent_oracle(
        c in credentials(),
        mode in any::<u32>(),
        owner_uid in any::<u32>(),
        owner_gid in any::<u32>(),
    ) {
        let next = c.exec_setid(mode, (owner_uid, owner_gid));
        let (expected, privileged) = oracle_exec_setid(&c, mode, (owner_uid, owner_gid));
        prop_assert_eq!(&next, &expected);
        prop_assert_eq!(next.runs_privileged(), privileged);
        prop_assert_eq!((next.uid, next.gid), (c.uid, c.gid));
        prop_assert_eq!(next.groups(), c.groups());
    }

    /// No setuid/setgid bit: exec preserves the credentials exactly.
    #[test]
    fn exec_without_setid_bits_preserves_credentials(
        c in credentials(),
        mode in any::<u32>(),
        owner_uid in any::<u32>(),
        owner_gid in any::<u32>(),
    ) {
        prop_assume!(mode & (S_ISUID | S_ISGID) == 0);
        let next = c.exec_setid(mode, (owner_uid, owner_gid));
        prop_assert_eq!(next, c);
        // Исполнение обычного файла не меняет и состояние «privileged process».
        prop_assert_eq!(next.runs_privileged(), c.runs_privileged());
    }

    /// `runs_privileged` is exactly "effective id differs from real id" after
    /// the exec, and it is the *state* the syscall log names.
    #[test]
    fn runs_privileged_tracks_effective_versus_real_ids(
        c in credentials(),
        mode in any::<u32>(),
        owner_uid in any::<u32>(),
        owner_gid in any::<u32>(),
    ) {
        let next = c.exec_setid(mode, (owner_uid, owner_gid));
        prop_assert_eq!(
            next.runs_privileged(),
            next.euid != next.uid || next.egid != next.gid
        );
    }

    /// A setuid-root exec from an unprivileged identity yields euid 0 while the
    /// real identity stays the caller's — the A1 contract in one law.
    #[test]
    fn setuid_root_exec_raises_euid_but_keeps_real_id(
        uid in 1u32..=u32::MAX,
        gid in 1u32..=u32::MAX,
    ) {
        let user = Creds::new(uid, gid, uid, gid, uid, gid);
        let next = user.exec_setid(S_ISUID | 0o755, (0, 0));
        prop_assert_eq!((next.uid, next.euid, next.suid), (uid, 0, 0));
        prop_assert_eq!((next.gid, next.egid, next.sgid), (gid, gid, gid));
        prop_assert!(next.is_root());
        prop_assert!(next.runs_privileged());
        // Права считаются по effective id: обход r/w для root, но не для x.
        prop_assert!(next.may(0o000, (uid, gid), Access::Read));
    }

    /// `for_login` maps all six ids to the target, carries the group set
    /// verbatim, and refuses an oversized set instead of truncating it.
    #[test]
    fn for_login_is_the_exact_target_identity(
        uid in any::<u32>(),
        gid in any::<u32>(),
        groups in prop::collection::vec(any::<u32>(), 0..(GROUPS_CAP + 2)),
    ) {
        match Creds::for_login(uid, gid, &groups) {
            Some(c) => {
                prop_assert!(groups.len() <= GROUPS_CAP);
                prop_assert_eq!(
                    (c.uid, c.gid, c.euid, c.egid, c.suid, c.sgid),
                    (uid, gid, uid, gid, uid, gid)
                );
                prop_assert_eq!(c.groups(), &groups[..]);
            }
            None => prop_assert!(groups.len() > GROUPS_CAP),
        }
    }
}
