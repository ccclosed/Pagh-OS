//! Эффективные обработчики файловых I/O syscall'ов Linux.
//!
//! Это **ядерная** половина компонента `io`: она связывает чистые планировщики
//! из [`super::io`] (`plan_read`/`plan_lseek`) и [`super::stat`] (`encode_stat`)
//! с [`FdTable`] текущего `Compat_Process`, консолью ядра и деревом VFS/ext2.
//!
//! Модуль вынесен в отдельный файл намеренно: `io.rs` включается в `host-tests`
//! через `#[path]`, чтобы его чистые планировщики проверялись на хосте (R11.6).
//! Эти обработчики используют ядерные `memory`/`vfs`/`task`, которых на хосте
//! нет, поэтому `io.rs` остаётся чисто host-тестируемым.
//!
//! ## Безопасность пользовательских указателей
//!
//! Каждый обработчик, принимающий пользовательский указатель, прогоняет его
//! через единую точку [`super::check_user_ptr`] (проверка диапазона + обход
//! страниц) ДО разыменования. Во время syscall активный CR3 — PML4 вызывающего
//! процесса, поэтому проверенный указатель нижней половины доступен из ring 0.
//!
//! ## Дисциплина блокировок
//!
//! [`crate::task::compat::with_current_compat`] держит спинлок `COMPAT_STATES`
//! (с выключенными прерываниями) на всё время выполнения замыкания. Чтение VFS
//! с диска может ждать прерывания устройства, поэтому обработчики никогда не
//! держат этот лок во время вызова VFS: дескриптор резолвится (клонируется
//! `Arc<dyn VfsNode>`, снимается смещение) под локом, лок отпускается,
//! выполняется I/O, затем лок ненадолго берётся снова для фиксации смещения.
#![allow(dead_code)]

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::task::compat;
use crate::task::fd::{OpenObject, PipeEndpoint, PipeReadResult, PipeWriteResult};
use crate::vfs::memfd::MemFd;
use crate::vfs::{self, VfsNode};

use super::cred::{Access, Creds};
use super::dirent::{d_type_for, dirent_reclen, encode_dirent64};
use super::errno::Errno;
use super::io::{plan_lseek, plan_read};
use super::{check_user_ptr, check_user_ptr_writable};
// Два ioctl foreground-группы (`tcgetpgrp`/`tcsetpgrp`), как и бит `TOSTOP`,
// берутся из чистой модели (`super::tty`), чтобы ядро и host-свойства не
// разошлись в номерах ABI.
use super::stat::{encode_stat, LinuxStat, S_IFDIR, S_IFLNK, S_IFREG};
use super::tty::{TIOCGPGRP, TIOCSPGRP, TOSTOP};
use super::unix_sock;

/// Биты типа `st_mode` для символьного устройства (консоль/stdin), чтобы
/// `fstat` стандартного потока сообщал правдоподобный (не regular) тип.
const S_IFCHR: u32 = 0o020000;

/// Биты типа `st_mode` для сокета (концы socketpair).
const S_IFSOCK: u32 = 0o140000;

/// Максимум байт, принимаемый `read`/`write`, как `int` в Linux (R2.2,
/// 0..=2_147_483_647). Большие запросы отклоняются с `EINVAL`, а не пытаются
/// выделить многогигабайтный буфер.
const COUNT_MAX: u64 = 0x7FFF_FFFF;

/// Сентинел «текущий рабочий каталог» для dir fd в `openat`.
const AT_FDCWD: u64 = (-100i64) as u64;
/// Флаг `faccessat`: проверять по effective id, а не по real id.
const AT_EACCESS: u64 = 0x200;

/// Флаг `newfstatat`: не разыменовывать конечную символическую ссылку (`lstat`).
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
/// Флаг `unlinkat`: удалить каталог, а не файл.
const AT_REMOVEDIR: u64 = 0x200;

/// Права по умолчанию для regular-файла на ext2. Всё работает под root
/// (однопользовательская система), программы из apt должны быть исполняемыми:
/// с 0644 git отказывался от своих remote-хелперов («unable to find remote
/// helper»), потому что в режиме statx не было X-битов.
const DEFAULT_FILE_PERMS: u32 = 0o755;

/// Дескриптор, сведённый к конкретной цели и отвязанный от лока
/// `COMPAT_STATES`, чтобы последующий (возможно, блокирующий) I/O шёл без лока.
enum Resolved {
    /// fd 1/2 — консоль ядра.
    Console,
    /// fd 0 — стандартный ввод (не для записи).
    Stdin,
    /// Regular-файл на ext2: клонированный узел и смещение на момент резолва.
    File {
        node: Arc<dyn VfsNode>,
        offset: u64,
    },
    PipeRead(Arc<PipeEndpoint>),
    PipeWrite(Arc<PipeEndpoint>),
    /// Один конец AF_UNIX socketpair (rx — входящий, tx — исходящий).
    Socket {
        rx: Arc<PipeEndpoint>,
        tx: Arc<PipeEndpoint>,
    },
    /// Экземпляр timerfd.
    Timerfd(Arc<crate::sync::spinlock::Spinlock<crate::task::fd::TimerfdState>>),
    /// Экземпляр signalfd и снимок его маски.
    Signalfd(u64),
    /// Счётчик eventfd.
    Eventfd {
        val: Arc<crate::sync::spinlock::Spinlock<u64>>,
        semaphore: bool,
    },
    /// Экземпляр epoll (напрямую через read/write не читается и не пишется).
    Epoll,
    /// AF_INET TCP поверх собственного стека (данные — через примитивы net::).
    InetTcp(Arc<crate::arch::x86_64::linux::inet_sock::InetTcp>),
    /// AF_INET UDP поверх собственного стека.
    InetUdp(Arc<crate::arch::x86_64::linux::inet_sock::InetUdp>),
    /// Открытый каталог (не байтовый поток): read/write/pread/pwrite отклоняются.
    Dir,
}

/// Резолвит `fd` текущего процесса, клонируя узел, чтобы вызывающий мог
/// отпустить лок `COMPAT_STATES` до I/O VFS. `None` — дескриптор отсутствует
/// или закрыт, либо у процесса нет compat-состояния (→ `EBADF`).
/// Публичные копирующие хелперы для соседних модулей (диспетчеризация inet_sock).
/// Заполнить буфер statx, описывающий AF_INET-сокет.
fn write_statx_sock(buf: u64) -> Result<u64, Errno> {
    const S_IFSOCK: u32 = 0o_140000;
    write_statx(buf, 0, S_IFSOCK | 0o755, 0);
    Ok(0)
}

pub(crate) fn copy_in_pub(buf: u64, count: u64) -> alloc::vec::Vec<u8> {
    copy_in(buf, count)
}
pub(crate) fn copy_out_pub(buf: u64, data: &[u8]) -> Result<(), Errno> {
    copy_out(buf, data)
}

/// Повторно проверить `ptr` на `src.len()` байт и скопировать `src` наружу.
///
/// ПОЧЕМУ: если проверить буфер один раз на входе, а затем блокироваться в
/// цикле `yield_current()`, остаётся окно TOCTOU — соседний поток `CLONE_VM`
/// может munmap/mprotect страницу, пока обработчик вытеснен. Устаревшая запись
/// из ring 0 даёт #PF режима ядра и останавливает машину; повторная проверка
/// прямо перед записью превращает это в `-EFAULT`.
pub(crate) fn copy_out_checked(ptr: u64, src: &[u8]) -> Result<(), Errno> {
    check_user_ptr_writable(ptr, src.len() as u64)?;
    copy_out(ptr, src)
}

/// Свести ошибку записи в VFS к errno.
///
/// Узел без `write` — файл только для чтения; таковы файлы `/proc`, и Linux
/// отвечает на них `EACCES`. Любая другая ошибка сохраняет прежний `EINVAL`.
fn write_errno(e: crate::vfs::VfsError) -> Errno {
    match e {
        crate::vfs::VfsError::NotSupported => Errno::EACCES,
        // `/dev/full`: Linux отвечает ENOSPC, и POSIX-программы полагаются на
        // эту ошибку в тестах записи.
        crate::vfs::VfsError::NoSpace => Errno::ENOSPC,
        _ => Errno::EINVAL,
    }
}

/// Человекочитаемый вид дескриптора для watchdog/телеметрии.
pub(crate) fn fd_kind(fd: u64) -> &'static str {
    match resolve_fd(fd as u32) {
        None => "closed",
        Some(Resolved::Stdin) => "stdin",
        Some(Resolved::Console) => "console",
        Some(Resolved::Dir) => "dir",
        Some(Resolved::InetTcp(_)) => "inet-tcp",
        Some(Resolved::InetUdp(_)) => "inet-udp",
        Some(Resolved::PipeRead(_)) => "pipe-r",
        Some(Resolved::PipeWrite(_)) => "pipe-w",
        Some(Resolved::Socket { .. }) => "sock",
        Some(Resolved::Eventfd { .. }) => "eventfd",
        Some(Resolved::Timerfd(_)) => "timerfd",
        Some(Resolved::Signalfd(_)) => "signalfd",
        Some(Resolved::Epoll) => "epoll",
        Some(Resolved::File { .. }) => "file",
    }
}

fn resolve_fd(fd: u32) -> Option<Resolved> {
    compat::with_current_compat(|cs| {
        cs.fds.get(fd).map(|obj| match obj {
            OpenObject::Console => Resolved::Console,
            OpenObject::Stdin => Resolved::Stdin,
            OpenObject::PipeRead(e) => Resolved::PipeRead(Arc::clone(e)),
            OpenObject::PipeWrite(e) => Resolved::PipeWrite(Arc::clone(e)),
            OpenObject::Socket { rx, tx } => Resolved::Socket {
                rx: Arc::clone(rx),
                tx: Arc::clone(tx),
            },
            OpenObject::File { node, offset, .. } => Resolved::File {
                node: Arc::clone(node),
                offset: *offset,
            },
            OpenObject::Dir { .. } => Resolved::Dir,
            OpenObject::Timerfd(state) => Resolved::Timerfd(Arc::clone(state)),
            OpenObject::Signalfd { mask } => Resolved::Signalfd(*mask.lock()),
            OpenObject::Eventfd { val, semaphore } => Resolved::Eventfd {
                val: Arc::clone(val),
                semaphore: *semaphore,
            },
            // Не байтовые потоки — те же отказы read/write/seek, что у epoll-fd.
            OpenObject::InetTcp(t) => Resolved::InetTcp(Arc::clone(&t)),
            OpenObject::InetUdp(u) => Resolved::InetUdp(Arc::clone(&u)),
            OpenObject::UnixListener(_) => Resolved::Epoll,
            OpenObject::UnixSocketUnbound { .. } => Resolved::Epoll,
            OpenObject::Epoll { .. } => Resolved::Epoll,
        })
    })
    .flatten()
}

/// Клонировать узел VFS, стоящий за открытым regular-дескриптором, для
/// вызывающих вне этого модуля, которым нужны байты файла без лока compat
/// (сейчас — file-backed отображения в `mem_sys::sys_mmap`). `None` — для
/// отсутствующих дескрипторов и всего, что не является ext2-`File`.
pub fn file_node_for_fd(fd: u32) -> Option<Arc<dyn VfsNode>> {
    match resolve_fd(fd) {
        Some(Resolved::File { node, .. }) => Some(node),
        _ => None,
    }
}

/// Узел, стоящий за открытым дескриптором, **и путь VFS, с которым его
/// открыли**. `mmap` нужны оба: узел — чтобы скопировать байты файла, путь —
/// для колонки pathname в `/proc/<pid>/maps` (`docs/procfs.md` §14).
pub fn file_backing_for_fd(fd: u32) -> Option<(Arc<dyn VfsNode>, String)> {
    compat::with_current_compat(|cs| match cs.fds.get(fd) {
        Some(OpenObject::File { node, path, .. }) => Some((Arc::clone(node), path.clone())),
        _ => None,
    })
    .flatten()
}

/// `f_flags` открытого дескриптора (или `None`, если fd закрыт): нужен
/// `mmap(MAP_SHARED)` для проверки «открыт на запись» и `fcntl` для `F_GETFL`.
pub fn fd_open_flags(fd: u32) -> Option<u64> {
    compat::with_current_compat(|cs| cs.fds.open_flags(fd)).flatten()
}

/// Зафиксировать новое смещение открытого дескриптора (no-op, если это сейчас
/// не открытый `File`). Вызывается после `read`/`write`/`lseek`, сдвинувших
/// смещение.
fn set_fd_offset(fd: u32, new_off: u64) {
    compat::with_current_compat(|cs| {
        if let Some(OpenObject::File { offset, .. }) = cs.fds.get_mut(fd) {
            *offset = new_off;
        }
    });
}

/// Записать `slice` в консоль ядра, используя тот же путь serial-консоли, что и
/// legacy `SYS_WRITE`: валидный UTF-8-префикс пишется как `&str`, хвостовые
/// невалидные байты — по одному, чтобы вывод не паниковал.
fn console_write(slice: &[u8]) {
    use crate::drivers::Console;
    let console = crate::drivers::serial::console();
    // Дублировать каждый байт stdout/stderr (и эхо stdin) ещё и в
    // framebuffer-консоль: раньше вывод шёл ТОЛЬКО в serial, и
    // Compat_Process был невидим в графическом окне QEMU — шелл печатает
    // через kprintln!+fb_println!, а вывод python обходил framebuffer.
    // FB-писатель сам обрабатывает \n и 0x08, так что эхо line-редактора
    // тоже рисуется правильно. Весь compat-вывод stdout/stderr идёт через
    // VT-эмулятор. Если счётчик растёт, а экран чёрный — виноват VT-рендерер;
    // если не растёт — UI-клиент не рисует.
    diag_count(
        &DIAG_CONSOLE_BYTES,
        "console bytes",
        slice.len() as u64,
        8192,
    );
    crate::drivers::vt::write(slice);
    match core::str::from_utf8(slice) {
        Ok(s) => {
            console.write_str(s);
        }
        Err(e) => {
            let valid_up_to = e.valid_up_to();
            // SAFETY: `from_utf8` гарантирует, что `slice[..valid_up_to]` — валидный UTF-8.
            let prefix = unsafe { core::str::from_utf8_unchecked(&slice[..valid_up_to]) };
            console.write_str(prefix);
            for &byte in &slice[valid_up_to..] {
                let mut tmp = [0u8; 4];
                let s = (byte as char).encode_utf8(&mut tmp);
                console.write_str(s);
            }
        }
    }
}

/// Скопировать `len` проверенных пользовательских байт по `ptr` в свой буфер.
///
/// ПРЕДУСЛОВИЕ: `[ptr, ptr+len)` уже прошёл [`check_user_ptr`].
fn copy_in(ptr: u64, len: u64) -> Vec<u8> {
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        // SAFETY: диапазон проверен (в границах, все страницы отображены), а
        // активный CR3 — user PML4 вызывающего процесса, поэтому источник читаем.
        unsafe {
            core::ptr::copy_nonoverlapping(ptr as *const u8, buf.as_mut_ptr(), len as usize);
        }
    }
    buf
}

/// Скопировать `src` наружу в пользовательский буфер по `ptr`.
///
/// Право записи проверяется ЗДЕСЬ, на единственном низкоуровневом choke point'е
/// всех копирований в user-память этого модуля (F-r4-arch-1): буфер, который
/// прошёл старый `check_user_ptr` (только `USER_ACCESSIBLE`), мог оказаться
/// present read-only — store в него давал #PF в ring 0. Возврат `EFAULT` вместо
/// записи сохраняет контракт syscall-слоя: отказ валидации никогда не трогает
/// страницу.
fn copy_out(ptr: u64, src: &[u8]) -> Result<(), Errno> {
    if src.is_empty() {
        return Ok(());
    }
    check_user_ptr_writable(ptr, src.len() as u64)?;
    // SAFETY: диапазон проверен выше (включая право записи/COW); активный CR3 —
    // user PML4 вызывающего, поэтому приёмник доступен на запись.
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), ptr as *mut u8, src.len());
    }
    Ok(())
}

/// Прочитать NUL-терминированную строку пути из памяти пользователя, проверяя
/// страницу каждого байта перед разыменованием. Путь ограничен 4096 байтами
/// (`EINVAL`, если длиннее или не валидный UTF-8).
fn read_user_cstr(ptr: u64) -> Result<String, Errno> {
    const PATH_MAX: usize = 4096;
    let mut bytes: Vec<u8> = Vec::new();
    let mut addr = ptr;
    for _ in 0..PATH_MAX {
        check_user_ptr(addr, 1)?;
        // SAFETY: одиночный байт по `addr` только что проверен как отображённый/в диапазоне.
        let b = unsafe { *(addr as *const u8) };
        if b == 0 {
            return String::from_utf8(bytes).map_err(|_| Errno::EINVAL);
        }
        bytes.push(b);
        addr += 1;
    }
    Err(Errno::EINVAL)
}

/// Блокирующее/неблокирующее чтение конца очереди (pipe или AF_UNIX-сокета).
///
/// Ожидание — уступка CPU с перечитыванием очереди. На каждой уступке
/// перепроверяется владение очередью: закрытый из соседнего потока дескриптор
/// EOF не выставляет (счётчики очереди считают концы, а не fd), поэтому без
/// [`unix_sock::endpoint_wait_verdict`] заблокированный `read` спал бы вечно —
/// тот же класс зависания, что watchdog печатает для `recvfrom` (nr 45).
fn read_pipe(e: &PipeEndpoint, dst: &mut [u8]) -> Result<usize, Errno> {
    loop {
        match e.read(dst) {
            PipeReadResult::Data(n) => return Ok(n),
            PipeReadResult::Eof => return Ok(0),
            PipeReadResult::WouldBlock if e.nonblocking() => return Err(Errno::EAGAIN),
            PipeReadResult::WouldBlock => match unix_sock::endpoint_wait_verdict(e) {
                super::sock_wait::WaitVerdict::Continue => crate::task::scheduler::yield_current(),
                super::sock_wait::WaitVerdict::Eof => return Ok(0),
                super::sock_wait::WaitVerdict::Ebadf => return Err(Errno::EBADF),
            },
        }
    }
}
fn write_pipe(e: &PipeEndpoint, src: &[u8]) -> Result<usize, Errno> {
    loop {
        match e.write(src) {
            PipeWriteResult::Data(n) => return Ok(n),
            PipeWriteResult::Broken => return Err(Errno::EPIPE),
            PipeWriteResult::WouldBlock if e.nonblocking() => return Err(Errno::EAGAIN),
            PipeWriteResult::WouldBlock => crate::task::scheduler::yield_current(),
        }
    }
}

/// `read(2)` на `/dev/input/event0`: одно 24-байтное `struct input_event` за
/// вызов.
///
/// Linux evdev требует `count >= sizeof(struct input_event)` (иначе `EINVAL`) и
/// возвращает ровно одно событие. Пустая очередь блокирует вызов до события;
/// `O_NONBLOCK` даёт `EAGAIN`, доставимый сигнал — `EINTR`. Nonblock-флаг
/// читается ДО цикла ожидания: compat-лок нельзя держать, пока уступаем CPU.
fn read_evdev_event(fd: u32, buf: u64, count: u64) -> Result<u64, Errno> {
    const EV_SIZE: u64 = crate::drivers::evdev::EVENT_SIZE as u64;
    if count < EV_SIZE {
        return Err(Errno::EINVAL);
    }
    check_user_ptr_writable(buf, EV_SIZE)?;
    let nonblock = fd_open_flags(fd)
        .map(|f| f & crate::task::fd::O_NONBLOCK != 0)
        .unwrap_or(false);
    let mut event = [0u8; crate::drivers::evdev::EVENT_SIZE];
    loop {
        if crate::drivers::evdev::read_event(&mut event) {
            // Повторная проверка приёмника: буфер мог исчезнуть, пока вызов
            // уступал CPU (тот же TOCTOU-контракт, что у pipe/read).
            copy_out_checked(buf, &event)?;
            return Ok(EV_SIZE);
        }
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        if super::signal::has_deliverable_current() {
            return Err(Errno::EINTR);
        }
        crate::task::scheduler::yield_current();
    }
}

