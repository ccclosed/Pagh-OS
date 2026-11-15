//! Креденшелы процесса и решения о правах POSIX.
//!
//! Только `core`, без аллокаций, общая с крейтом `host-tests` (включение через
//! `#[path]`, свойство `p56`). Отвечает ровно на три вопроса:
//!
//!   * [`Creds::may`] — может ли процесс с такими креденшелами обратиться к
//!     файлу с этим `mode` и владельцем `(uid, gid)` для
//!     [`Access::Read`]/[`Access::Write`]/[`Access::Execute`]? Биты
//!     owner/group/other используют **effective** id; root обходит read/write,
//!     но, точно как в Linux, всё равно нуждается хотя бы в одном
//!     execute-бите для `X`.
//!   * [`Creds::setuid`]/[`Creds::setgid`] — переход `setuid(2)`/`setgid(2)`,
//!     включая правила saved-set-id.
//!   * [`Creds::setreuid`]/[`Creds::setresuid`] и групповые аналоги — более
//!     гранулярные переходы `set[res][ug]id`.
//!
//! ## Почему отдельно от обработчиков syscall
//!
//! Это решения, которые рецензент должен уметь проверить, не читая VFS,
//! планировщик и ветки syscall'ов. Хранение здесь также означает, что
//! host-property-тест исполняет *тот же* исходник, что компилирует ядро, а не
//! копию, которая может разойтись.
//!
//! ## Что намеренно не моделируется
//!
//! Дополнительных групп, capabilities и user namespace нет.
//! [`Creds::gid`]/[`Creds::egid`] — единственная групповая идентичность, которую
//! видит проверка прав. `fsuid`/`fsgid` (отдельная идентичность файловой системы
//! в Linux) тоже не моделируются: проверки прав используют `euid`/`egid`, что и
//! документирует `setuid(2)` для процесса, никогда не звавшего `setfsuid(2)`.
#![allow(dead_code)]

/// Шесть POSIX-идентичностей процесса.
///
/// Каждое поле — простой `u32` (Linux `uid_t`/`gid_t`); ядро никогда не выдаёт
/// `(uid_t)-1` как реальную идентичность.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Creds {
    /// Real user id (идентификатор владельца).
    pub uid: u32,
    /// Real group id (идентификатор группы владельца).
    pub gid: u32,
    /// Effective user id — то, что используют проверки прав.
    pub euid: u32,
    /// Effective group id — то, что используют проверки прав.
    pub egid: u32,
    /// Saved set-user-ID (восстанавливается `execve` на привилегированном
    /// бинаре; pagh сохраняет его через `exec`, поскольку setuid-бинари не
    /// моделируются).
    pub suid: u32,
    /// Saved set-group-ID (сохранённый group id).
    pub sgid: u32,
}

impl Creds {
    /// Идентичность, с которой стартуют все запущенные ядром процессы и шелл
    /// загрузки. Сохранение root поддерживает существующий userland (CPython,
    /// apt).
    pub const ROOT: Creds = Creds {
        uid: 0,
        gid: 0,
        euid: 0,
        egid: 0,
        suid: 0,
        sgid: 0,
    };

    /// Удобный конструктор для тестов и для политики запуска не от root.
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

    /// Effective user id (`geteuid(2)` и идентичность проверки прав).
    #[inline]
    pub const fn effective_uid(&self) -> u32 {
        self.euid
    }

    /// Effective group id (`getegid(2)`) — используется проверками прав.
    #[inline]
    pub const fn effective_gid(&self) -> u32 {
        self.egid
    }

    /// Привилегирован ли процесс для целей файловой системы?
    ///
    /// pagh моделирует `CAP_DAC_OVERRIDE`/`CAP_DAC_READ_SEARCH` root как
    /// `euid == 0`; наборов capabilities нет.
    #[inline]
    pub const fn is_root(&self) -> bool {
        self.euid == 0
    }

    /// Проверка прав POSIX для `access` по `mode` и владельцу `(uid, gid)`.
    ///
    /// * Effective id выбирает класс: `euid == uid владельца` → биты owner; иначе
    ///   `egid == gid владельца` → биты group; иначе биты other.
    /// * `euid == 0` полностью обходит read и write (семантика capabilities
    ///   Linux), но **не** execute: root всё равно нуждается хотя бы в одном из
    ///   трёх execute-битов. Это правило не даёт исполнить неисполняемый файл
    ///   только потому, что вызывающий — root.
    ///
    /// `mode` может нести биты типа (`S_IFREG` и т. п.); участвуют только
    /// младшие девять битов прав.
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

