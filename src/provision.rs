//! Идемпотентное первичное заполнение файловой системы при первой загрузке pagh.

use crate::arch::x86_64::linux::cred::gid;
use crate::os_identity;
use crate::vfs::{VfsError, VfsNode};
use alloc::sync::Arc;

// Версия ОС берётся из версии крейта (Cargo.toml), чтобы они не расходились.
const README: &[u8] = b"Welcome to pagh!\n\nTry:\n  help\n  nano --settings\n  nano /mnt/home/user/notes.txt\n  apt update\n";

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

/// Записать `/etc/os-release`, обновив версию у файла, который уже объявляет
/// Pagh-OS.
///
/// Обычный [`write_once`] здесь недостаточен: версия ОС — это
/// `CARGO_PKG_VERSION` **работающего** ядра, и после обновления ядра на
/// существующем диске файл иначе навсегда застыл бы на версии первого boot'а
/// (именно это случилось с историческим `/etc/pagh-release`: на тестовом
/// образе он до сих пор `2.3.0`). Поэтому канонический os-release
/// перезаписывается, когда меняется версия; файл с другой идентичностью —
/// пользовательская правка, её не трогаем.
fn write_os_release(path: &str, data: &[u8]) -> Result<(), VfsError> {
    match crate::vfs::lookup_path(path) {
        Ok(node) if node.is_directory() => Err(VfsError::InvalidArgument),
        Ok(node) => {
            let mut existing = [0u8; 512];
            let read = node.read(0, &mut existing).unwrap_or(0);
            if !os_identity::is_canonical_os_release(&existing[..read]) {
                return Ok(());
            }
            if read == data.len() && &existing[..read] == data {
                return Ok(());
            }
            node.truncate(0)?;
            if node.write(0, data)? != data.len() {
                return Err(VfsError::IoError);
            }
            node.sync();
            Ok(())
        }
        Err(VfsError::NotFound) => write_once(path, data),
        Err(error) => Err(error),
    }
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
    // Идентичность ОС — стандартный `/etc/os-release`, а не только
    // исторический `pagh-release`: его читают fastfetch и os-release-совместимые
    // инструменты. `write_os_release` создаёт файл на диске, провижиненном
    // старым ядром, обновляет версию у канонического файла и не трогает чужую
    // идентичность (пользовательскую правку).
    write_os_release(
        "/mnt/etc/os-release",
        &os_identity::format_os_release(env!("CARGO_PKG_VERSION")),
    )?;
    ensure_dir_owned("/mnt/home/oleg", 1000, gid::OLEG, 0o755)?;
    Ok(())
}

/// Записать человекочитаемое зеркало выбора монтирования в
/// `/mnt/etc/pagh-mount-choice`.
///
/// РЕШЕНИЕ принимает не этот файл, а сырой маркер сектора 1 тома
/// (`crate::fs::mount_choice`): читать `/mnt` до монтирования нельзя, а
/// монтирование — ровно то, что выбор и решает. Файл нужен оператору: спросить
/// «почему меню больше не появляется» и получить ответ из `cat`.
///
/// `write_once`, как и весь посев: первый ответ остаётся, повтор не переписывает
/// историю. Ошибка не фатальна — том уже смонтирован, и решение записано в
/// маркере.
pub fn record_mount_choice(device: &str, action: &str) {
    let body = alloc::format!("device={device}\naction={action}\n");
    if let Err(error) = write_once("/mnt/etc/pagh-mount-choice", body.as_bytes()) {
        crate::warn!("pagh mount choice record not written: {:?}", error);
    }
}

/// Установить release-метаданные, скелет домашнего каталога и примеры при первой
/// загрузке. Существующие файлы не перезаписываются, кроме канонического
/// `/etc/os-release`, у которого обновляется версия (см. [`write_os_release`]).
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
        let version = env!("CARGO_PKG_VERSION");
        ensure_dir("/mnt/etc")?;
        ensure_dir("/mnt/home/user")?;
        ensure_dir("/mnt/usr/share/pagh")?;
        // Резолвер glibc читает их до того, как заработает любой getaddrinfo;
        // 10.0.2.3 — встроенный DNS-форвардер QEMU slirp.
        write_once(
            "/mnt/etc/resolv.conf",
            b"nameserver 10.0.2.3\noptions timeout:1 attempts:2\n",
        )?;
        write_once("/mnt/etc/hosts", b"127.0.0.1 localhost\n::1 localhost\n");
        write_once("/mnt/etc/motd", &os_identity::format_motd(version))?;
        write_once("/mnt/home/user/README.txt", README)?;
        write_once(
            "/mnt/usr/share/pagh/LICENSE-NOTICE.txt",
            b"pagh kernel and bundled development environment: MIT.\n",
        )?;
        seed_identity()?;
        // Маркер «провижининг завершён» пишется ПОСЛЕДНИМ: если загрузка
        // оборвалась посередине, следующий boot увидит его отсутствие и
        // безопасно доделает остаток (все записи идемпотентны). Раньше маркер
        // шёл первым, и обрыв после него навсегда оставлял диск полу-засеянным.
        write_once(
            "/mnt/etc/pagh-release",
            &os_identity::format_pagh_release(version),
        )?;
        Ok::<(), VfsError>(())
    })();
    match result {
        Ok(()) => crate::info!("pagh first-boot files installed"),
        Err(error) => crate::warn!("pagh first-boot provisioning failed: {:?}", error),
    }
}