/// `read` (0): скопировать до `count` байт файла с текущего смещения в буфер
/// пользователя, сдвинуть смещение на скопированное и вернуть это число
/// (R2.3). `EBADF` для отсутствующего fd (R2.14); чтение консоли/stdin даёт 0.
pub fn sys_read(fd: u64, buf: u64, count: u64) -> Result<u64, Errno> {
    if count > COUNT_MAX {
        return Err(Errno::EINVAL);
    }
    // evdev читает одно событие (24 байта), поэтому полный `count` ему не
    // обязан быть отображён: Linux проверяет только `sizeof(input_event)`.
    // Остальные файловые/pipe-пути сохраняют прежнюю проверку всего буфера.
    if let Some(Resolved::File { node, .. }) = resolve_fd(fd as u32) {
        if node.is_vtinput() {
            return Err(Errno::EBADF);
        }
        if node.is_evdev() {
            return read_evdev_event(fd as u32, buf, count);
        }
    }
    check_user_ptr(buf, count)?;

    match resolve_fd(fd as u32) {
        None => Err(Errno::EBADF),
        // Stdin теперь интерактивен — блокирующее построчное чтение с
        // PS/2-клавиатуры (эхо, backspace, ^D = EOF). Раньше здесь был
        // мгновенный EOF, и CPython молча завершался с кодом 0.
        Some(Resolved::Console) | Some(Resolved::Stdin) => {
            // Контроль доступа к терминалу: фоновый JOB, читающий терминал,
            // останавливается (SIGTTIN ставится его группе, здесь -EINTR —
            // группа паркуется на возврате из syscall). Проверка ДО чтения,
            // чтобы процесс не съел чужое нажатие.
            crate::drivers::tty::check_access(super::tty::TtyAccess::Read)?;
            let raw = crate::task::compat::with_current_compat(|cs| cs.raw_mode).unwrap_or(false);
            if raw {
                read_stdin_raw(buf, count)
            } else {
                read_stdin_line(buf, count)
            }
        }
        Some(Resolved::Dir) => Err(Errno::EISDIR),
        Some(Resolved::PipeWrite(_)) => Err(Errno::EBADF),
        Some(Resolved::Signalfd(mask)) => {
            return super::signalfd::read_signalfd(mask, buf, count);
        }
        Some(Resolved::Timerfd(state)) => {
            if count < 8 {
                return Err(Errno::EINVAL);
            }
            check_user_ptr(buf, 8)?;
            loop {
                let due = state.lock().take(crate::task::scheduler::ticks());
                if due > 0 {
                    // Цикл выше уступает CPU: повторная проверка перед публикацией
                    // (исчезнувший буфер — EFAULT, а не #PF в ring 0).
                    copy_out_checked(buf, &due.to_le_bytes())?;
                    return Ok(8);
                }
                if state.lock().nonblocking {
                    return Err(Errno::EAGAIN);
                }
                if super::signal::has_deliverable_current() {
                    return Err(Errno::EINTR);
                }
                crate::task::scheduler::yield_current();
            }
        }
        Some(Resolved::Eventfd { val, semaphore }) => {
            // чтение eventfd: блокируется, пока val > 0, затем возвращает u64 и сбрасывает счётчик.
            if count < 8 {
                return Err(Errno::EINVAL);
            }
            check_user_ptr(buf, 8)?;
            loop {
                let mut v = val.lock();
                if *v > 0 {
                    let out = if semaphore {
                        *v -= 1;
                        1u64
                    } else {
                        let r = *v;
                        *v = 0;
                        r
                    };
                    drop(v);
                    // Цикл ожидания уступает CPU: повторная проверка перед публикацией
                    // (исчезнувший буфер — EFAULT, а не #PF в ring 0).
                    copy_out_checked(buf, &out.to_ne_bytes())?;
                    return Ok(8);
                }
                drop(v);
                crate::task::scheduler::yield_current();
            }
        }
        Some(Resolved::Epoll) => Err(Errno::EINVAL),
        Some(Resolved::InetTcp(t)) => {
            let mut data = vec![0u8; count as usize];
            let n = crate::arch::x86_64::linux::inet_sock::tcp_read(fd, &mut data)?;
            copy_out_checked(buf, &data[..n])?;
            return Ok(n as u64);
        }
        Some(Resolved::InetUdp(_)) => Err(Errno::ENOTCONN), // только recvfrom
        Some(Resolved::PipeRead(e)) => {
            let mut data = vec![0u8; count as usize];
            let n = read_pipe(&e, &mut data)?;
            if n > 0 {}
            copy_out_checked(buf, &data[..n])?;
            Ok(n as u64)
        }
        Some(Resolved::Socket { rx, .. }) => {
            let mut data = vec![0u8; count as usize];
            let n = read_pipe(&rx, &mut data)?;
            if n > 0 {
                diag_count(&DIAG_SOCK_R, "sock read", n as u64, 16384);
            }
            copy_out_checked(buf, &data[..n])?;
            Ok(n as u64)
        }
        Some(Resolved::File { node, offset }) => {
            let size = node.size();
            let (copied, _) = plan_read(size, offset, count);
            if copied == 0 {
                return Ok(0);
            }
            let mut kbuf = vec![0u8; copied as usize];
            let n = node.read(offset, &mut kbuf).map_err(|e| {
                // Настоящая ошибка чтения VFS/ext2 — не «неверный аргумент»:
                // сообщаем EIO и логируем, что именно сломалось.
                crate::error!(
                    "[linux] file read failed: {:?} ino={} off={} len={}",
                    e,
                    node.fs_ino(),
                    offset,
                    copied
                );
                Errno::EIO
            })?;
            copy_out_checked(buf, &kbuf[..n])?;
            let new_off = offset + n as u64;
            set_fd_offset(fd as u32, new_off);
            Ok(n as u64)
        }
    }
}

/// `write` (1): записать `count` пользовательских байт в дескриптор. fd 1/2
/// (консоль) пишут в консоль ядра и возвращают `count` (R2.2); файловый
/// дескриптор пишет по своему смещению и сдвигает его; stdin не пишется
/// (`EBADF`); отсутствующий fd — `EBADF` (R2.14).

/// `copy_file_range` (326): скопировать диапазон байт между двумя regular-файлами.
///
/// Оба дескриптора должны быть regular-файлами; `flags` должен быть 0. Каждый
/// необязательный указатель смещения, если не NULL, читается до копирования и
/// записывается после (собственное смещение fd не меняется); NULL означает
/// использовать и сдвигать смещение fd. Короткая запись завершает цикл и
/// возвращает число скопированных байт.
pub fn sys_copy_file_range(
    fd_in: u64,
    off_in: u64,
    fd_out: u64,
    off_out: u64,
    len: u64,
    flags: u64,
) -> Result<u64, Errno> {
    if flags != 0 || len > COUNT_MAX {
        return Err(Errno::EINVAL);
    }
    let (input_node, input_current) = match resolve_fd(fd_in as u32) {
        Some(Resolved::File { node, offset }) => (node, offset),
        _ => return Err(Errno::EINVAL),
    };
    let (output_node, output_current) = match resolve_fd(fd_out as u32) {
        Some(Resolved::File { node, offset }) => (node, offset),
        _ => return Err(Errno::EINVAL),
    };
    let mut read_and_advance = |ptr: u64, current: u64| -> Result<u64, Errno> {
        if ptr == 0 {
            return Ok(current);
        }
        check_user_ptr(ptr, 8)?;
        let raw = copy_in_pub(ptr, 8);
        Ok(u64::from_le_bytes(raw[..8].try_into().unwrap_or([0; 8])))
    };
    let mut in_off = read_and_advance(off_in, input_current)?;
    let mut out_off = read_and_advance(off_out, output_current)?;
    let mut total = 0u64;
    let mut buf = [0u8; 8192];
    while total < len {
        let want = (len - total).min(buf.len() as u64) as usize;
        let n = match input_node.read(in_off, &mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return Err(Errno::EINVAL),
        };
        let written = output_node.write(out_off, &buf[..n]).map_err(write_errno)?;
        in_off += n as u64;
        out_off += written as u64;
        total += written as u64;
        if written < n {
            break;
        }
    }
    if off_in != 0 {
        copy_out_pub(off_in, &in_off.to_le_bytes());
    } else {
        set_fd_offset(fd_in as u32, in_off);
    }
    if off_out != 0 {
        copy_out_pub(off_out, &out_off.to_le_bytes());
    } else {
        set_fd_offset(fd_out as u32, out_off);
    }
    Ok(total)
}

/// `sendfile` (40): скопировать байты из regular-файла в файл/сокет/консоль.
///
/// Поддерживается только regular-файл на входе; выходом может быть regular-файл,
/// консоль, pipe, конец AF_UNIX socketpair или AF_INET-поток. Входное смещение
/// берётся из `*offset`, если он не NULL (и записывается обратно), иначе из
/// текущего смещения fd, которое сдвигается. Короткая запись на выходе
/// завершает копирование и возвращает перенесённое число байт, как в Linux.
pub fn sys_sendfile(out_fd: u64, in_fd: u64, offset: u64, count: u64) -> Result<u64, Errno> {
    if count > COUNT_MAX {
        return Err(Errno::EINVAL);
    }
    let (input_node, input_offset) = match resolve_fd(in_fd as u32) {
        Some(Resolved::File { node, offset }) => (node, offset),
        Some(_) | None => return Err(Errno::EINVAL),
    };
    let output = match resolve_fd(out_fd as u32) {
        Some(value) => value,
        None => return Err(Errno::EBADF),
    };
    // sendfile в терминал — это запись в него: тот же контроль доступа, что у
    // `write(2)`, проверяется один раз, прежде чем хоть один байт дойдёт до консоли.
    if matches!(output, Resolved::Console) {
        crate::drivers::tty::check_access(super::tty::TtyAccess::Write)?;
    }
    let mut in_offset = if offset != 0 {
        check_user_ptr(offset, 8)?;
        let raw = copy_in_pub(offset, 8);
        u64::from_le_bytes(raw[..8].try_into().unwrap_or([0; 8]))
    } else {
        input_offset
    };
    let mut out_offset = match &output {
        Resolved::File { offset, .. } => *offset,
        _ => 0,
    };
    let mut total = 0u64;
    let mut buf = [0u8; 8192];
    while total < count {
        let want = (count - total).min(buf.len() as u64) as usize;
        let n = match input_node.read(in_offset, &mut buf[..want]) {
            Ok(n) => n,
            Err(_) => return Err(Errno::EINVAL),
        };
        if n == 0 {
            break;
        }
        let written = match &output {
            Resolved::Console => {
                console_write(&buf[..n]);
                n
            }
            Resolved::File { node, .. } => {
                let written = node.write(out_offset, &buf[..n]).map_err(write_errno)?;
                out_offset += written as u64;
                written
            }
            Resolved::PipeWrite(end) => write_pipe(end, &buf[..n])?,
            Resolved::Socket { tx, .. } => write_pipe(tx, &buf[..n])?,
            Resolved::InetTcp(_) => {
                crate::arch::x86_64::linux::inet_sock::tcp_write(out_fd, &buf[..n])?
            }
            Resolved::InetUdp(_) => {
                crate::arch::x86_64::linux::inet_sock::udp_write_fd(out_fd, &buf[..n])?
            }
            _ => return Err(Errno::EINVAL),
        };
        in_offset += n as u64;
        total += written as u64;
        if written < n {
            break;
        }
    }
    if offset != 0 {
        // Цикл копирования выше может блокироваться на pipe/сокете и уступать
        // CPU; указатель смещения проверен один раз до цикла, поэтому перед
        // публикацией проверяем его снова.
        copy_out_checked(offset, &in_offset.to_le_bytes())?;
    } else {
        set_fd_offset(in_fd as u32, in_offset);
    }
    if let Resolved::File { .. } = output {
        set_fd_offset(out_fd as u32, out_offset);
    }
    Ok(total)
}

