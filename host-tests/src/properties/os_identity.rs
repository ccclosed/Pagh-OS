// Идентичность ОС (`docs/os-identity.md`).
//
// Pagh — самостоятельная ОС с Linux-совместимым syscall ABI, а не
// Linux-дистрибутив. Свойства фиксируют две половины этого контракта:
//
//  * `/etc/os-release` называет систему `Pagh-OS` (`ID=pagh`) и несёт
//    настоящую версию крейта — именно этот файл читает `fastfetch` для строки
//    `OS`, поэтому строка не может снова стать `linux-pagh`/`pagh OS`;
//  * `uname(2)` остаётся `Linux` по `sysname` — это UTS-совместимость с Linux
//    ABI, а не имя ОС; константы зафиксированы, чтобы «улучшение» sysname не
//    проехало молча и не сломало configure-скрипты.

use crate::os_identity::{
    format_motd, format_os_release, format_pagh_release, is_canonical_os_release, OS_ID, OS_NAME,
    UTS_DOMAINNAME, UTS_MACHINE, UTS_NODENAME, UTS_RELEASE, UTS_SYSNAME, UTS_VERSION,
};
use proptest::prelude::*;
use std::collections::BTreeMap;

/// Версия, которую реально может содержать `CARGO_PKG_VERSION` (semver с
/// необязательным pre-release-хвостом). Никаких пробелов/переводов строк,
/// поэтому однострочность — свойство форматтера, а не санитайзера.
fn version_strategy() -> impl Strategy<Value = String> {
    "[0-9]{1,2}\\.[0-9]{1,2}\\.[0-9]{1,2}(-[0-9A-Za-z.]{1,12})?"
}

/// Разобрать `KEY=VALUE`/`KEY="VALUE"` в карту. Дубликаты — ошибка: os-release
/// с двумя `NAME=` неоднозначен для парсера, и форматтер не должен их выдавать.
fn fields(out: &[u8]) -> BTreeMap<String, String> {
    let text = core::str::from_utf8(out).expect("os-release is ASCII");
    let mut map = BTreeMap::new();
    for line in text.trim_end_matches('\n').lines() {
        let (key, value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("line without '=': {line:?}"));
        assert!(!key.is_empty(), "empty key in {line:?}");
        let value = value.trim_matches('"');
        assert!(
            map.insert(key.to_string(), value.to_string()).is_none(),
            "duplicate key {key:?}"
        );
    }
    map
}

proptest! {
    /// `fastfetch` (и любой os-release-парсер) видит `NAME=Pagh-OS`, `ID=pagh`
    /// и настоящую версию; Linux-имени здесь нет.
    #[test]
    fn os_release_names_pagh_os(version in version_strategy()) {
        let out = format_os_release(&version);
        let text = core::str::from_utf8(&out).unwrap();
        prop_assert!(text.ends_with('\n'));
        prop_assert!(!text.contains('\0'));
        prop_assert!(!text.contains("Linux"));
        prop_assert!(!text.contains("linux"));

        let map = fields(&out);
        prop_assert_eq!(map.len(), 6);
        prop_assert_eq!(map.get("NAME").map(String::as_str), Some("Pagh-OS"));
        prop_assert_eq!(map.get("ID").map(String::as_str), Some("pagh"));
        prop_assert_eq!(map.get("VERSION").map(String::as_str), Some(version.as_str()));
        prop_assert_eq!(map.get("VERSION_ID").map(String::as_str), Some(version.as_str()));
        let pretty = format!("Pagh-OS {version}");
        prop_assert_eq!(map.get("PRETTY_NAME").map(String::as_str), Some(pretty.as_str()));
        prop_assert_eq!(map.get("ARCH").map(String::as_str), Some("x86_64"));
    }

    /// `pagh-release` — компактное зеркало той же идентичности; старое
    /// `NAME=pagh OS` не возвращается.
    #[test]
    fn pagh_release_mirrors_canonical_name(version in version_strategy()) {
        let out = format_pagh_release(&version);
        let text = core::str::from_utf8(&out).unwrap();
        prop_assert!(!text.contains("pagh OS"));
        prop_assert!(!text.contains("Linux"));

        let map = fields(&out);
        prop_assert_eq!(map.len(), 4);
        prop_assert_eq!(map.get("NAME").map(String::as_str), Some(OS_NAME));
        prop_assert_eq!(map.get("ID").map(String::as_str), Some(OS_ID));
        prop_assert_eq!(map.get("VERSION").map(String::as_str), Some(version.as_str()));
        prop_assert_eq!(map.get("ARCH").map(String::as_str), Some("x86_64"));
    }

    /// `/etc/motd` первого boot'а называет ту же ОС и версию.
    #[test]
    fn motd_names_pagh_os(version in version_strategy()) {
        let out = format_motd(&version);
        let text = core::str::from_utf8(&out).unwrap();
        let mut lines = text.trim_end_matches('\n').lines();
        let first = format!("Pagh-OS {version}");
        prop_assert_eq!(lines.next(), Some(first.as_str()));
        prop_assert!(!text.contains("linux"));
        prop_assert!(!text.contains("pagh OS"));
    }
}

/// Перезапись версии разрешена только для файла, который объявляет именно
/// Pagh-OS. Пользовательская правка `NAME=OtherOS` (или файл без `ID=pagh`)
/// должна пережить boot, иначе `write_os_release` молча затрёт её.
#[test]
fn only_canonical_os_release_is_refreshable() {
    let ours = format_os_release("2.24.0");
    assert!(is_canonical_os_release(&ours));

    let unquoted = b"NAME=Pagh-OS\nID=pagh\nVERSION=1\n";
    assert!(is_canonical_os_release(unquoted));

    assert!(!is_canonical_os_release(b""));
    assert!(!is_canonical_os_release(b"NAME=\"OtherOS\"\nID=pagh\n"));
    assert!(!is_canonical_os_release(b"NAME=\"Pagh-OS\"\nID=other\n"));
    assert!(!is_canonical_os_release(b"NAME=\"Pagh-OS\"\n"));
    assert!(!is_canonical_os_release(b"\xff\xfe"));
}

/// `uname(2)` — Linux-совместимый UTS-идентификатор, а не имя ОС. Если кто-то
/// решит «исправить» sysname на `Pagh`, этот тест упадёт: менять его можно
/// только вместе с `docs/os-identity.md` и повторным аудитом Linux-userland,
/// который опрашивает `uname -s`.
#[test]
fn uname_stays_linux_compat_uts() {
    assert_eq!(UTS_SYSNAME, "Linux");
    assert_eq!(UTS_NODENAME, "pagh");
    assert_eq!(UTS_RELEASE, "6.1.0-pagh");
    assert_eq!(UTS_VERSION, "#1 pagh compat");
    assert_eq!(UTS_MACHINE, "x86_64");
    assert_eq!(UTS_DOMAINNAME, "(none)");
}