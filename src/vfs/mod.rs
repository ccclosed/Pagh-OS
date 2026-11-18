// vfs/mod.rs — виртуальная файловая система: трейт VfsNode, /dev/null, /dev/serial
// 64-битное ядро ОС x86_64 на Rust (#![no_std])

/// Символьные устройства варианта B: `/dev/fb0` и `/dev/input/event0`.
pub mod desktop_dev;
pub mod elf;
/// Чистый `core`-only классификатор ELF + выбор bias для static-PIE
/// (host-тестируемо, R11.6). Отделён от `elf`, чтобы не тянуть зависимости ядра и
/// пейджинга; `elf` реэкспортирует его публичные элементы.
pub mod elf_classify;
/// Чистый host-тестируемый резолвер путей с симлинками: обход по компонентам,
/// стек `..`, относительные/абсолютные цели ссылок, бюджет `SYMLOOP_MAX`.
/// Адаптер ниже подаёт ему хендлы `Arc<dyn VfsNode>`.
pub mod link_walk;
/// `memfd_create`: анонимный файл с общими страницами для `mmap(MAP_SHARED)`.
pub mod memfd;
/// Синтетический `/proc`: узлы, ленивый рендер, per-pid и per-thread view.
pub mod procfs;
/// Чистый (`core`+`alloc`) рендеринг текста + таблица путей/inode для
/// синтетического `/proc`. Выделен, чтобы `host-tests` включали его через
/// `#[path]` и проверяли свойства ровно тех форматов, что печатает ядро
/// (контракт `docs/procfs.md` §7).
pub mod procfs_format;
/// In-memory `/tmp` — единственная часть дерева с настоящими
/// `create_dir`/`create_file`.
pub mod ramfs;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

pub type VfsResult<T> = Result<T, VfsError>;

/// Бит sticky (`S_ISVTX`) в `VfsNode::mode` — тот же бит, что у Linux.
///
/// VFS его только *хранит* (например, `/tmp` — `0o1777`); решение о том, кому
/// разрешено удалить запись в sticky-каталоге, принимает syscall-слой через
/// `cred::Creds::may_delete_child`. Никакой проверки идентичности внутри VFS
/// нет — инвариант `AGENTS.md` §11.
pub const S_ISVTX: u32 = 0o1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsError {
    NotFound,
    NotSupported,
    InvalidArgument,
    IoError,
    /// Нет места на устройстве (`ENOSPC`): `/dev/full` отвергает любую запись,
    /// как Linux; файловые системы, которым не хватило блоков, пока сообщают
    /// `IoError` (историческое поведение ext2).
    NoSpace,
    AlreadyExists,
}

/// Заменяет содержимое обычного файла на `bytes`.
///
/// Семантика POSIX `open(..., O_TRUNC)`: за пределами `bytes.len()` не должно
/// остаться ничего от старого содержимого. Запись со смещения 0 — *не* замена:
/// `write f BBB` поверх 20-байтного файла оставлял хвост `AAAAAAAAAAAAAAAAA`.
/// Усечение живёт здесь, а не в каждом месте вызова, иначе следующий вызывающий
/// о нём забудет.
pub fn replace_contents(node: &Arc<dyn VfsNode>, bytes: &[u8]) -> VfsResult<usize> {
    node.truncate(0)?;
    if bytes.is_empty() {
        return Ok(0);
    }
    node.write(0, bytes)
}

/// Снимок ёмкости ФС для `statfs`/`fstatfs`: счётчики блоков — в единицах
/// `block_size`. Отдают ФС, которые ведут учёт (ext2); `fs_stat() == None`
/// (дефолт трейта) означает, что у ФС узла нет ёмкости для отчёта (ramfs,
/// синтетические узлы /dev).
#[derive(Debug, Clone, Copy)]
pub struct FsStat {
    pub block_size: u64,
    pub blocks_total: u64,
    pub blocks_free: u64,
    pub inodes_total: u64,
    pub inodes_free: u64,
}