pub fn sys_write(fd: u64, buf: u64, count: u64) -> Result<u64, Errno> {
    if count > COUNT_MAX {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(buf, count)?;

    match resolve_fd(fd as u32) {
        None => Err(Errno::EBADF),
        Some(Resolved::Stdin) => Err(Errno::EBADF),
        Some(Resolved::Dir) => Err(Errno::EISDIR),
        Some(Resolved::Timerfd(_)) | Some(Resolved::Signalfd(_)) => Err(Errno::EINVAL),
        Some(Resolved::Eventfd { val, .. }) => {
            if count < 8 {
                return Err(Errno::EINVAL);
            }
            check_user_ptr(buf, 8)?;
            // SAFETY: проверено выше
            let add = unsafe { core::ptr::read_unaligned(buf as *const u64) };
            let mut count = val.lock();
            *count = count.saturating_add(add);
            Ok(8)
        }
        Some(Resolved::Epoll) => Err(Errno::EINVAL),
        Some(Resolved::PipeRead(_)) => Err(Errno::EBADF),
        Some(Resolved::InetTcp(_)) => {
            let data = copy_in(buf, count);
            let n = crate::arch::x86_64::linux::inet_sock::tcp_write(fd, &data)?;
            Ok(n as u64)
        }
        Some(Resolved::InetUdp(_)) => {
            let data = copy_in(buf, count);
            let n = crate::arch::x86_64::linux::inet_sock::udp_write_fd(fd, &data)?;
            Ok(n as u64)
        }
        Some(Resolved::PipeWrite(e)) => {
            let data = copy_in(buf, count);

            Ok(write_pipe(&e, &data)? as u64)
        }
        Some(Resolved::Socket { tx, .. }) => {
            let data = copy_in(buf, count);
            let n = write_pipe(&tx, &data)?;
            if n > 0 {
                diag_count(&DIAG_SOCK_W, "sock write", n as u64, 16384);
            }
            Ok(n as u64)
        }
        Some(Resolved::Console) => {
            // Контроль доступа к терминалу: фоновый JOB, пишущий в терминал,
            // останавливается при TOSTOP (SIGTTOU + -EINTR), как в Linux; если
            // TOSTOP снят, запись проходит.
            crate::drivers::tty::check_access(super::tty::TtyAccess::Write)?;
            let data = copy_in(buf, count);
            console_write(&data);
            Ok(count)
        }
        Some(Resolved::File { node, .. }) if node.is_vtconsole() => {
            // Shared surface — read-only: экран публикует только ядро.
            Err(Errno::EBADF)
        }
        Some(Resolved::File { node, offset }) => {
            let data = copy_in(buf, count);
            let n = node.write(offset, &data).map_err(write_errno)?;
            set_fd_offset(fd as u32, offset + n as u64);
            Ok(n as u64)
        }
    }
}

/// `writev` (20): gather-запись до `iovcnt` записей `iovec`. fd 1/2 выводят
/// каждый буфер в консоль; файловый дескриптор пишет их по порядку, сдвигая
/// смещение; возвращается суммарное число байт (R2.2). Каждый `iov_base`
/// проверяется через точку контроля указателей до чтения.
pub fn sys_writev(fd: u64, iov: u64, iovcnt: u64) -> Result<u64, Errno> {
    // struct iovec { void *iov_base; size_t iov_len; } — 16 байт на x86_64.
    const IOV_SIZE: u64 = 16;
    const IOV_MAX: u64 = 1024;
    if iovcnt == 0 {
        return Ok(0);
    }
    if iovcnt > IOV_MAX {
        return Err(Errno::EINVAL);
    }
    // Проверить сам массив iovec, прежде чем читать из него записи.
    check_user_ptr(iov, iovcnt * IOV_SIZE)?;

    let target = resolve_fd(fd as u32).ok_or(Errno::EBADF)?;
    if matches!(target, Resolved::Stdin) {
        return Err(Errno::EBADF);
    }
    if matches!(target, Resolved::InetTcp(_)) {
        // gather-запись сразу в TCP-сокет; считаем суммарный объём
        let mut total: u64 = 0;
        for i in 0..iovcnt {
            let entry = iov + i * 16;
            // Та же повторная проверка, что и в общем цикле ниже: `tcp_write`
            // может блокироваться и уступать CPU, а соседний поток — снять
            // отображение массива.
            check_user_ptr(entry, IOV_SIZE)?;
            let base = unsafe { *(entry as *const u64) };
            let len = unsafe { *((entry + 8) as *const u64) };
            if len == 0 {
                continue;
            }
            check_user_ptr(base, len)?;
            let data = copy_in(base, len);
            let n = crate::arch::x86_64::linux::inet_sock::tcp_write(fd, &data)?;
            total += n as u64;
            if (n as u64) < len {
                break;
            }
        }
        return Ok(total);
    }
    if matches!(target, Resolved::Dir) {
        return Err(Errno::EISDIR);
    }
    if matches!(target, Resolved::Eventfd { .. } | Resolved::Epoll) {
        return Err(Errno::EINVAL);
    }
    if matches!(target, Resolved::PipeRead(_)) {
        return Err(Errno::EBADF);
    }

    // Для файлового случая ведём текущее смещение и фиксируем его в конце.
    let mut file_off = match &target {
        Resolved::File { offset, .. } => *offset,
        _ => 0,
    };
    let mut total: u64 = 0;

    for i in 0..iovcnt {
        let entry = iov + i * IOV_SIZE;
        // Проверять запись на каждой итерации: scalar read/write предыдущей
        // записи может блокироваться и уступать CPU, и поток-сосед по CLONE_VM
        // снимет отображение массива iovec. Без этого чтение `entry` из ring 0
        // падало в режиме ядра и останавливало машину.
        check_user_ptr(entry, IOV_SIZE)?;
        // SAFETY: 16-байтная запись только что проверена.
        let base = unsafe { *(entry as *const u64) };
        let len = unsafe { *((entry + 8) as *const u64) };
        if len == 0 {
            continue;
        }
        if len > COUNT_MAX {
            return Err(Errno::EINVAL);
        }
        check_user_ptr(base, len)?;
        let data = copy_in(base, len);
        match &target {
            Resolved::Console => {
                // Тот же контроль доступа к терминалу, что у `write(2)`:
                // фоновый job под TOSTOP останавливается до первой записи.
                crate::drivers::tty::check_access(super::tty::TtyAccess::Write)?;
                console_write(&data);
                total += len;
            }
            Resolved::PipeWrite(e) => {
                let n = write_pipe(e, &data)?;
                total += n as u64;
                if n < data.len() {
                    break;
                }
            }
            Resolved::File { node, .. } => {
                let n = node.write(file_off, &data).map_err(write_errno)?;
                file_off += n as u64;
                total += n as u64;
            }
            Resolved::Socket { tx, .. } => {
                let n = write_pipe(tx, &data)?;
                total += n as u64;
                if n < data.len() {
                    break;
                }
            }
            // Stdin/PipeRead/Dir/Eventfd/Epoll отсекаются проверками выше.
            _ => unreachable!(),
        }
    }

    if matches!(target, Resolved::File { .. }) {
        set_fd_offset(fd as u32, file_off);
    }
    Ok(total)
}

/// `readv` (19) — vectored-чтение. Заполняет iovec по порядку, останавливаясь
/// на коротком чтении или ошибке (короткий счётчик — допустимый результат
/// readv); вызывающие (glibc stdio, libuv) доделывают остаток циклом. Scalar-
/// примитив наследует блокирующую семантику `sys_read`.
pub fn sys_readv(fd: u64, iov: u64, iovcnt: u64) -> Result<u64, Errno> {
    const IOV_SIZE: u64 = 16;
    const IOV_MAX: u64 = 1024;
    if iovcnt == 0 {
        return Ok(0);
    }
    if iovcnt > IOV_MAX {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(iov, iovcnt * IOV_SIZE)?;
    let mut total: u64 = 0;
    for i in 0..iovcnt {
        let entry = iov + i * IOV_SIZE;
        // Проверять запись на каждой итерации: scalar read/write предыдущей
        // записи может блокироваться и уступать CPU, и поток-сосед по CLONE_VM
        // снимет отображение массива iovec. Без этого чтение `entry` из ring 0
        // падало в режиме ядра и останавливало машину.
        check_user_ptr(entry, IOV_SIZE)?;
        // SAFETY: 16-байтная запись только что проверена.
        let base = unsafe { *(entry as *const u64) };
        let len = unsafe { *((entry + 8) as *const u64) };
        if len == 0 {
            continue;
        }
        let n = sys_read(fd, base, len)?;
        total += n;
        if n < len {
            break;
        }
    }
    Ok(total)
}

/// `preadv` (295) — позиционное vectored-чтение; первый непустой
/// iovec через `sys_pread64` (смещение дескриптора не сдвигается).
pub fn sys_preadv(fd: u64, iov: u64, iovcnt: u64, offset: u64) -> Result<u64, Errno> {
    const IOV_SIZE: u64 = 16;
    const IOV_MAX: u64 = 1024;
    if iovcnt == 0 {
        return Ok(0);
    }
    if iovcnt > IOV_MAX {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(iov, iovcnt * IOV_SIZE)?;
    let mut total: u64 = 0;
    for i in 0..iovcnt {
        let entry = iov + i * IOV_SIZE;
        // Проверять запись на каждой итерации: scalar read/write предыдущей
        // записи может блокироваться и уступать CPU, и поток-сосед по CLONE_VM
        // снимет отображение массива iovec. Без этого чтение `entry` из ring 0
        // падало в режиме ядра и останавливало машину.
        check_user_ptr(entry, IOV_SIZE)?;
        // SAFETY: 16-байтная запись только что проверена.
        let base = unsafe { *(entry as *const u64) };
        let len = unsafe { *((entry + 8) as *const u64) };
        if len == 0 {
            continue;
        }
        let n = sys_pread64(fd, base, len, offset + total)?;
        total += n;
        if n < len {
            break;
        }
    }
    Ok(total)
}

/// `pwritev` (296) — позиционная vectored-запись; первый непустой
/// iovec через `sys_pwrite64`.
pub fn sys_pwritev(fd: u64, iov: u64, iovcnt: u64, offset: u64) -> Result<u64, Errno> {
    const IOV_SIZE: u64 = 16;
    const IOV_MAX: u64 = 1024;
    if iovcnt == 0 {
        return Ok(0);
    }
    if iovcnt > IOV_MAX {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(iov, iovcnt * IOV_SIZE)?;
    let mut total: u64 = 0;
    for i in 0..iovcnt {
        let entry = iov + i * IOV_SIZE;
        // Проверять запись на каждой итерации: scalar read/write предыдущей
        // записи может блокироваться и уступать CPU, и поток-сосед по CLONE_VM
        // снимет отображение массива iovec. Без этого чтение `entry` из ring 0
        // падало в режиме ядра и останавливало машину.
        check_user_ptr(entry, IOV_SIZE)?;
        // SAFETY: 16-байтная запись только что проверена.
        let base = unsafe { *(entry as *const u64) };
        let len = unsafe { *((entry + 8) as *const u64) };
        if len == 0 {
            continue;
        }
        let n = sys_pwrite64(fd, base, len, offset + total)?;
        total += n;
        if n < len {
            break;
        }
    }
    Ok(total)
}

/// `fsync`/`fdatasync` (74/75). nvim делает fsync ShaDa-файла при записи;
/// WAL ext2 журналирует каждую запись уже во время syscall (ordered mode),
/// поэтому грязного кеша для сброса нет — успех на любом валидном fd.
pub fn sys_fsync(fd: u64) -> Result<u64, Errno> {
    match resolve_fd(fd as u32) {
        None => Err(Errno::EBADF),
        Some(_) => Ok(0),
    }
}

/// Прочитать cwd текущего процесса (абсолютный), по умолчанию `/`, если
/// compat-состояния нет (нативная задача, гоняющая эти обработчики в тестах).
fn current_cwd() -> String {
    compat::with_current_compat(|cs| cs.cwd.clone()).unwrap_or_else(|| String::from("/"))
}

/// Нормализовать `path`: разрешить её относительно текущего рабочего каталога,
/// если она относительная, и схлопнуть `.`/`..`/пустые компоненты в чистый
/// абсолютный путь.
///
/// `..` в корне остаётся в корне; результат всегда начинается с `/` и не имеет
/// завершающего слэша (кроме голого `/`). Это единственное место, где
/// относительные пути `open`/`openat`/`access`/`chdir`/`statfs`/`readlink`
/// становятся абсолютными до [`vfs::lookup_path`].
fn resolve_path(path: &str) -> String {
    let combined = if path.starts_with('/') {
        String::from(path)
    } else {
        let mut c = current_cwd();
        if !c.ends_with('/') {
            c.push('/');
        }
        c.push_str(path);
        c
    };

    let mut stack: Vec<&str> = Vec::new();
    for comp in combined.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    if stack.is_empty() {
        return String::from("/");
    }
    let mut out = String::new();
    for comp in &stack {
        out.push('/');
        out.push_str(comp);
    }
    // Гостевое отображение chroot: Linux-программы видят дерево ext2 как `/`.
    // Специальные деревья ядра остаются на месте (/dev-узлы, ramfs /tmp, /proc,
    // явное представление /mnt), всё остальное резолвится под /mnt — именно так
    // /etc/resolv.conf виден резолверу glibc. Предикат сравнивает целые
    // компоненты пути (`/process` — гостевой путь, а не путь `/proc`).
    if out != "/" && !super::io::guest_path_keeps_root(&out) {
        let mut mapped = String::with_capacity(out.len() + 4);
        mapped.push_str("/mnt");
        mapped.push_str(&out);
        return mapped;
    }
    out
}

/// Разрешить уже отображённый в госте абсолютный путь через обход ссылок VFS
/// (контракт `EXT2-LINKS.md` §4).
///
/// `follow_final` выбирает семантику syscall: `true` для
/// `stat`/`open`/`access`/`chdir`/`execve` (разыменовывать и последний
/// компонент), `false` для `lstat`, `readlink`, `unlink` и `rmdir` (ответ —
/// сама ссылка, промежуточные компоненты всё равно разыменовываются). Сам обход
/// (стек компонентов, обработка `..`, относительные/абсолютные цели, бюджет
/// `SYMLOOP_MAX = 40`) живёт в host-property-тестируемом `vfs::link_walk`; здесь
/// только отображение `WalkError` → `Errno`.
fn walk(abs: &str, follow_final: bool) -> Result<Arc<dyn VfsNode>, Errno> {
    walk_as(abs, follow_final, current_creds())
}

/// [`walk`] с явными креденшелами.
///
/// Обходчик ссылок спрашивает предикат креденшелов перед поиском имени в
/// каталоге, поэтому здесь POSIX-право поиска в каталоге (`x`) проверяется на
/// **каждом** промежуточном компоненте, а не только на конечном узле (иначе
/// uid 1000 мог прочитать файл 0644 внутри каталога 0700, принадлежащего root).
/// `access(2)` передаёт набор по real id, все остальные syscall'ы — по
/// effective id.
fn walk_as(abs: &str, follow_final: bool, creds: Creds) -> Result<Arc<dyn VfsNode>, Errno> {
    let may_search =
        |node: &Arc<dyn VfsNode>| creds.may(node.mode(), node.owner(), Access::Execute);
    match vfs::lookup_path_walk_checked(abs, follow_final, &may_search) {
        Ok(node) => Ok(node),
        Err(vfs::link_walk::WalkError::NotFound) => Err(Errno::ENOENT),
        Err(vfs::link_walk::WalkError::NotDir) => Err(Errno::ENOTDIR),
        Err(vfs::link_walk::WalkError::TooLong) => Err(Errno::ENAMETOOLONG),
        Err(vfs::link_walk::WalkError::AccessDenied) => Err(Errno::EACCES),
        Err(vfs::link_walk::WalkError::TooManyLinks) => {
            crate::warn!(
                "[linux] symlink budget {} exhausted at '{}': ELOOP",
                vfs::link_walk::SYMLOOP_MAX,
                abs
            );
            Err(Errno::ELOOP)
        }
    }
}

/// Построить [`OpenObject`] для уже разрешённого абсолютного пути, выделив для
/// него новый дескриптор. Каталог становится [`OpenObject::Dir`] со снимком
/// потомков (для `getdents64`); файл — [`OpenObject::File`] со смещением 0.
/// `flags` — реальные флаги `open(2)` вызывающего, они сохраняются для
/// `/proc/<pid>/fdinfo/<n>`.
fn open_resolved(abs: &str, flags: u64) -> Result<u64, Errno> {
    // `open(2)` разыменовывает конечную симлинк-ссылку; `/proc/self/exe` —
    // как раз такая ссылка, и открытие должно вернуть реальный образ, а не узел
    // ссылки.
    let node = walk(abs, true)?;
    let obj = if node.is_directory() {
        let children = node.readdir().unwrap_or_default();
        OpenObject::Dir {
            path: String::from(abs),
            children,
            index: 0,
        }
    } else {
        OpenObject::File {
            node: Arc::clone(&node),
            path: String::from(abs),
            offset: 0,
        }
    };
    let fd = compat::with_current_compat(|cs| cs.fds.alloc_with_flags(obj, flags))
        .ok_or(Errno::EBADF)??;
    if node.is_vtconsole() {
        // Публикация включается после успешной аллокации fd: клиент сразу
        // получает снимок уже нарисованного shell-текста.
        crate::drivers::vtsurface::activate();
    }
    Ok(fd as u64)
}

// Флаги создания для open/openat (восьмеричные значения из fcntl.h).
const O_CREAT_FL: u64 = 0o100;
const O_EXCL_FL: u64 = 0o200;
const O_TRUNC_FL: u64 = 0o1000;
/// `open(2)` обязан вернуть `ELOOP`, если конечный компонент — символическая ссылка.
const O_NOFOLLOW_FL: u64 = 0o400000;

/// Разрешить пользовательский путь (относительно cwd, если относительный) и
/// выделить новый дескриптор (R2.4). Учитывает O_CREAT/O_EXCL (создаёт
/// regular-файл через `create_file` родительского каталога — то есть работает
/// на ramfs /tmp; деревья только для чтения по-прежнему отказывают) и
/// best-effort O_TRUNC. `ENOENT`, если пути нет и O_CREAT не задан (R2.5).
/// Общий код для `open`/`openat`.
fn open_path(path: &str, flags: u64, mode: u32) -> Result<u64, Errno> {
    let abs = resolve_path(path);
    let trimmed = abs.trim_end_matches('/');
    let lookup_target = if trimmed.is_empty() { "/" } else { trimmed };

    // O_NOFOLLOW: *конечный* компонент не должен быть ссылкой. Linux сообщает
    // здесь ELOOP, а не EINVAL.
    if flags & O_NOFOLLOW_FL != 0 {
        if let Ok(node) = walk(lookup_target, false) {
            if node.is_symlink() {
                return Err(Errno::ELOOP);
            }
        }
    }
    // O_CREAT|O_EXCL отвергает любую существующую запись каталога — включая
    // висячую символическую ссылку, которую проверка разыменования ниже иначе
    // пропустила бы (она отвечает «ENOENT» для ссылки с исчезнувшей целью).
    if flags & O_CREAT_FL != 0 && flags & O_EXCL_FL != 0 && walk(lookup_target, false).is_ok() {
        return Err(Errno::EEXIST);
    }

    match walk(lookup_target, true) {
        Ok(node) => {
            if flags & O_CREAT_FL != 0 && flags & O_EXCL_FL != 0 {
                return Err(Errno::EEXIST);
            }
            // Проверка прав ДО побочного эффекта O_TRUNC: вызывающий, которому
            // нельзя писать в файл, не должен иметь возможности его опустошить.
            // Root обходит read/write, но для обхода каталогов через VFS ему всё
            // равно нужен execute-бит, и этот обход уже произошёл выше.
            check_open_mode(&node, flags)?;
            // `/dev/fb0`: экран забирает первый открывший (композитор), а на
            // serial-only boot устройства нет — ENODEV, не «пустой файл».
            if node.is_fbdev() {
                if node.fb_info().is_none() {
                    return Err(Errno::ENODEV);
                }
                crate::drivers::framebuffer::takeover_acquire();
            }
            // `/dev/vtconsole0` — read-only снимок консоли; запись/чтение
            // vtinput — строго по назначению (ABI: 0600, read-only/write-only).
            if node.is_vtconsole() {
                if node.vtconsole_info().is_none() {
                    return Err(Errno::ENODEV);
                }
                if flags & 0o3 != 0 {
                    return Err(Errno::EACCES);
                }
            }
            if node.is_vtinput() && flags & 0o3 == 0 {
                return Err(Errno::EACCES);
            }
            if flags & O_TRUNC_FL != 0 && !node.is_directory() {
                // Best effort: узлы без поддержки truncate (обычные файлы ext2
                // в этом минимальном слое) сохраняют содержимое.
                let _ = node.truncate(0);
            }
        }
        Err(e) => {
            // Дальше к созданию может пройти только «отсутствует»: цикл
            // (ELOOP), промежуточный не-каталог (ENOTDIR) или слишком длинное
            // развёртывание должны сообщаться как есть и никогда не превращаться
            // в новый файл.
            if flags & O_CREAT_FL == 0 || e != Errno::ENOENT {
                return Err(e);
            }
            let (parent, name) = match trimmed.rfind('/') {
                Some(0) => ("/", &trimmed[1..]),
                Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
                None => return Err(Errno::ENOENT),
            };
            if name.is_empty() {
                return Err(Errno::EINVAL);
            }
            // Родитель разрешается через обходчик ссылок: `O_CREAT` под
            // каталогом-симлинком (`/lib64/...`) должен попасть в его цель.
            let dir = walk(parent, true)?;
            // Создание записи каталога требует write + execute на родителе.
            check_dir_mutation(&dir)?;
            let created = dir.create_file(name).map_err(|e| {
                crate::warn!(
                    "[linux] open(O_CREAT) failed: {:?} parent={} name={}",
                    e,
                    parent,
                    name
                );
                Errno::EIO
            })?;
            // Аргумент mode у `open(2)`, урезанный umask процесса, — это реальный
            // набор прав файла; `0` означает, что вызывающий не передал mode
            // (некоторые пути libc так делают), и Linux трактует это как
            // `0666 & ~umask` для случая чтения/записи без exec.
            stamp_creation_owner(&created);
            let requested = if mode == 0 { 0o666 } else { mode & 0o7777 };
            let _ = apply_node_mode(&created, requested & !current_umask());
        }
    }
    open_resolved(&abs, flags)
}

/// `open` (2): открыть существующий путь ext2 (относительно cwd, если путь
/// относительный), выделив наименьший fd ≥ 3 (R2.4); `ENOENT`, если пути нет
/// (R2.5). Каталоги открываются дескриптором каталога, пригодным для `getdents64`.
pub fn sys_open(path: u64, flags: u64, mode: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    let fd = open_path(&p, flags, mode as u32)?;
    // Учитывать O_CLOEXEC: execve вычищает помеченные дескрипторы.
    if flags & O_CLOEXEC != 0 {
        compat::with_current_compat(|cs| cs.fds.set_cloexec(fd as u32, true));
    }
    Ok(fd)
}

/// `openat` (257): как `open`. `AT_FDCWD` (и любой dirfd в этом минимальном
/// слое) разрешает путь относительно cwd процесса; абсолютные пути игнорируют dirfd.
pub fn sys_openat(dirfd: u64, path: u64, flags: u64, mode: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    // Абсолютные пути игнорируют dirfd; относительные разрешаются относительно
    // cwd (единственной базы каталога, которую отслеживает этот слой), что
    // покрывает AT_FDCWD.
    let _ = dirfd;
    let fd = open_path(&p, flags, mode as u32)?;
    // Учитывать O_CLOEXEC: execve вычищает помеченные дескрипторы.
    if flags & O_CLOEXEC != 0 {
        compat::with_current_compat(|cs| cs.fds.set_cloexec(fd as u32, true));
    }
    Ok(fd)
}

/// `close` (3): освободить дескриптор или `EBADF`, если он не открыт (R2.6, R2.14).
pub fn sys_close(fd: u64) -> Result<u64, Errno> {
    let kind = fd_kind(fd);
    if kind == "inet-udp" {}
    if kind == "inet-tcp" {
        crate::arch::x86_64::linux::inet_sock::tcp_close_fd(fd);
    }
    let kind = fd_kind(fd);
    if kind.starts_with("pipe") {}
    // ЗАМЕЧАНИЕ: UDP-сокет здесь намеренно НЕ удаляется. Любой путь, берущий
    // NET.lock из обработчика syscall, рискует дедлоком: спинлоки выключают IF,
    // и сетевой поток не может выполниться, чтобы отпустить свою сторону. Слот
    // SocketSet освободится только при перезагрузке.
    // `/dev/vtconsole0`: закрытие последнего fd выключает публикацию shared
    // surface (уже взятые mmap-снимки остаются валидны до munmap).
    let closing_vtconsole = matches!(
        resolve_fd(fd as u32),
        Some(Resolved::File { ref node, .. }) if node.is_vtconsole()
    );
    let res = compat::with_current_compat(|cs| cs.fds.close(fd as u32));
    match res {
        Some(Ok(())) => {
            if closing_vtconsole {
                crate::drivers::vtsurface::deactivate();
            }
            Ok(0)
        }
        Some(Err(e)) => Err(e),
        None => Err(Errno::EBADF),
    }
}

/// `lseek` (8): переместить смещение дескриптора по `whence`/`offset`,
/// вернув новое абсолютное смещение (R2.7), либо `EINVAL` при плохом whence или
/// отрицательном результате (R2.15). Консоль/stdin не ищутся (`EINVAL`);
/// отсутствующий fd — `EBADF`.
pub fn sys_lseek(fd: u64, offset: u64, whence: u64) -> Result<u64, Errno> {
    match resolve_fd(fd as u32) {
        None => Err(Errno::EBADF),
        Some(Resolved::InetTcp(_)) | Some(Resolved::InetUdp(_)) => Err(Errno::ESPIPE),
        // Консоль не поддерживает позиционирование — ESPIPE, как pipe.
        // CPython при старте пробует lseek на fd 0/1/2, чтобы выбрать
        // буферизацию; EINVAL трижды засорял diag-лог на каждый старт.
        Some(Resolved::Console) | Some(Resolved::Stdin) => Err(Errno::ESPIPE),
        Some(Resolved::PipeRead(_)) | Some(Resolved::PipeWrite(_)) => Err(Errno::ESPIPE),
        Some(Resolved::Socket { .. }) => Err(Errno::ESPIPE),
        Some(Resolved::Eventfd { .. })
        | Some(Resolved::Epoll)
        | Some(Resolved::Timerfd(_))
        | Some(Resolved::Signalfd(_)) => Err(Errno::ESPIPE),
        // Дескриптор каталога поддерживает перемотку/позиционирование курсора
        // записей: SEEK_SET задаёт индекс курсора и возвращает его. Остальные
        // whence — EINVAL.
        Some(Resolved::Dir) => {
            if whence != super::io::SEEK_SET as u64 {
                return Err(Errno::EINVAL);
            }
            compat::with_current_compat(|cs| {
                if let Some(OpenObject::Dir { index, .. }) = cs.fds.get_mut(fd as u32) {
                    *index = offset as usize;
                }
            });
            Ok(offset)
        }
        Some(Resolved::File { node, .. }) if node.is_vtconsole() || node.is_vtinput() => {
            Err(Errno::ESPIPE)
        }
        Some(Resolved::File { node, offset: cur }) => {
            let size = node.size();
            let new_off = plan_lseek(whence as u32, cur, size, offset as i64)?;
            set_fd_offset(fd as u32, new_off);
            Ok(new_off)
        }
    }
}

/// `ftruncate` (77): изменить размер открытого regular-файла.
///
/// Нужен композитору для memfd: `memfd_create` даёт пустой файл, `ftruncate`
/// задаёт размер SHM-буфера, дальше `mmap(MAP_SHARED)` отображает страницы.
/// Дескриптор обязан быть открыт на запись; символьные устройства (`fb0`,
/// `event0`) не усекаются. Смещение fd Linux не меняет — так же и здесь.
pub fn sys_ftruncate(fd: u64, length: u64) -> Result<u64, Errno> {
    let acc = fd_open_flags(fd as u32).ok_or(Errno::EBADF)? & 0o3;
    if acc == 0 {
        return Err(Errno::EINVAL);
    }
    match resolve_fd(fd as u32) {
        Some(Resolved::File { node, .. }) => {
            if node.is_fbdev() || node.is_evdev() || node.is_vtconsole() || node.is_vtinput() {
                return Err(Errno::EINVAL);
            }
            node.truncate(length).map_err(truncate_errno)?;
            Ok(0)
        }
        Some(_) => Err(Errno::EINVAL),
        None => Err(Errno::EBADF),
    }
}

/// `truncate` (76): то же по пути (путь разыменовывается, как в `open`).
pub fn sys_truncate(path: u64, length: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    let node = walk(&abs, true)?;
    if node.is_directory() {
        return Err(Errno::EISDIR);
    }
    node.truncate(length).map_err(truncate_errno)?;
    Ok(0)
}

fn truncate_errno(e: crate::vfs::VfsError) -> Errno {
    match e {
        crate::vfs::VfsError::NotSupported | crate::vfs::VfsError::InvalidArgument => Errno::EINVAL,
        _ => Errno::EIO,
    }
}

/// `st_dev`, сообщаемый для всех файлов VFS (произвольный, но ненулевой; 8:0).
pub(crate) const STAT_DEV_VFS: u64 = 0x0800;

/// Ненулевой запасной inode для синтетических узлов без идентичности ФС:
/// FNV-1a от имени узла с установленным старшим битом, чтобы он никогда не
/// совпал с настоящим inode ext2 (те влезают в 32 бита) и не читался как ноль.
fn synth_ino(name: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        h = (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h | (1 << 63)
}

/// stat-идентичность inode узла: его ino из ФС, если есть, иначе синтетический
/// FNV-запасной. Это ключ индексации `mode_overrides`, поэтому он должен
/// совпадать с тем, что сообщают `write_stat`/`write_statx`.
/// `/proc/<pid>/fdinfo/<n>` использует его же, чтобы `ino:` дескриптора
/// совпадал с `stat`.
pub(crate) fn node_ino(node: &Arc<dyn VfsNode>) -> u64 {
    match node.fs_ino() {
        0 => synth_ino(node.name()),
        ino => ino,
    }
}

/// Переопределение битов прав для `ino` в текущем процессе (после `chmod` или
/// на момент создания), если оно есть.
fn mode_override_perms(ino: u64) -> Option<u32> {
    compat::with_current_compat(|cs| cs.mode_overrides.get(&ino).copied()).flatten()
}

// ─── проверки прав на границе syscall ───────────────────────────────────────
//
// Креденшелы и проверки прав живут ЗДЕСЬ, в слое syscall. VFS несёт только
// метаданные (`VfsNode::mode`/`owner`); провижининг загрузки, шелл и
// установщик пакетов по-прежнему зовут методы VFS напрямую, без креденшелов,
// и потому не затронуты — та же форма, что в Linux (VFS не знает вызывающего;
// `inode_permission()` выполняется в пути syscall).
//
// Всё ниже использует EFFECTIVE id процесса (см. `cred::Creds::may`).

/// Снимок креденшелов вызывающего процесса (`ROOT` для нативной задачи).
fn current_creds() -> Creds {
    compat::current_creds()
}

/// Может ли вызывающий обратиться к `node` для `access`? Иначе `EACCES`.
fn check_node_access(node: &Arc<dyn VfsNode>, access: Access) -> Result<(), Errno> {
    if current_creds().may(node.mode(), node.owner(), access) {
        Ok(())
    } else {
        Err(Errno::EACCES)
    }
}

/// Создание, удаление или переименование записи требуют write И execute на
/// родительском каталоге (POSIX). Root обходит write, но, как и в любом вызове
/// `may`, ему всё равно нужен execute-бит: каталог с правами `0644` не
/// просматривается даже root.
fn check_dir_mutation(dir: &Arc<dyn VfsNode>) -> Result<(), Errno> {
    let creds = current_creds();
    let mode = dir.mode();
    let owner = dir.owner();
    if creds.may(mode, owner, Access::Write) && creds.may(mode, owner, Access::Execute) {
        Ok(())
    } else {
        Err(Errno::EACCES)
    }
}

/// Какой доступ к существующему узлу требует уже открытый вызов `open(2)`.
///
/// `O_RDONLY` требует read; `O_WRONLY` — write; `O_RDWR` — оба; `O_TRUNC`
/// дополнительно требует write (Linux проверяет право на усечение отдельно:
/// read-only дескриптор с усечением — всё равно запись).
fn check_open_mode(node: &Arc<dyn VfsNode>, flags: u64) -> Result<(), Errno> {
    let acc = flags & 0o3;
    let creds = current_creds();
    let mode = node.mode();
    let owner = node.owner();
    let read_ok = || creds.may(mode, owner, Access::Read);
    let write_ok = || creds.may(mode, owner, Access::Write);
    match acc {
        0 => {
            if read_ok() {
                Ok(())
            } else {
                Err(Errno::EACCES)
            }
        }
        1 => {
            if write_ok() {
                Ok(())
            } else {
                Err(Errno::EACCES)
            }
        }
        2 => {
            if read_ok() && write_ok() {
                Ok(())
            } else {
                Err(Errno::EACCES)
            }
        }
        _ => Err(Errno::EINVAL),
    }
    .and_then(|()| {
        if flags & O_TRUNC_FL != 0 && !write_ok() {
            Err(Errno::EACCES)
        } else {
            Ok(())
        }
    })
}

/// Сохранить `perms` на `node` и отразить их в stat-переопределении процесса.
///
/// ext2 пишет inode. Синтетические узлы (`/proc`, `/dev`, ramfs) не имеют хранилища
/// метаданных и возвращают `NotSupported`; для них историческое переопределение
/// остаётся единственной записью, поэтому вызывающий в любом случае продолжает
/// звать [`record_mode_override`].
fn apply_node_mode(node: &Arc<dyn VfsNode>, perms: u32) -> Result<(), Errno> {
    match node.set_mode(perms) {
        Ok(()) | Err(crate::vfs::VfsError::NotSupported) => {
            record_mode_override(node_ino(node), perms);
            Ok(())
        }
        Err(_) => Err(Errno::EIO),
    }
}

/// Проставить новому узлу effective uid/egid создателя.
///
/// Писатель ext2 не имеет креденшелов и ставит `(0, 0)`; граница syscall — то
/// место, где известна личность вызывающего, поэтому не-root создатель должен
/// стать владельцем здесь, иначе он никогда не сможет сделать `chmod` только что
/// созданного файла. `(0, 0)` root — это уже то, что пишет писатель, поэтому
/// лишняя журналируемая транзакция для identity по умолчанию пропускается.
/// Синтетические узлы не хранят владельца (`NotSupported`); это документированное
/// ограничение ramfs/procfs.
fn stamp_creation_owner(node: &Arc<dyn VfsNode>) {
    let creds = current_creds();
    if creds.euid != 0 || creds.egid != 0 {
        let _ = node.set_owner(creds.euid, creds.egid);
    }
}

/// Проверка root-или-владелец, общая для `chmod`/`fchmod`.
fn check_chmod_allowed(node: &Arc<dyn VfsNode>) -> Result<(), Errno> {
    if current_creds().may_chmod(node.owner()) {
        Ok(())
    } else {
        Err(Errno::EPERM)
    }
}

/// Проверка только-root, общая для `chown`/`fchown`.
fn check_chown_allowed() -> Result<(), Errno> {
    if current_creds().may_chown() {
        Ok(())
    } else {
        Err(Errno::EPERM)
    }
}

/// Разрешить пару `(uid, gid)` для `chown(2)`: `(u32)-1` означает «оставить
/// текущий». ext2 хранит оба как `u16`, поэтому id вне диапазона — `EINVAL`,
/// а не молчаливое усечение.
fn resolve_chown_ids(node: &Arc<dyn VfsNode>, uid: u64, gid: u64) -> Result<(u32, u32), Errno> {
    let (cur_uid, cur_gid) = node.owner();
    let new_uid = optional_owner_id(uid, cur_uid);
    let new_gid = optional_owner_id(gid, cur_gid);
    if new_uid > u16::MAX as u32 || new_gid > u16::MAX as u32 {
        return Err(Errno::EINVAL);
    }
    Ok((new_uid, new_gid))
}

#[inline]
fn optional_owner_id(raw: u64, current: u32) -> u32 {
    match raw as u32 {
        u32::MAX => current,
        v => v,
    }
}

/// Применить `chown` к `node` или вернуть `EPERM`, если у узла нет постоянного владельца.
fn apply_node_owner(node: &Arc<dyn VfsNode>, uid: u64, gid: u64) -> Result<(), Errno> {
    let (new_uid, new_gid) = resolve_chown_ids(node, uid, gid)?;
    match node.set_owner(new_uid, new_gid) {
        Ok(()) => Ok(()),
        // Синтетические узлы не хранят владельца; сообщить успех было бы ложью
        // (последующий `stat`/проверка прав всё равно увидели бы `(0, 0)`).
        Err(crate::vfs::VfsError::NotSupported) => Err(Errno::EPERM),
        Err(crate::vfs::VfsError::InvalidArgument) => Err(Errno::EINVAL),
        Err(_) => Err(Errno::EIO),
    }
}

/// Текущий umask процесса (022 для нативных задач без compat-состояния).
fn current_umask() -> u32 {
    compat::with_current_compat(|cs| cs.umask).unwrap_or(0o022)
}

/// Записать `perms` (биты 0o7777) для `ino` в переопределения прав текущего процесса.
fn record_mode_override(ino: u64, perms: u32) {
    compat::with_current_compat(|cs| {
        cs.mode_overrides.insert(ino, perms);
    });
}

/// Биты прав `st_mode` для `node`: переопределение процесса, если оно записано,
/// иначе давние значения по умолчанию (0755 файлы / 0700 каталоги — всё работает
/// под root, а установленным из apt программам нужны X-биты).
fn stat_mode_for(node: &Arc<dyn VfsNode>) -> u32 {
    // Переопределение хранится для синтетических узлов (`/proc`, `/dev`, ramfs),
    // которые не могут сохранить режим; реальные файлы отвечают из своего inode,
    // который используют и проверки прав, так что stat больше не может
    // расходиться с access.
    let perms = match mode_override_perms(node_ino(node)) {
        Some(p) => p,
        None => node.mode() & 0o7777,
    };
    if node.is_directory() {
        S_IFDIR | perms
    } else if node.is_symlink() {
        S_IFLNK | perms
    } else if node.is_fbdev() || node.is_evdev() || node.is_vtconsole() || node.is_vtinput() {
        // Символьные устройства варианта B: композитор видит S_IFCHR, как
        // `/dev/fb0`/`/dev/input/event0` в Linux, и может отличить их от
        // regular-файла до open.
        S_IFCHR | perms
    } else {
        S_IFREG | perms
    }
}

/// Заполнить [`LinuxStat`] для узла и скопировать его в проверенный буфер пользователя.
fn write_stat(node: &Arc<dyn VfsNode>, statbuf: u64) -> Result<u64, Errno> {
    let mode = stat_mode_for(node);
    let (uid, gid) = node.owner();
    let mut stat = encode_stat(node.size(), mode, uid, gid);
    // Идентичность файла важна. ld.so из glibc дедуплицирует загруженные
    // разделяемые объекты по паре (st_dev, st_ino) из `fstat`, а link_map
    // главного исполняемого файла несёт полностью нулевой file id. Пока
    // st_dev/st_ino оставались нулевыми для всех файлов, каждая заново открытая
    // библиотека «совпадала» с главным бинарём, загрузчик переиспользовал её
    // link_map вместо отображения libc, и запуск умирал с потоком «no version
    // information available», а затем undefined `__libc_start_main, version
    // GLIBC_2.34` (exit 127).
    stat.st_dev = STAT_DEV_VFS;
    stat.st_ino = node_ino(node);
    // Жёсткие ссылки: ext2 сообщает `i_links_count`, поэтому два имени одного
    // inode выглядят одним файлом. Синтетические узлы по умолчанию 1.
    stat.st_nlink = node.nlink();
    stat.st_blocks = (stat.st_size + 511) / 512;
    write_stat_struct(&stat, statbuf)?;
    Ok(0)
}

/// Заполнить [`LinuxStat`], описывающий саму символическую ссылку (`lstat`).
///
/// `st_size` — длина цели, `st_nlink` — счётчик ссылок inode, как Linux
/// сообщает для симлинка; `st_blocks` равен 0, потому что только *медленная*
/// ссылка ext2 (цель ≥ 60 байт) владеет блоком данных, а точность уровня `du`
/// здесь не нужна (описано в `src/vfs/README.md`).
fn write_link_stat(node: &Arc<dyn VfsNode>, statbuf: u64) -> Result<(), Errno> {
    let len = node.read_link().map(|t| t.len() as u64).unwrap_or(0);
    let (uid, gid) = node.owner();
    let mut stat = encode_stat(len, S_IFLNK | 0o777, uid, gid);
    stat.st_dev = STAT_DEV_VFS;
    stat.st_ino = node_ino(node);
    stat.st_nlink = node.nlink();
    stat.st_blocks = 0;
    write_stat_struct(&stat, statbuf)
}

/// Скопировать полностью собранный [`LinuxStat`] в проверенный буфер пользователя.
fn write_stat_struct(stat: &LinuxStat, statbuf: u64) -> Result<(), Errno> {
    let bytes = unsafe {
        core::slice::from_raw_parts(
            stat as *const LinuxStat as *const u8,
            core::mem::size_of::<LinuxStat>(),
        )
    };
    copy_out(statbuf, bytes)
}

/// `stat` (4): статус файла по пути в пользовательский `struct stat`.
///
/// Старый (до `newfstatat`) номер x86_64, который всё ещё зовут статические
/// musl-бинари: Rust-std `Command::spawn`/`Path::exists` внутри
/// `pagh-desktop` упирались в него и получали `ENOSYS`, из-за чего запуск
/// дочернего процесса (`--spawn-demo`, панель Files/Terminal) падал с
/// `Function not implemented (os error 38)`.
///
/// Семантика — как у Linux `stat`: конечная символическая ссылка
/// разыменовывается (в отличие от `lstat`, то есть `newfstatat` с
/// `AT_SYMLINK_NOFOLLOW`), относительный путь разрешается от cwd, а разметка
/// результата — тот же 144-байтный [`LinuxStat`], что у `fstat`/`newfstatat`
/// (x86_64 `struct stat`). Поэтому обработчик — тонкая обёртка над проверенной
/// обвязкой `newfstatat`: одна реализация поиска в VFS, одна проверка буфера
/// (`EFAULT` до записи) и один набор `ENOENT`/`ENOTDIR`/`ELOOP`.
pub fn sys_stat(path: u64, statbuf: u64) -> Result<u64, Errno> {
    sys_newfstatat(AT_FDCWD, path, statbuf, 0)
}

/// `fstat` (5): заполнить пользовательский `struct stat` для открытого
/// дескриптора (R2.8). `EBADF` для отсутствующего fd (R2.14). Консоль/stdin
/// сообщают символьное устройство.
pub fn sys_fstat(fd: u64, statbuf: u64) -> Result<u64, Errno> {
    check_user_ptr_writable(statbuf, core::mem::size_of::<LinuxStat>() as u64)?;
    match resolve_fd(fd as u32) {
        None => Err(Errno::EBADF),
        Some(Resolved::InetTcp(_)) | Some(Resolved::InetUdp(_)) => {
            // S_IFSOCK с режимом вроде 0644; libuv спрашивает только тип.
            write_statx_sock(statbuf)
        }
        Some(Resolved::Socket { .. }) => {
            // Концы socketpair сообщают S_IFSOCK — uv_guess_handle из libuv
            // проверяет тип stdio-дескрипторов.
            let stat = encode_stat(0, S_IFSOCK | 0o666, 0, 0);
            write_stat_struct(&stat, statbuf)?;
            Ok(0)
        }
        Some(Resolved::Console)
        | Some(Resolved::Stdin)
        | Some(Resolved::PipeRead(_))
        | Some(Resolved::PipeWrite(_))
        | Some(Resolved::Eventfd { .. })
        | Some(Resolved::Timerfd(_))
        | Some(Resolved::Signalfd(_))
        | Some(Resolved::Epoll) => {
            let stat = encode_stat(0, S_IFCHR | 0o620, 0, 0);
            write_stat_struct(&stat, statbuf)?;
            Ok(0)
        }
        Some(Resolved::Dir) => {
            let stat = encode_stat(0, S_IFDIR | 0o700, 0, 0);
            write_stat_struct(&stat, statbuf)?;
            Ok(0)
        }
        Some(Resolved::File { node, .. }) => write_stat(&node, statbuf),
    }
}

/// `newfstatat` (262): stat пути (абсолютного или относительного `AT_FDCWD`) в
/// пользовательский `struct stat` (R2.8); `ENOENT`, если пути нет (R2.5).
pub fn sys_newfstatat(_dirfd: u64, path: u64, statbuf: u64, flags: u64) -> Result<u64, Errno> {
    check_user_ptr_writable(statbuf, core::mem::size_of::<LinuxStat>() as u64)?;
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    if flags & AT_SYMLINK_NOFOLLOW != 0 {
        // `lstat`: описать саму ссылку, а не то, куда она указывает
        // (промежуточные компоненты обходчик всё равно разыменовывает).
        let node = walk(&abs, false)?;
        if node.is_symlink() {
            write_link_stat(&node, statbuf)?;
            return Ok(0);
        }
        return write_stat(&node, statbuf);
    }
    // `stat`: разыменовать конечную ссылку (висячая — ENOENT, цикл — ELOOP).
    let node = walk(&abs, true)?;
    write_stat(&node, statbuf)
}

/// `ioctl` (16): консоль/stdin отвечают на базовые tty-запросы (`TCGETS`,
/// `TCSETS*`, `TIOCGWINSZ`), чтобы `isatty()` видел терминал. Повальный
/// `EINVAL` заставлял CPython считать stdin не-tty pipe, читать мгновенный EOF и
/// завершаться с 0, так и не показав приглашение. Остальные дескрипторы
/// по-прежнему получают `EINVAL`; отсутствующий fd — `EBADF`.
pub fn sys_ioctl(fd: u64, request: u64, arg: u64) -> Result<u64, Errno> {
    // FIOCLEX/FIONCLEX ставят/снимают у fd флаг close-on-exec. Здесь ioctl
    // принимается как успех на любом валидном fd, а не как EINVAL:
    // `_Py_set_inheritable()` из CPython выдаёт ioctl(fd, FIOCLEX) для каждого
    // файла модуля, который открывает importlib, и считает любую errno кроме
    // ENOTTY/EACCES фатальной ошибкой I/O — это был OSError(Errno 22) из
    // get_data, обрывавший запуск на «Failed to import encodings module».
    const FIONCLEX: u64 = 0x5450;
    const FIOCLEX: u64 = 0x5451;
    if request == FIOCLEX || request == FIONCLEX {
        return match resolve_fd(fd as u32) {
            None => Err(Errno::EBADF),
            Some(_) => Ok(0),
        };
    }
    // FIONBIO. В libuv на Linux uv__nonblock — это ioctl(FIONBIO), а НЕ
    // fcntl(F_SETFL). uv_pipe_open() вызывает его на dup-конце socketpair
    // RPC-канала встроенного nvim; ответ ENOTTY ломал pipe_open, канал не
    // доходил до event loop (зависание с чёрным экраном). Зеркалит ветку
    // F_SETFL O_NONBLOCK ниже.
    const FIONBIO: u64 = 0x5421;
    if request == FIONBIO {
        check_user_ptr(arg, 4)?;
        let on = unsafe { core::ptr::read_unaligned(arg as *const u32) } != 0;

        return compat::with_current_compat(|cs| {
            let obj = cs.fds.get_mut(fd as u32).ok_or(Errno::EBADF)?;
            match obj {
                OpenObject::PipeRead(e) => *e = e.with_nonblocking(on),
                OpenObject::PipeWrite(e) => *e = e.with_nonblocking(on),
                OpenObject::Socket { rx, tx } => {
                    let nrx = rx.with_nonblocking(on);
                    let ntx = tx.with_nonblocking(on);
                    *rx = nrx;
                    *tx = ntx;
                }
                OpenObject::UnixListener(l) => l.inner.lock().nonblocking = on,
                OpenObject::UnixSocketUnbound { nonblocking } => *nonblocking = on,
                OpenObject::Stdin => {
                    STDIN_NONBLOCK.store(on, core::sync::atomic::Ordering::Relaxed);
                }
                _ => {}
            }
            cs.fds.set_nonblocking_flag(fd as u32, on);
            Ok(0)
        })
        .unwrap_or(Err(Errno::EBADF));
    }
    match resolve_fd(fd as u32) {
        None => Err(Errno::EBADF),
        Some(Resolved::File { node, .. }) if node.is_fbdev() => fb_ioctl(&node, request, arg),
        Some(Resolved::File { node, .. }) if node.is_vtconsole() => {
            vtconsole_ioctl(&node, request, arg)
        }
        Some(Resolved::Console) | Some(Resolved::Stdin) => tty_ioctl(request, arg),
        // tty-запрос на не-tty дескриптор — «неподходящий ioctl для
        // устройства», а не «неверный аргумент». isatty() из glibc ждёт ENOTTY;
        // EINVAL ещё и зажигал diag на каждой пробе isatty у CPython (TCGETS на
        // fd 3) и заливал serial-лог.
        Some(_) => {
            // Пробы tty (isatty и компании) на не-tty fd — рутинный шум;
            // любой ДРУГОЙ неизвестный запрос — пробел compat-поверхности,
            // который стоит видеть на экране.
            if !(0x5401..=0x5420).contains(&request) {}
            Err(Errno::ENOTTY)
        }
    }
}

// ─── поверхность tty ────────────────────────────────────────────────────────

/// `ioctl(/dev/fb0)`: `FBIOGET_VSCREENINFO`/`FBIOGET_FSCREENINFO`.
///
/// Это единственная часть fbdev UAPI, которая нужна композитору: геометрия,
/// stride, биты на пиксель и физическая база scanout. `FBIOPUT_*`/`FBIOBLANK`
/// не реализованы и честно отвечают `ENOTTY` — притворяться, что режим
/// переключён, значило бы отдать композитору неверный stride.
fn fb_ioctl(node: &Arc<dyn VfsNode>, request: u64, arg: u64) -> Result<u64, Errno> {
    use crate::drivers::fb_abi::{
        encode_fix, encode_var, FbBitfield, FixScreenInfo, VarScreenInfo, FBIOGET_FSCREENINFO,
        FBIOGET_VSCREENINFO, FB_FIX_SIZE, FB_VAR_SIZE, FB_VISUAL_TRUECOLOR,
    };
    let info = node.fb_info().ok_or(Errno::ENODEV)?;
    match request {
        FBIOGET_VSCREENINFO => {
            let var = VarScreenInfo {
                xres: info.width,
                yres: info.height,
                xres_virtual: info.width,
                yres_virtual: info.height,
                xoffset: 0,
                yoffset: 0,
                bits_per_pixel: info.bpp,
                grayscale: 0,
                red: FbBitfield {
                    offset: info.red_offset,
                    length: info.red_length,
                },
                green: FbBitfield {
                    offset: info.green_offset,
                    length: info.green_length,
                },
                blue: FbBitfield {
                    offset: info.blue_offset,
                    length: info.blue_length,
                },
                transp: FbBitfield {
                    offset: info.transp_offset,
                    length: info.transp_length,
                },
                ..Default::default()
            };
            check_user_ptr_writable(arg, FB_VAR_SIZE as u64)?;
            copy_out(arg, &encode_var(&var))?;
            Ok(0)
        }
        FBIOGET_FSCREENINFO => {
            let fix = FixScreenInfo {
                id: crate::drivers::fb_abi::padded_id(b"paghfb"),
                smem_start: info.phys,
                smem_len: core::cmp::min(info.smem_len, u32::MAX as u64) as u32,
                // FB_TYPE_PACKED_PIXELS
                type_: 0,
                type_aux: 0,
                visual: FB_VISUAL_TRUECOLOR,
                line_length: info.pitch,
            };
            check_user_ptr_writable(arg, FB_FIX_SIZE as u64)?;
            copy_out(arg, &encode_fix(&fix))?;
            Ok(0)
        }
        _ => Err(Errno::ENOTTY),
    }
}

/// `ioctl(/dev/vtconsole0)`: `PGVT_IOC_INFO` (ABI v1).
///
/// Отдаёт геометрию и `total_bytes`, чтобы клиент мог отобразить/промапить
/// поверхность, не угадывая размер. Прочие запросы — `ENOTTY`; режим/размер
/// меняет только ядро.
fn vtconsole_ioctl(node: &Arc<dyn VfsNode>, request: u64, arg: u64) -> Result<u64, Errno> {
    use crate::drivers::vt_abi::{encode_info, PgvtInfo, PGVT_IOC_INFO};
    let info = node.vtconsole_info().ok_or(Errno::ENODEV)?;
    if request != PGVT_IOC_INFO {
        return Err(Errno::ENOTTY);
    }
    let out = PgvtInfo {
        abi_version: 1,
        total_bytes: info.total_bytes,
        cols: info.cols as u32,
        rows: info.rows as u32,
        cell_size: 16,
        header_len: 128,
        reserved: [0, 0],
    };
    check_user_ptr_writable(arg, 32)?;
    copy_out(arg, &encode_info(&out))?;
    Ok(0)
}

/// `TCGETS`: прочитать атрибуты терминала (`struct termios`).
const TCGETS: u64 = 0x5401;
/// `TCSETS` / `TCSETSW` / `TCSETSF`: задать атрибуты терминала (принимаются, игнорируются).
const TCSETS: u64 = 0x5402;
const TCSETSW: u64 = 0x5403;
const TCSETSF: u64 = 0x5404;
/// `TIOCGWINSZ`: прочитать размер окна (`struct winsize`).
const TIOCGWINSZ: u64 = 0x5413;
const TIOCSWINSZ: u64 = 0x5414;

/// Размер Linux `struct termios` в байтах (4×u32 + c_line + c_cc[19]).
const TERMIOS_SIZE: usize = 36;

/// Собрать правдоподобный образ `struct termios` в cooked-режиме: ICRNL|IXON,
/// OPOST|ONLCR, B38400|CS8|CREAD, ISIG|ICANON|ECHO|ECHOE|ECHOK|IEXTEN плюс
/// стандартные управляющие символы (^C, ^\, DEL, ^U, ^D как VEOF, ...).
///
/// Три бита, которые ядро действительно реализует — `ICANON` и `ECHO`
/// (на процесс, ставятся `TCSETS`) и `TOSTOP` (политика фоновой записи на весь
/// терминал, см. `drivers::tty`) — сообщаются из живого состояния. Программа,
/// которая сохраняет атрибуты, меняет их и восстанавливает, должна получить
/// обратно то, что поставила, а не жёстко зашитый образ.
fn build_termios() -> [u8; TERMIOS_SIZE] {
    let mut t = [0u8; TERMIOS_SIZE];
    let c_iflag: u32 = 0x0500; // ICRNL | IXON
    let c_oflag: u32 = 0x0005; // OPOST | ONLCR
    let c_cflag: u32 = 0x00BF; // B38400 | CS8 | CREAD
    let (raw_now, echo_now) = crate::task::compat::with_current_compat(|cs| (cs.raw_mode, cs.echo))
        .unwrap_or((false, true));
    let mut c_lflag: u32 = 0x8A3B; // ISIG|ICANON|ECHO|ECHOE|ECHOK|ECHOCTL|ECHOKE|IEXTEN
    if raw_now {
        c_lflag &= !0x0002u32;
    } // ICANON
    if !echo_now {
        c_lflag &= !0x0008u32;
    } // ECHO
    if crate::drivers::tty::tostop() {
        c_lflag |= TOSTOP;
    }
    t[0..4].copy_from_slice(&c_iflag.to_le_bytes());
    t[4..8].copy_from_slice(&c_oflag.to_le_bytes());
    t[8..12].copy_from_slice(&c_cflag.to_le_bytes());
    t[12..16].copy_from_slice(&c_lflag.to_le_bytes());
    t[16] = 0; // c_line
               // c_cc: VINTR VQUIT VERASE VKILL VEOF VTIME VMIN VSWTC VSTART VSTOP VSUSP
               //       VEOL VREPRINT VDISCARD VWERASE VLNEXT VEOL2 (остальное — нули)
    let c_cc: [u8; 19] = [
        3, 28, 127, 21, 4, 0, 1, 0, 17, 19, 26, 0, 18, 15, 23, 22, 0, 0, 0,
    ];
    t[17..36].copy_from_slice(&c_cc);
    t
}

/// ioctl консоли/stdin: ровно столько tty-поверхности, чтобы `isatty()` сказал
/// «да» (glibc выдаёт `TCGETS`), а программы узнали окно 80×25.
fn tty_ioctl(request: u64, arg: u64) -> Result<u64, Errno> {
    match request {
        TCGETS => {
            check_user_ptr_writable(arg, TERMIOS_SIZE as u64)?;
            copy_out(arg, &build_termios())?;
            Ok(0)
        }
        // Если ICANON снят, переключаемся в raw-режим, чтобы чтения
        // возвращали по одному байту (это нужно nvim/less/vim).
        TCSETS | TCSETSW | TCSETSF => {
            if arg != 0 {
                // Проверка указателя обязательна: если проигнорировать её
                // результат и всё равно прочитать `c_lflag`, плохой указатель
                // будет разыменован из ring 0 — класс kernel #GP/#PF с
                // остановкой машины через непроверенный ioctl.
                check_user_ptr(arg, TERMIOS_SIZE as u64)?;
                // c_lflag лежит по смещению 12; ICANON = 0x02
                let c_lflag = unsafe { core::ptr::read_unaligned((arg + 12) as *const u32) };
                let raw = c_lflag & 0x02 == 0; // ICANON снят => raw-режим
                                               // Отслеживать и ECHO (0x08), чтобы raw-чтения знали,
                                               // должен ли сам kernel эхо-печатать введённые символы.
                let echo = c_lflag & 0x08 != 0;
                crate::task::compat::with_current_compat(|cs| {
                    if cs.raw_mode != raw || cs.echo != echo {}
                    cs.raw_mode = raw;
                    cs.echo = echo;
                });
                // TOSTOP (0x100) — состояние ТЕРМИНАЛА, а не процесса: он решает,
                // останавливает ли запись фонового job (drivers::tty).
                crate::drivers::tty::set_tostop(c_lflag & TOSTOP != 0);
            }
            Ok(0)
        }
        TIOCGWINSZ => {
            check_user_ptr_writable(arg, 8)?;
            let (cols, rows) = crate::drivers::vt::dimensions();
            // struct winsize { ws_row: u16, ws_col: u16, ws_xpixel: u16, ws_ypixel: u16 }
            let mut ws = [0u8; 8];
            ws[0..2].copy_from_slice(&rows.to_le_bytes());
            ws[2..4].copy_from_slice(&cols.to_le_bytes());
            copy_out(arg, &ws)?;
            Ok(0)
        }
        // Запись размера окна принимается и игнорируется (readline шлёт
        // TIOCSWINSZ при старте, распространяя вычисленный размер).
        TIOCSWINSZ => Ok(0),
        // Управление заданиями терминала: сообщить/сменить foreground process
        // group терминала. Раньше оба запроса отвечали про ВЫЗЫВАЮЩЕГО (сообщить
        // вызывающего, принять любую смену молча), поэтому `tcgetpgrp` врал
        // каждому фоновому job, а bash при каждом старте печатал «cannot set
        // terminal process group (-1)» + «no job control in this shell».
        TIOCGPGRP => {
            check_user_ptr_writable(arg, 4)?;
            let pg = crate::drivers::tty::foreground_pgid() as u32;
            copy_out(arg, &pg.to_le_bytes())?;
            Ok(0)
        }
        TIOCSPGRP => {
            // Аргумент — `int *` (tcsetpgrp из glibc передаёт &pgrp): 32-битное
            // слово знаковое, поэтому отрицательный pgid — EINVAL (tiocspgrp в
            // Linux отвергает `pgrp < 0` до любого поиска). Сессий здесь нет,
            // поэтому `tty::may_set_foreground` аппроксимирует проверку сессии
            // Linux правилом «группа должна быть живой, либо текущей/шелловой
            // группой терминала», иначе возвращает EPERM.
            check_user_ptr(arg, 4)?;
            // SAFETY: проверено выше; чтение невыровненное, потому что
            // пользовательский указатель не даёт гарантии выравнивания.
            let raw = unsafe { core::ptr::read_unaligned(arg as *const u32) };
            let target = super::tty::decode_pgrp_arg(raw)?;
            let live = !compat::group_pids(target).is_empty();
            super::tty::may_set_foreground(
                target,
                live,
                crate::drivers::tty::shell_pgid(),
                crate::drivers::tty::foreground_pgid(),
            )?;
            crate::drivers::tty::set_foreground_pgid(target);
            Ok(0)
        }
        _ => Err(Errno::ENOTTY),
    }
}

// Байты cooked-строки, которые предыдущий read(2) не потребил —
// readline вычерпывает ввод по одному байту за read после select().
static STDIN_PENDING: crate::sync::spinlock::Spinlock<alloc::vec::Vec<u8>> =
    crate::sync::spinlock::Spinlock::new(alloc::vec::Vec::new());

/// Положить байты в stdin текущего foreground-процесса (`/dev/vtinput` →
/// Linux-программа). Ограничение 4 КиБ: медленный читатель не должен дать
/// userspace бесконечно растить kernel-heap.
pub fn push_stdin_bytes(bytes: &[u8]) {
    const MAX_PENDING: usize = 4096;
    let mut q = STDIN_PENDING.lock();
    let room = MAX_PENDING.saturating_sub(q.len());
    if room != 0 {
        q.extend_from_slice(&bytes[..core::cmp::min(bytes.len(), room)]);
    }
}

// Libuv переводит сырой tty в O_NONBLOCK и мультиплексирует его через epoll;
// блокирующее чтение в ядре останавливает весь TUI event loop nvim.
static STDIN_NONBLOCK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Истина, когда raw-чтение stdin может продвинуться прямо сейчас: остались
/// байты, в очереди есть ответы на VT-запросы или неразобранный сканкод клавиатуры.
pub(crate) fn stdin_input_available() -> bool {
    !STDIN_PENDING.lock().is_empty()
        || crate::drivers::vt::has_input_responses()
        || crate::drivers::ps2_kbd::has_pending()
}

// Телеметрия чёрного экрана. Считает байты через три сегмента конвейера
// (stdout клиента → VT, чтения/записи RPC socketpair) и логирует первый байт и
// затем каждый `step`-й, чтобы один взгляд на экран показывал, ГДЕ встал
// конвейер nvim.
static DIAG_CONSOLE_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static DIAG_SOCK_R: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static DIAG_SOCK_W: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static DIAG_STDIN_EAGAIN: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

fn diag_count(ctr: &core::sync::atomic::AtomicU64, label: &str, n: u64, step: u64) {
    let old = ctr.fetch_add(n, core::sync::atomic::Ordering::Relaxed);
    let new = old + n;
    if old == 0 || old / step != new / step {}
}
/// Жёсткий предел, чтобы патологический paste не растил cooked-строку безгранично.
const STDIN_LINE_MAX: usize = 4096;

/// Блокирующее построчное чтение stdin с PS/2-клавиатуры.
///
/// Семантика cooked-tty: печатные символы эхом выводятся по мере ввода,
/// Backspace стирает, Tab разворачивается в четыре пробела (один байт на
/// отрисованную колонку упрощает учёт стирания), Enter завершает строку
/// (возвращается вместе с завершающим `\n`), Ctrl+D на пустой строке даёт EOF
/// (`Ok(0)`). Блокировка — через уступку планировщику между опросами, поэтому
/// foreground-ожидание шелла и таймер работают, пока бинарь сидит в `read`.

// В raw-режиме (ICANON снят) немедленно вернуть один байт;
// спецклавиши отображаются в ANSI-последовательности, чтобы терминальный слой
// nvim работал.
fn read_stdin_raw(buf: u64, count: u64) -> Result<u64, Errno> {
    use crate::shell::keys::{Decoder, KeyEvent};
    if count == 0 {
        return Ok(0);
    }
    // Поставить в очередь накопившиеся ответы на VT-запросы (DA1/DSR/DECRQM/OSC),
    // чтобы терминальный опрос nvim получил ответы через stdin.
    {
        let resp = crate::drivers::vt::take_input_responses();
        if !resp.is_empty() {
            STDIN_PENDING.lock().extend_from_slice(&resp);
        }
    }
    // Сначала вычерпать накопившиеся байты (raw-режим тоже может их оставлять).
    {
        let mut pending = STDIN_PENDING.lock();
        if !pending.is_empty() {
            let n = core::cmp::min(count as usize, pending.len());
            let head: alloc::vec::Vec<u8> = pending.drain(..n).collect();
            copy_out_checked(buf, &head)?;
            return Ok(n as u64);
        }
    }
    let mut decoder = Decoder::new();
    // После первого потреблённого сканкода подождать ограниченное число
    // опросов, чтобы завершились многобайтные последовательности (стрелки с
    // префиксом E0), но одиночное отпускание клавиши не парковало
    // неблокирующего читателя навсегда.
    let mut consumed_any = false;
    let mut spins = 0u32;
    loop {
        let sc = crate::drivers::get_char("keyboard").and_then(|kbd| kbd.read_char());
        let sc = match sc {
            Some(b) => b,
            None => {
                // Доставимый сигнал (pending ∧ ¬blocked) превращает блокирующее
                // чтение в -EINTR; эпилог диспетчеризации доставит его сразу
                // после возврата из обработчика (обработчик или завершение по
                // умолчанию).
                if super::signal::has_deliverable_current() {
                    return Err(Errno::EINTR);
                }
                // Соблюдать O_NONBLOCK: блокировка здесь останавливала TUI-цикл
                // nvim ещё до отрисовки первого кадра.
                if STDIN_NONBLOCK.load(core::sync::atomic::Ordering::Relaxed)
                    && (!consumed_any || spins > 200)
                {
                    if !DIAG_STDIN_EAGAIN.swap(true, core::sync::atomic::Ordering::Relaxed) {}
                    return Err(Errno::EAGAIN);
                }
                spins += 1;
                crate::task::scheduler::yield_current();
                continue;
            }
        };
        consumed_any = true;
        let ev = match decoder.feed(sc) {
            Some(ev) => ev,
            None => continue,
        };
        // Кодировать KeyEvent в байты (ANSI-последовательности для спецклавиш)
        let bytes: alloc::vec::Vec<u8> = match ev {
            KeyEvent::Char(c) => {
                let mut tmp = [0u8; 4];
                alloc::vec::Vec::from(c.encode_utf8(&mut tmp).as_bytes())
            }
            KeyEvent::Enter => alloc::vec![b'\r'],
            KeyEvent::Backspace => alloc::vec![0x7F], // DEL
            KeyEvent::Escape => alloc::vec![0x1B],
            KeyEvent::Tab => alloc::vec![b'\t'],
            KeyEvent::Up => alloc::vec![0x1B, b'[', b'A'],
            KeyEvent::Down => alloc::vec![0x1B, b'[', b'B'],
            KeyEvent::Right => alloc::vec![0x1B, b'[', b'C'],
            KeyEvent::Left => alloc::vec![0x1B, b'[', b'D'],
            KeyEvent::Home => alloc::vec![0x1B, b'[', b'H'],
            KeyEvent::End => alloc::vec![0x1B, b'[', b'F'],
            KeyEvent::PageUp => alloc::vec![0x1B, b'[', b'5', b'~'],
            KeyEvent::PageDown => alloc::vec![0x1B, b'[', b'6', b'~'],
            KeyEvent::Delete => alloc::vec![0x1B, b'[', b'3', b'~'],
            KeyEvent::Ctrl(c) => alloc::vec![(c as u8) & 0x1F],
        };
        if bytes.is_empty() {
            continue;
        }
        // Настоящий tty продолжает эхо и в raw-режиме, пока не снят ECHO.
        // readline bash работает здесь без базы terminfo, поэтому сама НЕ
        // перерисовывает строку ввода и полагалась на это эхо ядра — введённые
        // символы появлялись только когда Enter наконец сбрасывал строку. nvim
        // и полный readline снимают ECHO и потому не могут задвоить эхо.
        // Escape-последовательности (0x1B...) не эхо-печатаются никогда.
        if crate::task::compat::with_current_compat(|cs| cs.echo).unwrap_or(false) {
            match bytes[0] {
                b'\r' | b'\n' => console_write(b"\r\n"),
                0x7F | 0x08 => console_write(b"\x08 \x08"),
                b if b >= 0x20 => console_write(&bytes),
                _ => {}
            }
        }
        let n = core::cmp::min(count as usize, bytes.len());
        copy_out_checked(buf, &bytes[..n])?;
        if n < bytes.len() {
            STDIN_PENDING.lock().extend_from_slice(&bytes[n..]);
        }
        return Ok(n as u64);
    }
}

fn read_stdin_line(buf: u64, count: u64) -> Result<u64, Errno> {
    use crate::shell::keys::{Decoder, KeyEvent};
    if count == 0 {
        return Ok(0);
    }
    {
        let mut pending = STDIN_PENDING.lock();
        if !pending.is_empty() {
            let n = core::cmp::min(count as usize, pending.len());
            let head: alloc::vec::Vec<u8> = pending.drain(..n).collect();
            copy_out_checked(buf, &head)?;
            return Ok(n as u64);
        }
    }
    let mut decoder = Decoder::new();
    let mut line: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    loop {
        let sc = crate::drivers::get_char("keyboard").and_then(|kbd| kbd.read_char());
        let sc = match sc {
            Some(b) => b,
            None => {
                // Доставимый сигнал → -EINTR (см. read_stdin_raw).
                if super::signal::has_deliverable_current() {
                    return Err(Errno::EINTR);
                }
                // Нет готового сканкода: дать другим задачам (и IRQ, питающему
                // кольцевой буфер) поработать вместо сжигания кванта.
                crate::task::scheduler::yield_current();
                continue;
            }
        };
        let ev = match decoder.feed(sc) {
            Some(ev) => ev,
            None => continue,
        };
        match ev {
            KeyEvent::Enter | KeyEvent::Char('\n') => {
                console_write(b"\n");
                line.push(b'\n');
                break;
            }
            KeyEvent::Tab | KeyEvent::Char('\t') => {
                for _ in 0..4 {
                    if line.len() < STDIN_LINE_MAX {
                        console_write(b" ");
                        line.push(b' ');
                    }
                }
            }
            KeyEvent::Char(c) => {
                let mut utf8 = [0u8; 4];
                let s = c.encode_utf8(&mut utf8);
                console_write(s.as_bytes());
                line.extend_from_slice(s.as_bytes());
            }
            KeyEvent::Backspace => {
                if line.pop().is_some() {
                    console_write(b"\x08 \x08");
                }
            }
            KeyEvent::Ctrl('d') => {
                if line.is_empty() {
                    return Ok(0); // EOF
                }
            }
            _ => {}
        }
        if line.len() >= STDIN_LINE_MAX {
            break; // патологическая длина строки: отдать то, что есть
        }
    }
    let n = core::cmp::min(count as usize, line.len());
    copy_out_checked(buf, &line[..n])?;
    if n < line.len() {
        // Сохранить хвост: readline просит по одному байту за read(2).
        STDIN_PENDING.lock().extend_from_slice(&line[n..]);
    }
    Ok(n as u64)
}

/// `access` (21): успех (0), когда путь существует в дереве VFS/ext2, иначе
/// `ENOENT` (R2.5). Запрошенный режим доступа на этом слое не проверяется.
pub fn sys_access(path: u64, mode: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    if mode & !0o7 != 0 {
        return Err(Errno::EINVAL);
    }
    // POSIX: access(2) проверяет REAL id (а не effective); обход для root всё
    // равно действует, но setuid-подобное повышение нельзя использовать, чтобы
    // прощупать права, которых у процесса не будет после сброса. Сюда входит и
    // право поиска в каталоге при обходе, поэтому walk получает real id, а не
    // только финальную проверку прав.
    let creds = current_creds();
    // REAL-проекция, но с сохранением дополнительных групп: `setgroups(2)`
    // меняет общий набор, поэтому `access(2)` обязан видеть group-класс так же,
    // как обычный путь прав.
    let real = creds.with_real_ids();
    // `access(2)` разыменовывает симлинки (висячая ссылка — ENOENT).
    let node = walk_as(&abs, true, real)?;
    let owner = node.owner();
    let file_mode = node.mode();
    let wanted = [
        (0o4, Access::Read),
        (0o2, Access::Write),
        (0o1, Access::Execute),
    ];
    for (bit, access) in wanted {
        if mode & bit != 0 && !real.may(file_mode, owner, access) {
            return Err(Errno::EACCES);
        }
    }
    Ok(0)
}

/// `memfd_create` (319): анонимный дескриптор файла в RAM.
///
/// Имя — только метка (`/memfd:<name>` для `/proc/.../fd` и отладки); имя в VFS
/// не создаётся, как в Linux. `MFD_CLOEXEC` ставит FD_CLOEXEC;
/// `MFD_ALLOW_SEALING` принимается (печати не реализованы, поэтому вызовы
/// sealing не экспонируются); остальные флаги — `EINVAL`. Исполнение memfd не
/// поддерживается, потому что `execve` разрешает путь VFS.
pub fn sys_memfd_create(name: u64, flags: u64) -> Result<u64, Errno> {
    const MFD_CLOEXEC: u64 = 0x0001;
    const MFD_ALLOW_SEALING: u64 = 0x0002;
    if flags & !(MFD_CLOEXEC | MFD_ALLOW_SEALING) != 0 {
        return Err(Errno::EINVAL);
    }
    let requested = read_user_cstr(name)?;
    let node = Arc::new(MemFd::new(&alloc::format!("memfd:{requested}"))) as Arc<dyn VfsNode>;
    let obj = OpenObject::File {
        node,
        path: alloc::format!("/memfd:{requested}"),
        offset: 0,
    };
    let fd = compat::with_current_compat(|cs| {
        let fd = cs.fds.alloc_with_flags(obj, crate::task::fd::O_RDWR)?;
        if flags & MFD_CLOEXEC != 0 {
            cs.fds.set_cloexec(fd, true);
        }
        Ok::<u64, Errno>(fd as u64)
    })
    .ok_or(Errno::EBADF)??;
    Ok(fd)
}

/// `mkdir` (83): создать каталог в смонтированном дереве VFS/ext2.
/// CPython пробует его для каталогов pyc-кеша; ENOSYS терпелся, но давал
/// предупреждение «unsupported syscall» при каждом старте интерпретатора.
/// `rename` (82). В трейте VFS нет нативного rename, поэтому это эмуляция на
/// уровне syscall: скопировать содержимое файла в целевой путь, затем удалить
/// источник. Не атомарно (неважно для однопользовательского ядра) и только для
/// файлов: переименование каталогов возвращает EACCES с WARN, чтобы будущая
/// версия могла добавить настоящее переименование dirent в ext2.
fn rename_paths(old: &str, new: &str, flags: u64) -> Result<u64, Errno> {
    const RENAME_NOREPLACE: u64 = 1;
    if flags & !RENAME_NOREPLACE != 0 {
        return Err(Errno::EINVAL);
    }
    let oldabs_s = resolve_path(old);
    let newabs_s = resolve_path(new);
    let oldabs = oldabs_s.trim_end_matches('/');
    let newabs = newabs_s.trim_end_matches('/');
    if oldabs.is_empty() || newabs.is_empty() {
        return Err(Errno::EINVAL);
    }
    if oldabs == newabs {
        return Ok(0);
    }
    // Источник разрешается обходчиком с проверкой креденшелов, причём конечный
    // компонент НЕ разыменовывается (rename действует на саму ссылку): обход
    // требует `x` на каждом родителе, как и на стороне приёмника ниже.
    let node = walk(oldabs, false)?;
    if node.is_directory() {
        crate::warn!(
            "[linux] rename: directory rename not supported: {} -> {}",
            oldabs,
            newabs
        );
        return Err(Errno::EACCES);
    }
    if flags & RENAME_NOREPLACE != 0 && walk(newabs, false).is_ok() {
        return Err(Errno::EEXIST);
    }
    // Разрешить и авторизовать ОБА родительских каталога до любого побочного
    // эффекта: rename, отклонённый на стороне источника, не должен успеть
    // создать или перезаписать приёмник.
    let (nparent, nname) = match newabs.rfind('/') {
        Some(0) => ("/", &newabs[1..]),
        Some(i) => (&newabs[..i], &newabs[i + 1..]),
        None => return Err(Errno::ENOENT),
    };
    if nname.is_empty() {
        return Err(Errno::EINVAL);
    }
    let (oparent, oname) = match oldabs.rfind('/') {
        Some(0) => ("/", &oldabs[1..]),
        Some(i) => (&oldabs[..i], &oldabs[i + 1..]),
        None => return Err(Errno::ENOENT),
    };
    if oname.is_empty() {
        return Err(Errno::EINVAL);
    }
    // Родители приёмника разыменовывают ссылки (rename в каталог-симлинк
    // должен попасть в его цель); *источник* намеренно остаётся
    // неразыменованным — см. замечание выше.
    let ndir = walk(nparent, true)?;
    check_dir_mutation(&ndir)?;
    let odir = walk(oparent, true)?;
    check_dir_mutation(&odir)?;
    // Прочитать весь исходный файл.
    //
    // `i_size` контролируется образом и может быть разреженным обещанием на
    // ~4 ГиБ при горстке блоков, поэтому копия ограничена и может отказать:
    // `data.resize(i_size, 0)` раньше выделял заявленный размер одним махом и
    // ронял ядро, а затем обходил файл блок за блоком. Файл выше предела
    // отклоняется с ENOMEM (rename — эмуляция копированием; настоящее О(1)
    // переименование dirent в ext2 не реализовано).
    const RENAME_COPY_MAX: u64 = 64 * 1024 * 1024;
    let size = node.size();
    if size > RENAME_COPY_MAX {
        crate::warn!(
            "[linux] rename: source too large to copy ({} bytes): {} -> {}",
            size,
            oldabs,
            newabs
        );
        return Err(Errno::ENOMEM);
    }
    let size = size as usize;
    let mut data = ::alloc::vec::Vec::new();
    // `try_reserve` отказывает вместо abort, когда аллокатор не может
    // обслужить запрос — то же правило, которого ramfs уже держится для записи.
    data.try_reserve_exact(size).map_err(|_| Errno::ENOMEM)?;
    data.resize(size, 0u8);
    let mut off = 0usize;
    while off < size {
        let n = node
            .read(off as u64, &mut data[off..])
            .map_err(|_| Errno::EIO)?;
        if n == 0 {
            break;
        }
        off += n;
    }
    data.truncate(off);
    // Создать/заменить приёмник и записать содержимое.
    if ndir.lookup(nname).is_ok() {
        ndir.remove(nname).map_err(|e| {
            crate::warn!("[linux] rename: replace target failed: {:?} {}", e, newabs);
            Errno::EIO
        })?;
    }
    let target = ndir.create_file(nname).map_err(|e| {
        crate::warn!("[linux] rename: create target failed: {:?} {}", e, newabs);
        Errno::EIO
    })?;
    let mut woff = 0usize;
    while woff < data.len() {
        let n = target.write(woff as u64, &data[woff..]).map_err(|e| {
            crate::warn!("[linux] rename: write target failed: {:?} {}", e, newabs);
            Errno::EIO
        })?;
        if n == 0 {
            return Err(Errno::EIO);
        }
        woff += n;
    }
    // Удалить источник (оба родителя авторизованы до любой мутации).
    odir.remove(oname).map_err(|e| {
        crate::warn!("[linux] rename: unlink source failed: {:?} {}", e, oldabs);
        Errno::EIO
    })?;
    Ok(0)
}

pub fn sys_rename(oldpath: u64, newpath: u64) -> Result<u64, Errno> {
    let o = read_user_cstr(oldpath)?;
    let n = read_user_cstr(newpath)?;
    rename_paths(&o, &n, 0)
}

/// `renameat` (264): dirfd игнорируются (пути разрешаются относительно cwd,
/// что в этом минимальном слое покрывает AT_FDCWD, как в openat).
pub fn sys_renameat(
    _olddirfd: u64,
    oldpath: u64,
    _newdirfd: u64,
    newpath: u64,
) -> Result<u64, Errno> {
    let o = read_user_cstr(oldpath)?;
    let n = read_user_cstr(newpath)?;
    rename_paths(&o, &n, 0)
}

/// `renameat2` (316): как renameat; учитывается только RENAME_NOREPLACE.
pub fn sys_renameat2(
    _olddirfd: u64,
    oldpath: u64,
    _newdirfd: u64,
    newpath: u64,
    flags: u64,
) -> Result<u64, Errno> {
    let o = read_user_cstr(oldpath)?;
    let n = read_user_cstr(newpath)?;
    rename_paths(&o, &n, flags)
}

pub fn sys_mkdir(path: u64, mode: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    let trimmed = abs.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(Errno::EEXIST); // "/" существует всегда
    }
    // Проверять через обходчик с креденшелами (а не сырым lookup VFS), чтобы
    // отсутствие `x` у предка давало EACCES, а не попытку создания.
    if walk(trimmed, false).is_ok() {
        return Err(Errno::EEXIST);
    }
    let (parent, name) = match trimmed.rfind('/') {
        Some(0) => ("/", &trimmed[1..]),
        Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
        None => return Err(Errno::ENOENT),
    };
    if name.is_empty() {
        return Err(Errno::EINVAL);
    }
    let dir = walk(parent, true)?;
    check_dir_mutation(&dir)?;
    let created = dir.create_dir(name).map_err(|e| {
        crate::error!(
            "[linux] mkdir failed: {:?} parent={} name={}",
            e,
            parent,
            name
        );
        Errno::EIO
    })?;
    // Применить аргумент mode из `mkdir(2)` (урезанный umask) к inode, чтобы
    // stat и проверки прав совпадали с первого же lookup.
    stamp_creation_owner(&created);
    let requested = if mode == 0 {
        0o777
    } else {
        mode as u32 & 0o7777
    };
    let _ = apply_node_mode(&created, requested & !current_umask());
    Ok(0)
}

/// `unlink` (87): удалить файл, не являющийся каталогом (git удаляет свои
/// `*.lock` вокруг каждой записи конфига). Каталоги отклоняются с `EISDIR`, как
/// unlink(2); удаление пустого каталога — дело rmdir.
pub fn sys_unlink(path: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    let trimmed = abs.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(Errno::EISDIR); // нельзя удалить "/"
    }
    let node = walk(trimmed, false)?;
    if node.is_directory() {
        return Err(Errno::EISDIR);
    }
    let (parent, name) = match trimmed.rfind('/') {
        Some(0) => ("/", &trimmed[1..]),
        Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
        None => return Err(Errno::ENOENT),
    };
    let dir = walk(parent, true)?;
    check_dir_mutation(&dir)?;
    dir.remove(name).map_err(|e| {
        crate::warn!("[linux] unlink failed: {:?} path={}", e, trimmed);
        Errno::EIO
    })?;
    Ok(0)
}

/// `rmdir` (84): удалить пустой каталог. Не-каталоги отклоняются с `ENOTDIR`;
/// непустой каталог падает в слое ext2 (`ENOTEMPTY`), как rmdir(2).
pub fn sys_rmdir(path: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    let trimmed = abs.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(Errno::EINVAL); // нельзя удалить "/"
    }
    let node = walk(trimmed, false)?;
    if !node.is_directory() {
        return Err(Errno::ENOTDIR);
    }
    let (parent, name) = match trimmed.rfind('/') {
        Some(0) => ("/", &trimmed[1..]),
        Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
        None => return Err(Errno::ENOENT),
    };
    let dir = walk(parent, true)?;
    check_dir_mutation(&dir)?;
    dir.remove(name).map_err(|e| {
        crate::warn!("[linux] rmdir failed: {:?} path={}", e, trimmed);
        Errno::EIO
    })?;
    Ok(0)
}

/// `chmod` (90): сменить биты прав файла.
///
/// Кто может: только root или владелец файла (`EPERM`). Биты пишутся в inode
/// ext2 через [`VfsNode::set_mode`]; узлы без хранилища метаданных (`/dev`,
/// `/proc`, ramfs) сохраняют прежнее переопределение на процесс, чтобы
/// обработчики семейства `stat` всё равно сообщали запрошенный режим.
/// Переопределение записывается и для ext2, потому что `stat_mode_for` смотрит
/// в него первым (существующий userland ждёт старый дефолт 0755 для
/// нетронутых файлов).
pub fn sys_chmod(path: u64, mode: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    // `chmod(2)` разыменовывает симлинки: биты принадлежат inode цели.
    let node = walk(&abs, true)?;
    check_chmod_allowed(&node)?;
    apply_node_mode(&node, (mode & 0o7777) as u32)?;
    Ok(0)
}

/// `fchmod` (91): `chmod` на открытом дескрипторе.
///
/// Дескриптор резолвится под локом реестра (с клонированием узла или пути
/// каталога), а проверка/запись выполняются уже без него — запись в VFS может
/// блокироваться на устройстве.
pub fn sys_fchmod(fd: u64, mode: u64) -> Result<u64, Errno> {
    let node = fd_node(fd)?;
    check_chmod_allowed(&node)?;
    apply_node_mode(&node, (mode & 0o7777) as u32)?;
    Ok(0)
}

/// `fchmodat` (268): `chmod` относительно базы fd, с флагами.
///
/// Этот минимальный слой трактует любой `dirfd` как `AT_FDCWD` (абсолютный путь
/// или относительно cwd), как `openat`/`newfstatat`; `AT_SYMLINK_NOFOLLOW`
/// оставляет сам узел ссылки вместо разыменования. Остальные флаги — `EINVAL`.
pub fn sys_fchmodat(dirfd: u64, path: u64, mode: u64, flags: u64) -> Result<u64, Errno> {
    let _ = dirfd; // эквивалент AT_FDCWD в этом минимальном compat-слое.
    if flags & !AT_SYMLINK_NOFOLLOW != 0 {
        return Err(Errno::EINVAL);
    }
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    let node = walk(&abs, follow)?;
    check_chmod_allowed(&node)?;
    apply_node_mode(&node, (mode & 0o7777) as u32)?;
    Ok(0)
}

/// `faccessat` (269): `access` относительно базы fd, с `AT_EACCESS`.
///
/// `dirfd` трактуется как `AT_FDCWD` (см. `sys_fchmodat`). Без `AT_EACCESS`
/// проверка идёт по REAL id, как `access(2)`; с ним — по effective. С
/// `AT_SYMLINK_NOFOLLOW` целью проверки становится конечная симлинк-ссылка;
/// остальные флаги — `EINVAL`.
pub fn sys_faccessat(dirfd: u64, path: u64, mode: u64, flags: u64) -> Result<u64, Errno> {
    let _ = dirfd; // эквивалент AT_FDCWD в этом минимальном compat-слое.
    if flags & !(AT_EACCESS | AT_SYMLINK_NOFOLLOW) != 0 || mode & !0o7 != 0 {
        return Err(Errno::EINVAL);
    }
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    let creds = current_creds();
    let check = if flags & AT_EACCESS != 0 {
        creds
    } else {
        // REAL-проекция с сохранением дополнительных групп (см. `sys_access`).
        creds.with_real_ids()
    };
    let node = walk_as(&abs, follow, check)?;
    let owner = node.owner();
    let file_mode = node.mode();
    for (bit, access) in [
        (0o4, Access::Read),
        (0o2, Access::Write),
        (0o1, Access::Execute),
    ] {
        if mode & bit != 0 && !check.may(file_mode, owner, access) {
            return Err(Errno::EACCES);
        }
    }
    Ok(0)
}

/// `mkdirat` (258): `mkdir` с базой AT_FDCWD/dirfd.
///
/// `dirfd` в этом минимальном слое эквивалентен AT_FDCWD, как в
/// `openat`/`newfstatat`; права на создание берутся из `mode` & umask в
/// существующем пути `sys_mkdir`.
pub fn sys_mkdirat(dirfd: u64, path: u64, mode: u64) -> Result<u64, Errno> {
    let _ = dirfd;
    sys_mkdir(path, mode)
}

/// `unlinkat` (263): удалить файл или каталог (rmdir) с `AT_REMOVEDIR`.
pub fn sys_unlinkat(dirfd: u64, path: u64, flags: u64) -> Result<u64, Errno> {
    let _ = dirfd;
    if flags & !AT_REMOVEDIR != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & AT_REMOVEDIR != 0 {
        sys_rmdir(path)
    } else {
        sys_unlink(path)
    }
}

/// `fchownat` (260): `chown` относительно AT_FDCWD/dirfd с учётом
/// `AT_SYMLINK_NOFOLLOW` для конечного компонента.
pub fn sys_fchownat(dirfd: u64, path: u64, uid: u64, gid: u64, flags: u64) -> Result<u64, Errno> {
    let _ = dirfd;
    if flags & !AT_SYMLINK_NOFOLLOW != 0 {
        return Err(Errno::EINVAL);
    }
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    let node = walk(&abs, flags & AT_SYMLINK_NOFOLLOW == 0)?;
    check_chown_allowed()?;
    apply_node_owner(&node, uid, gid)?;
    Ok(0)
}

/// `chown` (92): сменить владельца файла. Только root (`EPERM`); аргумент
/// `(uid_t)-1` оставляет текущий id. Только inode ext2 умеют хранить владельца,
/// поэтому синтетический узел отвечает `EPERM`, а не изображает запись.
pub fn sys_chown(path: u64, uid: u64, gid: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    // `chown(2)` разыменовывает симлинки: владелец принадлежит inode цели.
    let node = walk(&abs, true)?;
    check_chown_allowed()?;
    apply_node_owner(&node, uid, gid)?;
    Ok(0)
}

/// `fchown` (93): `chown` на открытом дескрипторе (только root).
pub fn sys_fchown(fd: u64, uid: u64, gid: u64) -> Result<u64, Errno> {
    let node = fd_node(fd)?;
    check_chown_allowed()?;
    apply_node_owner(&node, uid, gid)?;
    Ok(0)
}

/// Свести открытый дескриптор к узлу VFS для метаданных-операций вроде
/// `fchmod`/`fchown`/`fstat`.
///
/// Дескриптор regular-файла несёт узел напрямую. Дескриптор каталога несёт
/// только путь, под которым его открыли (снимок детей — для `getdents64`),
/// поэтому он разрешается заново через обходчик ссылок после отпускания лока
/// реестра. Любой другой вид дескриптора — `EINVAL`; отсутствующий — `EBADF`.
fn fd_node(fd: u64) -> Result<Arc<dyn VfsNode>, Errno> {
    enum Found {
        File(Arc<dyn VfsNode>),
        Dir(String),
        Other,
        Absent,
    }
    let found = compat::with_current_compat(|cs| match cs.fds.get(fd as u32) {
        None => Found::Absent,
        Some(OpenObject::File { node, .. }) => Found::File(Arc::clone(node)),
        Some(OpenObject::Dir { path, .. }) => Found::Dir(path.clone()),
        Some(_) => Found::Other,
    });
    match found {
        Some(Found::File(node)) => Ok(node),
        Some(Found::Dir(path)) => walk(&path, true),
        Some(Found::Other) => Err(Errno::EINVAL),
        Some(Found::Absent) | None => Err(Errno::EBADF),
    }
}

// AT_FDCWD остаётся упомянутым, чтобы намерение (работа с dir-fd) было
// задокументировано, хотя абсолютные пути делают его инертным в этом слое.
const _: u64 = AT_FDCWD;

// ───────────────────── каталог / путь / fd (совместимость с Linux-бинами) ─────────────────────

/// `getdents64` (217): сериализовать записи каталога из открытого fd каталога в
/// буфер пользователя как упакованные `struct linux_dirent64`, сдвигая курсор
/// дескриптора. Возвращает число записанных байт, `0` в конце каталога. `EBADF`
/// для отсутствующего fd, `ENOTDIR` для не-каталога, `EINVAL`, если буфер меньше
/// даже первой оставшейся записи.
///
/// Дети каталога сняты снимком при `open`, поэтому всё выполняется под локом
/// `COMPAT_STATES` (без блокирующего вызова VFS), а курсор двигается через
/// `get_mut`. `d_ino`/`d_off` каждой записи синтезируются (VFS не даёт номеров
/// inode): `d_ino` — позиция ребёнка с 1, `d_off` — следующий курсор.
pub fn sys_getdents64(fd: u64, buf: u64, count: u64) -> Result<u64, Errno> {
    check_user_ptr(buf, count)?;

    let result = compat::with_current_compat(|cs| match cs.fds.get_mut(fd as u32) {
        None => Err(Errno::EBADF),
        Some(OpenObject::Dir {
            children, index, ..
        }) => {
            let mut out: Vec<u8> = Vec::new();
            let mut hit_limit_immediately = false;
            while *index < children.len() {
                let child = &children[*index];
                let name = child.name().as_bytes();
                let reclen = dirent_reclen(name.len());
                if out.len() + reclen > count as usize {
                    if out.is_empty() {
                        hit_limit_immediately = true;
                    }
                    break;
                }
                let d_type = d_type_for(child.is_directory(), child.is_symlink());
                // Настоящая идентичность, чтобы `readdir` и `stat` сходились
                // (`ls -i`, `find`); прежний `index + 1` был позицией курсора,
                // а не inode.
                let d_ino = node_ino(child);
                let d_off = (*index as i64) + 1;
                let rec = encode_dirent64(d_ino, d_off, d_type, name);
                out.extend_from_slice(&rec);
                *index += 1;
            }
            if hit_limit_immediately {
                Err(Errno::EINVAL)
            } else {
                Ok(out)
            }
        }
        Some(_) => Err(Errno::ENOTDIR),
    });

    match result {
        None => Err(Errno::EBADF),
        Some(Err(e)) => Err(e),
        Some(Ok(out)) => {
            copy_out(buf, &out)?;
            Ok(out.len() as u64)
        }
    }
}

/// `getcwd` (79): записать текущий рабочий каталог процесса (с NUL на конце)
/// в буфер пользователя, вернув число записанных байт вместе с NUL.
/// `ERANGE`, если буфер мал для пути с терминатором.
pub fn sys_getcwd(buf: u64, size: u64) -> Result<u64, Errno> {
    let cwd = current_cwd();
    let bytes = cwd.as_bytes();
    let need = bytes.len() + 1; // включить NUL-терминатор
    if size < need as u64 {
        return Err(Errno::ERANGE);
    }
    check_user_ptr_writable(buf, need as u64)?;
    // Скопировать путь, затем NUL-терминатор.
    copy_out(buf, bytes)?;
    // SAFETY: байт по buf+bytes.len() лежит внутри проверенного диапазона `need`.
    unsafe {
        *((buf + bytes.len() as u64) as *mut u8) = 0;
    }
    Ok(need as u64)
}

/// `chdir` (80): разрешить `path` (относительно cwd, если относительный),
/// убедиться, что это существующий каталог, и сделать его cwd процесса.
/// `ENOENT`, если пути нет, `ENOTDIR`, если это не каталог.
pub fn sys_chdir(path: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    // `chdir(2)` разыменовывает симлинки (`cd` в ссылку на каталог работает).
    let node = walk(&abs, true)?;
    if !node.is_directory() {
        return Err(Errno::ENOTDIR);
    }
    // Обход проверяет право поиска на родителях; `chdir(2)` дополнительно
    // требует `x` на самом каталоге (POSIX), иначе процесс мог бы сделать
    // непросматриваемый каталог своим cwd и читать относительные записи.
    check_node_access(&node, Access::Execute)?;
    compat::with_current_compat(|cs| cs.cwd = abs).ok_or(Errno::EBADF)?;
    Ok(0)
}

/// `fchdir` (81): сделать cwd процесса путь, под которым открыли дескриптор
/// каталога. `EBADF` для отсутствующего fd, `ENOTDIR`, если fd — не каталог.
pub fn sys_fchdir(fd: u64) -> Result<u64, Errno> {
    let resolved = compat::with_current_compat(|cs| match cs.fds.get(fd as u32) {
        None => Err(Errno::EBADF),
        Some(OpenObject::Dir { path, .. }) => {
            let p = path.clone();
            cs.cwd = p;
            Ok(())
        }
        Some(_) => Err(Errno::ENOTDIR),
    });
    match resolved {
        None => Err(Errno::EBADF),
        Some(Err(e)) => Err(e),
        Some(Ok(())) => Ok(0),
    }
}

/// `dup` (32): продублировать `oldfd` в наименьший свободный дескриптор и
/// вернуть его. `EBADF`, если `oldfd` не открыт.
pub fn sys_dup(oldfd: u64) -> Result<u64, Errno> {
    let r = compat::with_current_compat(|cs| cs.fds.dup(oldfd as u32))
        .unwrap_or(Err(Errno::EBADF))
        .map(|fd| fd as u64);
    // Nvim делает dup() своего stdio, прежде чем спрятать его за stderr;
    // EBADF здесь означает, что fd 0/1 уже пропали к старту сервера.
    let _ = &r;
    r
}

/// Верхняя граница номеров дескрипторов (soft-лимит RLIMIT_NOFILE в Linux).
/// `dup2`/`dup3` обязаны отвергать `newfd` на этой границе и выше, а не растить
/// fd-таблицу до запрошенного индекса `(u32)`: враждебный или ошибочный
/// `dup2(fd, 0x7FFFFFFF)` иначе попытался бы выделить ~8 миллиардов слотов и
/// уронил ядро. `fcntl(F_DUPFD/F_DUPFD_CLOEXEC)` применяет ту же границу к
/// своему `arg` (минимальному дескриптору): эта ветка когда-то была пропущена,
/// и `fcntl(fd, F_DUPFD, 0x2A000000)` вырастил таблицу до 704 МиБ, а паника
/// аллокации остановила машину.
pub const NOFILE_MAX: u64 = 1024;
// Запасной предел уровня таблицы (`FdSlots::try_alloc`) и эта граница слоя
// syscall должны совпадать, иначе один из двух слоёв станет мёртвым кодом.
const _: () = assert!(NOFILE_MAX as usize == crate::task::fd_alloc::FD_TABLE_LIMIT);

/// `dup2` (33): продублировать `oldfd` в явный дескриптор `newfd`, сначала
/// закрыв то, что занимает `newfd`. Если `oldfd == newfd` и `oldfd` валиден, он
/// возвращается без изменений (без закрытия); `EBADF`, если `oldfd` невалиден;
/// `EBADF`, когда `newfd` не меньше `RLIMIT_NOFILE` (Linux проверяет границу
/// как невалидный дескриптор; прежний `EINVAL` расходился с ABI).
pub fn sys_dup2(oldfd: u64, newfd: u64) -> Result<u64, Errno> {
    if newfd >= NOFILE_MAX {
        return Err(Errno::EBADF);
    }
    // Перепайка stdio при spawn — именно там потеря RPC-канала nvim была бы
    // незаметна; dup2/dup3 достаточно редки, чтобы логировать каждый вызов.

    compat::with_current_compat(|cs| {
        // `oldfd` должен быть валиден в любом случае.
        if cs.fds.get(oldfd as u32).is_none() {
            return Err(Errno::EBADF);
        }
        if oldfd == newfd {
            return Ok(newfd);
        }
        cs.fds
            .dup_to(oldfd as u32, newfd as u32)
            .map(|fd| fd as u64)
    })
    .unwrap_or(Err(Errno::EBADF))
}

/// `dup3` (292): как `dup2`, но `oldfd == newfd` — ошибка (`EINVAL`), а
/// единственный принимаемый флаг — `O_CLOEXEC` (здесь игнорируется). `EBADF`,
/// если `oldfd` невалиден.
pub fn sys_dup3(oldfd: u64, newfd: u64, flags: u64) -> Result<u64, Errno> {
    if newfd >= NOFILE_MAX {
        return Err(Errno::EBADF);
    }
    if oldfd == newfd {
        return Err(Errno::EINVAL);
    }
    if flags & !O_CLOEXEC != 0 {
        return Err(Errno::EINVAL);
    }
    compat::with_current_compat(|cs| {
        let fd = cs.fds.dup_to(oldfd as u32, newfd as u32)?;
        // Единственный флаг dup3 — пометить новый дескриптор close-on-exec.
        if flags & O_CLOEXEC != 0 {
            cs.fds.set_cloexec(fd, true);
        }
        Ok(fd as u64)
    })
    .unwrap_or(Err(Errno::EBADF))
}

// Команды fcntl.
const F_DUPFD: u64 = 0;
const F_GETFD: u64 = 1;
const F_SETFD: u64 = 2;
const F_GETFL: u64 = 3;
const F_SETFL: u64 = 4;
const F_DUPFD_CLOEXEC: u64 = 1030;

/// `fcntl` (72): подмножество управления дескрипторами.
///   * `F_DUPFD` → продублировать `fd` в наименьший свободный дескриптор `>= arg`.
///   * `F_DUPFD_CLOEXEC` → то же плюс пометить дубликат close-on-exec.
///   * `F_GETFD`/`F_SETFD` → FD_CLOEXEC отслеживается по-настоящему
///     (libuv помечает им свой fork-error pipe, и execve обязан его вычищать).
///   * `F_GETFL` → O_RDWR плюс O_NONBLOCK, если дескриптор неблокирующий.
///   * `F_SETFL` → O_NONBLOCK реально применяется к pipe, сокетам и
///     слушателям (libuv читает до EAGAIN, так что это обязано работать);
///     остальные флаги принимаются и игнорируются.
///   * всё остальное → `EINVAL`.
pub fn sys_fcntl(fd: u64, cmd: u64, arg: u64) -> Result<u64, Errno> {
    // Дочерняя инициализация libuv переставляет stdio-дескрипторы через F_DUPFD.
    if cmd == F_DUPFD || cmd == F_DUPFD_CLOEXEC {}
    const FD_CLOEXEC: u64 = 1;
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            // `arg` — выбранный вызывающим минимальный номер дескриптора.
            // Linux отвергает отрицательный аргумент или `>= RLIMIT_NOFILE`
            // с EINVAL; отвергаем здесь — по полному u64, до усечения к u32, —
            // чтобы fd-таблицу вообще никогда не просили вырасти до индекса
            // атакующего.
            if arg >= NOFILE_MAX {
                return Err(Errno::EINVAL);
            }
            compat::with_current_compat(|cs| {
                let newfd = cs.fds.dup_min(fd as u32, arg as u32)?;
                cs.fds.set_cloexec(newfd, cmd == F_DUPFD_CLOEXEC);
                Ok(newfd as u64)
            })
            .unwrap_or(Err(Errno::EBADF))
        }
        F_GETFD => compat::with_current_compat(|cs| {
            if cs.fds.get(fd as u32).is_none() {
                return Err(Errno::EBADF);
            }
            Ok(if cs.fds.is_cloexec(fd as u32) {
                FD_CLOEXEC
            } else {
                0
            })
        })
        .unwrap_or(Err(Errno::EBADF)),
        F_SETFD => compat::with_current_compat(|cs| {
            if cs.fds.get(fd as u32).is_none() {
                return Err(Errno::EBADF);
            }
            cs.fds.set_cloexec(fd as u32, arg & FD_CLOEXEC != 0);
            Ok(0)
        })
        .unwrap_or(Err(Errno::EBADF)),
        F_GETFL | F_SETFL => {
            const O_NONBLOCK_FL: u64 = 0x800;
            const O_RDWR_FL: u64 = 2;
            compat::with_current_compat(|cs| {
                let obj = cs.fds.get_mut(fd as u32).ok_or(Errno::EBADF)?;
                if cmd == F_GETFL {
                    let nb = match &*obj {
                        OpenObject::PipeRead(e) | OpenObject::PipeWrite(e) => e.nonblocking(),
                        OpenObject::Socket { rx, .. } => rx.nonblocking(),
                        OpenObject::UnixListener(l) => l.inner.lock().nonblocking,
                        OpenObject::UnixSocketUnbound { nonblocking } => *nonblocking,
                        // Stdin сообщает своё реальное состояние nonblocking.
                        OpenObject::Stdin => {
                            STDIN_NONBLOCK.load(core::sync::atomic::Ordering::Relaxed)
                        }
                        _ => false,
                    };
                    Ok(if nb {
                        O_RDWR_FL | O_NONBLOCK_FL
                    } else {
                        O_RDWR_FL
                    })
                } else {
                    let on = arg & O_NONBLOCK_FL != 0;
                    match obj {
                        OpenObject::PipeRead(e) => *e = e.with_nonblocking(on),
                        OpenObject::PipeWrite(e) => *e = e.with_nonblocking(on),
                        OpenObject::Socket { rx, tx } => {
                            let nrx = rx.with_nonblocking(on);
                            let ntx = tx.with_nonblocking(on);
                            *rx = nrx;
                            *tx = ntx;
                        }
                        OpenObject::UnixListener(l) => l.inner.lock().nonblocking = on,
                        OpenObject::UnixSocketUnbound { nonblocking } => *nonblocking = on,
                        // Libuv переключает O_NONBLOCK на сыром tty.
                        OpenObject::Stdin => {
                            STDIN_NONBLOCK.store(on, core::sync::atomic::Ordering::Relaxed);
                        }
                        _ => {}
                    }
                    cs.fds.set_nonblocking_flag(fd as u32, on);
                    Ok(0)
                }
            })
            .unwrap_or(Err(Errno::EBADF))
        }
        _ => Err(Errno::EINVAL),
    }
}

