//! Process credentials and POSIX permission decisions.
//!
//! Pure, allocation-free, `core`-only logic shared with the `host-tests` crate
//! (`#[path]` include, property `p56`). It answers three questions and nothing
//! else:
//!
//!   * [`Creds::may`] — may a process with these credentials access a file with
//!     this `mode` and `(uid, gid)` owner, for [`Access::Read`]/[`Access::Write`]/
//!     [`Access::Execute`]? Owner/group/other bits use the **effective** ids;
//!     root bypasses read/write but, exactly like Linux, still needs at least one
//!     execute bit for `X`.
//!   * [`Creds::setuid`]/[`Creds::setgid`] — the `setuid(2)`/`setgid(2)`
//!     transition, including the saved-set-id rules.
//!   * [`Creds::setreuid`]/[`Creds::setresuid`] and the group analogues — the
//!     more granular `set[res][ug]id` transitions.
//!
//! ## Why this is separate from the syscall handlers
//!
//! These are the decisions a reviewer must be able to check without reading the
//! VFS, the scheduler or the syscall arms. Keeping them here also means the
//! host property test executes the *same* source the kernel compiles instead of
//! a copy that can drift.
//!
//! ## What is deliberately not modelled
//!
//! There are no supplementary groups, no capabilities and no user namespace.
//! [`Creds::gid`]/[`Creds::egid`] are the only group identity a permission
//! check sees. `fsuid`/`fsgid` (Linux's separate filesystem identity) are not
//! modelled either: permission checks use `euid`/`egid`, which is what
//! `setuid(2)` documents for a process that never calls `setfsuid(2)`.
#![allow(dead_code)]

/// The six POSIX ids a process carries.
///
/// Every field is a plain `u32` (Linux `uid_t`/`gid_t`); the kernel never hands
/// out `(uid_t)-1` as a real identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Creds {
    /// Real user id.
    pub uid: u32,
    /// Real group id.
    pub gid: u32,
    /// Effective user id — what permission checks use.
    pub euid: u32,
    /// Effective group id — what permission checks use.
    pub egid: u32,
    /// Saved set-user-ID (restored by `execve` on a privileged binary; pagh
    /// preserves it across `exec` because setuid binaries are not modelled).
    pub suid: u32,
    /// Saved set-group-ID.
    pub sgid: u32,
}

impl Creds {
    /// The identity every kernel-spawned process and the boot shell starts
    /// with. Keeping this root preserves the existing userland (CPython, apt).
    pub const ROOT: Creds = Creds {
        uid: 0,
        gid: 0,
        euid: 0,
        egid: 0,
        suid: 0,
        sgid: 0,
    };

    /// Convenience constructor for tests and for a non-root spawn policy.
    pub const fn new(uid: u32, gid: u32, euid: u32, egid: u32, suid: u32, sgid: u32) -> Self {
        Creds {
            uid,
            gid,
            euid,
            egid,
            suid,
            sgid,
        }
    }

    /// The effective user id (`geteuid(2)` and the permission check identity).
    #[inline]
    pub const fn effective_uid(&self) -> u32 {
        self.euid
    }

    /// The effective group id (`getegid(2)`).
    #[inline]
    pub const fn effective_gid(&self) -> u32 {
        self.egid
    }

    /// Is this process privileged for filesystem purposes?
    ///
    /// pagh models root's `CAP_DAC_OVERRIDE`/`CAP_DAC_READ_SEARCH` as
    /// `euid == 0`; there are no capability sets.
    #[inline]
    pub const fn is_root(&self) -> bool {
        self.euid == 0
    }

    /// POSIX permission check for `access` against `mode` and owner
    /// `(uid, gid)`.
    ///
    /// * Effective ids select the class: `euid == owner uid` → owner bits;
    ///   otherwise `egid == owner gid` → group bits; otherwise other bits.
    /// * `euid == 0` bypasses read and write entirely (Linux capability
    ///   semantics), but **not** execute: root still needs at least one of the
    ///   three execute bits. This is the rule that stops a non-executable file
    ///   from being execed just because the caller is root.
    ///
    /// `mode` may carry type bits (`S_IFREG` etc.); only the low nine permission
    /// bits participate.
    pub fn may(&self, mode: u32, owner: (u32, u32), access: Access) -> bool {
        let perms = mode & 0o777;
        if self.euid == 0 {
            return match access {
                Access::Read | Access::Write => true,
                Access::Execute => perms & 0o111 != 0,
            };
        }
        let class_shift = if self.euid == owner.0 {
            6
        } else if self.egid == owner.1 {
            3
        } else {
            0
        };
        (perms >> class_shift) & access.bit() != 0
    }