pub trait VfsNode: Send + Sync {
    fn name(&self) -> &str;
    fn is_directory(&self) -> bool;
    /// Ёмкость ФС за этим узлом, если ФС её ведёт.
    fn fs_stat(&self) -> Option<FsStat> {
        None
    }
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(VfsError::NotSupported)
    }
    fn write(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::NotSupported)
    }
    /// Изменяет размер обычного файла. Реализации обязаны обнулять новые байты.
    fn truncate(&self, _size: u64) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }
    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        Err(VfsError::NotSupported)
    }
    fn lookup(&self, _name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        Err(VfsError::NotFound)
    }
    fn size(&self) -> u64 {
        0
    }
    /// Стабильный, уникальный в пределах ФС номер inode для идентичности в
    /// стиле `stat`, либо 0, когда у узла нет настоящей идентичности (синтетика
    /// /dev).
    ///
    /// glibc ld.so дедуплицирует загруженные объекты по паре `(st_dev, st_ino)`
    /// из `fstat`, поэтому разные файлы не должны делить пару.
    fn fs_ino(&self) -> u64 {
        0
    }

    /// Здесь возвращаются не биты прав (`0o7777`) с типом, а только девять бит
    /// прав плюс setuid/setgid/sticky — ровно то, что принимает `chmod(2)`.
    ///
    /// По умолчанию `0o755` для каталогов и `0o644` для файлов. Метаданные —
    /// это **данные**, а не политика: VFS никогда не проверяет credentials
    /// вызывающего. Syscall-слой читает эти значения и обеспечивает доступ;
    /// ядро-внутренние вызывающие (boot provisioning, установщик пакетов, shell)
    /// продолжают ходить через VFS без credential.
    fn mode(&self) -> u32 {
        if self.is_directory() {
            0o755
        } else {
            0o644
        }
    }

    /// `(uid, gid)` владельца узла. Синтетические узлы по умолчанию `(0, 0)`
    /// (root) — то же ставит ext2-писатель на новые inode.
    fn owner(&self) -> (u32, u32) {
        (0, 0)
    }

    /// Сохраняет новые биты прав. По умолчанию `NotSupported`: узлы без on-disk
    /// метаданных (`/dev`, `/proc`, ramfs) не могут хранить mode, и syscall-слой
    /// держит для этого случая прежний per-process override.
    fn set_mode(&self, _mode: u32) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    /// Сохраняет нового владельца. По умолчанию `NotSupported` (см.
    /// [`VfsNode::set_mode`]); писать `i_uid`/`i_gid` есть только у ext2-inode.
    fn set_owner(&self, _uid: u32, _gid: u32) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    /// Является ли узел симлинком?
    ///
    /// По умолчанию `false`; переопределяют только `/proc/self/exe` и ext2.
    /// `readlink(2)`, `lstat` и `d_type` ветвятся по этому.
    fn is_symlink(&self) -> bool {
        false
    }

    /// Цель ссылки, когда [`is_symlink`](VfsNode::is_symlink) истинно.
    ///
    /// Пути в этом ядре — UTF-8 `String` (`read_user_cstr`, `resolve_path`),
    /// поэтому и цель — `String`. `None` означает «не ссылка или ссылка без
    /// цели».
    fn read_link(&self) -> Option<String> {
        None
    }

    /// Число хардлинков на узел — Linux `st_nlink`.
    ///
    /// ext2 переопределяет через `i_links_count` (файл сообщает число имён на
    /// него, каталог — `2 + подкаталоги`); синтетические узлы (`/dev`, `/proc`)
    /// остаются с дефолтной единицей.
    fn nlink(&self) -> u64 {
        1
    }

    /// Создаёт симлинк `name` на сырые байты `target`. По умолчанию
    /// `NotSupported`: держать его могут только записываемые каталоги (ext2).
    fn create_symlink(&self, _name: &str, _target: &[u8]) -> VfsResult<Arc<dyn VfsNode>> {
        Err(VfsError::NotSupported)
    }

    /// Создаёт хардлинк `name` на существующий узел `target` — тот же inode под
    /// вторым именем. По умолчанию `NotSupported`.
    fn link(&self, _name: &str, _target: &Arc<dyn VfsNode>) -> VfsResult<Arc<dyn VfsNode>> {
        Err(VfsError::NotSupported)
    }

    // ── мутирующие операции каталога ──
    //
    // По умолчанию `NotSupported`, чтобы read-only узлам (например, `/dev`) не
    // требовалось изменений; ext2-каталог переопределяет их, направляя в
    // журналируемый путь записи.

    /// Создаёт дочерний каталог `name` и возвращает его узел.
    fn create_dir(&self, _name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        Err(VfsError::NotSupported)
    }
    /// Создаёт пустой дочерний файл `name` и возвращает его узел.
    fn create_file(&self, _name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        Err(VfsError::NotSupported)
    }
    /// Удаляет дочернюю запись `name` (файл или пустой каталог).
    fn remove(&self, _name: &str) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }
    /// Сбрасывает буферизованное состояние на стабильный носитель. По умолчанию
    /// no-op.
    fn sync(&self) {}

    // ── символьные устройства варианта B (desktop) ─────────────────────────
    //
    // Трейт описывает и обычные файлы, и устройства, поэтому device-specific
    // ответы — это дефолтные `None`/`false`: ext2/ramfs/proc о них не знают, а
    // syscall-слой (open/mmap/ioctl/read/poll) ветвится по ним. Сами узлы живут
    // в [`desktop_dev`]; здесь только контракт, чтобы `VfsNode` оставался
    // единственной точкой, через которую syscall-слой видит открытый объект.

    /// Геометрия Limine-фреймбуфера, если узел — `/dev/fb0`.
    fn fb_info(&self) -> Option<crate::drivers::framebuffer::FbInfo> {
        None
    }

    /// Является ли узел `/dev/fb0` (даже если фреймбуфер не поднялся —
    /// `open(2)` должен вернуть `ENODEV`, а не открыть обычный «файл»).
    fn is_fbdev(&self) -> bool {
        false
    }

    /// Является ли узел `/dev/input/event0` (read/poll идут через очередь
    /// [`crate::drivers::evdev`], а не через байтовое смещение файла).
    fn is_evdev(&self) -> bool {
        false
    }

    /// Кадр страницы `index` файла для `mmap(MAP_SHARED)`, либо `None`, если
    /// узел не поддерживает общие страницы (тогда `MAP_SHARED` — `EINVAL`).
    ///
    /// Возвращённый кадр пинится самим файлом; вызывающий обязан добавить
    /// ссылку отображения (`pmm::cow_ref`) и пометить лист `SHARED_BIT`.
    fn shared_page(&self, _index: u64) -> Option<u64> {
        None
    }

    /// Является ли узел `memfd` (единственный файловый тип с общими
    /// страницами). `MAP_SHARED` на обычном файле отвергается, а не
    /// превращается в приватную копию.
    fn is_memfd(&self) -> bool {
        false
    }

    /// Является ли узел `/dev/vtconsole0` (read-only shared VT surface).
    fn is_vtconsole(&self) -> bool {
        false
    }

    /// Геометрия shared VT surface; `None` — поверхность не поднялась
    /// (open вернёт `ENODEV`).
    fn vtconsole_info(&self) -> Option<crate::drivers::vtsurface::VtConsoleInfo> {
        None
    }

    /// Физический кадр страницы shared VT surface для read-only `mmap`.
    fn vtconsole_page(&self, _index: u64) -> Option<u64> {
        None
    }

    /// Является ли узел `/dev/vtinput` (write-only приём VT-байтов).
    fn is_vtinput(&self) -> bool {
        false
    }
}

