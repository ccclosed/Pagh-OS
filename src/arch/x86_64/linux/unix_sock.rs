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
use alloc::vec::Vec;

use super::check_user_ptr;
use super::errno::Errno;
use crate::sync::spinlock::Spinlock;
use crate::task::compat;
use crate::task::fd::{socket_pair_endpoints, OpenObject, UnixListenerState};

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
        );
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
                );
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
                if addr != 0 && addrlen_ptr != 0 && check_user_ptr(addrlen_ptr, 4).is_ok() {
                    let want = unsafe { core::ptr::read_unaligned(addrlen_ptr as *const u32) };
                    if want >= 2 && check_user_ptr(addr, 2).is_ok() {
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
    check_user_ptr(addrlen_ptr, 4)?;
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
        check_user_ptr(addr, n as u64)?;
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
    check_user_ptr(optlen, 4)?;
    let want = unsafe { core::ptr::read_unaligned(optlen as *const u32) };
    if want < 4 {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(optval, 4)?;
    unsafe {
        core::ptr::write_unaligned(optval as *mut u32, value);
        core::ptr::write_unaligned(optlen as *mut u32, 4u32);
    }
    Ok(0)
}
