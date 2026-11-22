//! Идентичность ОС: как Pagh-OS называет себя в стандартных интерфейсах.
//!
//! Pagh — самостоятельная ОС с Linux-совместимым syscall ABI, а не
//! Linux-дистрибутив. Поэтому идентичность разделена на три поверхности, и
//! каждая отвечает на свой вопрос:
//!
//! * `/etc/os-release` ([`format_os_release`]) — **имя ОС** для userland:
//!   `NAME="Pagh-OS"`, `ID=pagh`, `PRETTY_NAME="Pagh-OS <версия>"`. Это
//!   стандартный файл, который читают `fastfetch`, glibc-скрипты и любые
//!   os-release-совместимые инструменты; `provision.rs` создаёт его на первом
//!   boot'е, досоздаёт на уже провижиненном диске и обновляет `VERSION` при
//!   смене версии ядра (файл с чужой идентичностью не трогается).
//! * `/proc/sys/kernel/ostype` — **имя ядра** (`Pagh`), см.
//!   [`crate::vfs::procfs_format::KERNEL_OSTYPE`]. Linux-овское `Linux` там не
//!   печатается намеренно.
//! * `uname(2)` ([`UTS_SYSNAME`] …) — **UTS-совместимость Linux ABI**. Sysname
//!   остаётся `Linux`, потому что его опрашивают Linux-бинарники, configure-
//!   скрипты и `config.guess`; менять его значило бы ломать совместимость,
//!   ради которой слой и существует. Это ABI-идентификатор, а не имя ОС:
//!   `fastfetch` берёт строку `OS` из `/etc/os-release`, а `Kernel` — из
//!   `uname` (`Linux 6.1.0-pagh`), и вторая строка описывает UTS-совместимость.
//!
//! Полная модель и обоснование — `docs/os-identity.md`.
//!
//! Модуль `core`+`alloc`-only, поэтому host-тесты `#[path]`-включают ровно тот
//! исходник, который компилирует ядро (свойство `os_identity`).

use alloc::vec::Vec;

/// Каноническое имя ОС в `/etc/os-release` (`NAME=`).
pub const OS_NAME: &str = "Pagh-OS";

/// Машинный идентификатор ОС в `/etc/os-release` (`ID=`).
pub const OS_ID: &str = "pagh";

/// Архитектура в `/etc/os-release` (`ARCH=`).
pub const OS_ARCH: &str = "x86_64";

/// `uname(2).sysname`: Linux-совместимое UTS-имя, не имя ОС.
pub const UTS_SYSNAME: &str = "Linux";

/// `uname(2).nodename`; совпадает с `/proc/sys/kernel/hostname`.
pub const UTS_NODENAME: &str = "pagh";

/// `uname(2).release`: строка ABI-совместимости, не версия ОС.
pub const UTS_RELEASE: &str = "6.1.0-pagh";

/// `uname(2).version`: фиксированный build-тег.
pub const UTS_VERSION: &str = "#1 pagh compat";

/// `uname(2).machine`.
pub const UTS_MACHINE: &str = "x86_64";

/// `uname(2).domainname`: в pagh нет NIS-домена.
pub const UTS_DOMAINNAME: &str = "(none)";

/// Версия `VERSION`/`PRETTY_NAME` в уже существующем `/etc/os-release`
/// принадлежит Pagh-OS, то есть файл можно безопасно обновить до текущей
/// версии ядра.
///
/// Проверяются только две identity-строки (`NAME`/`ID`), а не версия: именно
/// они объявляют ОС, и ровно они не должны тихо меняться. Файл с другой
/// идентичностью считается пользовательской правкой и не перезаписывается.
/// Принимаются и кавыченная, и голая форма — os-release допускает обе.
pub fn is_canonical_os_release(existing: &[u8]) -> bool {
    let Ok(text) = core::str::from_utf8(existing) else {
        return false;
    };
    let mut named = false;
    let mut identified = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        named |= line == "NAME=\"Pagh-OS\"" || line == "NAME=Pagh-OS";
        identified |= line == "ID=pagh";
    }
    named && identified
}

/// Отрендерить `/etc/os-release` для версии `version`.
///
/// Формат — пять стандартных полей плюс `ARCH`; значения с пробелами
/// (`PRETTY_NAME`) в кавычках, как ожидают os-release-парсеры (`fastfetch`
/// снимает кавычки сам). Версия — `CARGO_PKG_VERSION`, единственный источник
/// версии в проекте; здесь она параметр, чтобы форматтер оставался чистым.
pub fn format_os_release(version: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    out.extend_from_slice(b"NAME=\"");
    out.extend_from_slice(OS_NAME.as_bytes());
    out.extend_from_slice(b"\"\nID=");
    out.extend_from_slice(OS_ID.as_bytes());
    out.extend_from_slice(b"\nVERSION=\"");
    out.extend_from_slice(version.as_bytes());
    out.extend_from_slice(b"\"\nVERSION_ID=\"");
    out.extend_from_slice(version.as_bytes());
    out.extend_from_slice(b"\"\nPRETTY_NAME=\"");
    out.extend_from_slice(OS_NAME.as_bytes());
    out.push(b' ');
    out.extend_from_slice(version.as_bytes());
    out.extend_from_slice(b"\"\nARCH=");
    out.extend_from_slice(OS_ARCH.as_bytes());
    out.push(b'\n');
    out
}

/// Отрендерить `/etc/pagh-release` — зеркало идентичности для человека и
/// маркер завершённого провижининга.
///
/// Это не os-release (его роль выше); файл остаётся компактным и называет ту
/// же ОС и версию, чтобы `cat /mnt/etc/pagh-release` не показывал старое
/// `NAME=pagh OS`. Существование файла — guard первого boot'а, содержимое не
/// парсится ядром.
pub fn format_pagh_release(version: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(96);
    out.extend_from_slice(b"NAME=\"");
    out.extend_from_slice(OS_NAME.as_bytes());
    out.extend_from_slice(b"\"\nID=");
    out.extend_from_slice(OS_ID.as_bytes());
    out.extend_from_slice(b"\nVERSION=\"");
    out.extend_from_slice(version.as_bytes());
    out.extend_from_slice(b"\"\nARCH=");
    out.extend_from_slice(OS_ARCH.as_bytes());
    out.push(b'\n');
    out
}

/// Отрендерить `/etc/motd` первого boot'а.
pub fn format_motd(version: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    out.extend_from_slice(OS_NAME.as_bytes());
    out.push(b' ');
    out.extend_from_slice(version.as_bytes());
    out.extend_from_slice(
        b"\nType 'help' to list commands. Network apt and nano+ are available.\n",
    );
    out
}