// ─── /dev/null ────────────────────────────────────────────────────────────

struct NullDevice;

impl VfsNode for NullDevice {
    fn name(&self) -> &str {
        "null"
    }
    fn is_directory(&self) -> bool {
        false
    }

    fn read(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Ok(0)
    }

    fn write(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        Ok(buf.len())
    }

    /// `/dev/null` — 0666: это не ресурс, а сток; его доступность не должна
    /// зависеть от идентичности (иначе редирект `2>/dev/null` ломается у
    /// непривилегированного процесса).
    fn mode(&self) -> u32 {
        0o666
    }
}

// ─── /dev/zero и /dev/full ─────────────────────────────────────────────────

/// `/dev/zero`: чтение даёт нули, запись отбрасывается. Linux-совместимый узел
/// для `mmap`-паддинга/POSIX-программ; 0666, как `/dev/null`.
struct ZeroDevice;

impl VfsNode for ZeroDevice {
    fn name(&self) -> &str {
        "zero"
    }
    fn is_directory(&self) -> bool {
        false
    }

    fn read(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn write(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        Ok(buf.len())
    }

    fn mode(&self) -> u32 {
        0o666
    }
}

/// `/dev/full`: чтение даёт нули, запись всегда `ENOSPC`. Linux-тест-узел для
/// обработки ошибок записи; 0666, потому что отказ — часть контракта узла, а не
/// вопрос прав.
struct FullDevice;

impl VfsNode for FullDevice {
    fn name(&self) -> &str {
        "full"
    }
    fn is_directory(&self) -> bool {
        false
    }

