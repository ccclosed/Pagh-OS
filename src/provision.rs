//! Идемпотентное первичное заполнение файловой системы при первой загрузке pagh.

use crate::arch::x86_64::linux::cred::gid;
use crate::vfs::{VfsError, VfsNode};
use alloc::sync::Arc;

// Версия ОС берётся из версии крейта (Cargo.toml), чтобы они не расходились.
const RELEASE: &[u8] = concat!(
    "NAME=pagh OS\nID=pagh\nVERSION=",
    env!("CARGO_PKG_VERSION"),
    "\nARCH=x86_64\n"
)
.as_bytes();
const MOTD: &[u8] = concat!(
    "pagh OS ",
    env!("CARGO_PKG_VERSION"),
    "\nType 'help' to list commands. Network apt and nano+ are available.\n"
)
.as_bytes();
const README: &[u8] = b"Welcome to pagh!\n\nTry:\n  help\n  nano --settings\n  nano /mnt/home/user/notes.txt\n  cargo run /mnt/examples/hello\n  apt update\n";
const CARGO: &[u8] = b"[package]\nname = \"pagh-hello\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
const MAIN_RS: &[u8] = b"fn main() {\n    let values = [8, 16, 32];\n    println!(\"Hello from pagh mini-Rust! sum = {}\", values.iter().sum());\n}\n";

/// Минимальная база идентичностей фазы 1 «группы отдельно от root»
/// (`docs/security/groups.md`): root и единственный непривилегированный
/// пользователь `oleg` (uid/gid 1000). Паролей нет — поле `x` оставлено
/// заполнителем, а `su`/`login` в фазе 1 не аутентифицируют (см. docs).
///
/// Формат — ровно семь двоеточий-полей `/etc/passwd`; `id`/`groups` в shell
/// разбирают именно его (и `/etc/group`), поэтому поля не переставлять.
const PASSWD: &[u8] = b"\
root:x:0:0:root:/root:/bin/sh
oleg:x:1000:1000:Oleg:/home/oleg:/bin/sh
";

/// `/etc/group` фазы 1. Номера — [`gid`] в `cred.rs`: tty=5, disk=6, wheel=10,
/// video=44, net=90, input=105. `oleg` включён в `tty`, `video` и `input`,
/// потому что desktop-композитор работает от его имени; `wheel`/`disk`/`net`
/// намеренно пусты (это privileged-группы фазы 2).
const GROUP: &[u8] = b"\
root:x:0:
tty:x:5:oleg
disk:x:6:
wheel:x:10:
video:x:44:oleg
net:x:90:
input:x:105:oleg
oleg:x:1000:
";

/// `/etc/shadow` (не-root A4): парольные хэши отдельно от читаемого всеми
/// `/etc/passwd`.
///
/// Обе учётки **закрыты** на свежем диске: `root:*` (root-консоль — kernel-код,
/// пароль ей не нужен, а `su root` из не-root сессии без пароля был бы
/// эскалацией) и `oleg:!` (вход открывается только явным
/// `passwd oleg <новый пароль>`, а не угадываемым паролем по умолчанию).
/// Формат — семь полей Linux; `passwd`/`login` читают его через
/// `shell::shadow` (`$6$` SHA-512-crypt), остальные поля сохраняются.
const SHADOW: &[u8] = b"\
root:*:19000:0:99999:7:::
oleg:!:19000:0:99999:7:::
";

fn ensure_dir(path: &str) -> Result<Arc<dyn VfsNode>, VfsError> {
    let mut node = crate::vfs::lookup_path("/")?;
    for part in path.split('/').filter(|part| !part.is_empty()) {
        node = match node.lookup(part) {
            Ok(child) if child.is_directory() => child,
            Ok(_) => return Err(VfsError::InvalidArgument),
            Err(VfsError::NotFound) => node.create_dir(part)?,
            Err(error) => return Err(error),
        };
    }
    Ok(node)
}

fn write_once(path: &str, data: &[u8]) -> Result<(), VfsError> {
    if crate::vfs::lookup_path(path).is_ok() {
        return Ok(());
    }
    let split = path.rfind('/').ok_or(VfsError::InvalidArgument)?;
    let parent = if split == 0 { "/" } else { &path[..split] };
    let dir = ensure_dir(parent)?;
    let file = dir.create_file(&path[split + 1..])?;
    if file.write(0, data)? != data.len() {
        return Err(VfsError::IoError);
    }
    file.sync();
    Ok(())
}

