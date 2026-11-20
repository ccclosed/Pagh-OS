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
//!   * [`Creds::exec_setid`] — `execve` на образе с `S_ISUID`/`S_ISGID`
//!     (Linux `bprm_fill_uid()`): euid/egid и соответствующие saved id берутся
//!     у владельца файла, real id и дополнительные группы сохраняются.
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
//! Capabilities и user namespace не моделируются. `fsuid`/`fsgid` (отдельная
//! идентичность файловой системы в Linux) тоже не моделируются: проверки прав
//! используют `euid`/`egid`, что и документирует `setuid(2)` для процесса,
//! никогда не звавшего `setfsuid(2)`.
//!
//! ## Дополнительные группы (фаза 1 «группы отдельно от root»)
//!
//! [`Creds::groups`] — фиксированный набор до [`GROUPS_CAP`] supplementary gid'ов
//! (`setgroups(2)`). Linux проверяет групповой класс прав, если gid владельца
//! равен **либо** `egid`, **либо** любому supplementary gid'у; именно это делает
//! [`Creds::may`]. Набор наследуется `fork`/`clone` и переживает `execve`: Linux
//! не сбрасывает supplementary-набор на setuid-файле, это делает `su`/`login`
//! явным `setgroups(2)` перед `setuid(2)` ([`Creds::for_login`]).
//!
//! Почему cap, а не `Vec`: `Creds` — `Copy`-снимок в `CompatState`, который
//! копируется на каждом `fork`/проверке прав в `no_std`-ядре с фиксированной
//! кучей. Linux `NGROUPS_MAX` = 65536; здесь осознанно 32 — это больше, чем
//! нужно фазе 1 (root, wheel, disk, tty, video, input, net), и не раздувает
//! состояние процесса. `setgroups(2)` с большим `size` отвергается `EINVAL`,
//! а не обрезается молча.
#![allow(dead_code)]

/// Максимум дополнительных групп у процесса (`setgroups(2)`).
///
/// Linux `NGROUPS_MAX` = 65536; pagh сознательно ограничивает набор 32
/// группами и документирует это в `docs/security/groups.md` как ограничение
/// фазы 1.
pub const GROUPS_CAP: usize = 32;

/// Бит set-user-ID в `st_mode` (`S_ISUID`): `execve` ставит `euid = suid =`
/// владельцу файла.
pub const S_ISUID: u32 = 0o4000;

/// Бит set-group-ID в `st_mode` (`S_ISGID`): `execve` ставит `egid = sgid =`
/// группе файла.
pub const S_ISGID: u32 = 0o2000;

/// Номера групп pagh (фаза 1).
///
/// Числа взяты из общепринятых Linux-конвенций, чтобы `/etc/group` и образы
/// Debian-подобных систем не расходились: `tty = 5`, `disk = 6`, `wheel = 10`,
/// `video = 44`, `input = 105`. `net = 90` — номер исторической группы `net`,
/// который pagh резервирует для сетевого администрирования (сам по себе он пока
/// не даёт прав: `security-model.md` §capabilities, фаза 2). Занятость номеров в
/// образе userland не проверяется — это наши собственные узлы/файлы.
pub mod gid {
    /// Группа дисковых устройств (raw block devices, фаза 2).
    pub const DISK: u32 = 6;
    /// Группа терминалов: `/dev/vtconsole0`.
    pub const TTY: u32 = 5;
    /// Группа администраторов (`su`, фаза 2 login/setuid).
    pub const WHEEL: u32 = 10;
    /// Группа доступа к фреймбуферу: `/dev/fb0`.
    pub const VIDEO: u32 = 44;
    /// Группа устройств ввода: `/dev/input/event0`, `/dev/vtinput`.
    pub const INPUT: u32 = 105;
    /// Группа сетевого администрирования (фаза 2).
    pub const NET: u32 = 90;
    /// Группа пользователя `oleg` (uid 1000) — основная в `/etc/group`.
    pub const OLEG: u32 = 1000;
}