    fn read(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn write(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::NoSpace)
    }

    fn mode(&self) -> u32 {
        0o666
    }
}

// ─── /dev/serial ──────────────────────────────────────────────────────────

struct SerialDevice;

impl VfsNode for SerialDevice {
    fn name(&self) -> &str {
        "serial"
    }
    fn is_directory(&self) -> bool {
        false
    }

    fn read(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        // SAFETY: Чтение портов LSR и DATA COM1.
        unsafe {
            let mut lsr = x86_64::instructions::port::Port::<u8>::new(0x3FD);
            if lsr.read() & 1 != 0 {
                let mut data = x86_64::instructions::port::Port::<u8>::new(0x3F8);
                buf[0] = data.read();
                Ok(1)
            } else {
                Ok(0)
            }
        }
    }

    fn write(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        // Каждый байт уходит в COM1 дословно. Байтовый API передаёт байт без
        // изменений (R8.1, R8.2), а запись всегда успешна, поэтому возвращается
        // полная длина входа (R8.3).
        crate::drivers::serial::write_bytes(buf);
        Ok(buf.len())
    }
}

// ─── каталог /dev ───────────────────────────────────────────────────────────

struct DevDirectory {
    children: Vec<Arc<dyn VfsNode>>,
}

impl VfsNode for DevDirectory {
    fn name(&self) -> &str {
        "dev"
    }
    fn is_directory(&self) -> bool {
        true
    }

    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        Ok(self.children.clone())
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        for child in &self.children {
            if child.name() == name {
                return Ok(Arc::clone(child));
            }
        }
        Err(VfsError::NotFound)
    }
}

// ─── обёртка точки монтирования ─────────────────────────────────────────────

/// Показывает произвольное поддерево `VfsNode` под выбранным именем монтирования.
///
/// `mount_at("/mnt", ext2_root)` оборачивает корень ФС (чей собственный `name()`
/// — `"/"`) в `MountNode` с именем `"mnt"` и вставляет его в детей корня VFS,
/// так что `lookup_path("/mnt")` его находит, а `lookup_path("/mnt/<file>")`
/// уходит прямо в смонтированное дерево. Каждый метод `VfsNode` форвардится во
/// внутренний узел.
struct MountNode {
    name: String,
    inner: Arc<dyn VfsNode>,
}