    /// Может ли процесс с такими креденшелами сменить режим узла, принадлежащего
    /// `owner`? Linux: root или владелец файла (`CAP_FOWNER` сведён к
    /// `euid == 0`).
    #[inline]
    pub fn may_chmod(&self, owner: (u32, u32)) -> bool {
        self.euid == 0 || self.euid == owner.0
    }

    /// Может ли процесс с такими креденшелами сменить владельца узла? Linux:
    /// только root (`CAP_CHOWN` сведён к `euid == 0`).
    #[inline]
    pub fn may_chown(&self) -> bool {
        self.euid == 0
    }

    /// `setuid(2)`: задать real/effective/saved user id.
    ///
    /// * привилегированный (`euid == 0`): любая цель; все три становятся `uid`
    ///   (Linux очищает повышенный saved id на этом пути);
    /// * иначе `uid` должен быть текущим real или saved id; effective id
    ///   меняется и, по малой модели этого ядра, saved id следует за ним (чтобы
    ///   процесс, отказавшийся от разрешённой идентичности или вернувший её, не
    ///   мог удержать недостижимое третье состояние).
    ///
    /// `None` — `EPERM` на границе syscall.
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

    /// `setgid(2)`: групповой аналог [`Creds::setuid`].
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

    /// `setreuid(2)`: задать real и/или effective user id.
    ///
    /// * привилегированный: можно менять любой id (`None` — «не менять»);
    /// * непривилегированный: real id может стать только текущим real или saved
    ///   id, а effective — только текущим real, effective или saved id;
    /// * как в Linux, saved id меняется, только когда переход реально запрошен:
    ///   запрошенная смена real id или смена effective, после которой
    ///   `euid != ruid`, сбрасывает saved id в новый effective id.
    ///   `setreuid(-1, -1)` — no-op.
    ///
    /// `None` — `EPERM` на границе syscall.
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

    /// `setregid(2)`: групповой аналог [`Creds::setreuid`].
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

    /// `setresuid(2)`: задать произвольное подмножество real/effective/saved
    /// user id.
    ///
    /// Непривилегированный вызывающий может двигать каждый заданный id только
    /// внутри набора `{uid, euid, suid}` (значений, которыми уже владеет);
    /// привилегированный может задать любое. `None` в аргументе — «не менять»
    /// (`(uid_t)-1`).
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

    /// `setresgid(2)`: групповой аналог [`Creds::setresuid`].
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

/// О каком виде доступа идёт проверка прав.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Execute,
}

impl Access {
    /// Бит права для этого доступа в одной триаде owner/group/other.
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
        // Класс owner побеждает, даже когда групповые биты запретили бы.
        assert!(c.may(OWNER_RW, (7, 8), Access::Read));
        assert!(c.may(OWNER_RW, (7, 8), Access::Write));
        assert!(!c.may(OWNER_RW, (7, 8), Access::Execute));
        // Класс group, когда не владелец.
        assert!(c.may(0o040, (1, 8), Access::Read));
        assert!(!c.may(0o400, (1, 8), Access::Read));
        // Класс other, когда ни то ни другое.
        assert!(c.may(WORLD_READ, (1, 2), Access::Read));
        assert!(!c.may(WORLD_READ, (1, 2), Access::Write));
        // Проверка владельца идёт по euid, а не по uid.
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

        // Ребёнок setuid-root: ruid 1000, euid/suid 0.
        let setuid_root = Creds::new(1000, 1000, 0, 1000, 0, 1000);
        let released = setuid_root.setuid(1000).expect("back to real uid");
        assert_eq!(
            (released.uid, released.euid, released.suid),
            (1000, 1000, 1000)
        );
        let back = setuid_root.setuid(0).expect("saved id is permitted");
        assert_eq!(back.euid, 0);

        // НЕ-root процесс с saved id, к которому может вернуться: может
        // переключиться на этот saved id, но не изобрести третий.
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
        // Real id может стать только текущим real или saved id; 1001 — ни тем,
        // ни другим.
        assert_eq!(saved_zero.setreuid(Some(1001), Some(1001)), None);
        // Оба запрошенных id принадлежат процессу (real = saved = 0
        // достижимы), поэтому переход удаётся, а saved id следует за новым
        // effective id.
        assert_eq!(
            saved_zero.setreuid(Some(0), Some(0)),
            Some(Creds::new(0, 1000, 0, 1000, 0, 1000))
        );
        // Запрос no-op не трогает ни один id.
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
