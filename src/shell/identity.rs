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
use crate::arch::x86_64::linux::cred::{Creds, GROUPS_CAP};
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
    ensure_boot_session();
    let stored = *SESSION.lock();
    stored.unwrap_or(Creds::ROOT)
}

/// Заменить сессионную идентичность (только из обработчиков `su`/`login`).
pub fn set_session(creds: Creds) {
    *SESSION.lock() = Some(creds);
}

/// Почему [`login_creds`] не смог построить идентичность входа.
///
/// Отдельный тип, а не `Option`: `su`/`login` печатают разные сообщения для
/// «нет такого пользователя» и «база групп недоступна», и эти два случая нельзя
/// молча смешивать с «войти нельзя».
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginError {
    /// Нет записи в `/etc/passwd`.
    UnknownUser,
    /// `/etc/group` не разбирается (пустой результат при существующем
    /// пользователе) — набор групп не построить.
    GroupDatabase,
    /// Пользователь состоит в большем числе групп, чем [`GROUPS_CAP`].
    TooManyGroups,
    /// Цель — uid 0 (`root`): вход root в этой модели не через пароль, а как
    /// явный сброс root-сессии, и `login_creds` его не выдаёт.
    RootTarget,
}

/// Креденшелы входа пользователя `name` из `/etc/passwd` + `/etc/group`.
///
/// Это единственное место, где строится «настоящая» не-root идентичность:
/// `uid = euid = suid =` uid записи, `gid = egid = sgid =` её gid, а
/// дополнительные группы — явные членства из `/etc/group` (как `setgroups(2)`
/// перед `setuid(2)` в Linux login). `root` (uid 0) не выдаётся: сброс
/// root-сессии — отдельная явная операция, а не «вход».
pub fn login_creds(name: &str) -> Result<Creds, LoginError> {
    let user = lookup_user(name).ok_or(LoginError::UnknownUser)?;
    if user.uid == 0 {
        return Err(LoginError::RootTarget);
    }
    let supplementary =
        supplementary_groups_for_user(&user.name).ok_or(LoginError::GroupDatabase)?;
    if supplementary.len() > GROUPS_CAP {
        return Err(LoginError::TooManyGroups);
    }
    Creds::for_login(user.uid, user.gid, &supplementary).ok_or(LoginError::TooManyGroups)
}

/// `/etc/shadow` в VFS-пространстве shell (`docs/security/groups.md` §4a).
///
/// Отдельный файл, как в Linux: `/etc/passwd` остаётся читаемым всеми и не несёт
/// хэшей, а shadow создаётся `provision.rs` с режимом `0600 root:root`.
pub const SHADOW_PATH: &str = "/mnt/etc/shadow";

/// Записи `/etc/shadow` (пусто, если файла нет или он не читается).
pub fn load_shadow() -> Vec<super::shadow::ShadowEntry> {
    super::shadow::parse_shadow(&read_text(SHADOW_PATH))
}

/// Совпадает ли `password` с паролем пользователя `name`.
///
/// Fail-closed: нет файла/записи, хэш закрыт (`!`/`*`) или метод не `$6$` —
/// `false` (см. [`super::shadow::verify_password`]).
pub fn verify_user_password(name: &str, password: &str) -> bool {
    let entries = load_shadow();
    let stored = super::shadow::hash_for(&entries, name).unwrap_or("");
    super::shadow::verify_password(stored, password)
}

/// Задан ли у пользователя входной пароль (хэш есть и не закрыт)?
pub fn has_password(name: &str) -> bool {
    let entries = load_shadow();
    !super::shadow::is_locked(super::shadow::hash_for(&entries, name).unwrap_or(""))
}

/// Записать новый пароль пользователя `name`.
///
/// Salt — 16 случайных байт (`misc::random_bytes_16`) в алфавите `crypt(3)`;
/// формат — `$6$<salt>$<sha512-crypt>` (Linux-совместимый, см. `shadow.rs`).
/// Пустой пароль отвергается: `passwd` не должен молча открыть учётку.
/// Возвращает `false` на пустом пароле и на ошибке записи VFS.
pub fn set_user_password(name: &str, password: &str) -> bool {
    if password.is_empty() || password.len() > super::shadow::PASSWORD_MAX {
        return false;
    }
    let salt = super::shadow::salt_from_bytes(&crate::arch::x86_64::linux::misc::random_bytes_16());
    let hash = super::shadow::hash_password(password, &salt);
    let text = read_text(SHADOW_PATH);
    let next = super::shadow::set_password(&text, name, &hash);
    write_text(SHADOW_PATH, &next)
}

