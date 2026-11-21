//! Поиск программы по `PATH` (`lxrun <program>` и shell-fallback для голого
//! имени) — чистая логика, общая с host-тестами.
//!
//! Здесь живёт только решение «какие абсолютные пути пробовать»: каталоги PATH
//! ([`search_dirs`]) и пути-кандидаты для имени ([`candidates`]). Сама проба
//! VFS, чтение заголовка ELF и запуск — в `commands.rs` (`find_path_program`),
//! потому что они трогают VFS. Модуль `core` + `alloc`-only, поэтому
//! host-свойство `lxpath_paths` исполняет ровно тот исходник, который
//! компилирует ядро.
//!
//! # Семантика execvp
//!
//! * По `PATH` ищется **только имя без `/`** ([`is_bare_name`]). Абсолютный или
//!   относительный путь (`./fastfetch`, `/mnt/bin/busybox`) — это уже путь:
//!   `lxrun` исполняет его как раньше, а диспетчер shell не подменяет его
//!   поиском.
//! * PATH по умолчанию — [`DEFAULT_PATH`] (`/mnt/usr/bin:/usr/bin:/bin`).
//!   `/usr/bin` и `/bin` в этой ОС указывают на ext2-диск через `/mnt`
//!   (тот же fallback, что делает `resolve_exec_path` для полного пути), поэтому
//!   каталог `d` пробуется и как `d`, и как `/mnt{d}` — см.
//!   `commands::find_path_program`.
//! * Если вызывающий задал `PATH` в окружении, его непустые компоненты идут
//!   первыми, а недостающие значения по умолчанию добавляются следом
//!   ([`search_dirs`]). Дубликаты каталогов схлопываются.
//! * Пустые компоненты (`::`, ведущее/завершающее `:`) **пропускаются**. POSIX
//!   трактует пустой префикс как текущий каталог; shell фазы 1 не должен молча
//!   исполнять программу из CWD, поэтому это сознательное отклонение,
//!   задокументированное и покрытое свойством.

use alloc::string::String;
use alloc::vec::Vec;

/// PATH по умолчанию для `lxrun` и fallback-запуска: сначала установленный
/// userland Debian (`/mnt/usr/bin`), затем стандартные места (в этой ОС они
/// достижимы через `/mnt`-fallback).
pub const DEFAULT_PATH: &str = "/mnt/usr/bin:/usr/bin:/bin";

/// Каталоги [`DEFAULT_PATH`] по отдельности — чтобы не разбирать строку на
/// каждом запуске.
pub const DEFAULT_DIRS: [&str; 3] = ["/mnt/usr/bin", "/usr/bin", "/bin"];

/// Содержит ли имя программы `/` (то есть задан путь, а не имя для поиска).
///
/// Ровно правило `execvp(3)`: слэш в любом месте (включая ведущий) означает
/// «путь, а не поиск по PATH».
pub fn is_bare_name(token: &str) -> bool {
    !token.contains('/')
}

/// Каталоги поиска: непустые компоненты `env_path` (если он задан), затем
/// [`DEFAULT_DIRS`], которых там ещё нет.
///
/// Порядок сохраняется, дубликаты схлопываются. Пустые компоненты
/// пропускаются (см. модульную документацию). `None` и пустая строка дают
/// ровно [`DEFAULT_DIRS`].
pub fn search_dirs(env_path: Option<&str>) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::with_capacity(DEFAULT_DIRS.len() + 4);
    if let Some(list) = env_path {
        for dir in list.split(':') {
            if !dir.is_empty() && !out.contains(&dir) {
                out.push(dir);
            }
        }
    }
    for dir in DEFAULT_DIRS {
        if !out.contains(&dir) {
            out.push(dir);
        }
    }
    out
}

/// Абсолютные пути-кандидаты для имени `name` в каталогах `dirs`.
///
/// `name` с `/` (путь, а не имя) и пустое имя дают пустой список: поиск по PATH
/// к ним не применяется. Каждый кандидат — `dir` + `/` + `name`, без двойного
/// слэша, когда каталог уже заканчивается им (в том числе для `dir == "/"`).
pub fn candidates(name: &str, dirs: &[&str]) -> Vec<String> {
    if name.is_empty() || !is_bare_name(name) {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(dirs.len());
    for dir in dirs {
        if dir.is_empty() {
            continue;
        }
        let mut cand = String::with_capacity(dir.len() + 1 + name.len());
        cand.push_str(dir);
        if !dir.ends_with('/') {
            cand.push('/');
        }
        cand.push_str(name);
        out.push(cand);
    }
    out
}

/// Список каталогов одной строкой `:`-разделения — для диагностики
/// `not found in PATH (searched: …)`.
pub fn display_dirs(dirs: &[&str]) -> String {
    let mut out = String::new();
    for (i, dir) in dirs.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        out.push_str(dir);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_names_follow_execvp_slash_rule() {
        assert!(is_bare_name("nano"));
        assert!(is_bare_name("fastfetch"));
        assert!(!is_bare_name("/mnt/usr/bin/nano"));
        assert!(!is_bare_name("./fastfetch"));
        assert!(!is_bare_name("bin/fastfetch"));
        assert!(is_bare_name(""));
    }

    #[test]
    fn defaults_are_the_documented_path() {
        assert_eq!(search_dirs(None), DEFAULT_DIRS.to_vec());
        assert_eq!(search_dirs(Some("")), DEFAULT_DIRS.to_vec());
        assert_eq!(display_dirs(&DEFAULT_DIRS), DEFAULT_PATH);
    }

    #[test]
    fn env_path_comes_first_and_defaults_fill_the_gaps() {
        assert_eq!(
            search_dirs(Some("/opt/bin:/mnt/usr/bin")),
            alloc::vec!["/opt/bin", "/mnt/usr/bin", "/usr/bin", "/bin"]
        );
        // Пустые компоненты пропускаются, дубликаты схлопываются.
        assert_eq!(
            search_dirs(Some("::/bin:/bin:")),
            alloc::vec!["/bin", "/mnt/usr/bin", "/usr/bin"]
        );
    }

    #[test]
    fn candidates_join_without_double_slash() {
        assert_eq!(
            candidates("nano", &["/mnt/usr/bin", "/usr/bin", "/"]),
            alloc::vec![
                String::from("/mnt/usr/bin/nano"),
                String::from("/usr/bin/nano"),
                String::from("/nano")
            ]
        );
        // Путь и пустое имя — не повод для поиска.
        assert!(candidates("/mnt/usr/bin/nano", &DEFAULT_DIRS).is_empty());
        assert!(candidates("../nano", &DEFAULT_DIRS).is_empty());
        assert!(candidates("", &DEFAULT_DIRS).is_empty());
        assert!(candidates("nano", &[]).is_empty());
    }
}
