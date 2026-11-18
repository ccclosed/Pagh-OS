//! Syscall'ы AF_INET-сокетов поверх собственного стека TCP/IP ядра (примитивы
//! `net::`).
//!
//! TCP покрывает curl/git-remote-https; UDP — резолвер glibc, который читает
//! `/mnt/etc/resolv.conf` и говорит DNS через `sendto`/`recvfrom`. Блокирующие
//! вызовы уступают CPU планировщику, чтобы другие задачи работали, пока поток
//! опроса сети двигает стек.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::errno::Errno;
use crate::net::{self, IpEndpoint};
use crate::task::compat;
use crate::task::fd::OpenObject;

pub const AF_INET: u64 = 2;
const AF_INET6: u64 = 10;

const SOCK_STREAM: u64 = 1;
const SOCK_DGRAM: u64 = 2;
const SOCK_TYPE_MASK: u64 = 0xf;
const SOCK_NONBLOCK: u64 = 0o_4000;
const SOCK_CLOEXEC: u64 = 0o_2000000;

const SOL_SOCKET: u64 = 1;
const SO_ERROR: u64 = 4;
const SO_TYPE: u64 = 3;

const EINPROGRESS_ERR: u32 = 115; // используется через отображение Errno ниже

/// Один AF_INET stream-сокет. `handle` равен `None` до connect(2).
pub struct InetTcp {
    pub handle: crate::sync::spinlock::Spinlock<Option<net::SocketHandle>>,
    pub nonblocking: AtomicBool,
    pub so_error: AtomicU32,
    pub connected: AtomicBool,
    pub eof: AtomicBool,
    /// `shutdown(SHUT_RD)` (nr 48): приём закрыт, последующие `recv` → `Ok(0)`.
    /// Флаг на сокете, а не на fd: `dup`-дубликаты видят то же самое.
    pub read_shut: AtomicBool,
    /// `shutdown(SHUT_WR)` (nr 48): передача закрыта, FIN уже заказан, а
    /// собственная запись отвечает `EPIPE` (`EPIPE`, не `ENOTCONN`: соединение
    /// ещё принимает).
    pub write_shut: AtomicBool,
}

/// Один AF_INET datagram-сокет, привязанный к эфемерному локальному порту.
pub struct InetUdp {
    pub handle: net::SocketHandle,
    pub nonblocking: AtomicBool,
    /// Назначение по умолчанию, запомненное connect(2): последующие
    /// write(2)/send(2) без адреса идут сюда (именно так резолвер glibc шлёт
    /// запросы через подключённый UDP-сокет).
    pub peer: crate::sync::spinlock::Spinlock<Option<IpEndpoint>>,
}

