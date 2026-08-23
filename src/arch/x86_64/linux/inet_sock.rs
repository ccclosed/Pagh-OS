//! AF_INET socket syscalls over the smoltcp stack (`net::` primitives).
//!
//! TCP covers curl/git-remote-https; UDP covers glibc's resolver, which reads
//! `/mnt/etc/resolv.conf` and speaks DNS over `sendto`/`recvfrom`. Blocking
//! calls funnel through the scheduler yield so other tasks keep running while
//! the network poll thread advances the interface.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::errno::Errno;
use crate::net;
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

const EINPROGRESS_ERR: u32 = 115; // used through Errno mapping below

/// One AF_INET stream socket. `handle` is `None` until connect(2).
pub struct InetTcp {
    pub handle: crate::sync::spinlock::Spinlock<Option<smoltcp::iface::SocketHandle>>,
    pub nonblocking: AtomicBool,
    pub so_error: AtomicU32,
    pub connected: AtomicBool,
    pub eof: AtomicBool,
}

/// One AF_INET datagram socket bound to an ephemeral local port.
pub struct InetUdp {
    pub handle: smoltcp::iface::SocketHandle,
    pub nonblocking: AtomicBool,
    /// Default destination remembered by connect(2): subsequent write(2)/send(2)
    /// without an address go here (this is exactly how glibc's resolver sends
    /// its queries over a connected UDP socket).
    pub peer: crate::sync::spinlock::Spinlock<Option<smoltcp::wire::IpEndpoint>>,
}

impl Drop for InetUdp {
    fn drop(&mut self) {
        // Remove the smoltcp socket from the shared set so the net thread
        // doesn't keep polling a dead endpoint.
        crate::net::udp_remove(self.handle);
    }
}

fn parse_sockaddr_in(addr: u64, len: u64) -> Result<(u16, [u8; 4]), Errno> {
    use super::check_user_ptr;
    if addr == 0 || len < 8 {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(addr, 8)?;
    // SAFETY: range validated above
    let family = unsafe { core::ptr::read_unaligned(addr as *const u16) };
    if family as u64 != AF_INET && family as u64 != AF_INET6 {
        return Err(Errno::EAFNOSUPPORT);
    }
    // SAFETY: as above
    let port = u16::from_be(unsafe { core::ptr::read_unaligned((addr + 2) as *const u16) });
    // SAFETY: as above
    let octets: [u8; 4] =
        unsafe { core::ptr::read_unaligned((addr + 4) as *const [u8; 4]) };
    Ok((port, octets))
}

fn endpoint(port: u16, octets: [u8; 4]) -> smoltcp::wire::IpEndpoint {
    smoltcp::wire::IpEndpoint {
        addr: smoltcp::wire::IpAddress::v4(octets[0], octets[1], octets[2], octets[3]),
        port,
    }
}

/// `socket(AF_INET, ...)`: STREAM → lazy TCP slot, DGRAM → bound UDP socket.
pub fn sys_socket_in(domain: u64, ty: u64) -> Result<u64, Errno> {
    if domain != AF_INET {
        // IPv6 unsupported for now — fail fast with a distinct errno.
        return Err(Errno::EAFNOSUPPORT);
    }
    let base = ty & SOCK_TYPE_MASK;
    if base != SOCK_STREAM && base != SOCK_DGRAM {
        return Err(Errno::EINVAL);
    }
    crate::warn!(
        "[DIAG] inet socket domain={} type={} -> {}",
        domain,
        ty,
        if base == SOCK_STREAM { "tcp" } else { "udp" }
    );
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
            });
            let fd = cs.fds.alloc(OpenObject::InetTcp(sock));
            if cloexec { cs.fds.set_cloexec(fd, true); }
            Ok(fd as u64)
        }
        _ => {
            let handle = net::udp_open().map_err(|_| Errno::ENOMEM)?;
            let sock = Arc::new(InetUdp {
                handle,
                nonblocking: AtomicBool::new(nonblocking),
                peer: crate::sync::spinlock::Spinlock::new(None),
            });
            let fd = cs.fds.alloc(OpenObject::InetUdp(sock));
            if cloexec { cs.fds.set_cloexec(fd, true); }
            Ok(fd as u64)
        }
    })
    .unwrap_or(Err(Errno::EBADF))
}

