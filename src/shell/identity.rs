//! Сессионная идентичность shell и доступ к `/etc/passwd` + `/etc/group`.
//!
//! ## Зачем отдельный слой
//!
//! Ядро-native shell — не `Compat_Process`: у него нет [`Creds`] в
//! `CompatState`, а VFS вызывается из ring 0 напрямую. Фаза 1 «группы отдельно
//! от root» всё же должна дать пользователю `id`, `groups` и `su`, поэтому
//! shell держит **сессионную** идентичность: креденшелы, с которыми он запускает
//! Linux-программы (`lxrun`). `su` меняет её; `run_linux_binary_as` кладёт
//! снимок в `CompatState` ребёнка, и дальше все проверки прав слоя syscall
//! работают обычным путём.
//!
//! **Явное ограничение фазы 1** (подробно — `docs/security/groups.md`):
//! встроенные команды shell (`cat`, `ls`, `write`, `pkg`, …) остаются
//! kernel-кодом и исполняются с привилегиями ring 0 независимо от сессии. `su`
//! поэтому не является границей безопасности для shell itself; он честно
//! понижает только процессы, которые shell запускает через `lxrun`. Это
//! осознанно: менять архитектуру «VFS не знает вызывающего» в фазе 1 нельзя
//! (boot provisioning, установщик пакетов и shell вызывают VFS напрямую), а
//! login/setuid/password — предмет фазы 2.
#![allow(dead_code)]

use alloc::string::String;
use alloc::vec::Vec;

use super::identity_db::{self, GroupEntry, UserEntry};
use crate::arch::x86_64::linux::cred::Creds;
use crate::sync::spinlock::Spinlock;

/// Путь, который видят и shell (VFS), и Linux-программы (гостевое отображение
/// `/etc` → `/mnt/etc`).
pub const PASSWD_PATH: &str = "/mnt/etc/passwd";
/// `/etc/group` для shell-команд.
pub const GROUP_PATH: &str = "/mnt/etc/group";

/// Верхняя граница для `/etc/passwd` и `/etc/group`: реальные файлы — сотни
/// байт; всё, что больше 64 КиБ, — не наша база идентичностей.
const DB_MAX_BYTES: usize = 64 * 1024;

/// Сессионная идентичность shell. `None` — свежая сессия root (как
/// [`Creds::ROOT`]).
static SESSION: Spinlock<Option<Creds>> = Spinlock::new(None);

/// Текущая сессионная идентичность (root, пока `su` не сменил её).
pub fn current() -> Creds {
    let stored = *SESSION.lock();
    stored.unwrap_or(Creds::ROOT)
}

/// Заменить сессионную идентичность (только из обработчика `su`).
pub fn set_session(creds: Creds) {
    *SESSION.lock() = Some(creds);
}

/// Прочитать файл VFS в `String`; пустая строка при любой ошибке.
///
/// Чтение идёт кусками до `size()`; короткое чтение не зацикливает.
fn read_text(path: &str) -> String {
    let Ok(node) = crate::vfs::lookup_path(path) else {
        return String::new();
    };
    let size = node.size() as usize;
    if size == 0 || size > DB_MAX_BYTES {
        return String::new();
    }
    let mut buf = alloc::vec![0u8; size];
    let mut off = 0usize;
    while off < size {
        match node.read(off as u64, &mut buf[off..]) {
            Ok(0) => break,
            Ok(n) => off += n,
            Err(_) => return String::new(),
        }
    }
    buf.truncate(off);
    String::from_utf8(buf).unwrap_or_default()
}

/// Пользователи из `/etc/passwd` (пусто, если файла нет или он битый).
pub fn load_users() -> Vec<UserEntry> {
    identity_db::parse_passwd(&read_text(PASSWD_PATH))
}

/// Группы из `/etc/group`.
pub fn load_groups() -> Vec<GroupEntry> {
    identity_db::parse_group(&read_text(GROUP_PATH))
}

/// Пользователь по имени.
pub fn lookup_user(name: &str) -> Option<UserEntry> {
    identity_db::user_by_name(&load_users(), name).cloned()
}

/// Групповой набор пользователя для `id`/`groups`: основная группа первой,
/// затем явные членства из `/etc/group`.
pub fn display_groups_for_user(name: &str) -> Option<Vec<u32>> {
    identity_db::groups_for_user(&load_users(), &load_groups(), name)
}

/// Дополнительные группы пользователя — загружаются в `setgroups` при `su`.
pub fn supplementary_groups_for_user(name: &str) -> Option<Vec<u32>> {
    identity_db::supplementary_groups_for_user(&load_users(), &load_groups(), name)
}

/// Групповой набор сессии глазами syscall-семантики: `egid` + дополнительные
/// gid'ы, без дубликатов.
pub fn session_groups() -> Vec<u32> {
    let creds = current();
    let mut out = Vec::with_capacity(1 + creds.groups().len());
    out.push(creds.egid);
    for gid in creds.groups() {
        if !out.contains(gid) {
            out.push(*gid);
        }
    }
    out
}

/// Имя пользователя по uid для вывода; числовая строка, если базы нет.
pub fn user_label(uid: u32) -> String {
    let users = load_users();
    match identity_db::user_name(&users, uid) {
        Some(name) => String::from(name),
        None => alloc::format!("{}", uid),
    }
}

/// Имя группы по gid для вывода; числовая строка, если базы нет.
pub fn group_label(gid: u32) -> String {
    let groups = load_groups();
    match identity_db::group_name(&groups, gid) {
        Some(name) => String::from(name),
        None => alloc::format!("{}", gid),
    }
}

/// Строка `44(video)` из gid для `id`/`su`; без имени — просто число.
pub fn format_gid(gid: u32) -> String {
    let groups = load_groups();
    match identity_db::group_name(&groups, gid) {
        Some(name) => alloc::format!("{}({})", gid, name),
        None => alloc::format!("{}", gid),
    }
}

/// Групповые метки через запятую (порядок сохраняется).
pub fn format_gids(gids: &[u32]) -> String {
    let groups = load_groups();
    let mut out = String::new();
    for (i, gid) in gids.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match identity_db::group_name(&groups, *gid) {
            Some(name) => {
                out.push_str(&alloc::format!("{}({})", gid, name));
            }
            None => {
                out.push_str(&alloc::format!("{}", gid));
            }
        }
    }
    out
}