impl VfsNode for MountNode {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_directory(&self) -> bool {
        self.inner.is_directory()
    }
    fn fs_stat(&self) -> Option<FsStat> {
        self.inner.fs_stat()
    }
    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.inner.read(offset, buf)
    }
    fn write(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.inner.write(offset, buf)
    }
    fn truncate(&self, size: u64) -> VfsResult<()> {
        self.inner.truncate(size)
    }
    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        self.inner.readdir()
    }
    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        self.inner.lookup(name)
    }
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn mode(&self) -> u32 {
        self.inner.mode()
    }
    fn owner(&self) -> (u32, u32) {
        self.inner.owner()
    }
    fn set_mode(&self, mode: u32) -> VfsResult<()> {
        self.inner.set_mode(mode)
    }
    fn set_owner(&self, uid: u32, gid: u32) -> VfsResult<()> {
        self.inner.set_owner(uid, gid)
    }
    fn is_symlink(&self) -> bool {
        self.inner.is_symlink()
    }
    fn read_link(&self) -> Option<String> {
        self.inner.read_link()
    }
    fn nlink(&self) -> u64 {
        self.inner.nlink()
    }
    fn create_symlink(&self, name: &str, target: &[u8]) -> VfsResult<Arc<dyn VfsNode>> {
        self.inner.create_symlink(name, target)
    }
    fn link(&self, name: &str, target: &Arc<dyn VfsNode>) -> VfsResult<Arc<dyn VfsNode>> {
        self.inner.link(name, target)
    }
    fn create_dir(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        self.inner.create_dir(name)
    }
    fn create_file(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        self.inner.create_file(name)
    }
    fn remove(&self, name: &str) -> VfsResult<()> {
        self.inner.remove(name)
    }
    fn sync(&self) {
        self.inner.sync()
    }
    fn fb_info(&self) -> Option<crate::drivers::framebuffer::FbInfo> {
        self.inner.fb_info()
    }
    fn is_fbdev(&self) -> bool {
        self.inner.is_fbdev()
    }
    fn is_evdev(&self) -> bool {
        self.inner.is_evdev()
    }
    fn shared_page(&self, index: u64) -> Option<u64> {
        self.inner.shared_page(index)
    }
    fn is_memfd(&self) -> bool {
        self.inner.is_memfd()
    }
    fn is_vtconsole(&self) -> bool {
        self.inner.is_vtconsole()
    }
    fn vtconsole_info(&self) -> Option<crate::drivers::vtsurface::VtConsoleInfo> {
        self.inner.vtconsole_info()
    }
    fn vtconsole_page(&self, index: u64) -> Option<u64> {
        self.inner.vtconsole_page(index)
    }
    fn is_vtinput(&self) -> bool {
        self.inner.is_vtinput()
    }
}

// ─── корневой каталог ───────────────────────────────────────────────────────

struct RootDirectory {
    // Мутабельно, чтобы `mount_at` мог вставлять новые записи верхнего уровня
    // (например, `/mnt`) после boot. Под спинлоком; `lookup`/`readdir` берут его
    // ненадолго.
    children: Spinlock<Vec<Arc<dyn VfsNode>>>,
}

impl VfsNode for RootDirectory {
    fn name(&self) -> &str {
        "/"
    }
    fn is_directory(&self) -> bool {
        true
    }

    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        Ok(self.children.lock().clone())
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        for child in self.children.lock().iter() {
            if child.name() == name {
                return Ok(Arc::clone(child));
            }
        }
        Err(VfsError::NotFound)
    }
}

// ─── Initialization ───────────────────────────────────────────────────────

use crate::sync::spinlock::Spinlock;

static VFS_ROOT: Spinlock<Option<Arc<dyn VfsNode>>> = Spinlock::new(None);
/// Типизированный хендл того же корня, что и в `VFS_ROOT`: `mount_at` вставляет
/// детей, не разыменовывая `dyn VfsNode` вниз.
static ROOT_DIR: Spinlock<Option<Arc<RootDirectory>>> = Spinlock::new(None);

/// Одна запись таблицы монтирования VFS — как её показывает `/proc/mounts`.
#[derive(Clone, Debug)]
pub struct MountInfo {
    /// Абсолютная точка монтирования (`"/"`, `"/mnt"`, `"/proc"`, ...).
    pub point: alloc::string::String,
    /// Имя драйвера ФС для третьего поля (`rootfs`, `ext2`, `proc`, ...).
    pub fs: &'static str,
}

static MOUNTS: Spinlock<Option<Vec<MountInfo>>> = Spinlock::new(None);

fn record_mount(point: &str, fs: &'static str) {
    let mut mounts = MOUNTS.lock();
    let table = mounts.get_or_insert_with(Vec::new);
    table.retain(|entry| entry.point != point);
    table.push(MountInfo {
        point: alloc::string::String::from(point),
        fs,
    });
}