    /// May a process with these credentials change the mode of a node owned by
    /// `owner`? Linux: root, or the file's owner (`CAP_FOWNER` collapsed to
    /// `euid == 0`).
    #[inline]
    pub fn may_chmod(&self, owner: (u32, u32)) -> bool {
        self.euid == 0 || self.euid == owner.0
    }

    /// May a process with these credentials change the owner of a node? Linux:
    /// root only (`CAP_CHOWN` collapsed to `euid == 0`).
    #[inline]
    pub fn may_chown(&self) -> bool {
        self.euid == 0
    }

    /// `setuid(2)`: set the real/effective/saved user ids.
    ///
    /// * privileged (`euid == 0`): any target; all three become `uid` (Linux
    ///   clears any elevated saved id on this path);
    /// * otherwise `uid` must be the current real or saved id; the effective id
    ///   is changed and, per this kernel's small model, the saved id follows it
    ///   (so a process that gives up or re-acquires a permitted identity cannot
    ///   keep an unreachable third state).
    ///
    /// `None` is `EPERM` at the syscall boundary.
    pub fn setuid(&self, uid: u32) -> Option<Creds> {
        let mut next = *self;
        if self.euid == 0 {
            next.uid = uid;
            next.euid = uid;
            next.suid = uid;
            return Some(next);
        }
        if uid == self.uid || uid == self.suid {
            next.euid = uid;
            next.suid = uid;
            return Some(next);
        }
        None
    }

    /// `setgid(2)`: the group analogue of [`Creds::setuid`].
    pub fn setgid(&self, gid: u32) -> Option<Creds> {
        let mut next = *self;
        if self.euid == 0 {
            next.gid = gid;
            next.egid = gid;
            next.sgid = gid;
            return Some(next);
        }
        if gid == self.gid || gid == self.sgid {
            next.egid = gid;
            next.sgid = gid;
            return Some(next);
        }
        None
    }

    /// `setreuid(2)`: set the real and/or effective user id.
    ///
    /// * privileged: either id may be changed (`None` means "leave unchanged");
    /// * unprivileged: the real id may only be the current real or saved id, and
    ///   the effective id may only be the current real, effective or saved id;
    /// * following Linux, the saved id changes only when a transition is actually
    ///   requested: a requested real-id change, or an effective-id change that
    ///   leaves `euid != ruid`, resets the saved id to the new effective id.
    ///   `setreuid(-1, -1)` is a no-op.
    ///
    /// `None` is `EPERM` at the syscall boundary.
    pub fn setreuid(&self, uid: Option<u32>, euid: Option<u32>) -> Option<Creds> {
        let mut next = *self;
        if self.euid != 0 {
            if let Some(u) = uid {
                if u != self.uid && u != self.suid {
                    return None;
                }
            }
            if let Some(e) = euid {
                if e != self.uid && e != self.euid && e != self.suid {
                    return None;
                }
            }
        }
        if let Some(u) = uid {
            next.uid = u;
        }
        if let Some(e) = euid {
            next.euid = e;
        }
        if uid.is_some() || (euid.is_some() && next.euid != next.uid) {
            next.suid = next.euid;
        }
        Some(next)
    }

    /// `setregid(2)`: the group analogue of [`Creds::setreuid`].
    pub fn setregid(&self, gid: Option<u32>, egid: Option<u32>) -> Option<Creds> {
        let mut next = *self;
        if self.euid != 0 {
            if let Some(g) = gid {
                if g != self.gid && g != self.sgid {
                    return None;
                }
            }
            if let Some(e) = egid {
                if e != self.gid && e != self.egid && e != self.sgid {
                    return None;
                }
            }
        }
        if let Some(g) = gid {
            next.gid = g;
        }
        if let Some(e) = egid {
            next.egid = e;
        }
        if gid.is_some() || (egid.is_some() && next.egid != next.gid) {
            next.sgid = next.egid;
        }
        Some(next)
    }