/// `readlink` (89): скопировать цель символической ссылки по `path` в буфер
/// пользователя (контракт `EXT2-LINKS.md` §4.2).
///
/// NUL-терминатор не пишется, результат урезается до `bufsiz`; конечный
/// компонент **не** разыменовывается (в этом смысл вызова), промежуточные —
/// разыменовываются. `ENOENT` для отсутствующего пути, `EINVAL` для узла, не
/// являющегося ссылкой, *и* для `bufsiz == 0` (Linux сначала проверяет размер).
/// `/proc/self/exe` теперь обычный узел-ссылка (контракт `docs/procfs.md` §5.4),
/// поэтому `uv_exepath` из libuv — progpath nvim — работает через тот же путь,
/// что и любая другая ссылка.
pub fn sys_readlink(path: u64, buf: u64, bufsiz: u64) -> Result<u64, Errno> {
    if bufsiz == 0 {
        return Err(Errno::EINVAL);
    }
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    let node = walk(&abs, false)?;
    if !node.is_symlink() {
        return Err(Errno::EINVAL);
    }
    let target = node.read_link().ok_or(Errno::ENOENT)?;
    let bytes = target.as_bytes();
    let n = core::cmp::min(bytes.len() as u64, bufsiz);
    check_user_ptr_writable(buf, n)?;
    copy_out(buf, &bytes[..n as usize])?;
    Ok(n)
}