/// Разобрать `sockaddr_in` ИЛИ `sockaddr_in6` в [`IpEndpoint`].
/// v4-mapped IPv6-адрес (`::ffff:a.b.c.d`) схлопывается до IPv4-формы, как в
/// dual-stack сокетах Linux.
pub(super) fn parse_sockaddr_ep(addr: u64, len: u64) -> Result<IpEndpoint, Errno> {
    use super::check_user_ptr;
    if addr == 0 || len < 8 {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(addr, 8)?;
    // SAFETY: диапазон проверен выше
    let family = unsafe { core::ptr::read_unaligned(addr as *const u16) };
    match family as u64 {
        AF_INET => {
            let (port, o) = parse_sockaddr_in(addr, len)?;
            Ok(IpEndpoint::new(net::IpAddr::V4(net::Ipv4Addr(o)), port))
        }
        AF_INET6 => {
            // struct sockaddr_in6: family(2) port(2) flowinfo(4) addr(16) scope(4)
            if len < 28 {
                return Err(Errno::EINVAL);
            }
            check_user_ptr(addr, 28)?;
            // SAFETY: диапазон проверен выше
            let port = u16::from_be(unsafe { core::ptr::read_unaligned((addr + 2) as *const u16) });
            // SAFETY: как выше
            let octets: [u8; 16] =
                unsafe { core::ptr::read_unaligned((addr + 8) as *const [u8; 16]) };
            let v6 = net::Ipv6Addr(octets);
            let ep_addr = match v6.to_v4_mapped() {
                Some(v4) => net::IpAddr::V4(net::Ipv4Addr(v4.octets())),
                None => net::IpAddr::V6(v6),
            };
            Ok(IpEndpoint::new(ep_addr, port))
        }
        _ => Err(Errno::EAFNOSUPPORT),
    }
}

fn parse_sockaddr_in(addr: u64, len: u64) -> Result<(u16, [u8; 4]), Errno> {
    use super::check_user_ptr;
    if addr == 0 || len < 8 {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(addr, 8)?;
    // SAFETY: диапазон проверен выше
    let family = unsafe { core::ptr::read_unaligned(addr as *const u16) };
    if family as u64 != AF_INET && family as u64 != AF_INET6 {
        return Err(Errno::EAFNOSUPPORT);
    }
    // SAFETY: как выше
    let port = u16::from_be(unsafe { core::ptr::read_unaligned((addr + 2) as *const u16) });
    // SAFETY: как выше
    let octets: [u8; 4] = unsafe { core::ptr::read_unaligned((addr + 4) as *const [u8; 4]) };
    Ok((port, octets))
}

/// `socket(AF_INET, ...)`: STREAM → ленивый TCP-слот, DGRAM → привязанный UDP-сокет.
pub fn sys_socket_in(domain: u64, ty: u64) -> Result<u64, Errno> {
    if domain != AF_INET && domain != AF_INET6 {
        return Err(Errno::EAFNOSUPPORT);
    }
    let base = ty & SOCK_TYPE_MASK;
    if base != SOCK_STREAM && base != SOCK_DGRAM {
        return Err(Errno::EINVAL);
    }

    let nonblocking = ty & SOCK_NONBLOCK != 0;
    let cloexec = ty & SOCK_CLOEXEC != 0;
    compat::with_current_compat(|cs| match base {
        SOCK_STREAM => {
            let sock = Arc::new(InetTcp {
                handle: crate::sync::spinlock::Spinlock::new(None),
                nonblocking: AtomicBool::new(nonblocking),
                so_error: AtomicU32::new(0),
                connected: AtomicBool::new(false),
                eof: AtomicBool::new(false),
                read_shut: AtomicBool::new(false),
                write_shut: AtomicBool::new(false),
            });
            let fd = cs.fds.alloc_with_flags(
                OpenObject::InetTcp(sock),
                crate::task::fd::O_RDWR | (ty & SOCK_NONBLOCK),
            )?;
            if cloexec {
                cs.fds.set_cloexec(fd, true);
            }
            Ok(fd as u64)
        }
        _ => {
            let handle = net::udp_open().map_err(|_| Errno::ENOMEM)?;
            let sock = Arc::new(InetUdp {
                handle,
                nonblocking: AtomicBool::new(nonblocking),
                peer: crate::sync::spinlock::Spinlock::new(None),
            });
            let fd = cs.fds.alloc_with_flags(
                OpenObject::InetUdp(sock),
                crate::task::fd::O_RDWR | (ty & SOCK_NONBLOCK),
            )?;
            if cloexec {
                cs.fds.set_cloexec(fd, true);
            }
            Ok(fd as u64)
        }
    })
    .unwrap_or(Err(Errno::EBADF))
}

/// `connect` на AF_INET TCP fd.
pub fn sys_connect_tcp(fd: u64, addr: u64, len: u64) -> Result<u64, Errno> {
    let remote = parse_sockaddr_ep(addr, len)?;
    let (sock, nonblocking) = compat::with_current_compat(|cs| match cs.fds.get(fd as u32) {
        Some(OpenObject::InetTcp(t)) => {
            Some((Arc::clone(t), t.nonblocking.load(Ordering::Relaxed)))
        }
        _ => None,
    })
    .unwrap_or(None)
    .ok_or(Errno::ENOTSOCK)?;

    let mut guard = sock.handle.lock();
    if guard.is_some() {
        return Err(Errno::EISCONN);
    }
    let handle = net::tcp_connect_buffered(remote, 16 * 1024, 16 * 1024).map_err(|_| {
        sock.so_error.store(EINPROGRESS_ERR, Ordering::Relaxed);
        Errno::ENETUNREACH
    })?;
    *guard = Some(handle);
    drop(guard);

    if nonblocking {
        // POSIX неблокирующий connect: завершение всплывает через SO_ERROR.
        sock.so_error.store(EINPROGRESS_ERR, Ordering::Relaxed);
        return Err(Errno::EINPROGRESS);
    }

    // Блокирующий режим: ограниченное ожидание по стенным часам, а не по
    // числу уступок. `ticks()` — тики LAPIC при TICK_HZ (1000), поэтому дедлайн
    // — реальное прошедшее время: подсчёт итераций цикла измерял давление
    // планировщика, а не секунды, и загруженная машина могла провалить connect,
    // который всё ещё продвигался.
    const CONNECT_TIMEOUT_MS: u64 = 5_000;
    // Сколько отказавший/недостижимый партнёр может держать вызывающего до
    // отчёта ECONNREFUSED (Linux сообщает сразу, но RST может прийти после
    // нескольких потерянных SYN).
    const CONNECT_REFUSED_MS: u64 = 200;
    let deadline = crate::task::scheduler::ticks() + CONNECT_TIMEOUT_MS;
    let refused_at = crate::task::scheduler::ticks() + CONNECT_REFUSED_MS;
    loop {
        if net::tcp_established(handle) {
            sock.connected.store(true, Ordering::Relaxed);
            sock.so_error.store(0, Ordering::Relaxed);
            return Ok(0);
        }
        let now = crate::task::scheduler::ticks();
        if now >= refused_at && net::tcp_dead_before_established(handle) {
            sock.so_error.store(111, Ordering::Relaxed); // ECONNREFUSED
            return Err(Errno::ECONNREFUSED);
        }
        if now >= deadline {
            sock.so_error.store(110, Ordering::Relaxed); // ETIMEDOUT
            return Err(Errno::ETIMEDOUT);
        }
        crate::task::scheduler::yield_current();
    }
}

/// `send`/`sendto`/`write` на AF_INET TCP fd.
///
/// `shutdown(SHUT_WR)` (nr 48) закрывает передачу раньше `close`, поэтому
/// запись после него — `EPIPE`; закрытие самого fd из соседнего потока
/// пробуждает ожидание `EBADF` (та же проверка владения, что у [`tcp_read`]).
pub fn tcp_write(fd: u64, data: &[u8]) -> Result<usize, Errno> {
    let sock = tcp_sock(fd)?;
    if !sock.connected.load(Ordering::Relaxed) {
        return Err(Errno::ENOTCONN);
    }
    let nb = sock.nonblocking.load(Ordering::Relaxed);
    let mut sent_all: usize = 0;
    loop {
        // Половина, закрытая `shutdown(SHUT_WR)`, больше не принимает байты.
        // Повторная проверка в цикле нужна, потому что соседний поток может
        // позвать `shutdown` по этому же сокету во время ожидания.
        if sock.write_shut.load(Ordering::Relaxed) {
            return Err(Errno::EPIPE);
        }
        // `close` забрал сетевой handle: соединения больше нет. `None` здесь —
        // именно `EPIPE`, а не `EBADF`: дескриптор мог остаться (`dup`), но
        // писать в освобождённый сокет нельзя.
        let handle = match *sock.handle.lock() {
            Some(h) => h,
            None => return Err(Errno::EPIPE),
        };
        let n = net::tcp_send_chunk(handle, &data[sent_all..]);
        sent_all += n;
        if sent_all >= data.len() {
            return Ok(sent_all);
        }
        if nb {
            return if sent_all > 0 {
                Ok(sent_all)
            } else {
                Err(Errno::EAGAIN)
            };
        }
        if sock.so_error.load(Ordering::Relaxed) != 0 {
            return Err(Errno::EPIPE);
        }
        crate::task::scheduler::yield_current();
        // Тот же вопрос, что у чтения: закрытый из соседнего потока дескриптор
        // не меняет состояние TCP, поэтому без проверки владения ожидание было
        // бы вечным (`stuck in syscall ? (nr=1 arg0=0x3 closed)`).
        if !tcp_socket_owned(&sock) {
            return if sent_all > 0 {
                Ok(sent_all)
            } else {
                Err(Errno::EBADF)
            };
        }
    }
}

/// `recv`/`read` на AF_INET TCP fd.
pub fn tcp_read(fd: u64, dst: &mut [u8]) -> Result<usize, Errno> {
    let sock = tcp_sock(fd)?;
    let mut handle = sock.handle.lock().ok_or(Errno::ENOTCONN)?;
    let nb = sock.nonblocking.load(Ordering::Relaxed);
    loop {
        // `shutdown(SHUT_RD)` закрывает приём: `recv` отвечает `0`, как после
        // EOF пира, хотя FIN от него не приходил.
        if sock.read_shut.load(Ordering::Relaxed) {
            // Linux отбрасывает входящие байты после `SHUT_RD`. Здесь они
            // осушаются при следующем `recv`: иначе очередь приёма заполнилась
            // бы и окно закрылось, подвесив пира. Названное упрощение —
            // отбрасывание не асинхронное, а по вызову.
            if let Some(h) = *sock.handle.lock() {
                let mut sink = [0u8; 1024];
                while net::tcp_recv_chunk(h, &mut sink) > 0 {}
            }
            return Ok(0);
        }
        if sock.eof.load(Ordering::Relaxed) {
            return Ok(0);
        }
        let n = net::tcp_recv_chunk(handle, dst);
        if n > 0 {
            return Ok(n);
        }
        if net::tcp_rx_at_eof(handle) {
            sock.eof.store(true, Ordering::Relaxed);
            return Ok(0);
        }
        if nb {
            return Err(Errno::EAGAIN);
        }
        crate::task::scheduler::yield_current();
        // Проснувшись, первым делом спрашиваем таблицу fd. Закрытый из
        // соседнего потока дескриптор EOF на TCP не выставляет (`Arc<InetTcp>`
        // и сетевой handle живут сами по себе), поэтому без этой проверки
        // ожидание было бы вечным — watchdog печатал
        // `stuck in syscall ? (nr=45 arg0=0x3 closed)`. Идентичность сокета
        // сохраняет ожидание при `dup`: пока хоть один дескриптор ссылается на
        // этот `Arc`, читать его законно.
        if !tcp_socket_owned(&sock) {
            return Err(Errno::EBADF);
        }
        // `close` мог забрать handle, пока мы спали; не трогаем освобождённый
        // сетевой handle. Такой сокет освобождён, то есть это конец потока.
        handle = match *sock.handle.lock() {
            Some(h) => h,
            None => return Ok(0),
        };
    }
}

/// Ссылается ли ещё хоть один дескриптор текущего процесса на `sock`.
///
/// Зеркало [`crate::task::fd::FdTable::holds_read_end`] для TCP: `dup`-дубликат —
/// владение, закрытие последнего fd — потеря владения и повод разбудить
/// блокирующее чтение `EBADF`.
fn tcp_socket_owned(sock: &Arc<InetTcp>) -> bool {
    let id = Arc::as_ptr(sock) as usize;
    compat::with_current_compat(|cs| cs.fds.holds_inet_tcp(id)).unwrap_or(false)
}

/// `close` на AF_INET TCP fd.
pub fn tcp_close_fd(fd: u64) {
    if let Ok(sock) = tcp_sock(fd) {
        if let Some(h) = sock.handle.lock().take() {
            net::tcp_close(h);
        }
    }
}

/// Применить `shutdown(2)` (nr 48) к AF_INET TCP-сокету.
///
/// `SHUT_WR` заказывает FIN через [`net::tcp_shutdown_wr`] — сокет при этом
/// остаётся в таблице стека и продолжает ПРИНИМАТЬ, в отличие от
/// [`tcp_close_fd`], снимающего слот; FIN отправляется один раз, повторный
/// `shutdown` той же половины идемпотентен. `SHUT_RD` поднимает флаг приёма,
/// который [`tcp_read`] превращает в `Ok(0)`.
///
/// `sock.handle.lock()` берётся на один оператор (копия [`net::SocketHandle`])
/// и отпускается до `NET`-лока внутри `tcp_shutdown_wr` — порядок «handle →
/// NET» тот же, что у записи и закрытия.
pub fn tcp_apply_shutdown(sock: &Arc<InetTcp>, read: bool, write: bool) {
    if read {
        sock.read_shut.store(true, Ordering::Relaxed);
    }
    if write && !sock.write_shut.swap(true, Ordering::Relaxed) {
        if let Some(h) = *sock.handle.lock() {
            net::tcp_shutdown_wr(h);
        }
    }
}

fn tcp_sock(fd: u64) -> Result<Arc<InetTcp>, Errno> {
    compat::with_current_compat(|cs| match cs.fds.get(fd as u32) {
        Some(OpenObject::InetTcp(t)) => Some(Arc::clone(t)),
        _ => None,
    })
    .unwrap_or(None)
    .ok_or(Errno::ENOTSOCK)
}

/// `connect` на AF_INET UDP fd: запомнить партнёра для write()/send().
pub fn udp_connect_fd(fd: u64, addr: u64, len: u64) -> Result<u64, Errno> {
    let sock = udp_sock(fd)?;
    *sock.peer.lock() = Some(parse_sockaddr_ep(addr, len)?);
    Ok(0)
}

/// write(2)/send(2) на подключённом AF_INET UDP fd.
pub fn udp_write_fd(fd: u64, data: &[u8]) -> Result<usize, Errno> {
    let sock = udp_sock(fd)?;
    let peer = sock.peer.lock().ok_or(Errno::ENOTCONN)?;

    net::udp_sendto(sock.handle, data, peer)
        .map(|_| data.len())
        .map_err(|_| Errno::EIO)
}

// ── UDP (путь резолвера glibc) ──────────────────────────────────────────────

fn udp_sock(fd: u64) -> Result<Arc<InetUdp>, Errno> {
    compat::with_current_compat(|cs| match cs.fds.get(fd as u32) {
        Some(OpenObject::InetUdp(u)) => Some(Arc::clone(u)),
        _ => None,
    })
    .unwrap_or(None)
    .ok_or(Errno::ENOTSOCK)
}

/// `sendto` на AF_INET UDP fd. NULL-адрес означает «подключённую» семантику,
/// которую мы не поддерживаем (резолвер glibc всегда передаёт адрес).
pub fn udp_sendto_fd(fd: u64, data: &[u8], addr: u64, len: u64) -> Result<usize, Errno> {
    let sock = udp_sock(fd)?;
    let ep = parse_sockaddr_ep(addr, len)?;

    net::udp_sendto(sock.handle, data, ep)
        .map(|_| data.len())
        .map_err(|_| Errno::EIO)
}

/// `recvfrom` на AF_INET/INET6 UDP fd; возвращает (байты, эндпоинт-источник).
pub fn udp_recvfrom_fd(fd: u64, dst: &mut [u8]) -> Result<(usize, IpEndpoint), Errno> {
    let sock = udp_sock(fd)?;
    static RECV_LOGGED: AtomicU32 = AtomicU32::new(0);
    if RECV_LOGGED.fetch_or(1 << (fd.min(31)), Ordering::Relaxed) & (1 << (fd.min(31))) == 0 {}
    let deadline_spins: u32 = 5000;
    let mut spins = 0u32;
    loop {
        if let Some((n, ep)) = net::udp_recvfrom(sock.handle, dst) {
            return Ok((n, ep));
        }
        if sock.nonblocking.load(Ordering::Relaxed) || spins >= deadline_spins {
            return Err(Errno::EAGAIN);
        }
        crate::task::scheduler::yield_current();
        spins += 1;
    }
}

/// `getsockopt` для AF_INET fd: настоящий `SO_ERROR`, честный `SO_TYPE`.
pub fn getsockopt_in(
    tcp: Option<&Arc<InetTcp>>,
    level: u64,
    optname: u64,
    optval: u64,
    optlen: u64,
) -> Result<u64, Errno> {
    if level != SOL_SOCKET {
        return Err(Errno::EINVAL);
    }
    let value: u32 = match (optname, tcp) {
        (SO_ERROR, Some(t)) => t.so_error.load(Ordering::Relaxed),
        (SO_ERROR, None) => 0,
        (SO_TYPE, _) => SOCK_STREAM as u32,
        _ => return Err(Errno::EINVAL),
    };
    write_opt(optval, optlen, value)
}

pub(super) fn write_opt(optval: u64, optlen: u64, value: u32) -> Result<u64, Errno> {
    use super::check_user_ptr_writable;
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