/// Снимок таблицы монтирования, отсортированный по точке, для `/proc/mounts`.
pub fn mount_snapshot() -> Vec<MountInfo> {
    let mut out = MOUNTS.lock().clone().unwrap_or_default();
    out.sort_by(|a, b| a.point.cmp(&b.point));
    out
}

pub fn init() {
    crate::debug!("Mounting /dev...");
    let null = Arc::new(NullDevice) as Arc<dyn VfsNode>;
    crate::debug!("/dev/null — ready");
    let zero = Arc::new(ZeroDevice) as Arc<dyn VfsNode>;
    crate::debug!("/dev/zero — ready");
    let full = Arc::new(FullDevice) as Arc<dyn VfsNode>;
    crate::debug!("/dev/full — ready");
    let serial = Arc::new(SerialDevice) as Arc<dyn VfsNode>;
    crate::debug!("/dev/serial — ready");
    // Устройства desktop-варианта B. Узлы создаются всегда: если фреймбуфера
    // нет (serial-only boot), open(/dev/fb0) сообщит ENODEV, а не ENOENT — так
    // композитор отличает «железа нет» от «путь не смонтирован».
    let fb0 = desktop_dev::fb0();
    crate::debug!("/dev/fb0 — ready");
    let input = desktop_dev::input_dir();
    crate::debug!("/dev/input/event0 — ready");
    let vtconsole = desktop_dev::vtconsole0();
    crate::debug!("/dev/vtconsole0 — ready");
    let vtinput = desktop_dev::vtinput();
    crate::debug!("/dev/vtinput — ready");
    let dev = Arc::new(DevDirectory {
        children: vec![
            Arc::clone(&null),
            Arc::clone(&zero),
            Arc::clone(&full),
            Arc::clone(&serial),
            Arc::clone(&fb0),
            Arc::clone(&input),
            Arc::clone(&vtconsole),
            Arc::clone(&vtinput),
        ],
    }) as Arc<dyn VfsNode>;
    // Записываемый in-memory /tmp: Linux-userspace считает, что он есть, —
    // nvim в vim_mktempdir делает mkdir("/tmp/nvim.XXXXXX").
    // 1777 + sticky: POSIX-контракт общего временного каталога. Без sticky
    // любой мог бы удалять чужие файлы, а без `w` для other не-root вообще не
    // смог бы создавать свои (прежние 0755).
    let tmp = Arc::new(ramfs::RamDir::new_with_mode("tmp", 0o1777)) as Arc<dyn VfsNode>;
    crate::debug!("/tmp (ramfs) - ready");
    let root = Arc::new(RootDirectory {
        children: Spinlock::new(vec![Arc::clone(&dev), tmp]),
    });
    *ROOT_DIR.lock() = Some(Arc::clone(&root));
    *VFS_ROOT.lock() = Some(root as Arc<dyn VfsNode>);
    record_mount("/", "rootfs");
    record_mount("/dev", "devtmpfs");
    record_mount("/tmp", "ramfs");
    // Синтетический /proc монтируется здесь, а не в `boot::init_fs`, чтобы
    // существовать и когда дерева ext2 нет или его отверг `format_policy`:
    // /proc/meminfo и /proc/cpuinfo — ровно нужная в этой ситуации диагностика.
    // Всё, что читает procfs (PMM, heap, тик-клок, планировщик), уже поднято:
    // `pmm::init`/`vmm::init`/`heap::init`/`scheduler::init` идут раньше в
    // `boot.rs`, а при монтировании ничего не рендерится.
    if let Err(e) = mount_at_named("/proc", procfs::root(), "proc") {
        crate::warn!("vfs: mounting /proc failed: {:?}", e);
    } else {
        crate::debug!("/proc (procfs) - ready");
    }
    crate::debug!("VFS Initialized");
}