/// `readlinkat` (267): как `readlink`; dirfd игнорируется (пути разрешаются
/// абсолютно / относительно cwd).
pub fn sys_readlinkat(_dirfd: u64, path: u64, buf: u64, bufsiz: u64) -> Result<u64, Errno> {
    sys_readlink(path, buf, bufsiz)
}

/// `pread64` (17): прочитать до `count` байт из `fd` по абсолютному `offset`,
/// НЕ сдвигая собственное смещение дескриптора. `EBADF` для отсутствующего fd;
/// `ESPIPE` для неищущегося потока (консоль/stdin); `EISDIR` для каталога.
pub fn sys_pread64(fd: u64, buf: u64, count: u64, offset: u64) -> Result<u64, Errno> {
    if count > COUNT_MAX {
        return Err(Errno::EINVAL);
    }
    check_user_ptr_writable(buf, count)?;
    match resolve_fd(fd as u32) {
        None => Err(Errno::EBADF),
        Some(Resolved::Console)
        | Some(Resolved::Stdin)
        | Some(Resolved::PipeRead(_))
        | Some(Resolved::PipeWrite(_))
        | Some(Resolved::Socket { .. })
        | Some(Resolved::InetTcp(_))
        | Some(Resolved::InetUdp(_))
        | Some(Resolved::Eventfd { .. })
        | Some(Resolved::Timerfd(_))
        | Some(Resolved::Signalfd(_))
        | Some(Resolved::Epoll) => Err(Errno::ESPIPE),
        Some(Resolved::Dir) => Err(Errno::EISDIR),
        Some(Resolved::File { node, .. }) => {
            let size = node.size();
            let (copied, _) = plan_read(size, offset, count);
            if copied == 0 {
                return Ok(0);
            }
            let mut kbuf = vec![0u8; copied as usize];
            let n = node.read(offset, &mut kbuf).map_err(|e| {
                // Настоящая ошибка чтения VFS/ext2 — не «неверный аргумент»:
                // сообщаем EIO и логируем, что именно сломалось.
                crate::error!(
                    "[linux] file read failed: {:?} ino={} off={} len={}",
                    e,
                    node.fs_ino(),
                    offset,
                    copied
                );
                Errno::EIO
            })?;
            copy_out(buf, &kbuf[..n])?;
            // ЗАМЕЧАНИЕ: смещение дескриптора намеренно НЕ обновляется.
            Ok(n as u64)
        }
    }
}

