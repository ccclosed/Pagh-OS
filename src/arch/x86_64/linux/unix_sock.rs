//! Минимальные AF_UNIX stream-сокеты —
//! socket(41)/connect(42)/accept(43)/bind(49)/listen(50)/getsockname(51)/accept4(288).
//!
//! Слушатели живут в глобальном реестре по пути; соединение — пара
//! перекрёстно связанных ядерных байтовых очередей (тот же примитив, что
//! socketpair(53)). Это ровно та поверхность, которая нужна nvim/libuv для
//! `server_start` (uv_pipe bind/listen/accept) и `nvim --remote` (connect).
#![allow(dead_code)]

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use super::cmsg;
use super::errno::Errno;
use super::{check_user_ptr, check_user_ptr_writable};
use crate::sync::spinlock::Spinlock;
use crate::task::compat;
use crate::task::fd::{
    socket_pair_endpoints, OpenObject, PassedFd, PipeEndpoint, PipeReadResult, PipeWriteResult,
    ScmReadResult, UnixListenerState,
};

const AF_UNIX: u64 = 1;
const SOCK_STREAM: u64 = 1;
const SOCK_TYPE_MASK: u64 = 0xf;
const SOCK_NONBLOCK: u64 = 0x800;
const SOCK_CLOEXEC: u64 = 0x8_0000;
const SUN_PATH_MAX: usize = 108;
const SOL_SOCKET: u64 = 1;
const SO_TYPE: u64 = 3;
const SO_ERROR: u64 = 4;
const SO_ACCEPTCONN: u64 = 30;

/// Глобальный реестр слушателей: привязанный путь sockaddr_un -> общее
/// состояние слушателя. Побеждает самый новый bind для пути; записи давно
/// закрытых слушателей перезаписываются при повторном bind (пути сокетов на
/// практике уникальны для запуска).
static LISTENERS: Spinlock<BTreeMap<String, Arc<UnixListenerState>>> =
    Spinlock::new(BTreeMap::new());