/// Подсоединяет произвольное поддерево `VfsNode` в дерево VFS по абсолютному
/// пути.
///
/// Упрощение v1: поддерживаются только **однокомпонентные** точки монтирования
/// (`"/mnt"`, но не `"/a/b"`). Узел вставляется ребёнком корня под этим именем,
/// обёрнутый в [`MountNode`], чтобы представлять это имя (собственный корень ФС
/// сообщает `"/"`). После этого `lookup_path("/mnt")` резолвится в монтирование,
/// а `lookup_path("/mnt/<file>")` уходит в смонтированное дерево (его собственные
/// `lookup`/`readdir` обрабатывают более глубокие компоненты). Существующие
/// записи верхнего уровня вроде `/dev` не трогаются. Повторное монтирование того
/// же имени заменяет прежнюю запись.
pub fn mount_at(path: &str, node: Arc<dyn VfsNode>) -> VfsResult<()> {
    mount_at_named(path, node, "unknown")
}

/// Как [`mount_at`], но ещё записывает имя драйвера ФС для `/proc/mounts`
/// (`"ext2"`, `"proc"`, ...).
pub fn mount_at_named(path: &str, node: Arc<dyn VfsNode>, fs: &'static str) -> VfsResult<()> {
    let name = match path.strip_prefix('/') {
        Some(rest) => rest.trim_end_matches('/'),
        None => return Err(VfsError::InvalidArgument),
    };
    // Только одноуровневые точки монтирования (v1): пустые и вложенные пути
    // отвергаются.
    if name.is_empty() || name.contains('/') {
        return Err(VfsError::InvalidArgument);
    }

    let root = match ROOT_DIR.lock().clone() {
        Some(r) => r,
        None => return Err(VfsError::NotFound),
    };

    let mount = Arc::new(MountNode {
        name: String::from(name),
        inner: node,
    }) as Arc<dyn VfsNode>;

    let mut children = root.children.lock();
    children.retain(|c| c.name() != name);
    children.push(mount);
    drop(children);
    record_mount(path.trim_end_matches('/'), fs);
    Ok(())
}

// ─── резолвинг путей ────────────────────────────────────────────────────────

/// Резолвит абсолютный путь от корня VFS до узла.
///
/// - путь ОБЯЗАН начинаться с `/` (иначе `VfsError::InvalidArgument`);
/// - `/` резолвится в сам корневой узел;
/// - пустые компоненты пропускаются, поэтому `/dev/serial`, `/dev/serial/` и
///   `//dev//serial` резолвятся одинаково;
/// - любой ненайденный компонент даёт `VfsError::NotFound`.
pub fn lookup_path(path: &str) -> VfsResult<Arc<dyn VfsNode>> {
    if !path.starts_with('/') {
        return Err(VfsError::InvalidArgument);
    }

    // Снимаем клон Arc корня и отпускаем спинлок VFS_ROOT до обхода дерева.
    // Узлы независимо подсчитаны и внутренне синхронизированы/иммутабельны,
    // поэтому держать спинлок VFS_ROOT во время `lookup` нельзя — иначе
    // реентерабельность/дедлок.
    let mut node = match VFS_ROOT.lock().clone() {
        Some(root) => root,
        None => return Err(VfsError::NotFound),
    };

    for component in path.split('/') {
        if component.is_empty() {
            continue;
        }
        node = node.lookup(component)?;
    }

    Ok(node)
}

/// Резолвит абсолютный путь, переходя по симлинкам.
///
/// `lookup_path` выше остаётся одношаговым примитивом без перехода (возвращает
/// сам узел ссылки) — это нужно `readlink`, `lstat`, `unlink` и `rmdir`. Всё, что
/// говорит о «файле, на который указывает путь», — `stat`/`open`/`access`/
/// `chdir`/`execve` — идёт через этот обход:
///
///   * промежуточные ссылки следуются всегда, конечная — только при
///     `follow_final`;
///   * пересечённая ссылка вставляет цель перед оставшимися компонентами
///     (относительная — от каталога ссылки, абсолютная — от корня), с бюджетом
///     `SYMLOOP_MAX` → [`link_walk::WalkError::TooManyLinks`];
///   * промежуточный не-каталог — [`link_walk::WalkError::NotDir`], разросшийся
///     путь ограничен ([`link_walk::WalkError::TooLong`]).
///
/// Сам алгоритм живёт в чистом модуле [`link_walk`]; эта функция — только
/// адаптер над `Arc<dyn VfsNode>`, поэтому обход проверяется свойствами на хосте
/// (R11.6).
pub fn lookup_path_walk(
    path: &str,
    follow_final: bool,
) -> Result<Arc<dyn VfsNode>, link_walk::WalkError> {
    lookup_path_walk_checked(path, follow_final, &|_| true)
}