/// `pwrite64` (18): записать `count` байт в `fd` по абсолютному `offset`, НЕ
/// сдвигая собственное смещение дескриптора. Запись в консоль игнорирует
/// смещение и выводит в консоль; `ESPIPE` для stdin; `EISDIR` для каталога;
/// `EBADF` для отсутствующего fd.
pub fn sys_pwrite64(fd: u64, buf: u64, count: u64, offset: u64) -> Result<u64, Errno> {
    if count > COUNT_MAX {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(buf, count)?;
    match resolve_fd(fd as u32) {
        None => Err(Errno::EBADF),
        Some(Resolved::Stdin) => Err(Errno::ESPIPE),
        Some(Resolved::PipeRead(_)) | Some(Resolved::PipeWrite(_)) => Err(Errno::ESPIPE),
        Some(Resolved::Socket { .. }) => Err(Errno::ESPIPE),
        Some(Resolved::InetTcp(_)) | Some(Resolved::InetUdp(_)) => Err(Errno::ESPIPE),
        Some(Resolved::Eventfd { .. })
        | Some(Resolved::Epoll)
        | Some(Resolved::Timerfd(_))
        | Some(Resolved::Signalfd(_)) => Err(Errno::ESPIPE),
        Some(Resolved::Dir) => Err(Errno::EISDIR),
        Some(Resolved::Console) => {
            // Контроль доступа к терминалу, как у `write(2)`/`writev(2)`.
            crate::drivers::tty::check_access(super::tty::TtyAccess::Write)?;
            let data = copy_in(buf, count);
            console_write(&data);
            Ok(count)
        }
        Some(Resolved::File { node, .. }) => {
            let data = copy_in(buf, count);
            let n = node.write(offset, &data).map_err(write_errno)?;
            // ЗАМЕЧАНИЕ: смещение дескриптора намеренно НЕ обновляется.
            Ok(n as u64)
        }
    }
}

/// Linux x86_64 `struct statfs` (подмножество, заполненное правдоподобными значениями).
#[repr(C)]
struct LinuxStatfs {
    f_type: i64,
    f_bsize: i64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_fsid: [i32; 2],
    f_namelen: i64,
    f_frsize: i64,
    f_flags: i64,
    f_spare: [i64; 4],
}

/// Магия суперблока ext2, сообщается в `f_type`, чтобы `df`-подобные пробы узнавали ФС.
const EXT2_SUPER_MAGIC: i64 = 0xEF53;

/// Собрать снимок `statfs`. Если ФС узла сообщает ёмкость (`vfs::FsStat`, ext2),
/// используются реальные счётчики блоков/inode; иначе подставляются счётчики
/// кадров PMM (ramfs, синтетические узлы /dev).
fn build_statfs_for(fs: Option<vfs::FsStat>) -> LinuxStatfs {
    match fs {
        Some(fs) => LinuxStatfs {
            f_type: EXT2_SUPER_MAGIC,
            f_bsize: fs.block_size as i64,
            f_blocks: fs.blocks_total,
            f_bfree: fs.blocks_free,
            f_bavail: fs.blocks_free,
            f_files: fs.inodes_total,
            f_ffree: fs.inodes_free,
            f_fsid: [0, 0],
            f_namelen: 255,
            f_frsize: fs.block_size as i64,
            f_flags: 0,
            f_spare: [0; 4],
        },
        None => {
            let total = crate::memory::pmm::total_frames() as u64;
            let free = crate::memory::pmm::free_frames() as u64;
            LinuxStatfs {
                f_type: EXT2_SUPER_MAGIC,
                f_bsize: 4096,
                f_blocks: total,
                f_bfree: free,
                f_bavail: free,
                f_files: total,
                f_ffree: free,
                f_fsid: [0, 0],
                f_namelen: 255,
                f_frsize: 4096,
                f_flags: 0,
                f_spare: [0; 4],
            }
        }
    }
}

/// Скопировать собранный `statfs` в проверенный буфер пользователя.
fn write_statfs(buf: u64, fs: Option<vfs::FsStat>) -> Result<(), Errno> {
    let sf = build_statfs_for(fs);
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &sf as *const LinuxStatfs as *const u8,
            core::mem::size_of::<LinuxStatfs>(),
        )
    };
    copy_out(buf, bytes)
}