    /// `setresuid(2)`: set any subset of real/effective/saved user ids.
    ///
    /// An unprivileged caller may only move each specified id within the set
    /// `{uid, euid, suid}` (the values it already owns); a privileged caller may
    /// set any value. `None` in an argument means "unchanged" (`(uid_t)-1`).
    pub fn setresuid(
        &self,
        uid: Option<u32>,
        euid: Option<u32>,
        suid: Option<u32>,
    ) -> Option<Creds> {
        if self.euid != 0 {
            for requested in [uid, euid, suid].into_iter().flatten() {
                if requested != self.uid && requested != self.euid && requested != self.suid {
                    return None;
                }
            }
        }
        let mut next = *self;
        if let Some(u) = uid {
            next.uid = u;
        }
        if let Some(e) = euid {
            next.euid = e;
        }
        if let Some(s) = suid {
            next.suid = s;
        }
        Some(next)
    }

    /// `setresgid(2)`: the group analogue of [`Creds::setresuid`].
    pub fn setresgid(
        &self,
        gid: Option<u32>,
        egid: Option<u32>,
        sgid: Option<u32>,
    ) -> Option<Creds> {
        if self.euid != 0 {
            for requested in [gid, egid, sgid].into_iter().flatten() {
                if requested != self.gid && requested != self.egid && requested != self.sgid {
                    return None;
                }
            }
        }
        let mut next = *self;
        if let Some(g) = gid {
            next.gid = g;
        }
        if let Some(e) = egid {
            next.egid = e;
        }
        if let Some(s) = sgid {
            next.sgid = s;
        }
        Some(next)
    }
}

/// What kind of access a permission check is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Execute,
}