/// Как [`lookup_path_walk`], но спрашивает `may_search` перед каждым lookup
/// каталога: syscall-слой передаёт эффективные credentials вызвавшего процесса,
/// и это обеспечивает POSIX-право поиска (`x`) на **каждом** пройденном
/// каталоге, включая каталоги, достигнутые переходом по ссылке.
///
/// Сам VFS остаётся без credential: `may_search` — предикат вызывающего над
/// метаданными узла (`mode`/`owner`), как и остальные проверки прав на границе
/// syscall. Ядро-внутренние вызывающие (boot provisioning, установщик пакетов,
/// `lxrun`) продолжают использовать непроверяемый [`lookup_path_walk`].
pub fn lookup_path_walk_checked(
    path: &str,
    follow_final: bool,
    may_search: &dyn Fn(&Arc<dyn VfsNode>) -> bool,
) -> Result<Arc<dyn VfsNode>, link_walk::WalkError> {
    if !path.starts_with('/') {
        return Err(link_walk::WalkError::NotFound);
    }
    let root = VFS_ROOT
        .lock()
        .clone()
        .ok_or(link_walk::WalkError::NotFound)?;
    // Хендлы узлов для этого резолва: чистый walker работает на `usize`-id и
    // никогда не видит `Arc`, поэтому адаптер интернирует каждый выдаваемый узел.
    let mut nodes = WalkNodes {
        nodes: alloc::vec![Arc::clone(&root)],
        root: 0,
        may_search,
    };
    let id = link_walk::resolve(&mut nodes, path, follow_final)?;
    nodes
        .nodes
        .get(id)
        .cloned()
        .ok_or(link_walk::WalkError::NotFound)
}

/// Интернер `Arc<dyn VfsNode>` за [`lookup_path_walk`].
struct WalkNodes<'a> {
    nodes: alloc::vec::Vec<Arc<dyn VfsNode>>,
    root: usize,
    /// Решение о credential для каталога, который обход собирается искать.
    may_search: &'a dyn Fn(&Arc<dyn VfsNode>) -> bool,
}

impl WalkNodes<'_> {
    fn intern(&mut self, node: Arc<dyn VfsNode>) -> usize {
        for (i, existing) in self.nodes.iter().enumerate() {
            if Arc::ptr_eq(existing, &node) {
                return i;
            }
        }
        self.nodes.push(node);
        self.nodes.len() - 1
    }
}

impl link_walk::LinkTree for WalkNodes<'_> {
    fn root(&self) -> usize {
        self.root
    }

    fn may_search(&self, id: usize) -> bool {
        match self.nodes.get(id) {
            Some(node) => (self.may_search)(node),
            // Id, который адаптер не интернировал, искать нельзя; fail closed.
            None => false,
        }
    }

    fn lookup(&mut self, id: usize, name: &str) -> Option<usize> {
        let child = self.nodes.get(id)?.lookup(name).ok()?;
        Some(self.intern(child))
    }

    fn is_dir(&self, id: usize) -> bool {
        self.nodes
            .get(id)
            .map(|n| n.is_directory())
            .unwrap_or(false)
    }

    fn link_target(&self, id: usize) -> Option<link_walk::LinkTarget> {
        let node = self.nodes.get(id)?;
        if !node.is_symlink() {
            return None;
        }
        match node.read_link() {
            // Цель прочитана: интерпретируем её в гостевом пространстве
            // (`/x` → `/mnt/x`).
            Some(target) if !target.is_empty() => Some(link_walk::map_guest_target(&target)),
            // Ссылка с пустой или нечитаемой целью обязана провалить обход
            // (`ENOENT`), а не молча резолвиться в сам узел ссылки.
            _ => Some(link_walk::LinkTarget {
                path: String::new(),
                absolute: false,
            }),
        }
    }
}