/// `statfs` (137): заполнить пользовательский `struct statfs` реальной ёмкостью
/// смонтированной ФС, когда она доступна (путь обязан существовать, иначе
/// `ENOENT`). Возвращает 0.
pub fn sys_statfs(path: u64, buf: u64) -> Result<u64, Errno> {
    check_user_ptr_writable(buf, core::mem::size_of::<LinuxStatfs>() as u64)?;
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    // `statfs(2)` разыменовывает симлинки, как `stat`.
    let node = walk(&abs, true)?;
    write_statfs(buf, node.fs_stat())?;
    Ok(0)
}

/// `fstatfs` (138): заполнить пользовательский `struct statfs` для открытого
/// дескриптора (он обязан быть валидным, иначе `EBADF`). Возвращает 0.
pub fn sys_fstatfs(fd: u64, buf: u64) -> Result<u64, Errno> {
    check_user_ptr_writable(buf, core::mem::size_of::<LinuxStatfs>() as u64)?;
    let fs = match resolve_fd(fd as u32) {
        None => return Err(Errno::EBADF),
        Some(Resolved::File { node, .. }) => node.fs_stat(),
        _ => None,
    };
    write_statfs(buf, fs)?;
    Ok(0)
}

const O_NONBLOCK: u64 = 0x800;
const O_CLOEXEC: u64 = 0x80000;
const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;
const POLLNVAL: i16 = 0x020;
pub fn sys_pipe(pipefd: u64) -> Result<u64, Errno> {
    sys_pipe2(pipefd, 0)
}
pub fn sys_pipe2(pipefd: u64, flags: u64) -> Result<u64, Errno> {
    if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    check_user_ptr_writable(pipefd, 8)?;
    let pair = compat::with_current_compat(|cs| {
        // F-r4-life-3: table full → EMFILE, а не рост за FD_TABLE_LIMIT.
        let p = cs.fds.pipe(flags & O_NONBLOCK != 0)?;
        if flags & O_CLOEXEC != 0 {
            cs.fds.set_cloexec(p.0, true);
            cs.fds.set_cloexec(p.1, true);
        }
        Ok::<(u32, u32), Errno>(p)
    })
    .ok_or(Errno::EBADF)??;

    let words = [pair.0, pair.1];
    let bytes = unsafe { core::slice::from_raw_parts(words.as_ptr() as *const u8, 8) };
    copy_out(pipefd, bytes)?;
    Ok(0)
}
fn poll_revents(fd: i32, events: i16) -> i16 {
    if fd < 0 {
        return 0;
    }
    match resolve_fd(fd as u32) {
        None => POLLNVAL,
        Some(Resolved::InetTcp(_)) => {
            // Консервативно: подключённый сокет всегда считается доступным для
            // записи и для чтения-при-наличии-данных; event loop curl всё равно
            // перепроверяет через recv/send.
            events & (POLLIN | POLLOUT)
        }
        Some(Resolved::InetUdp(_)) => events & (POLLIN | POLLOUT),
        Some(Resolved::PipeRead(e)) => {
            let mut o = if e.read_ready() { events & POLLIN } else { 0 };
            if e.peer_closed() {
                o |= POLLHUP
            }
            o
        }
        Some(Resolved::PipeWrite(e)) => {
            let mut o = if e.write_ready() { events & POLLOUT } else { 0 };
            if e.peer_closed() {
                o |= POLLERR
            }
            o
        }
        Some(Resolved::Socket { rx, tx }) => {
            let mut o = 0i16;
            if rx.read_ready() {
                o |= events & POLLIN
            }
            if tx.write_ready() {
                o |= events & POLLOUT
            }
            if rx.peer_closed() {
                o |= POLLHUP
            }
            o
        }
        Some(Resolved::Stdin) => {
            if stdin_input_available() {
                events & POLLIN
            } else {
                0
            }
        }
        Some(Resolved::Console) => events & POLLOUT,
        Some(Resolved::Signalfd(mask)) => {
            if crate::task::compat::signalfd_readable(mask) {
                events & POLLIN
            } else {
                0
            }
        }
        Some(Resolved::Timerfd(state)) => {
            if state.lock().pending(crate::task::scheduler::ticks()) > 0 {
                events & POLLIN
            } else {
                0
            }
        }
        Some(Resolved::Eventfd { val, .. }) => {
            let v = val.lock();
            if *v > 0 {
                events & POLLIN
            } else {
                0
            }
        }
        Some(Resolved::Epoll) => 0,
        Some(Resolved::File { node, .. }) => {
            if node.is_evdev() {
                // evdev: POLLIN ровно когда в очереди есть событие.
                if crate::drivers::evdev::has_events() {
                    events & POLLIN
                } else {
                    0
                }
            } else if node.is_fbdev() {
                // Фреймбуфер всегда готов к записи (mmap — отдельный путь).
                events & POLLOUT
            } else {
                events & (POLLIN | POLLOUT)
            }
        }
        Some(Resolved::Dir) => events & (POLLIN | POLLOUT),
    }
}
/// Опубликовать ядерную копию массива pollfd в пользовательский указатель,
/// предварительно проверив диапазон заново.
///
/// ПОЧЕМУ: `poll`/`ppoll` могут блокироваться на неограниченное время; массив
/// проверялся один раз на входе. Поток-сосед по `CLONE_VM` может munmap или
/// mprotect эту страницу, пока вызов вытеснен, а устаревшая запись из ring 0
/// даёт #PF режима ядра, который обработчик превращает в остановку машины.
/// Повторная проверка прямо перед записью превращает этот класс в `-EFAULT`.
fn publish_pollfds(fds: u64, bytes: u64, kfds: &[u8]) -> Result<(), Errno> {
    check_user_ptr_writable(fds, bytes)?;
    copy_out(fds, kfds)
}