/// Шесть POSIX-идентичностей процесса плюс дополнительные группы.
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
    /// Saved set-user-ID: переписывается `execve` на образе с `S_ISUID`
    /// ([`Creds::exec_setid`]) и `set*id`-переходами.
    pub suid: u32,
    /// Saved set-group-ID (сохранённый group id).
    pub sgid: u32,
    /// Дополнительные gid'ы (`setgroups(2)`), значимы только первые
    /// `ngroups` записей. Хвост всегда нулевой (инвариант конструкторов).
    groups: [u32; GROUPS_CAP],
    /// Сколько элементов `groups` задано (0..=[`GROUPS_CAP`]).
    ngroups: usize,
}

impl Creds {
    /// Идентичность, с которой стартуют все запущенные ядром процессы и шелл
    /// загрузки. Сохранение root поддерживает существующий userland (CPython,
    /// apt). Дополнительных групп нет: root обходит DAC и без них.
    pub const ROOT: Creds = Creds {
        uid: 0,
        gid: 0,
        euid: 0,
        egid: 0,
        suid: 0,
        sgid: 0,
        groups: [0; GROUPS_CAP],
        ngroups: 0,
    };

    /// Удобный конструктор для тестов и для политики запуска не от root.
    ///
    /// Дополнительные группы пусты; [`Creds::with_groups`] добавляет их.
    pub const fn new(uid: u32, gid: u32, euid: u32, egid: u32, suid: u32, sgid: u32) -> Self {
        Creds {
            uid,
            gid,
            euid,
            egid,
            suid,
            sgid,
            groups: [0; GROUPS_CAP],
            ngroups: 0,
        }
    }

    /// Копия с заданным дополнительным набором групп (внутренняя политика,
    /// тесты, запуск `su` из shell).
    ///
    /// `None`, когда `groups.len() > `[`GROUPS_CAP`] — вызывающий обязан
    /// сообщить ошибку, а не молча обрезать набор.
    pub fn with_groups(mut self, groups: &[u32]) -> Option<Creds> {
        if groups.len() > GROUPS_CAP {
            return None;
        }
        self.groups = [0; GROUPS_CAP];
        self.groups[..groups.len()].copy_from_slice(groups);
        self.ngroups = groups.len();
        Some(self)
    }

    /// Дополнительные группы в порядке, заданном `setgroups(2)`.
    #[inline]
    pub fn groups(&self) -> &[u32] {
        &self.groups[..self.ngroups]
    }

    /// Входит ли `gid` в групповую идентичность процесса?
    ///
    /// `egid` и любой дополнительный gid дают групповой класс прав; real `gid`
    /// сам по себе — нет (Linux решает по effective id, а supplementary-набор
    /// общий для всех трёх).
    #[inline]
    pub fn in_group(&self, gid: u32) -> bool {
        self.egid == gid || self.groups().contains(&gid)
    }

    /// `setgroups(2)`: заменить дополнительные группы.
    ///
    /// Только привилегированный вызывающий (`euid == 0`, модель `CAP_SETGID`);
    /// `size > `[`GROUPS_CAP`] или непривилегированный — `None` (`EINVAL`/
    /// `EPERM` на границе syscall). Gid 0 разрешён: Linux с `CAP_SETGID` не
    /// запрещает ни одного *числа* gid'а, запрет — только на идентичность
    /// вызывающего.
    pub fn setgroups(&self, groups: &[u32]) -> Option<Creds> {
        if self.euid != 0 || groups.len() > GROUPS_CAP {
            return None;
        }
        self.with_groups(groups)
    }

