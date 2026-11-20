//! Чистый разбор `/etc/passwd` и `/etc/group` для shell-команд `id`/`groups`.
//!
//! Модуль `core`+`alloc`-only и не знает VFS: файлы читает
//! [`super::identity`], а этот файл — разбор строк и групповые запросы. Так
//! host-свойства исполняют ровно тот исходник, который компилирует ядро
//! (философия тестирования в `AGENTS.md`), а in-QEMU selftest может звать те же
//! функции без VFS.
//!
//! Контракт фазы 1 — **минимум**: только passwd- и group-файлы, никаких
//! `shadow`/`gshadow`, NIS (`+`/`-` строки) и `getgrouplist(3)` с LDAP. Строки,
//! которые не разбираются как 7-польный passwd или 4-польный group, молча
//! пропускаются: boot не должен падать из-за одной кривой строки.
#![allow(dead_code)]

use alloc::string::String;
use alloc::vec::Vec;

/// Одна запись `/etc/passwd`: `name:passwd:uid:gid:gecos:home:shell`.
///
/// Поле `passwd` не хранится: паролей в фазе 1 нет, а тащить хэш в shell
/// незачем.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserEntry {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    /// Пятое поле (`gecos`); пустая строка, если отсутствует.
    pub gecos: String,
    pub home: String,
    pub shell: String,
}

/// Одна запись `/etc/group`: `name:passwd:gid:member,member,...`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupEntry {
    pub name: String,
    pub gid: u32,
    /// Члены группы в порядке файла (без пустых элементов).
    pub members: Vec<String>,
}

/// Разобрать `/etc/passwd`.
///
/// Комментарии (`#`) и NIS-строки (`+`/`-`) пропускаются; строки не с семью
/// полями, с пустым именем, с нечисловым uid/gid (в том числе отрицательным или
/// больше `u32`) — тоже. Дубликаты имён/uid не разрешаются: Linux-утилиты берут
/// первое совпадение, и `user_by_name` следует этому.
pub fn parse_passwd(text: &str) -> Vec<UserEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with('+')
            || line.starts_with('-')
        {
            continue;
        }
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 7 {
            continue;
        }
        let name = fields[0];
        if name.is_empty() {
            continue;
        }
        let (Ok(uid), Ok(gid)) = (fields[2].parse::<u32>(), fields[3].parse::<u32>()) else {
            continue;
        };
        out.push(UserEntry {
            name: String::from(name),
            uid,
            gid,
            gecos: String::from(fields[4]),
            home: String::from(fields[5]),
            shell: String::from(fields[6]),
        });
    }
    out
}

/// Разобрать `/etc/group`.
///
/// Как и [`parse_passwd`]: комментарии/NIS и кривые строки пропускаются, пустые
/// элементы списка членов отбрасываются (`video:x:44:` — группа без членов).
pub fn parse_group(text: &str) -> Vec<GroupEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with('+')
            || line.starts_with('-')
        {
            continue;
        }
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 4 {
            continue;
        }
        let name = fields[0];
        if name.is_empty() {
            continue;
        }
        let Ok(gid) = fields[2].parse::<u32>() else {
            continue;
        };
        let members = fields[3]
            .split(',')
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(String::from)
            .collect();
        out.push(GroupEntry {
            name: String::from(name),
            gid,
            members,
        });
    }
    out
}

/// Найти пользователя по имени (первое вхождение, как `getpwnam` на дублях).
pub fn user_by_name<'a>(users: &'a [UserEntry], name: &str) -> Option<&'a UserEntry> {
    users.iter().find(|u| u.name == name)
}

/// Найти пользователя по uid.
pub fn user_by_uid<'a>(users: &'a [UserEntry], uid: u32) -> Option<&'a UserEntry> {
    users.iter().find(|u| u.uid == uid)
}

/// Найти группу по gid (первое вхождение).
pub fn group_by_gid<'a>(groups: &'a [GroupEntry], gid: u32) -> Option<&'a GroupEntry> {
    groups.iter().find(|g| g.gid == gid)
}

/// Имя группы по gid, если оно есть в базе.
pub fn group_name<'a>(groups: &'a [GroupEntry], gid: u32) -> Option<&'a str> {
    group_by_gid(groups, gid).map(|g| g.name.as_str())
}

