// Feature: phase 1 "groups separate from root", property P60: the pure
// `/etc/passwd` + `/etc/group` layer the shell's `id`/`groups`/`su` commands use.
//
// The files are the *data* half of the group model; the enforcement half is
// `cred.rs` (P56) and the syscall boundary (`getgroups`/`setgroups`). A parser
// bug here would be silent: `id` would print plausible groups that do not match
// `/etc/group`, or `su` would load a different supplementary set than the file
// says. The properties below therefore compare the parser/query results against
// an independently written oracle over generated databases, and require the
// render→parse round trip to be exact for well-formed entries.

use proptest::prelude::*;

use crate::identity_db::{
    group_by_gid, groups_for_user, parse_group, parse_passwd, supplementary_groups_for_user,
    user_by_name, user_by_uid, GroupEntry, UserEntry,
};

/// Small alphabets keep collisions (same gid twice, same user name in two
/// groups) reachable; that is where dedup and primary-group handling live.
fn name() -> impl Strategy<Value = String> {
    "[a-z]{1,6}".prop_map(String::from)
}

fn user_strategy() -> impl Strategy<Value = UserEntry> {
    (name(), 0u32..8, 0u32..8).prop_map(|(name, uid, gid)| UserEntry {
        name,
        uid,
        gid,
        gecos: String::from("test"),
        home: String::from("/home/test"),
        shell: String::from("/bin/sh"),
    })
}

fn group_strategy() -> impl Strategy<Value = GroupEntry> {
    (name(), 0u32..8, prop::collection::vec(name(), 0..4)).prop_map(|(name, gid, members)| {
        GroupEntry {
            name,
            gid,
            members,
        }
    })
}

fn render_passwd(users: &[UserEntry]) -> String {
    let mut out = String::new();
    for u in users {
        out.push_str(&format!(
            "{}:x:{}:{}:{}:{}:{}\n",
            u.name, u.uid, u.gid, u.gecos, u.home, u.shell
        ));
    }
    out
}

fn render_group(groups: &[GroupEntry]) -> String {
    let mut out = String::new();
    for g in groups {
        out.push_str(&format!("{}:x:{}:{}\n", g.name, g.gid, g.members.join(",")));
    }
    out
}

/// Independent oracle: primary gid first, then every distinct group gid in file
/// order where the user is listed.
fn oracle_groups(users: &[UserEntry], groups: &[GroupEntry], name: &str) -> Option<Vec<u32>> {
    let user = users.iter().find(|u| u.name == name)?;
    let mut out = vec![user.gid];
    for group in groups {
        let listed = group.members.iter().any(|m| m == name);
        if listed && !out.contains(&group.gid) {
            out.push(group.gid);
        }
    }
    Some(out)
}

proptest! {
    /// Well-formed entries survive render→parse byte-for-byte. The generator
    /// never emits `:` or `,` in names, which is exactly the domain the parser
    /// promises to round-trip.
    #[test]
    fn passwd_round_trips(
        users in prop::collection::vec(user_strategy(), 0..6),
    ) {
        let parsed = parse_passwd(&render_passwd(&users));
        prop_assert_eq!(parsed, users);
    }

    #[test]
    fn group_round_trips(
        groups in prop::collection::vec(group_strategy(), 0..6),
    ) {
        let parsed = parse_group(&render_group(&groups));
        prop_assert_eq!(parsed, groups);
    }

    /// `groups_for_user` equals the independent oracle for every user name that
    /// appears in the generated passwd database.
    #[test]
    fn groups_for_user_matches_the_oracle(
        users in prop::collection::vec(user_strategy(), 1..5),
        groups in prop::collection::vec(group_strategy(), 0..6),
    ) {
        for user in &users {
            prop_assert_eq!(
                groups_for_user(&users, &groups, &user.name),
                oracle_groups(&users, &groups, &user.name)
            );
        }
        // A name absent from passwd has no group set at all — not even an empty
        // one (that would make `su unknown` look like a valid user).
        prop_assert_eq!(groups_for_user(&users, &groups, "definitely-absent"), None);
    }

    /// The set loaded into `setgroups(2)` is the display set minus the primary
    /// gid: the kernel already knows egid, so listing it again would make
    /// `getgroups(2)` report a group Linux would not.
    #[test]
    fn supplementary_groups_exclude_primary_and_duplicates(
        users in prop::collection::vec(user_strategy(), 1..5),
        groups in prop::collection::vec(group_strategy(), 0..6),
    ) {
        for user in &users {
            let display = groups_for_user(&users, &groups, &user.name).unwrap();
            let supp = supplementary_groups_for_user(&users, &groups, &user.name).unwrap();
            for gid in &supp {
                prop_assert_ne!(*gid, user.gid);
                prop_assert!(display.contains(gid));
            }
            // Every non-primary display gid is present exactly once.
            for gid in display.iter().skip(1) {
                prop_assert_eq!(supp.iter().filter(|g| *g == gid).count(), 1);
            }
        }
    }

    /// Lookups follow the first-match rule and never invent entries.
    #[test]
    fn lookups_are_consistent(
        users in prop::collection::vec(user_strategy(), 0..6),
        groups in prop::collection::vec(group_strategy(), 0..6),
    ) {
        // Generators may produce duplicate names/uids deliberately: the rule is
        // FIRST match (getpwnam on a duplicated database), not "any match".
        for user in &users {
            let first_by_name = users.iter().find(|u| u.name == user.name);
            prop_assert_eq!(user_by_name(&users, &user.name), first_by_name);
            let first_by_uid = users.iter().find(|u| u.uid == user.uid);
            prop_assert_eq!(user_by_uid(&users, user.uid), first_by_uid);
        }
        for group in &groups {
            let first_by_gid = groups.iter().find(|g| g.gid == group.gid);
            prop_assert_eq!(group_by_gid(&groups, group.gid), first_by_gid);
        }
    }

    /// Arbitrary text — including colon/comma garbage, CRLF, comments and
    /// out-of-range numbers — must never panic and never yields an empty name
    /// or a non-numeric id.
    #[test]
    fn arbitrary_text_is_skipped_not_panicked(text in ".*") {
        let users = parse_passwd(&text);
        for u in &users {
            prop_assert!(!u.name.is_empty());
            prop_assert!(!u.name.contains('\n'));
        }
        let groups = parse_group(&text);
        for g in &groups {
            prop_assert!(!g.name.is_empty());
            prop_assert!(g.members.iter().all(|m| !m.is_empty()));
        }
    }
}