/// Разобрать `struct sockaddr_un` из памяти пользователя в его путь.
/// Абстрактное пространство имён (ведущий NUL) и autobind (пустое имя)
/// отвергаются.
fn read_sockaddr_un(addr: u64, addrlen: u64) -> Result<String, Errno> {
    if addrlen < 2 || addrlen as usize > 2 + SUN_PATH_MAX + 8 {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(addr, addrlen)?;
    let family = unsafe { core::ptr::read_unaligned(addr as *const u16) };
    if family as u64 != AF_UNIX {
        return Err(Errno::EINVAL);
    }
    let max = core::cmp::min(addrlen.saturating_sub(2) as usize, SUN_PATH_MAX);
    let mut path: Vec<u8> = Vec::new();
    for i in 0..max {
        let b = unsafe { core::ptr::read((addr + 2 + i as u64) as *const u8) };
        if b == 0 {
            break;
        }
        path.push(b);
    }
    if path.is_empty() {
        return Err(Errno::EINVAL);
    }
    String::from_utf8(path).map_err(|_| Errno::EINVAL)
}

/// `socket` (41): только AF_UNIX + SOCK_STREAM (на стороне Linux нет сетевого
/// стека). SOCK_NONBLOCK запоминается и применяется, когда сокет становится
/// слушателем или соединением; SOCK_CLOEXEC сразу помечает fd.
pub fn sys_socket(domain: u64, ty: u64, _protocol: u64) -> Result<u64, Errno> {
    if domain != AF_UNIX {
        return Err(Errno::EINVAL);
    }
    if ty & SOCK_TYPE_MASK != SOCK_STREAM {
        return Err(Errno::EINVAL);
    }
    if ty & !(SOCK_TYPE_MASK | SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    compat::with_current_compat(|cs| {
        let fd = cs.fds.alloc_with_flags(
            OpenObject::UnixSocketUnbound {
                nonblocking: ty & SOCK_NONBLOCK != 0,
            },
            crate::task::fd::O_RDWR | (ty & SOCK_NONBLOCK),
        )?;
        if ty & SOCK_CLOEXEC != 0 {
            cs.fds.set_cloexec(fd, true);
        }
        Ok(fd as u64)
    })
    .unwrap_or(Err(Errno::EBADF))
}

/// `bind` (49): назвать сокет и зарегистрировать его в реестре слушателей.
pub fn sys_bind(fd: u64, addr: u64, addrlen: u64) -> Result<u64, Errno> {
    let path = read_sockaddr_un(addr, addrlen)?;
    let state = compat::with_current_compat(|cs| match cs.fds.get_mut(fd as u32) {
        None => Err(Errno::EBADF),
        Some(obj) => {
            let nb = match obj {
                OpenObject::UnixSocketUnbound { nonblocking } => *nonblocking,
                _ => return Err(Errno::EINVAL),
            };
            let state = Arc::new(UnixListenerState::new(path.clone(), nb));
            *obj = OpenObject::UnixListener(Arc::clone(&state));
            Ok(state)
        }
    })
    .unwrap_or(Err(Errno::EBADF))?;
    LISTENERS.lock().insert(path, state);
    Ok(0)
}

/// `listen` (50): открыть ворота для connect(2). Backlog неограничен.
pub fn sys_listen(fd: u64, _backlog: u64) -> Result<u64, Errno> {
    compat::with_current_compat(|cs| match cs.fds.get(fd as u32) {
        Some(OpenObject::UnixListener(l)) => {
            l.inner.lock().listening = true;
            Ok(0)
        }
        Some(_) => Err(Errno::EINVAL),
        None => Err(Errno::EBADF),
    })
    .unwrap_or(Err(Errno::EBADF))
}

/// `connect` (42): найти путь в реестре, поставить серверную пару концов в
/// очередь слушателя и сразу стать подключённым сокетом (у ядерных потоков нет
/// задержки рукопожатия). ENOENT заменяет ECONNREFUSED, когда никто не
/// слушает.
pub fn sys_connect(fd: u64, addr: u64, addrlen: u64) -> Result<u64, Errno> {
    let path = read_sockaddr_un(addr, addrlen)?;
    let listener = LISTENERS.lock().get(&path).cloned().ok_or(Errno::ENOENT)?;
    compat::with_current_compat(|cs| match cs.fds.get_mut(fd as u32) {
        None => Err(Errno::EBADF),
        Some(obj) => {
            let nb = match obj {
                OpenObject::UnixSocketUnbound { nonblocking } => *nonblocking,
                _ => return Err(Errno::EINVAL),
            };
            let mut inner = listener.inner.lock();
            if !inner.listening {
                return Err(Errno::ENOENT);
            }
            let ((crx, ctx), server) = socket_pair_endpoints(nb, false);
            inner.pending.push_back(server);
            drop(inner);
            *obj = OpenObject::Socket { rx: crx, tx: ctx };
            Ok(0)
        }
    })
    .unwrap_or(Err(Errno::EBADF))
}

/// `accept4` (288): вынуть подключение из очереди в свежий fd. Соблюдает
/// O_NONBLOCK слушателя (EAGAIN) и флаги SOCK_NONBLOCK/SOCK_CLOEXEC принятого
/// дескриптора; иначе блокируется, уступая CPU.
pub fn sys_accept4(fd: u64, addr: u64, addrlen_ptr: u64, flags: u64) -> Result<u64, Errno> {
    if flags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    loop {
        let attempt = compat::with_current_compat(|cs| {
            let listener = match cs.fds.get(fd as u32) {
                Some(OpenObject::UnixListener(l)) => Arc::clone(l),
                Some(_) => return Err(Errno::EINVAL),
                None => return Err(Errno::EBADF),
            };
            let mut inner = listener.inner.lock();
            if let Some((rx, tx)) = inner.pending.pop_front() {
                let nb = flags & SOCK_NONBLOCK != 0;
                let rx = rx.with_nonblocking(nb);
                let tx = tx.with_nonblocking(nb);
                drop(inner);
                let newfd = cs.fds.alloc_with_flags(
                    OpenObject::Socket { rx, tx },
                    crate::task::fd::O_RDWR | (flags & SOCK_NONBLOCK),
                )?;
                if flags & SOCK_CLOEXEC != 0 {
                    cs.fds.set_cloexec(newfd, true);
                }
                Ok(Some(newfd as u64))
            } else if inner.nonblocking {
                Err(Errno::EAGAIN)
            } else {
                Ok(None)
            }
        })
        .unwrap_or(Err(Errno::EBADF))?;
        match attempt {
            Some(newfd) => {
                // Партнёр принятого AF_UNIX-соединения безымянный: сообщаем
                // только семейство, как Linux.
                if addr != 0 && addrlen_ptr != 0 && check_user_ptr_writable(addrlen_ptr, 4).is_ok()
                {
                    let want = unsafe { core::ptr::read_unaligned(addrlen_ptr as *const u32) };
                    if want >= 2 && check_user_ptr_writable(addr, 2).is_ok() {
                        unsafe { core::ptr::write_unaligned(addr as *mut u16, AF_UNIX as u16) };
                    }
                    unsafe { core::ptr::write_unaligned(addrlen_ptr as *mut u32, 2) };
                }
                return Ok(newfd);
            }
            None => crate::task::scheduler::yield_current(),
        }
    }
}

/// `accept` (43): accept4 без флагов.
pub fn sys_accept(fd: u64, addr: u64, addrlen_ptr: u64) -> Result<u64, Errno> {
    sys_accept4(fd, addr, addrlen_ptr, 0)
}

/// `getsockname` (51): сообщить привязанный путь для слушателей и безымянный
/// адрес для подключённых/непривязанных сокетов (uv использует это, чтобы
/// напечатать имя pipe).
pub fn sys_getsockname(fd: u64, addr: u64, addrlen_ptr: u64) -> Result<u64, Errno> {
    check_user_ptr_writable(addrlen_ptr, 4)?;
    let path: Option<String> = compat::with_current_compat(|cs| match cs.fds.get(fd as u32) {
        Some(OpenObject::UnixListener(l)) => Ok(Some(l.path.clone())),
        Some(OpenObject::Socket { .. }) | Some(OpenObject::UnixSocketUnbound { .. }) => Ok(None),
        Some(_) => Err(Errno::EINVAL),
        None => Err(Errno::EBADF),
    })
    .unwrap_or(Err(Errno::EBADF))?;
    let mut out: Vec<u8> = Vec::with_capacity(2 + SUN_PATH_MAX + 1);
    out.extend_from_slice(&(AF_UNIX as u16).to_le_bytes());
    if let Some(p) = &path {
        out.extend_from_slice(p.as_bytes());
        out.push(0);
    }
    let full = out.len() as u32;
    let want = unsafe { core::ptr::read_unaligned(addrlen_ptr as *const u32) } as usize;
    let n = core::cmp::min(want, out.len());
    if n > 0 {
        check_user_ptr_writable(addr, n as u64)?;
        unsafe { core::ptr::copy_nonoverlapping(out.as_ptr(), addr as *mut u8, n) };
    }
    unsafe { core::ptr::write_unaligned(addrlen_ptr as *mut u32, full) };
    Ok(0)
}

/// `setsockopt` (54): принимается и игнорируется. У ядерной пары AF_UNIX нет
/// настраиваемых ручек (ни размеров буферов, ни передачи креденшелов), а libuv
/// ставит на свои pipe только косметические опции; сообщить успех — то
/// Linux-видимое поведение, которого ждут вызывающие.
pub fn sys_setsockopt(
    _fd: u64,
    _level: u64,
    _optname: u64,
    _optval: u64,
    _optlen: u64,
) -> Result<u64, Errno> {
    Ok(0)
}

/// `getsockopt` (55): отвечаем только `SOL_SOCKET`/`SO_ERROR` — «нет
/// ожидающей асинхронной ошибки» (0), что libuv опрашивает после неблокирующего
/// connect. Всё остальное — EINVAL, чтобы вызывающие откатывались к умолчаниям,
/// а не разбирали неинициализированную память.
pub fn sys_getsockopt(
    _fd: u64,
    level: u64,
    optname: u64,
    optval: u64,
    optlen: u64,
) -> Result<u64, Errno> {
    if level != SOL_SOCKET {
        return Err(Errno::EINVAL);
    }
    // SO_TYPE — то, что uv_guess_handle из libuv спрашивает сразу после того,
    // как fstat сообщил S_IFSOCK для stdio-дескрипторов канала встроенного nvim.
    // Отказ на нём делал весь канал UV_UNKNOWN_HANDLE и вешал рукопожатие
    // msgpack-RPC (TUI вечно ждал на мёртвом pipe).
    let value: u32 = match optname {
        SO_ERROR => 0,      // нет ожидающей асинхронной ошибки
        SO_TYPE => 1,       // SOCK_STREAM
        SO_ACCEPTCONN => 0, // «не слушающий сокет» нормально для проб
        _ => return Err(Errno::EINVAL),
    };
    check_user_ptr_writable(optlen, 4)?;
    let want = unsafe { core::ptr::read_unaligned(optlen as *const u32) };
    if want < 4 {
        return Err(Errno::EINVAL);
    }
    check_user_ptr_writable(optval, 4)?;
    unsafe {
        core::ptr::write_unaligned(optval as *mut u32, value);
        core::ptr::write_unaligned(optlen as *mut u32, 4u32);
    }
    Ok(0)
}

// ─── sendmsg (46) / recvmsg (47): AF_UNIX SCM_RIGHTS ─────────────────────────

/// Флаги `sendmsg`, которые принимает AF_UNIX SOCK_STREAM.
///
/// `MSG_OOB`/`MSG_PEEK` не входят: молча их проигнорировать значило бы
/// отправить/принять не то, что попросили, а Linux на stream-сокете для
/// `MSG_OOB` отвечает `EOPNOTSUPP`.
const SENDMSG_FLAGS: u64 = (cmsg::MSG_DONTWAIT | cmsg::MSG_EOR | cmsg::MSG_NOSIGNAL) as u64;
/// Флаги `recvmsg`: разовая неблокирующая операция и close-on-exec для
/// принятых дескрипторов.
const RECVMSG_FLAGS: u64 = (cmsg::MSG_DONTWAIT | cmsg::MSG_CMSG_CLOEXEC) as u64;
/// Флаги `recvfrom` на подключённый stream: только разовая неблокируемость.
const RECVFROM_FLAGS: u64 = cmsg::MSG_DONTWAIT as u64;

/// `struct msghdr` x86_64 (56 байт), прочитанный из пользовательской памяти.
struct Msghdr {
    /// `msg_name` (для подключённого AF_UNIX stream игнорируется).
    name: u64,
    /// `msg_namelen`.
    namelen: u32,
    /// `msg_iov` — массив `struct iovec`.
    iov: u64,
    /// `msg_iovlen`.
    iovlen: u64,
    /// `msg_control` — буфер ancillary data.
    control: u64,
    /// `msg_controllen` — его ёмкость.
    controllen: u64,
}

/// Прочитать `struct msghdr` из пользовательской памяти после проверки всех
/// 56 байт. Поля читаются `read_unaligned`: `msghdr` в C-программах выровнен,
/// но контракт syscall-слоя не полагается на выравнивание указателя.
fn read_msghdr(addr: u64) -> Result<Msghdr, Errno> {
    check_user_ptr(addr, cmsg::MSGHDR_SIZE as u64)?;
    // SAFETY: `check_user_ptr` подтвердил все 56 байт в пользовательской
    // половине; поля читаются невыровненно и не выходят за этот диапазон.
    unsafe {
        Ok(Msghdr {
            name: core::ptr::read_unaligned(addr as *const u64),
            namelen: core::ptr::read_unaligned((addr + cmsg::MSGHDR_NAMELEN_OFF) as *const u32),
            iov: core::ptr::read_unaligned((addr + cmsg::MSGHDR_IOV_OFF) as *const u64),
            iovlen: core::ptr::read_unaligned((addr + cmsg::MSGHDR_IOVLEN_OFF) as *const u64),
            control: core::ptr::read_unaligned((addr + cmsg::MSGHDR_CONTROL_OFF) as *const u64),
            controllen: core::ptr::read_unaligned(
                (addr + cmsg::MSGHDR_CONTROLLEN_OFF) as *const u64,
            ),
        })
    }
}

/// Записать назад поля `struct msghdr`, которые ядро обязано обновить:
/// `msg_namelen`, `msg_controllen`, `msg_flags`. Указатель перепроверяется
/// здесь, а не только на входе: вызов мог блокироваться и уступать CPU, а
/// сосед по `CLONE_VM` — снять страницу (TOCTOU-контракт `io_sys`).
fn write_msghdr_out(addr: u64, namelen: u32, controllen: u64, flags: u32) -> Result<(), Errno> {
    check_user_ptr_writable(addr, cmsg::MSGHDR_SIZE as u64)?;
    // SAFETY: диапазон 56 байт проверен на запись; три поля пишутся
    // невыровненно внутри него.
    unsafe {
        core::ptr::write_unaligned((addr + cmsg::MSGHDR_NAMELEN_OFF) as *mut u32, namelen);
        core::ptr::write_unaligned((addr + cmsg::MSGHDR_CONTROLLEN_OFF) as *mut u64, controllen);
        core::ptr::write_unaligned((addr + cmsg::MSGHDR_FLAGS_OFF) as *mut u32, flags);
    }
    Ok(())
}

/// Собрать данные `sendmsg` из массива `struct iovec` (16 байт: base + len).
///
/// Предел — [`super::abi::MAX_SOCKET_IO`]: sum длин больше него отвергается
/// `EMSGSIZE` до аллокации (тот же класс, что `socket_payload_len` у
/// `sendto`/`recvfrom` — нельзя дать вызывающему заказать аллокацию сырым
/// счётчиком).
fn gather_iovecs(iov: u64, iovlen: u64) -> Result<Vec<u8>, Errno> {
    if iovlen == 0 {
        return Ok(Vec::new());
    }
    let entry_bytes = cmsg::IOV_SIZE as u64;
    check_user_ptr(iov, iovlen * entry_bytes)?;
    let mut total: u64 = 0;
    for i in 0..iovlen {
        let entry = iov + i * entry_bytes;
        // Перепроверка на каждой итерации: массив iovec принадлежит
        // пользователю и мог исчезнуть между чтениями (TOCTOU-контракт io_sys).
        check_user_ptr(entry, entry_bytes)?;
        // SAFETY: 16-байтная запись проверена выше; читается поле iov_len.
        let len = unsafe { core::ptr::read_unaligned((entry + 8) as *const u64) };
        total = total.checked_add(len).ok_or(Errno::EMSGSIZE)?;
        if total > super::abi::MAX_SOCKET_IO {
            return Err(Errno::EMSGSIZE);
        }
    }
    let mut data: Vec<u8> = Vec::with_capacity(total as usize);
    for i in 0..iovlen {
        let entry = iov + i * entry_bytes;
        check_user_ptr(entry, entry_bytes)?;
        // SAFETY: запись iovec проверена выше; поля внутри 16 байт.
        let base = unsafe { core::ptr::read_unaligned(entry as *const u64) };
        let len = unsafe { core::ptr::read_unaligned((entry + 8) as *const u64) };
        if len == 0 {
            continue;
        }
        check_user_ptr(base, len)?;
        data.extend_from_slice(&super::io_sys::copy_in_pub(base, len));
    }
    Ok(data)
}

/// Суммарная ёмкость приёмных iovec, урезанная до `MAX_SOCKET_IO`.
///
/// Ёмкость больше предела не ошибка: Linux принимает буфер больше сообщения,
/// а ядро просто не выделяет/не копирует сверх своего предела.
fn recv_capacity(iov: u64, iovlen: u64) -> Result<u64, Errno> {
    if iovlen == 0 {
        return Ok(0);
    }
    let entry_bytes = cmsg::IOV_SIZE as u64;
    check_user_ptr(iov, iovlen * entry_bytes)?;
    let mut total: u64 = 0;
    for i in 0..iovlen {
        let entry = iov + i * entry_bytes;
        check_user_ptr(entry, entry_bytes)?;
        // SAFETY: 16-байтная запись iovec проверена.
        let len = unsafe { core::ptr::read_unaligned((entry + 8) as *const u64) };
        total = total.saturating_add(len);
    }
    Ok(core::cmp::min(total, super::abi::MAX_SOCKET_IO))
}

/// Разложить принятые байты по пользовательским iovec.
///
/// Недостача ёмкости (iovec-массив уменьшили между подсчётом и копией) —
/// `EFAULT`, а не молча усечённый результат.
fn scatter_iovecs(iov: u64, iovlen: u64, data: &[u8]) -> Result<(), Errno> {
    let mut off = 0usize;
    for i in 0..iovlen {
        if off == data.len() {
            break;
        }
        let entry = iov + i * cmsg::IOV_SIZE as u64;
        check_user_ptr(entry, cmsg::IOV_SIZE as u64)?;
        // SAFETY: запись iovec проверена; поля внутри 16 байт.
        let base = unsafe { core::ptr::read_unaligned(entry as *const u64) };
        let len = unsafe { core::ptr::read_unaligned((entry + 8) as *const u64) } as usize;
        let n = core::cmp::min(len, data.len() - off);
        if n > 0 {
            // copy_out_pub сам перепроверяет право записи (TOCTOU).
            super::io_sys::copy_out_pub(base, &data[off..off + n])?;
            off += n;
        }
    }
    if off != data.len() {
        return Err(Errno::EFAULT);
    }
    Ok(())
}

/// Установить принятые объекты в fd-таблицу получателя.
///
/// Каждый `SCM_RIGHTS`-fd — отдельный dup: получатель получает НОВЫЙ номер, а
/// отправитель сохраняет свой. `cloexec` — семантика `MSG_CMSG_CLOEXEC`
/// (дескрипторы вычищаются при `execve`, `task/process.rs` зовёт
/// `FdTable::close_cloexec`). Если таблица кончилась, уже установленные
/// дескрипторы закрываются — частичной доставки не бывает.
fn install_received(objects: Vec<PassedFd>, cloexec: bool) -> Result<Vec<u32>, Errno> {
    let mut installed: Vec<u32> = Vec::with_capacity(objects.len());
    let mut iter = objects.into_iter();
    let res = compat::with_current_compat(|cs| {
        for passed in iter {
            match cs.fds.alloc_with_flags(passed.object, passed.flags) {
                Ok(nfd) => {
                    if cloexec {
                        cs.fds.set_cloexec(nfd, true);
                    }
                    installed.push(nfd);
                }
                Err(e) => {
                    for fd in installed.drain(..) {
                        let _ = cs.fds.close(fd);
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    });
    match res {
        Some(Ok(())) => Ok(installed),
        Some(Err(e)) => Err(e),
        // Дойти сюда можно только без compat-состояния; установленных fd нет.
        None => Err(Errno::EBADF),
    }
}

/// Цель `sendmsg`/`recvmsg`, сведённая к одному из поддерживаемых стеков.
enum MsgTarget {
    /// Конец AF_UNIX socketpair: `tx` — очередь получателя.
    UnixTx(Arc<PipeEndpoint>),
    /// AF_INET TCP: fd passing не поддержан, данные — через свой стек.
    InetTcp,
    /// AF_INET UDP: данные/адрес — через свой стек.
    InetUdp,
}

/// `sendmsg` (46): данные и/или `SCM_RIGHTS` в AF_UNIX SOCK_STREAM.
///
/// Поддержано:
///  * данные из `msg_iov` (короткая запись и блокировка — как `write(2)`);
///  * cmsg `SOL_SOCKET`/`SCM_RIGHTS` с массивом `int fd` — несколько fd и
///    несколько cmsg в одном вызове, суммарно до [`cmsg::SCM_MAX_FD`];
///  * каждый fd валидируется в таблице ОТПРАВИТЕЛЯ (`EBADF`), получателю
///    уходит dup — отправитель свой дескриптор не теряет;
///  * `MSG_DONTWAIT`/`MSG_NOSIGNAL`/`MSG_EOR`.
///
/// Отказы: `EBADF` (нет fd или передан несуществующий fd), `ENOTSOCK` (не
/// сокет), `ENOTCONN` (AF_UNIX не подключён), `EINVAL` (битые/неизвестные
/// cmsg, неизвестные флаги), `ENOBUFS` (control-буфер больше предела),
/// `EMSGSIZE` (iovec больше 1 МиБ), `EAGAIN`/`EPIPE`.
pub fn sys_sendmsg(fd: u64, msg_addr: u64, flags: u64) -> Result<u64, Errno> {
    if flags & !SENDMSG_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    let hdr = read_msghdr(msg_addr)?;
    // Linux: size_t-поля msghdr ограничены INT_MAX; больше — ENOBUFS.
    if hdr.controllen > cmsg::INT_MAX {
        return Err(Errno::ENOBUFS);
    }
    // Собственный предел копии control-буфера (см. cmsg::MAX_CONTROL): на
    // 64 КиБ заканчивается любое законное AF_UNIX-cmsg.
    if hdr.controllen > cmsg::MAX_CONTROL as u64 {
        return Err(Errno::ENOBUFS);
    }
    // Предел ДО `iovlen * sizeof(iovec)`: сырой u64-счётчик иначе переполнял
    // бы умножение. Linux отвечает на это EMSGSIZE (UIO_MAXIOV).
    if hdr.iovlen > cmsg::IOV_MAX as u64 {
        return Err(Errno::EMSGSIZE);
    }
    let data = gather_iovecs(hdr.iov, hdr.iovlen)?;
    let fds: Vec<i32> = if hdr.controllen > 0 {
        if hdr.control == 0 {
            // Ненулевая длина при NULL-указателе: ядро читало бы cmsg из NULL.
            return Err(Errno::EFAULT);
        }
        check_user_ptr(hdr.control, hdr.controllen)?;
        let control = super::io_sys::copy_in_pub(hdr.control, hdr.controllen);
        cmsg::decode_send_control(&control).map_err(|_| Errno::EINVAL)?
    } else {
        Vec::new()
    };
    let target = compat::with_current_compat(|cs| match cs.fds.get(fd as u32) {
        None => Err(Errno::EBADF),
        Some(OpenObject::Socket { tx, .. }) => Ok(MsgTarget::UnixTx(Arc::clone(tx))),
        Some(OpenObject::UnixListener(_)) | Some(OpenObject::UnixSocketUnbound { .. }) => {
            Err(Errno::ENOTCONN)
        }
        Some(OpenObject::InetTcp(_)) => Ok(MsgTarget::InetTcp),
        Some(OpenObject::InetUdp(_)) => Ok(MsgTarget::InetUdp),
        Some(_) => Err(Errno::ENOTSOCK),
    })
    .unwrap_or(Err(Errno::EBADF))?;

    match target {
        MsgTarget::InetTcp => {
            // SCM_RIGHTS через TCP не передаётся: честный отказ вместо тихой
            // потери дескрипторов (Linux их тоже не передаёт, но здесь
            // пользователь узнает об этом сразу).
            if !fds.is_empty() {
                return Err(Errno::EINVAL);
            }
            let n = super::inet_sock::tcp_write(fd, &data)?;
            Ok(n as u64)
        }
        MsgTarget::InetUdp => {
            if !fds.is_empty() {
                return Err(Errno::EINVAL);
            }
            // Адрес данных побеждает «подключённую» семантику, как sendto(2).
            let n = if hdr.name != 0 && hdr.namelen >= 8 {
                super::inet_sock::udp_sendto_fd(fd, &data, hdr.name, hdr.namelen as u64)?
            } else {
                super::inet_sock::udp_write_fd(fd, &data)?
            };
            Ok(n as u64)
        }
        MsgTarget::UnixTx(tx) => {
            let mut objects: Option<Vec<PassedFd>> = if fds.is_empty() {
                None
            } else {
                Some(
                    compat::with_current_compat(|cs| {
                        let mut objs: Vec<PassedFd> = Vec::with_capacity(fds.len());
                        for &raw in fds.iter() {
                            if raw < 0 {
                                return Err(Errno::EBADF);
                            }
                            let obj = cs.fds.get(raw as u32).ok_or(Errno::EBADF)?;
                            // dup: получатель получит собственную запись в
                            // таблице, отправитель не теряет свой fd. Режим
                            // доступа (`f_flags`) наследуется — Linux dup его
                            // сохраняет, и PROT_WRITE mmap принятого memfd
                            // обязан остаться разрешённым.
                            let flags = cs
                                .fds
                                .open_flags(raw as u32)
                                .unwrap_or(crate::task::fd::O_RDONLY);
                            objs.push(PassedFd {
                                object: obj.dup_clone(),
                                flags,
                            });
                        }
                        Ok(objs)
                    })
                    .unwrap_or(Err(Errno::EBADF))?,
                )
            };
            // SOCK_STREAM без байтов данных: Linux не создаёт запись в потоке,
            // поэтому ancillary data не передаются (проверено: sendmsg(0 байт +
            // валидный fd) = 0, получатель ничего не видит). Валидация fd выше
            // уже отработала — Linux и на пустом сообщении отвечает EBADF за
            // неоткрытый fd, поэтому shortcut стоит после неё.
            if data.is_empty() {
                drop(objects.take());
                return Ok(0);
            }
            let nonblock = tx.nonblocking() || flags & cmsg::MSG_DONTWAIT as u64 != 0;
            let mut written = 0usize;
            loop {
                match tx.write_scm(&data[written..], &mut objects) {
                    PipeWriteResult::Data(n) => {
                        written += n;
                        if written == data.len() {
                            break;
                        }
                    }
                    PipeWriteResult::Broken => {
                        // Часть уже ушла — возвращаем короткую запись. fd к
                        // этому моменту привязаны к первому чанку; если не
                        // привязаны (written == 0), они просто не переданы.
                        if written == 0 {
                            return Err(Errno::EPIPE);
                        }
                        break;
                    }
                    PipeWriteResult::WouldBlock => {
                        if nonblock {
                            if written == 0 {
                                return Err(Errno::EAGAIN);
                            }
                            break;
                        }
                        crate::task::scheduler::yield_current();
                    }
                }
            }
            Ok(written as u64)
        }
    }
}

/// `sendto` (44) на подключённый AF_UNIX `SOCK_STREAM`.
///
/// Rust-std на static-musl реализует `UnixStream::write` через `send(...,
/// MSG_NOSIGNAL)` — это syscall `sendto`, а не `write`, поэтому без этой ветки
/// дочерний `pagh-desktop-demo` получал `EINVAL` на первом же `hello` и умирал
/// с `os error 22` (а сервер — без клиента). Поток-семантика та же, что у
/// `write`/`sendmsg` на `OpenObject::Socket`: пишем в очередь получателя, при
/// `WouldBlock` блокируемся, если не `MSG_DONTWAIT`/`O_NONBLOCK`; на закрытом
/// конце — `EPIPE`.
///
/// `dest_addr` подключённого stream-сокета не нужен: NULL/нулевая длина —
/// норма (так зовут `send`), ненулевой адрес принимается как no-op? Нет:
/// этот слой поддерживает только подключённые пары, поэтому ненулевой адрес —
/// `EINVAL` (честный отказ, а не тихое игнорирование).
pub fn sys_sendto_unix(
    fd: u64,
    buf: u64,
    len: u64,
    flags: u64,
    dest_addr: u64,
    dest_len: u64,
) -> Result<u64, Errno> {
    if flags & !SENDMSG_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    if dest_addr != 0 && dest_len != 0 {
        return Err(Errno::EINVAL);
    }
    // Поток байтов не имеет границ записи: длиннее [MAX_SOCKET_IO] — это не
    // ошибка, а короткая запись (Linux копирует сколько помещается и возвращает
    // счётчик; `write_all` вызывающего доносит остаток). Тот же предел остаётся
    // границей одной аллокации/копии.
    let len = super::abi::stream_send_len(len);
    let tx = compat::with_current_compat(|cs| match cs.fds.get(fd as u32) {
        None => Err(Errno::EBADF),
        Some(OpenObject::Socket { tx, .. }) => Ok(Arc::clone(tx)),
        Some(OpenObject::UnixListener(_)) | Some(OpenObject::UnixSocketUnbound { .. }) => {
            Err(Errno::ENOTCONN)
        }
        Some(_) => Err(Errno::ENOTSOCK),
    })
    .unwrap_or(Err(Errno::EBADF))?;
    if len == 0 {
        return Ok(0);
    }
    check_user_ptr(buf, len)?;
    let data = super::io_sys::copy_in_pub(buf, len);
    let nonblock = tx.nonblocking() || flags & cmsg::MSG_DONTWAIT as u64 != 0;
    let mut written = 0usize;
    loop {
        match tx.write(&data[written..]) {
            PipeWriteResult::Data(n) => {
                written += n;
                if written == data.len() {
                    return Ok(written as u64);
                }
            }
            PipeWriteResult::Broken => {
                return if written == 0 {
                    Err(Errno::EPIPE)
                } else {
                    Ok(written as u64)
                };
            }
            PipeWriteResult::WouldBlock => {
                if nonblock {
                    return if written == 0 {
                        Err(Errno::EAGAIN)
                    } else {
                        Ok(written as u64)
                    };
                }
                crate::task::scheduler::yield_current();
            }
        }
    }
}

/// `recvfrom` (45) на подключённый AF_UNIX `SOCK_STREAM`.
///
/// Зеркало [`sys_sendto_unix`]: Rust-std `UnixStream::read` зовёт `recv(...)`,
/// то есть `recvfrom`. У подключённой stream-пары адреса отправителя нет:
/// если вызывающий передал непустые `src_addr`/`addr_len`, ядро пишет длину 0
/// (Linux `recv` сам передаёт NULL, а libuv — NULL/0). `MSG_DONTWAIT`
/// действует на один вызов, `EPIPE`/`EAGAIN` — как у `read`.
pub fn sys_recvfrom_unix(
    fd: u64,
    buf: u64,
    len: u64,
    flags: u64,
    src_addr: u64,
    addr_len: u64,
) -> Result<u64, Errno> {
    if flags & !RECVFROM_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    // Ёмкость буфера вызывающего; ядро вправе урезать её до предела, но не
    // должно выделять по сырому u64 (та же защита, что в UDP-пути).
    let cap = super::abi::socket_payload_len(len, false)?;
    check_user_ptr_writable(buf, cap)?;
    let rx = compat::with_current_compat(|cs| match cs.fds.get(fd as u32) {
        None => Err(Errno::EBADF),
        Some(OpenObject::Socket { rx, .. }) => Ok(Arc::clone(rx)),
        Some(OpenObject::UnixListener(_)) | Some(OpenObject::UnixSocketUnbound { .. }) => {
            Err(Errno::ENOTCONN)
        }
        Some(_) => Err(Errno::ENOTSOCK),
    })
    .unwrap_or(Err(Errno::EBADF))?;
    if cap == 0 {
        return Ok(0);
    }
    let mut data: Vec<u8> = vec![0u8; cap as usize];
    let nonblock = rx.nonblocking() || flags & cmsg::MSG_DONTWAIT as u64 != 0;
    let n = loop {
        match rx.read(&mut data) {
            PipeReadResult::Data(n) => break n,
            PipeReadResult::Eof => break 0,
            PipeReadResult::WouldBlock => {
                if nonblock {
                    return Err(Errno::EAGAIN);
                }
                match endpoint_wait_verdict(&rx) {
                    super::sock_wait::WaitVerdict::Continue => {
                        crate::task::scheduler::yield_current()
                    }
                    super::sock_wait::WaitVerdict::Eof => break 0,
                    super::sock_wait::WaitVerdict::Ebadf => return Err(Errno::EBADF),
                }
            }
        }
    };
    super::io_sys::copy_out_pub(buf, &data[..n])?;
    if src_addr != 0 && addr_len != 0 {
        // Подключённый stream-сокет не имеет адреса отправителя.
        check_user_ptr_writable(addr_len, 4)?;
        // SAFETY: addr_len — 4-байтный socklen_t, проверен выше; запись
        // допускает невыровненность.
        unsafe { core::ptr::write_unaligned(addr_len as *mut u32, 0) };
    }
    Ok(n as u64)
}

/// Решение для блокирующего чтения, у которого очередь ответила `WouldBlock`.
///
/// Очередей ожидания в ядре нет: чтение уступает CPU и перечитывает очередь.
/// Но закрытие ЧИТАЮЩЕГО fd из соседнего потока не выставляет EOF — счётчики
/// `readers`/`writers` живут на общую очередь, а сам syscall держит её `Arc`.
/// Без этой перепроверки задача уступала бы CPU вечно; watchdog печатал
/// `stuck in syscall ? (nr=45 arg0=0x3 closed)` (см. `super::sock_wait`).
///
/// `dup` другого дескриптора на ту же очередь оставляет владение, поэтому
/// закрытие одного из дубликатов законное ожидание не прерывает.
pub(crate) fn endpoint_wait_verdict(rx: &PipeEndpoint) -> super::sock_wait::WaitVerdict {
    let id = rx.queue_id();
    // `None` (нет compat-состояния) — читать эту очередь больше некому: EBADF.
    let referenced = compat::with_current_compat(|cs| cs.fds.holds_read_end(id)).unwrap_or(false);
    super::sock_wait::wait_verdict(referenced, rx.writers())
}

/// Куда свёлся `shutdown(2)`: подключённая AF_UNIX-пара или TCP.
///
/// Снимок `Arc` берётся под compat-локом и отпускается до любых блокирующих
/// действий (инвариант 4 из `AGENTS.md`): сама операция только ставит флаг и,
/// для TCP, отправляет FIN.
enum ShutTarget {
    /// Конец socketpair: `rx` — очередь, которую он читает, `tx` — в которую
    /// пишет.
    UnixStream {
        rx: Arc<PipeEndpoint>,
        tx: Arc<PipeEndpoint>,
    },
    /// AF_INET TCP: `shutdown` идёт в собственный стек.
    Tcp(Arc<super::inet_sock::InetTcp>),
}

/// `shutdown` (48): закрыть направление подключённого сокета.
///
/// Linux-семантику закрепляет чистая политика [`super::sock_shutdown`]:
///  * `SHUT_WR` — пишущая половина закрыта: пир-читатель получает `EOF`
///    (`Ok(0)`), собственная запись — `EPIPE`. У AF_UNIX ставится флаг очереди
///    (`PipeState::write_shut`), поэтому блокирующийся в `yield_current()`
///    читатель просыпается на следующем перечитывании; у TCP уходит FIN
///    (`net::tcp_shutdown_wr`), а сокет остаётся в таблице стека и принимает
///    дальше;
///  * `SHUT_RD` — приём закрыт: последующие `recv`/`read` отвечают `Ok(0)`, а
///    пир при записи в AF_UNIX-очередь получает `EPIPE` (никто не заберёт
///    байты);
///  * `SHUT_RDWR` — обе половины.
///
/// Отказы в порядке Linux (проверено пробой на хосте): `EBADF` (нет fd),
/// `ENOTSOCK` (не сокет: файл, pipe, консоль), `EINVAL` (не 0/1/2 — даже у
/// несоединённого сокета), `ENOTCONN` (TCP до `connect`, UDP). AF_UNIX без
/// соединения (несвязанный, слушающий) Linux НЕ отвергает: там закрывать
/// нечего, ответ `0`. Повторный вызов идемпотентен и успешен.
pub fn sys_shutdown(fd: u64, how: u64) -> Result<u64, Errno> {
    use super::sock_shutdown::{self, Action, FdKind};

    // Снимок: вид дескриптора и (для подключённых сокетов) его объект. Лок
    // отпускается здесь же.
    let (kind, target) = compat::with_current_compat(|cs| match cs.fds.get(fd as u32) {
        None => (FdKind::Absent, None),
        Some(OpenObject::Socket { rx, tx }) => (
            FdKind::UnixConnected,
            Some(ShutTarget::UnixStream {
                rx: Arc::clone(rx),
                tx: Arc::clone(tx),
            }),
        ),
        Some(OpenObject::InetTcp(sock)) => {
            let kind = if sock.connected.load(Ordering::Relaxed) {
                FdKind::TcpConnected
            } else {
                FdKind::TcpUnconnected
            };
            (kind, Some(ShutTarget::Tcp(Arc::clone(sock))))
        }
        // AF_UNIX без соединения — успешный no-op (как Linux); UDP и TCP до
        // connect политика отвергает через `FdKind`.
        Some(OpenObject::UnixListener(_)) | Some(OpenObject::UnixSocketUnbound { .. }) => {
            (FdKind::UnixUnconnected, None)
        }
        Some(OpenObject::InetUdp(_)) => (FdKind::Udp, None),
        Some(_) => (FdKind::NotSocket, None),
    })
    .unwrap_or((FdKind::Absent, None));

    let action = sock_shutdown::plan(kind, how)?;
    match action {
        Action::Noop => Ok(0),
        Action::Unix { read, write } => {
            if let Some(ShutTarget::UnixStream { rx, tx }) = target {
                if read {
                    rx.shutdown_read();
                }
                if write {
                    tx.shutdown_write();
                }
            }
            Ok(0)
        }
        Action::Tcp { read, write } => {
            if let Some(ShutTarget::Tcp(sock)) = target {
                super::inet_sock::tcp_apply_shutdown(&sock, read, write);
            }
            Ok(0)
        }
    }
}

/// `recvmsg` (47): данные и `SCM_RIGHTS` из AF_UNIX SOCK_STREAM.
///
/// Поведение повторяет Linux (проверено пробником на хосте, см.
/// `docs/desktop-b.md` §6.4):
///  * данные до позиции cmsg читаются как обычный поток; войдя в чанк с cmsg,
///    вызов забирает дескрипторы и не идёт дальше границы этого чанка;
///  * cmsg без места в control-буфере не пишется, а fd закрываются, при этом
///    выставляется `MSG_CTRUNC`; частичное размещение доставляет столько fd,
///    сколько влезло;
///  * `msg_controllen` на выходе — использованная ёмкость (Linux-значение);
///  * `msg_flags` на выходе — входные флаги плюс `MSG_CTRUNC`;
///  * `msg_namelen = 0` — у подключённого AF_UNIX stream адреса нет;
///  * `MSG_CMSG_CLOEXEC` помечает принятые fd `FD_CLOEXEC`.
///
/// Отказы: `EBADF`, `ENOTSOCK`, `ENOTCONN`, `EINVAL` (флаги/iovec), `ENOBUFS`
/// (control-буфер больше INT_MAX), `EAGAIN`, `EMFILE` (fd-таблица получателя),
/// `EFAULT`.
pub fn sys_recvmsg(fd: u64, msg_addr: u64, flags: u64) -> Result<u64, Errno> {
    if flags & !RECVMSG_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    let hdr = read_msghdr(msg_addr)?;
    if hdr.controllen > cmsg::INT_MAX {
        return Err(Errno::ENOBUFS);
    }
    if hdr.iovlen > cmsg::IOV_MAX as u64 {
        return Err(Errno::EMSGSIZE);
    }
    let cap = recv_capacity(hdr.iov, hdr.iovlen)?;
    let rx = compat::with_current_compat(|cs| match cs.fds.get(fd as u32) {
        None => Err(Errno::EBADF),
        Some(OpenObject::Socket { rx, .. }) => Ok(Arc::clone(rx)),
        Some(OpenObject::UnixListener(_)) | Some(OpenObject::UnixSocketUnbound { .. }) => {
            Err(Errno::ENOTCONN)
        }
        Some(_) => Err(Errno::ENOTSOCK),
    })
    .unwrap_or(Err(Errno::EBADF))?;
    if cap == 0 {
        // Пустой приёмный iovec: Linux возвращает 0, ничего не потребляя.
        write_msghdr_out(msg_addr, 0, 0, flags as u32)?;
        return Ok(0);
    }
    let mut buf: Vec<u8> = vec![0u8; cap as usize];
    let nonblock = rx.nonblocking() || flags & cmsg::MSG_DONTWAIT as u64 != 0;
    loop {
        match rx.read_scm(&mut buf) {
            ScmReadResult::Data(scm) => {
                // 1. Данные — в iovec вызывающего (ошибка здесь ещё не
                //    устанавливает дескрипторы, так что утечки нет).
                scatter_iovecs(hdr.iov, hdr.iovlen, &buf[..scm.n])?;
                // 2. Дескрипторы — в таблицу получателя, лишние — закрыть.
                let mut out_flags = flags as u32;
                let mut used = 0u64;
                if !scm.objects.is_empty() {
                    let capacity = if hdr.control == 0 {
                        0
                    } else {
                        hdr.controllen as usize
                    };
                    let plan = cmsg::plan_rights(scm.objects.len(), capacity);
                    if plan.truncated {
                        out_flags |= cmsg::MSG_CTRUNC;
                    }
                    let mut deliver = scm.objects;
                    let extra = deliver.split_off(plan.deliver);
                    drop(extra); // непоместившиеся fd закрываются (Linux так же)
                    let installed =
                        install_received(deliver, flags & cmsg::MSG_CMSG_CLOEXEC as u64 != 0)?;
                    if plan.deliver > 0 {
                        let fds_i32: Vec<i32> = installed.iter().map(|&f| f as i32).collect();
                        let mut control: Vec<u8> = vec![0u8; plan.cmsg_len];
                        // plan_rights и encode_rights делят одну раскладку:
                        // буфера ровно cmsg_len, поэтому None невозможен.
                        if cmsg::encode_rights(&mut control, &fds_i32).is_none() {
                            return Err(Errno::EINVAL);
                        }
                        if let Err(e) = super::io_sys::copy_out_pub(hdr.control, &control) {
                            // cmsg не доставлен — установленные fd закрываем,
                            // иначе они утекли бы в таблицу получателя.
                            let _ = compat::with_current_compat(|cs| {
                                for f in installed {
                                    let _ = cs.fds.close(f);
                                }
                            });
                            return Err(e);
                        }
                        used = plan.used as u64;
                    }
                }
                write_msghdr_out(msg_addr, 0, used, out_flags)?;
                return Ok(scm.n as u64);
            }
            ScmReadResult::Eof => {
                write_msghdr_out(msg_addr, 0, 0, flags as u32)?;
                return Ok(0);
            }
            ScmReadResult::WouldBlock => {
                if nonblock {
                    return Err(Errno::EAGAIN);
                }
                match endpoint_wait_verdict(&rx) {
                    super::sock_wait::WaitVerdict::Continue => {
                        crate::task::scheduler::yield_current()
                    }
                    super::sock_wait::WaitVerdict::Eof => {
                        write_msghdr_out(msg_addr, 0, 0, flags as u32)?;
                        return Ok(0);
                    }
                    super::sock_wait::WaitVerdict::Ebadf => return Err(Errno::EBADF),
                }
            }
        }
    }
}