/// `connect` on an AF_INET TCP fd.
pub fn sys_connect_tcp(fd: u64, addr: u64, len: u64) -> Result<u64, Errno> {
    let (port, octets) = parse_sockaddr_in(addr, len)?;
    crate::warn!(
        "[DIAG] inet connect fd={} -> {}.{}.{}.{}:{}",
        fd, octets[0], octets[1], octets[2], octets[3], port
    );
    let remote = endpoint(port, octets);
    let (sock, nonblocking) = compat::with_current_compat(|cs| match cs.fds.get(fd as u32) {
        Some(OpenObject::InetTcp(t)) => Some((Arc::clone(&t), t.nonblocking.load(Ordering::Relaxed))),
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
        // POSIX non-blocking connect: completion surfaces via SO_ERROR.
        sock.so_error.store(EINPROGRESS_ERR, Ordering::Relaxed);
        return Err(Errno::EINPROGRESS);
    }

    // Blocking: bounded wait (~5 s at the current tick rate), yielding to the
    // scheduler while the net thread drives smoltcp.
    let mut waited: u32 = 0;
    loop {
        if net::tcp_established(handle) {
            sock.connected.store(true, Ordering::Relaxed);
            sock.so_error.store(0, Ordering::Relaxed);
            return Ok(0);
        }
        if net::tcp_dead_before_established(handle) && waited > 50 {
            sock.so_error.store(111, Ordering::Relaxed); // ECONNREFUSED
            return Err(Errno::ECONNREFUSED);
        }
        if waited >= 5000 {
            sock.so_error.store(110, Ordering::Relaxed); // ETIMEDOUT
            return Err(Errno::ETIMEDOUT);
        }
        crate::task::scheduler::yield_current();
        waited += 1;
    }
}

/// `send`/`sendto`/`write` on an AF_INET TCP fd.
pub fn tcp_write(fd: u64, data: &[u8]) -> Result<usize, Errno> {
    let sock = tcp_sock(fd)?;
    if !sock.connected.load(Ordering::Relaxed) {
        return Err(Errno::ENOTCONN);
    }
    let handle = sock.handle.lock().ok_or(Errno::ENOTCONN)?;
    let nb = sock.nonblocking.load(Ordering::Relaxed);
    let mut sent_all: usize = 0;
    loop {
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
    }
}

/// `recv`/`read` on an AF_INET TCP fd.
pub fn tcp_read(fd: u64, dst: &mut [u8]) -> Result<usize, Errno> {
    let sock = tcp_sock(fd)?;
    let handle = sock.handle.lock().ok_or(Errno::ENOTCONN)?;
    let nb = sock.nonblocking.load(Ordering::Relaxed);
    loop {
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
    }
}

/// `close` on an AF_INET TCP fd.
pub fn tcp_close_fd(fd: u64) {
    if let Some(sock) = tcp_sock(fd).ok() {
        if let Some(h) = sock.handle.lock().take() {
            net::tcp_close(h);
        }
    }
}

fn tcp_sock(fd: u64) -> Result<Arc<InetTcp>, Errno> {
    compat::with_current_compat(|cs| match cs.fds.get(fd as u32) {
        Some(OpenObject::InetTcp(t)) => Some(Arc::clone(&t)),
        _ => None,
    })
    .unwrap_or(None)
    .ok_or(Errno::ENOTSOCK)
}

/// `connect` on an AF_INET UDP fd: remember the peer for write()/send().
pub fn udp_connect_fd(fd: u64, addr: u64, len: u64) -> Result<u64, Errno> {
    let sock = udp_sock(fd)?;
    let (port, octets) = parse_sockaddr_in(addr, len)?;
    *sock.peer.lock() = Some(endpoint(port, octets));
    Ok(0)
}

/// write(2)/send(2) on a connected AF_INET UDP fd.
pub fn udp_write_fd(fd: u64, data: &[u8]) -> Result<usize, Errno> {
    let sock = udp_sock(fd)?;
    let peer = sock.peer.lock().ok_or(Errno::ENOTCONN)?;
    crate::warn!(
        "[DIAG] udp write(fd) fd={} -> {:?} len={}",
        fd, peer, data.len()
    );
    net::udp_sendto(sock.handle, data, peer).map(|_| data.len()).map_err(|_| Errno::EIO)
}

// ── UDP (glibc resolver path) ────────────────────────────────────────────────

fn udp_sock(fd: u64) -> Result<Arc<InetUdp>, Errno> {
    compat::with_current_compat(|cs| match cs.fds.get(fd as u32) {
        Some(OpenObject::InetUdp(u)) => Some(Arc::clone(&u)),
        _ => None,
    })
    .unwrap_or(None)
    .ok_or(Errno::ENOTSOCK)
}

/// `sendto` on an AF_INET UDP fd. A NULL address means "connected" semantics
/// which we do not support (glibc's resolver always passes one).
pub fn udp_sendto_fd(fd: u64, data: &[u8], addr: u64, len: u64) -> Result<usize, Errno> {
    let sock = udp_sock(fd)?;
    let (port, octets) = parse_sockaddr_in(addr, len)?;
    crate::warn!(
        "[DIAG] udp sendto(fd) fd={} {}.{}.{}.{}:{} len={}",
        fd, octets[0], octets[1], octets[2], octets[3], port, data.len()
    );
    crate::warn!("[DIAG] udp sendto fd={} len={} port={}", fd, data.len(), port);
    net::udp_sendto(sock.handle, data, endpoint(port, octets))
        .map(|_| data.len())
        .map_err(|_| Errno::EIO)
}

/// `recvfrom` on an AF_INET UDP fd; returns (bytes, port, ipv4 octets).
pub fn udp_recvfrom_fd(
    fd: u64,
    dst: &mut [u8],
) -> Result<(usize, u16, [u8; 4]), Errno> {
    let sock = udp_sock(fd)?;
    static RECV_LOGGED: AtomicU32 = AtomicU32::new(0);
    if RECV_LOGGED.fetch_or(1 << (fd.min(31)), Ordering::Relaxed) & (1 << (fd.min(31))) == 0 {
        crate::warn!("[DIAG] udp recvfrom fd={} blocking (first)", fd);
    }
    let deadline_spins: u32 = 5000;
    let mut spins = 0u32;
    loop {
        if let Some((n, ep)) = net::udp_recvfrom(sock.handle, dst) {
            let o = match ep.addr {
                smoltcp::wire::IpAddress::Ipv4(v4) => v4.octets(),
                _ => [0, 0, 0, 0],
            };
            return Ok((n, ep.port, o));
        }
        if sock.nonblocking.load(Ordering::Relaxed) || spins >= deadline_spins {
            return Err(Errno::EAGAIN);
        }
        crate::task::scheduler::yield_current();
        spins += 1;
    }
}

/// `getsockopt` for AF_INET fds: real `SO_ERROR`, honest `SO_TYPE`.
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
    use super::check_user_ptr;
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
