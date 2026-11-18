// Feature: non-root phase 2, property P63: the sticky-directory deletion rule
// (`S_ISVTX`, POSIX /tmp semantics) the syscall layer applies on unlink/rmdir/
// rename on top of the ordinary write+execute check on the parent directory.
//
// The ordinary permission matrix is P56; this property isolates the *extra*
// restriction a 1777 directory imposes. The bug class it guards against is a
// silent one: any identity comparison that forgets the file owner (or grants a
// foreign file's deletion by group membership) still looks plausible in code
// review but lets one user destroy another user's file in `/tmp`.
//
// The rules, written as an independent oracle:
//   * no `S_ISVTX` in the directory mode -> the ordinary directory bits decide,
//     so this extra check never denies;
//   * sticky -> only root (`euid == 0`), the directory owner, or the entry owner
//     may delete/rename the entry. Supplementary groups grant nothing here.

use proptest::prelude::*;

use crate::cred::Creds;

/// Independent oracle for the sticky rule (written as positive exemptions
/// rather than the negation the implementation uses).
fn oracle_may_delete_child(
    c: &Creds,
    dir_mode: u32,
    dir_owner: (u32, u32),
    child_owner: (u32, u32),
) -> bool {
    let sticky = dir_mode & 0o1000 != 0;
    if !sticky {
        return true;
    }
    c.euid == 0 || c.euid == dir_owner.0 || c.euid == child_owner.0
}

fn ids() -> impl Strategy<Value = u32> {
    prop_oneof![Just(0u32), Just(1000u32), Just(1001u32), 0u32..4]
}

fn creds_with(uid: u32, euid: u32) -> Creds {
    Creds::new(uid, 0, euid, 0, uid, 0)
}

proptest! {
    /// The helper must agree with the oracle for every id/mode combination in a
    /// small universe that contains root, the directory owner, the entry owner
    /// and two unrelated users.
    #[test]
    fn sticky_rule_agrees_with_the_independent_oracle(
        euid in ids(),
        dir_uid in ids(),
        child_uid in ids(),
        mode_low in prop_oneof![Just(0o777u32), Just(0o755), Just(0o700), Just(0o7777)],
        sticky in any::<bool>(),
    ) {
        let dir_mode = mode_low | if sticky { 0o1000 } else { 0 };
        let c = creds_with(euid, euid);
        let dir_owner = (dir_uid, 0);
        let child_owner = (child_uid, 0);
        prop_assert_eq!(
            c.may_delete_child(dir_mode, dir_owner, child_owner),
            oracle_may_delete_child(&c, dir_mode, dir_owner, child_owner),
            "euid={} dir_uid={} child_uid={} mode={:o}",
            euid, dir_uid, child_uid, dir_mode
        );
    }

    /// A non-sticky directory can never deny on this check: the caller already
    /// enforced write+execute, and this is the only added rule.
    #[test]
    fn non_sticky_directory_never_adds_a_denial(
        euid in ids(),
        dir_uid in ids(),
        child_uid in ids(),
        mode_low in 0u32..0o1000,
    ) {
        let c = creds_with(euid, euid);
        prop_assert!(c.may_delete_child(mode_low, (dir_uid, 0), (child_uid, 0)));
    }
}

/// The concrete /tmp scenarios from the plan: 1777 root-owned directory.
#[test]
fn tmp_1777_scenarios() {
    let root = Creds::ROOT;
    let oleg = creds_with(1000, 1000);
    let anna = creds_with(1001, 1001);
    let tmp = 0o1777u32;
    let tmp_owner = (0u32, 0u32);

    // Another user's file cannot be deleted or renamed out of /tmp.
    assert!(!oleg.may_delete_child(tmp, tmp_owner, (1001, 1001)));
    assert!(!anna.may_delete_child(tmp, tmp_owner, (1000, 1000)));
    // Own file can.
    assert!(oleg.may_delete_child(tmp, tmp_owner, (1000, 1000)));
    // Root can.
    assert!(root.may_delete_child(tmp, tmp_owner, (1000, 1000)));
    assert!(root.may_delete_child(tmp, tmp_owner, (1001, 1001)));

    // The owner of the directory can (a user-owned 1777 directory).
    let home = (1000u32, 1000u32);
    assert!(oleg.may_delete_child(tmp, home, (1001, 1001)));

    // Group membership does not exempt: sticky is a uid rule.
    let grouped = oleg.with_groups(&[1001]).expect("group set fits");
    assert!(!grouped.may_delete_child(tmp, tmp_owner, (1001, 1001)));

    // Without the sticky bit the ordinary directory bits are the whole story.
    // (`0o6777` is setuid+setgid+rwx — sticky must not be inferred from `0o7777`,
    // which *does* carry S_ISVTX.)
    assert!(oleg.may_delete_child(0o777, tmp_owner, (1001, 1001)));
    assert!(oleg.may_delete_child(0o6777, tmp_owner, (1001, 1001)));
}