/// [`write_once`] с владельцем и режимом, выставленными **только** новому файлу.
///
/// Существующий файл не трогается вообще: пользователь мог сменить режим
/// `/etc/shadow` (или содержимое), и boot-путь не должен молча возвращать свои
/// права — та же политика, что у [`ensure_dir_owned`].
fn write_once_owned(
    path: &str,
    data: &[u8],
    uid: u32,
    gid: u32,
    mode: u32,
) -> Result<(), VfsError> {
    if crate::vfs::lookup_path(path).is_ok() {
        return Ok(());
    }
    let split = path.rfind('/').ok_or(VfsError::InvalidArgument)?;
    let parent = if split == 0 { "/" } else { &path[..split] };
    let dir = ensure_dir(parent)?;
    let file = dir.create_file(&path[split + 1..])?;
    if file.write(0, data)? != data.len() {
        return Err(VfsError::IoError);
    }
    file.set_owner(uid, gid)?;
    file.set_mode(mode)?;
    file.sync();
    Ok(())
}

/// Создать каталог, если его нет, и сразу выставить владельца/режим **только**
/// новому каталогу. Существующий каталог не трогается: пользователь мог
/// переименовать/ужать его, и boot-путь не должен молча возвращать свои права.
fn ensure_dir_owned(path: &str, uid: u32, gid: u32, mode: u32) -> Result<(), VfsError> {
    if crate::vfs::lookup_path(path).is_ok() {
        return Ok(());
    }
    let node = ensure_dir(path)?;
    node.set_owner(uid, gid)?;
    node.set_mode(mode)?;
    Ok(())
}

/// Идемпотентно установить идентичность-файлы фазы 1.
///
/// Отдельно от [`seed`], потому что `seed` выходит рано на уже
/// провижиненном диске: обновление ядра на существующем образе должно получить
/// `/etc/passwd`/`/etc/group` без переформатирования и без перезаписи
/// пользовательских правок (`write_once`).
fn seed_identity() -> Result<(), VfsError> {
    ensure_dir("/mnt/etc")?;
    write_once("/mnt/etc/passwd", PASSWD)?;
    write_once("/mnt/etc/group", GROUP)?;
    // Хэши паролей — только root: 0600 root:root, как в Linux.
    write_once_owned("/mnt/etc/shadow", SHADOW, 0, 0, 0o600)?;
    ensure_dir_owned("/mnt/home/oleg", 1000, gid::OLEG, 0o755)?;
    Ok(())
}

/// Установить release-метаданные, скелет домашнего каталога и примеры при первой
/// загрузке. Существующие файлы никогда не перезаписываются.
pub fn seed() {
    if crate::vfs::lookup_path("/mnt/etc/pagh-release").is_ok() {
        // Уже провижиненный диск: досоздать только то, что появилось в новых
        // версиях ядра (идентичности фазы 1), не трогая остальное.
        match seed_identity() {
            Ok(()) => {}
            Err(error) => crate::warn!("pagh identity files not installed: {:?}", error),
        }
        return;
    }
    let result = (|| {
        ensure_dir("/mnt/etc")?;
        ensure_dir("/mnt/home/user")?;
        ensure_dir("/mnt/usr/share/pagh")?;
        ensure_dir("/mnt/examples/hello/src")?;
        // Резолвер glibc читает их до того, как заработает любой getaddrinfo;
        // 10.0.2.3 — встроенный DNS-форвардер QEMU slirp.
        write_once(
            "/mnt/etc/resolv.conf",
            b"nameserver 10.0.2.3\noptions timeout:1 attempts:2\n",
        )?;
        write_once("/mnt/etc/hosts", b"127.0.0.1 localhost\n::1 localhost\n");
        write_once("/mnt/etc/motd", MOTD)?;
        write_once("/mnt/home/user/README.txt", README)?;
        write_once(
            "/mnt/usr/share/pagh/LICENSE-NOTICE.txt",
            b"pagh kernel and bundled development environment: MIT.\n",
        )?;
        write_once("/mnt/examples/hello/Cargo.toml", CARGO)?;
        write_once("/mnt/examples/hello/src/main.rs", MAIN_RS)?;
        seed_identity()?;
        // Маркер «провижининг завершён» пишется ПОСЛЕДНИМ: если загрузка
        // оборвалась посередине, следующий boot увидит его отсутствие и
        // безопасно доделает остаток (все записи идемпотентны). Раньше маркер
        // шёл первым, и обрыв после него навсегда оставлял диск полу-засеянным.
        write_once("/mnt/etc/pagh-release", RELEASE)?;
        Ok::<(), VfsError>(())
    })();
    match result {
        Ok(()) => crate::info!("pagh first-boot files installed"),
        Err(error) => crate::warn!("pagh first-boot provisioning failed: {:?}", error),
    }
}