    /// Взгляд на те же креденшелы через **real** id'ы — семантика `access(2)`.
    ///
    /// `access(2)` проверяет real uid/gid, чтобы setuid-подобное повышение не
    /// позволяло прощупать недоступное. Дополнительные группы при этом
    /// сохраняются: `setgroups(2)` меняет общий набор, а не effective-проекцию,
    /// поэтому отбрасывать их здесь было бы ошибкой (именно так group-класс
    /// `access()` перестал бы находить свои группы).
    pub fn with_real_ids(&self) -> Creds {
        let mut next = *self;
        next.euid = self.uid;
        next.egid = self.gid;
        next
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
    ///   `egid == gid владельца` **или** gid владельца входит в дополнительные
    ///   группы → биты group; иначе биты other.
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
        } else if self.in_group(owner.1) {
            3
        } else {
            0
        };
        (perms >> class_shift) & access.bit() != 0
    }

    /// Бит sticky (`S_ISVTX`) в младших битах `mode` каталога.
    ///
    /// POSIX: каталог с этим битом (`/tmp` 1777, `/var/tmp`) разрешает удаление
    /// или переименование записи только root'у, владельцу каталога и владельцу
    /// самой записи. Значение то же, что у Linux `S_ISVTX`; VFS хранит его в
    /// `VfsNode::mode` рядом с девятью битами прав, а решение принимает
    /// [`Creds::may_delete_child`].
    pub const S_ISVTX: u32 = 0o1000;

    /// Может ли процесс удалить/переименовать запись `child_owner` внутри
    /// каталога с режимом `dir_mode` и владельцем `dir_owner` — **sticky-правило**?
    ///
    /// Вызывающий уже проверил обычное условие записи (`w`+`x` на каталоге,
    /// см. [`Creds::may`]); здесь остаётся только дополнительный запрет sticky:
    ///
    /// * каталог без `S_ISVTX` — `true` (правило решается битами каталога);
    /// * sticky — `true` только для root (`CAP_FOWNER` сведён к `euid == 0`),
    ///   владельца каталога и владельца записи. Чужой файл в `/tmp` удалить
    ///   нельзя, свой — можно.
    ///
    /// Это чистая функция метаданных, как и [`Creds::may`]: VFS не знает
    /// вызывающего, syscall-слой передаёт ему `mode`/`owner` узлов.
    pub fn may_delete_child(
        &self,
        dir_mode: u32,
        dir_owner: (u32, u32),
        child_owner: (u32, u32),
    ) -> bool {
        if dir_mode & Self::S_ISVTX == 0 {
            return true;
        }
        self.is_root() || self.euid == dir_owner.0 || self.euid == child_owner.0
    }

    /// Может ли процесс занять привилегированный порт `< 1024`?
    ///
    /// Linux требует `CAP_NET_BIND_SERVICE`; pagh сводит набор capabilities к
    /// `euid == 0`, как и остальные проверки. Порты `>= 1024` свободны для всех,
    /// порт `0` (эфемерный) — тоже: ядро само выберет непривилегированный.
    ///
    /// Это чистая policy-функция: `bind(2)`/`listen(2)` в `inet_sock.rs` зовут её
    /// до любой работы со стеком, а host-свойство проверяет границу.
    #[inline]
    pub fn may_bind_port(&self, port: u16) -> bool {
        port == 0 || port >= 1024 || self.is_root()
    }

    /// Может ли процесс создать `SOCK_RAW`-сокет?
    ///
    /// Linux требует `CAP_NET_RAW`; pagh сводит набор capabilities к
    /// `euid == 0`, как и в [`Creds::may_bind_port`]. Это только гейт допуска:
    /// сам пакетный уровень в ядре не реализован, поэтому root получает
    /// `EOPNOTSUPP`, а не сокет (см. `inet_sock.rs`).
    #[inline]
    pub fn may_use_raw_socket(&self) -> bool {
        self.is_root()
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

    /// Креденшелы после успешного `execve` образа с `mode`/`owner`.
    ///
    /// Повторяет `bprm_fill_uid()` из `fs/exec.c` Linux: бит `S_ISUID` ставит
    /// `euid = suid = owner.uid`, бит `S_ISGID` — `egid = sgid = owner.gid`.
    /// Оба бита независимы. Real id, второй saved id и дополнительные группы
    /// **не** меняются: Linux тоже не трогает supplementary-набор на
    /// setuid-файле (его сбрасывает `su`/login явным `setgroups(2)`).
    ///
    /// Ограничение модели названо, а не подразумевается: у pagh нет
    /// `MS_NOSUID`-флагов монтирования, `ptrace(2)` и `dumpable`, поэтому
    /// «под отладчиком» и «nosuid-монтирование» эту функцию не отключают —
    /// применяется только inode-бит, а ставить его может лишь root (`chown`/
    /// `chmod` — root или владелец файла).
    pub fn exec_setid(&self, mode: u32, owner: (u32, u32)) -> Creds {
        let mut next = *self;
        if mode & S_ISUID != 0 {
            next.euid = owner.0;
            next.suid = owner.0;
        }
        if mode & S_ISGID != 0 {
            next.egid = owner.1;
            next.sgid = owner.1;
        }
        next
    }

    /// Работает ли процесс с effective id, отличным от real (Linux
    /// «privileged process»)?
    ///
    /// Это **состояние**, а не переход: после setuid/setgid-on-exec
    /// непривилегированный вызывающий как раз оказывается в нём, и именно это
    /// Linux называет `secureexec`/`AT_SECURE`. В pagh нет ни `AT_SECURE`, ни
    /// `dumpable`, поэтому флаг существует, чтобы сисколл-слой мог назвать
    /// состояние в логе, а не выводить его повторно.
    pub fn runs_privileged(&self) -> bool {
        self.euid != self.uid || self.egid != self.gid
    }

    /// Креденшелы входа пользователя: `login`/`su` дают процессу
    /// `uid = euid = suid` и `gid = egid = sgid` цели плюс её дополнительные
    /// группы из `/etc/group`.
    ///
    /// `None` — набор групп не влезает в [`GROUPS_CAP`]; вызывающий обязан
    /// сообщить ошибку, а не обрезать набор (та же политика, что у
    /// [`Creds::with_groups`]).
    pub fn for_login(uid: u32, gid: u32, supplementary: &[u32]) -> Option<Creds> {
        Creds::new(uid, gid, uid, gid, uid, gid).with_groups(supplementary)
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

    #[test]
    fn supplementary_gid_grants_the_group_class() {
        // egid 1000, но файл принадлежит группе video(44): только
        // дополнительная группа 44 даёт доступ; other-биты запрещены.
        let c = Creds::new(1000, 1000, 1000, 1000, 1000, 1000)
            .with_groups(&[44, 105])
            .expect("two supplementary groups fit");
        assert_eq!(c.groups(), &[44, 105]);
        assert!(c.in_group(44));
        assert!(c.in_group(105));
        assert!(c.in_group(1000), "egid is always a group identity");
        assert!(!c.in_group(10));
        assert!(c.may(0o640, (0, 44), Access::Read));
        assert!(!c.may(0o640, (0, 44), Access::Write));
        assert!(!c.may(0o660, (0, 10), Access::Read), "foreign group denied");
        assert!(
            c.may(0o004, (0, 10), Access::Read),
            "other bits still apply"
        );

        // Без дополнительной группы тот же файл закрыт (доказательство, что
        // доступ дал именно supplementary set, а не something else).
        let none = Creds::new(1000, 1000, 1000, 1000, 1000, 1000);
        assert!(!none.in_group(44));
        assert!(!none.may(0o640, (0, 44), Access::Read));
    }

    #[test]
    fn setgroups_is_root_only_capped_and_allows_gid_zero() {
        let root = Creds::ROOT;
        let with = root.setgroups(&[0, 44, 105]).expect("root may setgroups");
        assert_eq!(with.groups(), &[0, 44, 105]);
        assert!(with.in_group(0), "gid 0 may be listed explicitly");
        // Linux `setgroups(0, NULL)` очищает набор.
        assert!(with.setgroups(&[]).expect("clear").groups().is_empty());

        // Больше cap — отказ, а не молчаливое обрезание.
        let too_many = [7u32; GROUPS_CAP + 1];
        assert_eq!(root.setgroups(&too_many), None);
        // Ровно cap принимается.
        let exactly = [7u32; GROUPS_CAP];
        assert_eq!(
            root.setgroups(&exactly).expect("cap is inclusive").groups(),
            &exactly[..]
        );

        // Непривилегированный не может менять дополнительные группы даже на
        // пустой набор; euid, а не uid, решает (Linux CAP_SETGID).
        let plain = Creds::new(1000, 1000, 1000, 1000, 1000, 1000);
        assert_eq!(plain.setgroups(&[]), None);
        assert_eq!(plain.setgroups(&[44]), None);
        let setuid_root = Creds::new(1000, 1000, 0, 1000, 0, 1000);
        assert!(
            setuid_root.setgroups(&[44]).is_some(),
            "euid 0 is privileged"
        );
    }

    #[test]
    fn with_real_ids_projects_access_semantics_and_keeps_groups() {
        // setuid-подобное состояние: real 1000, effective 0, supplemental 44.
        let c = Creds::new(1000, 1000, 0, 44, 0, 44)
            .with_groups(&[105])
            .expect("groups fit");
        let real = c.with_real_ids();
        assert_eq!(
            (real.uid, real.gid, real.euid, real.egid, real.suid, real.sgid),
            (1000, 1000, 1000, 1000, 0, 44)
        );
        assert_eq!(real.groups(), &[105], "supplementary set survives access()");
        // effective-взгляд — root bypass; real-взгляд — обычные биты.
        assert!(c.may(0o000, (1000, 1000), Access::Read));
        assert!(!real.may(0o000, (1000, 1000), Access::Read));
        // И group-класс real-взгляда видит 105, но не 44 (egid стал 1000).
        assert!(real.may(0o040, (0, 105), Access::Read));
        assert!(!real.may(0o040, (0, 44), Access::Read));
    }

    #[test]
    fn gid_constants_are_distinct_and_match_the_documented_numbers() {
        let all = [
            gid::DISK,
            gid::TTY,
            gid::WHEEL,
            gid::VIDEO,
            gid::INPUT,
            gid::NET,
            gid::OLEG,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b, "group numbers must be distinct");
            }
        }
        assert_eq!(
            (
                gid::TTY,
                gid::DISK,
                gid::WHEEL,
                gid::VIDEO,
                gid::INPUT,
                gid::NET
            ),
            (5, 6, 10, 44, 105, 90)
        );
    }

    #[test]
    fn setuid_exec_of_a_root_binary_raises_euid_and_suid_only() {
        // uid 1000 запускает root-owned 04755: Linux ставит euid/suid = 0,
        // real id (1000) и группы не трогает.
        let user = Creds::new(1000, 1000, 1000, 1000, 1000, 1000);
        let next = user.exec_setid(0o4755, (0, 0));
        assert_eq!((next.uid, next.euid, next.suid), (1000, 0, 0));
        assert_eq!((next.gid, next.egid, next.sgid), (1000, 1000, 1000));
        assert!(next.is_root());
        assert!(next.runs_privileged());
        // Обычный исполняемый файл (0755 root:root) не меняет ничего.
        let plain = user.exec_setid(0o755, (0, 0));
        assert_eq!(plain, user);
        assert!(!plain.runs_privileged());
    }

    #[test]
    fn setgid_exec_sets_egid_and_sgid_and_leaves_the_user_identity_alone() {
        let user = Creds::new(1000, 1000, 1000, 1000, 1000, 1000);
        let next = user.exec_setid(0o2755, (0, gid::VIDEO));
        assert_eq!((next.gid, next.egid, next.sgid), (1000, 44, 44));
        assert_eq!((next.uid, next.euid, next.suid), (1000, 1000, 1000));
        assert!(next.runs_privileged());
        // Оба бита независимы.
        let both = user.exec_setid(S_ISUID | S_ISGID | 0o755, (0, 44));
        assert_eq!((both.euid, both.egid), (0, 44));
        assert_eq!((both.suid, both.sgid), (0, 44));
    }

    #[test]
    fn setid_exec_keeps_real_ids_and_supplementary_groups() {
        let user = Creds::new(1000, 1000, 1000, 1000, 1000, 1000)
            .with_groups(&[5, 44, 105])
            .unwrap();
        let next = user.exec_setid(0o6755, (0, 0));
        assert_eq!((next.uid, next.gid), (1000, 1000));
        assert_eq!(next.groups(), &[5, 44, 105]);
        assert_eq!((next.suid, next.sgid), (0, 0));
    }

    #[test]
    fn for_login_maps_every_id_to_the_target_and_carries_the_group_set() {
        let c = Creds::for_login(1000, 1000, &[5, 44, 105]).expect("three groups fit");
        assert_eq!(
            (c.uid, c.gid, c.euid, c.egid, c.suid, c.sgid),
            (1000, 1000, 1000, 1000, 1000, 1000)
        );
        assert_eq!(c.groups(), &[5, 44, 105]);
        assert!(!c.is_root());
        // Набор больше cap'а отвергается, а не обрезается.
        let too_many = [0u32; GROUPS_CAP + 1];
        assert!(Creds::for_login(1000, 1000, &too_many).is_none());
    }
}