pub fn sys_poll(fds: u64, nfds: u64, timeout: u64) -> Result<u64, Errno> {
    const SZ: u64 = 8;
    if nfds > 1024 {
        return Err(Errno::EINVAL);
    }
    let bytes = nfds.checked_mul(SZ).ok_or(Errno::EINVAL)?;
    check_user_ptr(fds, bytes)?;
    // Массив pollfd — память пользователя, и каждая итерация ожидания раньше
    // заново разыменовывала его после `yield_current()`. Брать приватную
    // ядерную копию один раз (проверенную непосредственно выше, без уступки
    // CPU между проверкой и копией), вести всё ожидание по копии и публиковать
    // её только через `publish_pollfds`, который заново проверяет приёмник.
    // Буфер, размонтированный во время ожидания, теперь даёт `-EFAULT`, а не
    // падение в ring 0 с остановкой машины.
    let mut kfds = copy_in(fds, bytes);
    let ms = timeout as i64;
    let deadline = if ms < 0 {
        None
    } else {
        Some(crate::task::scheduler::ticks().saturating_add((ms as u64).saturating_add(9) / 10))
    };
    loop {
        // Проверка живости после каждой уступки CPU (и до первого сканирования):
        // ядерная копия выше делает само ожидание безопасным от падений, но
        // syscall не должен блокироваться на буфере, которым вызывающий уже не
        // владеет. Диапазон, снятый соседним потоком, — это -EFAULT, точно как
        // проверка на входе, а не вечное ожидание.
        check_user_ptr(fds, bytes)?;
        let mut ready = 0;
        for i in 0..nfds {
            let off = (i * SZ) as usize;
            // `off + 8 <= kfds.len()` по построению (`nfds * 8` байт).
            let fd = i32::from_ne_bytes([kfds[off], kfds[off + 1], kfds[off + 2], kfds[off + 3]]);
            let ev = i16::from_ne_bytes([kfds[off + 4], kfds[off + 5]]);
            let rev = poll_revents(fd, ev);
            kfds[off + 6..off + 8].copy_from_slice(&rev.to_ne_bytes());
            if rev != 0 {
                ready += 1
            }
        }
        if ready != 0 || ms == 0 {
            publish_pollfds(fds, bytes, &kfds)?;
            return Ok(ready);
        }
        if let Some(end) = deadline {
            if crate::task::scheduler::ticks() >= end {
                // Таймаут: Linux оставляет все поля revents нулевыми — именно это
                // и записало в ядерную копию сканирование выше.
                publish_pollfds(fds, bytes, &kfds)?;
                return Ok(0);
            }
        }
        if super::signal::has_deliverable_current() {
            return Err(Errno::EINTR);
        }
        crate::task::scheduler::yield_current()
    }
}

// ---------------------------------------------------------------------------
// Минимальные select/pselect6/ppoll (nr 23/270/271). GNU readline (его грузит
// REPL CPython, раз установлен libreadline) ждёт stdin через pselect6 перед
// каждым байтом; ENOSYS оставлял её крутиться сразу после первого приглашения
// `>>>`. stdin cooked-tty всегда сообщается читаемым — следующий read(2) всё
// равно блокируется построчно, а остаток отдаётся из STDIN_PENDING по байту за
// чтение.
// ---------------------------------------------------------------------------
const FDSET_WORDS: usize = 16; // 1024 fd, как FD_SETSIZE

fn select_ready(fd: u64, want_write: bool) -> bool {
    match resolve_fd(fd as u32) {
        None => true, // сообщить готовность; следующая операция вернёт EBADF
        Some(Resolved::InetTcp(_)) | Some(Resolved::InetUdp(_)) => true,
        Some(Resolved::Stdin) => !want_write,
        Some(Resolved::Console) => true,
        Some(Resolved::PipeRead(e)) => !want_write && (e.read_ready() || e.peer_closed()),
        Some(Resolved::PipeWrite(e)) => want_write && (e.write_ready() || e.peer_closed()),
        Some(Resolved::Socket { rx, tx }) => {
            if want_write {
                tx.write_ready() || tx.peer_closed()
            } else {
                rx.read_ready() || rx.peer_closed()
            }
        }
        Some(Resolved::Signalfd(mask)) => {
            !want_write && crate::task::compat::signalfd_readable(mask)
        }
        Some(Resolved::Timerfd(state)) => {
            !want_write && state.lock().pending(crate::task::scheduler::ticks()) > 0
        }
        Some(Resolved::Eventfd { val, .. }) => {
            !want_write && {
                let v = val.lock();
                *v > 0
            }
        }
        Some(Resolved::Epoll) => false,
        Some(Resolved::File { node, .. }) => {
            if node.is_evdev() {
                !want_write && crate::drivers::evdev::has_events()
            } else {
                true
            }
        }
        Some(Resolved::Dir) => true,
    }
}

fn load_fdset(ptr: u64, nfds: u64) -> Result<[u64; FDSET_WORDS], Errno> {
    let mut set = [0u64; FDSET_WORDS];
    if ptr == 0 {
        return Ok(set);
    }
    let words = ((nfds + 63) / 64) as usize;
    check_user_ptr(ptr, (words as u64) * 8)?;
    for w in 0..words {
        set[w] = unsafe { *((ptr + (w as u64) * 8) as *const u64) };
    }
    Ok(set)
}

/// Записать один результирующий fdset по указателю, который УЖЕ проверен
/// [`store_fdsets`] (вызывающий проверяет все три до записи любого из них).
fn store_fdset(ptr: u64, nfds: u64, set: &[u64; FDSET_WORDS]) {
    if ptr == 0 {
        return;
    }
    let words = ((nfds + 63) / 64) as usize;
    for w in 0..words {
        unsafe { *((ptr + (w as u64) * 8) as *mut u64) = set[w] };
    }
}

/// Опубликовать три результирующих fdset `select(2)`, заново проверив каждый
/// ненулевой приёмник ДО записи любого из них.
///
/// ПОЧЕМУ: `select`/`pselect6` блокируются на неограниченное время, уступая
/// CPU через `yield_current()`. Fdsets проверялись один раз на входе, а соседний
/// поток по `CLONE_VM` может munmap один из них, пока вызов вытеснен — такая
/// устаревшая запись из ring 0 даёт #PF режима ядра и останавливает машину.
/// Проверка всех трёх заранее означает, что исчезнувший буфер даёт `-EFAULT`, и
/// ни один частичный результат не попадёт в уцелевшие наборы.
fn check_fdsets_out(nfds: u64, readfds: u64, writefds: u64, exceptfds: u64) -> Result<(), Errno> {
    let bytes = ((nfds + 63) / 64) * 8;
    for ptr in [readfds, writefds, exceptfds] {
        if ptr != 0 {
            check_user_ptr(ptr, bytes)?;
        }
    }
    Ok(())
}

fn store_fdsets(
    nfds: u64,
    readfds: u64,
    writefds: u64,
    exceptfds: u64,
    got_r: &[u64; FDSET_WORDS],
    got_w: &[u64; FDSET_WORDS],
) -> Result<(), Errno> {
    check_fdsets_out(nfds, readfds, writefds, exceptfds)?;
    store_fdset(readfds, nfds, got_r);
    store_fdset(writefds, nfds, got_w);
    // Исключительных условий на этом слое не бывает; сообщаем пустой набор.
    store_fdset(exceptfds, nfds, &[0u64; FDSET_WORDS]);
    Ok(())
}

fn do_select(
    nfds: u64,
    readfds: u64,
    writefds: u64,
    exceptfds: u64,
    timeout_ms: Option<i64>,
) -> Result<u64, Errno> {
    let nfds = nfds.min(1024);
    let want_r = load_fdset(readfds, nfds)?;
    let want_w = load_fdset(writefds, nfds)?;
    let _ = load_fdset(exceptfds, nfds)?; // исключительные условия здесь не срабатывают
    let deadline = match timeout_ms {
        None => None,
        Some(ms) if ms <= 0 => Some(0), // просканировать раз, затем таймаут
        Some(ms) => Some(
            crate::task::scheduler::ticks().saturating_add(((ms as u64).saturating_add(9)) / 10),
        ),
    };
    loop {
        // Проверка живости после каждой уступки CPU: цикл работает с ядерными
        // копиями, загруженными выше, поэтому сосед, размонтировавший выходной
        // fdset, обязан превратиться в -EFAULT на следующей итерации, а не в
        // бесконечное ожидание.
        check_fdsets_out(nfds, readfds, writefds, exceptfds)?;
        let mut got_r = [0u64; FDSET_WORDS];
        let mut got_w = [0u64; FDSET_WORDS];
        let mut ready = 0u64;
        for fd in 0..nfds {
            let (w, b) = ((fd / 64) as usize, 1u64 << (fd % 64));
            if want_r[w] & b != 0 && select_ready(fd, false) {
                got_r[w] |= b;
                ready += 1;
            }
            if want_w[w] & b != 0 && select_ready(fd, true) {
                got_w[w] |= b;
                ready += 1;
            }
        }
        let timed_out = match deadline {
            Some(end) => crate::task::scheduler::ticks() >= end,
            None => false,
        };
        if ready != 0 || timed_out {
            // Перепроверить выходы после (возможно, долгого) ожидания выше —
            // см. `store_fdsets`.
            store_fdsets(nfds, readfds, writefds, exceptfds, &got_r, &got_w)?;
            return Ok(ready);
        }
        if super::signal::has_deliverable_current() {
            return Err(Errno::EINTR);
        }
        crate::task::scheduler::yield_current();
    }
}

/// `pselect6` (270): таймаут — `struct timespec`; sigmask игнорируется
/// (маска на время ожидания не применяется).
pub fn sys_pselect6(
    nfds: u64,
    readfds: u64,
    writefds: u64,
    exceptfds: u64,
    timeout: u64,
    _sigmask: u64,
) -> Result<u64, Errno> {
    let ms = if timeout == 0 {
        None
    } else {
        check_user_ptr(timeout, 16)?;
        let sec = unsafe { *(timeout as *const i64) };
        let nsec = unsafe { *((timeout + 8) as *const i64) };
        Some(sec.saturating_mul(1000).saturating_add(nsec / 1_000_000))
    };
    do_select(nfds, readfds, writefds, exceptfds, ms)
}

/// `select` (23): таймаут — `struct timeval`.
pub fn sys_select(
    nfds: u64,
    readfds: u64,
    writefds: u64,
    exceptfds: u64,
    timeout: u64,
) -> Result<u64, Errno> {
    let ms = if timeout == 0 {
        None
    } else {
        check_user_ptr(timeout, 16)?;
        let sec = unsafe { *(timeout as *const i64) };
        let usec = unsafe { *((timeout + 8) as *const i64) };
        Some(sec.saturating_mul(1000).saturating_add(usec / 1_000))
    };
    do_select(nfds, readfds, writefds, exceptfds, ms)
}

/// `ppoll` (271): `poll` с таймаутом `struct timespec`; sigmask игнорируется.
pub fn sys_ppoll(fds: u64, nfds: u64, ts: u64, _sigmask: u64) -> Result<u64, Errno> {
    let ms: i64 = if ts == 0 {
        -1
    } else {
        check_user_ptr(ts, 16)?;
        let sec = unsafe { *(ts as *const i64) };
        let nsec = unsafe { *((ts + 8) as *const i64) };
        sec.saturating_mul(1000).saturating_add(nsec / 1_000_000)
    };
    sys_poll(fds, nfds, ms as u64)
}

// ─── socketpair (53) + statx (332) ───────────────────────────────────────────

const AF_UNIX: u64 = 1;
const SOCK_STREAM: u64 = 1;
const SOCK_TYPE_MASK: u64 = 0xf;
const SOCK_NONBLOCK: u64 = 0x800;
const SOCK_CLOEXEC: u64 = 0x80000;

/// `socketpair` (53): только AF_UNIX/SOCK_STREAM — канал, который libuv строит
/// для msgpack-rpc-сервера `nvim --embed`, dup2'нутый на stdio ребёнка. Опирается
/// на две перекрёстно связанные ядерные байтовые очереди (FdTable::socketpair),
/// поэтому оба дескриптора читаемы и записываемы.
pub fn sys_socketpair(domain: u64, sock_type: u64, protocol: u64, sv: u64) -> Result<u64, Errno> {
    if domain != AF_UNIX || protocol != 0 {
        return Err(Errno::EINVAL);
    }
    if sock_type & SOCK_TYPE_MASK != SOCK_STREAM
        || sock_type & !(SOCK_TYPE_MASK | SOCK_NONBLOCK | SOCK_CLOEXEC) != 0
    {
        return Err(Errno::EINVAL);
    }
    check_user_ptr_writable(sv, 8)?;
    let pair = compat::with_current_compat(|cs| {
        let p = cs.fds.socketpair(sock_type & SOCK_NONBLOCK != 0)?;
        if sock_type & SOCK_CLOEXEC != 0 {
            cs.fds.set_cloexec(p.0, true);
            cs.fds.set_cloexec(p.1, true);
        }
        Ok::<(u32, u32), Errno>(p)
    })
    .ok_or(Errno::EBADF)??;
    let words = [pair.0, pair.1];
    // SAFETY: два u32-дескриптора — ровно те 8 байт, которые socketpair пишет в sv.
    let bytes = unsafe { core::slice::from_raw_parts(words.as_ptr() as *const u8, 8) };
    copy_out(sv, bytes)?;
    Ok(0)
}

const STATX_BASIC_STATS: u32 = 0x7ff;
const AT_EMPTY_PATH: u64 = 0x1000;
const STATX_SIZE_BYTES: u64 = 256;

/// Заполнить обнулённый `struct statx` (256 байт) базовыми полями, которые
/// отслеживает pagh. Метки времени остаются нулевыми — старый путь `stat`
/// сообщает те же.
fn write_statx(buf: u64, size: u64, mode: u32, ino: u64) {
    // SAFETY: вызывающий проверил `buf` на STATX_SIZE_BYTES.
    unsafe {
        core::ptr::write_bytes(buf as *mut u8, 0, STATX_SIZE_BYTES as usize);
        core::ptr::write_unaligned(buf as *mut u32, STATX_BASIC_STATS); // stx_mask
        core::ptr::write_unaligned((buf + 0x04) as *mut u32, 4096); // stx_blksize
        core::ptr::write_unaligned((buf + 0x10) as *mut u32, 1); // stx_nlink
        core::ptr::write_unaligned((buf + 0x1c) as *mut u16, mode as u16); // stx_mode
        core::ptr::write_unaligned((buf + 0x20) as *mut u64, ino); // stx_ino
        core::ptr::write_unaligned((buf + 0x28) as *mut u64, size); // stx_size
        core::ptr::write_unaligned((buf + 0x30) as *mut u64, (size + 511) / 512);
        // stx_blocks
    }
}

/// `statx` (332): переводится на существующую обвязку stat. Поддерживает
/// обычный поиск по пути (абсолютному или относительно cwd; любой dirfd
/// трактуется как AT_FDCWD, как в `openat`) и fd-форму `AT_EMPTY_PATH`, которую
/// glibc использует для запросов в стиле fstat. `uv_fs_fstat` из libuv сначала
/// пробует statx и откатывается только при чистом ENOSYS от диспетчера —
/// которого ему больше не нужно.
pub fn sys_statx(dirfd: u64, path: u64, flags: u64, _mask: u64, buf: u64) -> Result<u64, Errno> {
    check_user_ptr_writable(buf, STATX_SIZE_BYTES)?;
    let p = read_user_cstr(path)?;
    if p.is_empty() && flags & AT_EMPTY_PATH != 0 {
        return match resolve_fd(dirfd as u32) {
            None => Err(Errno::EBADF),
            Some(Resolved::File { node, .. }) => {
                write_statx(buf, node.size(), stat_mode_for(&node), node_ino(&node));
                Ok(0)
            }
            Some(Resolved::Dir) => {
                write_statx(buf, 0, S_IFDIR | 0o700, 1);
                Ok(0)
            }
            Some(Resolved::Socket { .. }) => {
                write_statx(buf, 0, S_IFSOCK | 0o666, 1);
                Ok(0)
            }
            Some(_) => {
                write_statx(buf, 0, S_IFCHR | 0o620, 1);
                Ok(0)
            }
        };
    }
    let abs = resolve_path(&p);
    let node = walk(&abs, true)?;
    let mode = stat_mode_for(&node);
    let ino = node_ino(&node);
    write_statx(buf, node.size(), mode, ino);
    Ok(0)
}