impl Access {
    /// The permission bit for this access in one owner/group/other triad.
    #[inline]
    const fn bit(self) -> u32 {
        match self {
            Access::Read => 0o4,
            Access::Write => 0o2,
            Access::Execute => 0o1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORLD_READ: u32 = 0o444;
    const OWNER_RW: u32 = 0o600;

    #[test]
    fn root_bypasses_read_write_but_needs_an_execute_bit() {
        let root = Creds::ROOT;
        assert!(root.may(0o000, (1000, 1000), Access::Read));
        assert!(root.may(0o000, (1000, 1000), Access::Write));
        assert!(!root.may(0o000, (1000, 1000), Access::Execute));
        assert!(!root.may(0o644, (1000, 1000), Access::Execute));
        assert!(root.may(0o001, (1000, 1000), Access::Execute));
        assert!(root.may(0o100, (1000, 1000), Access::Execute));
        assert!(root.may(0o010, (1000, 1000), Access::Execute));
        assert!(root.may(0o111, (1000, 1000), Access::Execute));
    }

    #[test]
    fn owner_group_other_selection_uses_effective_ids() {
        let c = Creds::new(7, 8, 7, 8, 7, 8);
        // Owner class wins even when the group bits would deny.
        assert!(c.may(OWNER_RW, (7, 8), Access::Read));
        assert!(c.may(OWNER_RW, (7, 8), Access::Write));
        assert!(!c.may(OWNER_RW, (7, 8), Access::Execute));
        // Group class when not the owner.
        assert!(c.may(0o040, (1, 8), Access::Read));
        assert!(!c.may(0o400, (1, 8), Access::Read));
        // Other class when neither.
        assert!(c.may(WORLD_READ, (1, 2), Access::Read));
        assert!(!c.may(WORLD_READ, (1, 2), Access::Write));
        // Owner test is on euid, not uid.
        let c = Creds::new(1000, 8, 7, 8, 1000, 8);
        assert!(c.may(0o400, (7, 8), Access::Read));
        assert!(!c.may(0o040, (7, 8), Access::Read));
    }

    #[test]
    fn mode_type_bits_are_ignored() {
        let c = Creds::new(1000, 1000, 1000, 1000, 1000, 1000);
        assert!(c.may(0o100644, (0, 0), Access::Read));
        assert!(!c.may(0o100644, (0, 0), Access::Write));
        assert!(c.may(0o040755, (1, 1), Access::Execute));
    }

    #[test]
    fn root_setuid_rewrites_all_three_and_nonroot_is_restricted() {
        let root = Creds::ROOT;
        let c = root.setuid(1000).expect("root may setuid");
        assert_eq!((c.uid, c.euid, c.suid), (1000, 1000, 1000));

        // A setuid-root child: ruid 1000, euid/suid 0.
        let setuid_root = Creds::new(1000, 1000, 0, 1000, 0, 1000);
        let released = setuid_root.setuid(1000).expect("back to real uid");
        assert_eq!(
            (released.uid, released.euid, released.suid),
            (1000, 1000, 1000)
        );
        let back = setuid_root.setuid(0).expect("saved id is permitted");
        assert_eq!(back.euid, 0);

        // A NON-root process with a saved id it may revisit: it can switch to
        // that saved id, but cannot invent a third one.
        let saved_zero = Creds::new(1000, 1000, 1000, 1000, 0, 1000);
        assert!(saved_zero.setuid(0).is_some(), "saved id is permitted");
        assert_eq!(saved_zero.setuid(1001), None);

        let plain = Creds::new(1000, 1000, 1000, 1000, 1000, 1000);
        assert_eq!(plain.setuid(0), None, "unprivileged setuid(0) is EPERM");
        assert!(plain.setuid(1000).is_some());
    }

    #[test]
    fn root_setgid_mirrors_setuid() {
        let c = Creds::ROOT.setgid(4242).expect("root may setgid");
        assert_eq!((c.gid, c.egid, c.sgid), (4242, 4242, 4242));
        let plain = Creds::new(1000, 1000, 1000, 1000, 1000, 1000);
        assert_eq!(plain.setgid(0), None);
        let sgid_child = Creds::new(1000, 1000, 0, 4242, 0, 4242);
        assert_eq!(sgid_child.setgid(1000).expect("real gid").egid, 1000);
    }

    #[test]
    fn setreuid_saved_id_follows_the_linux_rule() {
        let root = Creds::ROOT;
        let c = root.setreuid(None, Some(1000)).unwrap();
        assert_eq!((c.uid, c.euid, c.suid), (0, 1000, 1000));
        let c = root.setreuid(Some(1000), Some(1000)).unwrap();
        assert_eq!((c.uid, c.euid, c.suid), (1000, 1000, 1000));

        let saved_zero = Creds::new(1000, 1000, 1000, 1000, 0, 1000);
        // The real id may only become the current real or saved id; 1001 is
        // neither.
        assert_eq!(saved_zero.setreuid(Some(1001), Some(1001)), None);
        // Both requested ids are owned (real = saved = 0 is reachable), so the
        // transition succeeds and the saved id follows the new effective id.
        assert_eq!(
            saved_zero.setreuid(Some(0), Some(0)),
            Some(Creds::new(0, 1000, 0, 1000, 0, 1000))
        );
        // A no-op request leaves every id alone.
        assert_eq!(saved_zero.setreuid(None, None), Some(saved_zero));
        assert_eq!(
            saved_zero.setreuid(Some(1000), Some(1000)),
            Some(Creds::new(1000, 1000, 1000, 1000, 1000, 1000))
        );
    }

    #[test]
    fn setresuid_accepts_only_owned_ids_without_privilege() {
        let c = Creds::new(1000, 1000, 1001, 1000, 1002, 1000);
        assert!(c.setresuid(Some(1002), None, None).is_some());
        assert!(c.setresuid(None, Some(1000), None).is_some());
        assert_eq!(c.setresuid(Some(1003), None, None), None);
        assert_eq!(c.setresuid(None, None, Some(0)), None);
        let root = Creds::ROOT
            .setresuid(Some(1), Some(2), Some(3))
            .expect("root may set any");
        assert_eq!((root.uid, root.euid, root.suid), (1, 2, 3));
    }

    #[test]
    fn setresgid_accepts_only_owned_ids_without_privilege() {
        let c = Creds::new(1000, 1000, 1000, 1001, 1000, 1002);
        assert!(c.setresgid(None, None, Some(1002)).is_some());
        assert_eq!(c.setresgid(Some(0), None, None), None);
        let root = Creds::ROOT
            .setresgid(Some(4), Some(5), Some(6))
            .expect("root may set any");
        assert_eq!((root.gid, root.egid, root.sgid), (4, 5, 6));
    }

    #[test]
    fn chmod_is_owner_or_root_and_chown_is_root_only() {
        let root = Creds::ROOT;
        assert!(root.may_chmod((1000, 1000)));
        assert!(root.may_chown());
        let owner = Creds::new(1000, 1000, 1000, 1000, 1000, 1000);
        assert!(owner.may_chmod((1000, 1000)));
        assert!(!owner.may_chmod((1001, 1000)));
        assert!(!owner.may_chown());
    }
}