/// Записать текст в файл VFS, создав его в родителе, если файла ещё нет.
///
/// Используется только для `/etc/shadow`: на дисках, провижиненных до появления
/// shadow, файла нет, а `passwd` обязан работать и там.
fn write_text(path: &str, text: &str) -> bool {
    let (node, created) = match crate::vfs::lookup_path(path) {
        Ok(node) => (node, false),
        Err(_) => {
            let Some((parent, leaf)) = path.rsplit_once('/') else {
                return false;
            };
            if parent.is_empty() || leaf.is_empty() {
                return false;
            }
            let Ok(dir) = crate::vfs::lookup_path(parent) else {
                return false;
            };
            match dir.create_file(leaf) {
                Ok(node) => (node, true),
                Err(_) => return false,
            }
        }
    };
    if created {
        // На диске, провижиненном до появления shadow, файла нет; создать его
        // с дефолтным режимом значило бы положить хэши читаемыми всем. Права
        // выставляются только новому файлу (существующий не трогаем) и отказ
        // `set_owner`/`set_mode` — это отказ записи, а не «запишем как вышло».
        if node.set_owner(0, 0).is_err() || node.set_mode(0o600).is_err() {
            return false;
        }
    }
    if crate::vfs::replace_contents(&node, text.as_bytes()).is_err() {
        return false;
    }
    node.sync();
    true
}

/// Файл, задающий **стартовую** идентичность shell (опция запуска desktop/
/// autorun не от root).
///
/// Первая непустая строка, не начинающаяся с `#`, — имя пользователя
/// `/etc/passwd`. Файла нет — сессия стартует как root (историческое
/// поведение). Битая строка/неизвестный пользователь — сессия остаётся root, а
/// причина печатается в лог: boot не должен зависеть от содержимого файла.
///
/// `provision.rs` намеренно **не** создаёт этот файл: по умолчанию всё как
/// раньше, а включение не-root сессии — явное действие администратора
/// (`write /mnt/etc/pagh-session oleg` + перезагрузка).
pub const SESSION_PATH: &str = "/mnt/etc/pagh-session";

/// Читался ли уже [`SESSION_PATH`] (решение принимается один раз за загрузку).
static BOOT_SESSION_READ: Spinlock<bool> = Spinlock::new(false);

/// Первое имя пользователя из [`SESSION_PATH`]; `None`, если файла нет,
/// только комментарии/пустые строки или чтение не удалось.
fn boot_session_name() -> Option<String> {
    let text = read_text(SESSION_PATH);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let name = line.split_whitespace().next()?;
        return Some(String::from(name));
    }
    None
}

/// Применить [`SESSION_PATH`] к сессии при первом обращении.
///
/// Вызывается из [`current`] лениво: shell готова к VFS-чтению только когда
/// команды уже исполняются, поэтому первое `id`/`su`/`lxrun` и есть корректная
/// точка входа. Ошибка не фатальна — сессия остаётся root, причина в логе.
fn ensure_boot_session() {
    {
        let mut read = BOOT_SESSION_READ.lock();
        if *read {
            return;
        }
        *read = true;
    }
    let Some(name) = boot_session_name() else {
        return;
    };
    // Сессию уже мог сменить `su` в этой же загрузке — не перетираем её.
    if SESSION.lock().is_some() {
        return;
    }
    match login_creds(&name) {
        Ok(creds) => {
            *SESSION.lock() = Some(creds);
            crate::info!(
                "[session] boot identity from {}: {} (uid={} gid={})",
                SESSION_PATH,
                name,
                creds.uid,
                creds.gid
            );
        }
        Err(err) => {
            crate::warn!(
                "[session] {}: '{}' unusable ({:?}); session stays root",
                SESSION_PATH,
                name,
                err
            );
        }
    }
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

/// Имя записи `/etc/passwd` для uid; `None`, если записи нет.
///
/// В отличие от [`user_label`] не подставляет число: вызывающий (`passwd`)
/// должен отличить «пользователь есть» от «uid без записи», а не писать
/// пароль под именем `1000`.
pub fn name_for_uid(uid: u32) -> Option<String> {
    let users = load_users();
    identity_db::user_name(&users, uid).map(String::from)
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