/// Имя пользователя по uid, если оно есть в базе.
pub fn user_name<'a>(users: &'a [UserEntry], uid: u32) -> Option<&'a str> {
    user_by_uid(users, uid).map(|u| u.name.as_str())
}

/// Полный групповой набор пользователя глазами `id`/`groups`: основная группа
/// (gid из passwd) первой, затем все группы, где имя пользователя перечислено
/// явно, в порядке `/etc/group`. Дубликаты gid схлопываются.
///
/// `None`, когда пользователя нет в базе: вызывающий отличает «нет такого
/// пользователя» от «есть, но без дополнительных групп».
pub fn groups_for_user(users: &[UserEntry], groups: &[GroupEntry], name: &str) -> Option<Vec<u32>> {
    let user = user_by_name(users, name)?;
    let mut out = Vec::with_capacity(1 + groups.len());
    out.push(user.gid);
    for group in groups {
        if group.members.iter().any(|m| m == name) && !out.contains(&group.gid) {
            out.push(group.gid);
        }
    }
    Some(out)
}

/// Дополнительные группы пользователя — то, что грузится в `setgroups(2)`:
/// только явные членства, без основной группы (она и так `egid`). Так
/// `getgroups(2)`/`id` показывают ровно то, что перечислено в `/etc/group`.
pub fn supplementary_groups_for_user(
    users: &[UserEntry],
    groups: &[GroupEntry],
    name: &str,
) -> Option<Vec<u32>> {
    let user = user_by_name(users, name)?;
    let mut out = Vec::new();
    for group in groups {
        if group.gid != user.gid
            && group.members.iter().any(|m| m == name)
            && !out.contains(&group.gid)
        {
            out.push(group.gid);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "# comment\nroot:x:0:0:root:/root:/bin/sh\noleg:x:1000:1000:Oleg:/home/oleg:/bin/sh\nbroken:x:notanumber:1000:::x\nshort:x:1000\n";
    const GROUP: &str =
        "root:x:0:\ntty:x:5:oleg\ndisk:x:6:\nvideo:x:44:oleg,alice\ninput:x:105:oleg\n";

    #[test]
    fn passwd_parses_valid_lines_and_skips_malformed() {
        let users = parse_passwd(PASSWD);
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].name, "root");
        assert_eq!((users[0].uid, users[0].gid), (0, 0));
        assert_eq!(users[1].home, "/home/oleg");
        assert!(user_by_name(&users, "oleg").is_some());
        assert!(user_by_uid(&users, 1000).is_some());
        assert!(user_by_uid(&users, 999).is_none());
    }

    #[test]
    fn group_parses_members_and_skips_empty_members() {
        let groups = parse_group(GROUP);
        assert_eq!(groups.len(), 5);
        assert!(groups[0].members.is_empty(), "root group has no members");
        assert_eq!(groups[1].members, ["oleg"]);
        assert_eq!(groups[3].gid, 44);
        assert_eq!(groups[3].members, ["oleg", "alice"]);
        assert_eq!(group_name(&groups, 105), Some("input"));
        assert_eq!(group_name(&groups, 7), None);
    }

    #[test]
    fn groups_for_user_puts_primary_first_and_dedups() {
        let users = parse_passwd(PASSWD);
        let groups = parse_group(GROUP);
        assert_eq!(
            groups_for_user(&users, &groups, "oleg"),
            Some(vec![1000, 5, 44, 105])
        );
        assert_eq!(
            supplementary_groups_for_user(&users, &groups, "oleg"),
            Some(vec![5, 44, 105])
        );
        // alice есть только как член video: основной группы в passwd нет.
        assert_eq!(groups_for_user(&users, &groups, "alice"), None);
        assert_eq!(groups_for_user(&users, &groups, "root"), Some(vec![0]));
        assert_eq!(
            supplementary_groups_for_user(&users, &groups, "root"),
            Some(vec![])
        );
    }

    #[test]
    fn crlf_and_whitespace_do_not_break_parsing() {
        let users = parse_passwd("root:x:0:0:root:/root:/bin/sh\r\n");
        assert_eq!(users.len(), 1);
        let groups = parse_group("video:x:44: oleg , alice \r\n");
        assert_eq!(groups[0].members, ["oleg", "alice"]);
    }
}
