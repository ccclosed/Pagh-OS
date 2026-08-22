//! Per-`Compat_Process` file-descriptor table (R2.4, R2.6, R2.14).
//!
//! Maps small integer fds to open objects, with 0/1/2 pre-bound to the standard
//! streams (R2.2) and fresh descriptors allocated as the lowest free index `>= 3`
//! (R2.4). The pure index-allocation and close bookkeeping lives in the
//! dependency-free [`super::fd_alloc`] module so it is host-testable for Property
//! 7; this module layers the kernel-only [`OpenObject`] (which embeds
//! `Arc<dyn VfsNode>`) and the shared [`Errno`] mapping on top of it.
#![allow(dead_code)]

use alloc::collections::{BTreeSet, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::x86_64::linux::errno::Errno;
use crate::sync::spinlock::Spinlock;
use crate::vfs::VfsNode;
use crate::arch::x86_64::linux::epoll_sys::EpollEntry;

use super::fd_alloc::FdSlots;

const PIPE_CAPACITY: usize = 64 * 1024;
struct PipeState {
    bytes: VecDeque<u8>,
    readers: usize,
    writers: usize,
}
pub struct PipeEndpoint {
    state: Arc<Spinlock<PipeState>>,
    read_end: bool,
    nonblocking: bool,
}
pub enum PipeReadResult {
    Data(usize),
    WouldBlock,
    Eof,
}
pub enum PipeWriteResult {
    Data(usize),
    WouldBlock,
    Broken,
}
impl PipeEndpoint {
    pub fn nonblocking(&self)->bool { self.nonblocking }
    pub fn read(&self,dst:&mut[u8])->PipeReadResult { let mut s=self.state.lock(); if !s.bytes.is_empty(){let n=core::cmp::min(dst.len(),s.bytes.len());for b in dst.iter_mut().take(n){*b=s.bytes.pop_front().unwrap();}PipeReadResult::Data(n)}else if s.writers==0{PipeReadResult::Eof}else{PipeReadResult::WouldBlock} }
    pub fn write(&self,src:&[u8])->PipeWriteResult { let mut s=self.state.lock();if s.readers==0{return PipeWriteResult::Broken}let room=PIPE_CAPACITY.saturating_sub(s.bytes.len());if room==0{return PipeWriteResult::WouldBlock}let n=core::cmp::min(room,src.len());s.bytes.extend(src[..n].iter().copied());PipeWriteResult::Data(n) }
    pub fn read_ready(&self)->bool {let s=self.state.lock();!s.bytes.is_empty()||s.writers==0}
    pub fn write_ready(&self)->bool {let s=self.state.lock();s.readers>0&&s.bytes.len()<PIPE_CAPACITY}
    pub fn peer_closed(&self)->bool {let s=self.state.lock();if self.read_end{s.writers==0}else{s.readers==0}}
    /// Clone this endpoint with a different O_NONBLOCK flag (fcntl
    /// F_SETFL). The clone registers as an extra reader/writer on the shared
    /// queue; the original's count drops when its last Arc is released.
    pub fn with_nonblocking(self: &Arc<Self>, on: bool) -> Arc<PipeEndpoint> {
        if self.nonblocking == on { return Arc::clone(self); }
        { let mut s = self.state.lock(); if self.read_end { s.readers += 1 } else { s.writers += 1 } }
        Arc::new(PipeEndpoint { state: Arc::clone(&self.state), read_end: self.read_end, nonblocking: on })
    }
}

/// An object a file descriptor can refer to.
///
/// Standard streams resolve to the kernel console / stdin; everything else is an
/// ext2-backed file reached through the VFS, carrying its own read/write offset.
pub enum OpenObject {
    /// The kernel console (pre-bound to fds 1 and 2 for stdout/stderr).
    Console,
    /// Standard input (pre-bound to fd 0).
    Stdin,
    /// An open ext2-backed file and its current byte offset.
    File {
        /// The VFS node backing this descriptor.
        node: Arc<dyn VfsNode>,
        /// Current read/write offset within the file.
        offset: u64,
    },
    /// An open directory: the absolute path it was opened under, the snapshot of
    /// its child nodes taken at open time, and the `getdents64` cursor index into
    /// that snapshot (Feature: linux-binary-compat). Snapshotting the children at
    /// open avoids re-reading the VFS (a potentially blocking operation) while the
    /// `COMPAT_STATES` lock is held during `getdents64`.
    PipeRead(Arc<PipeEndpoint>),
    PipeWrite(Arc<PipeEndpoint>),
    /// An eventfd counter (EFD_SEMAPHORE if semaphore=true).
    Eventfd { val: Arc<Spinlock<u64>>, semaphore: bool },
    /// One end of an AF_UNIX stream socketpair — a cross-connected
    /// pair of pipe endpoints (rx = this end's incoming bytes, tx = outgoing).
    Socket { rx: Arc<PipeEndpoint>, tx: Arc<PipeEndpoint> },
    /// A bound/listening AF_UNIX server socket.
    UnixListener(Arc<UnixListenerState>),
    /// Socket(2) created but not yet bound or connected.
    UnixSocketUnbound { nonblocking: bool },
    /// An epoll instance with its interest list.
    Epoll { interests: Arc<Spinlock<Vec<EpollEntry>>> },
    Dir {
        /// Absolute path the directory was opened under (used by `fchdir`).
        path: String,
        /// Child nodes captured at open time.
        children: Vec<Arc<dyn VfsNode>>,
        /// Index of the next child `getdents64` will emit.
        index: usize,
    },
}

impl Clone for OpenObject {
    fn clone(&self) -> Self {
        self.dup_clone()
    }
}

impl OpenObject {
    /// Produce an independent duplicate of this descriptor for `dup`/`dup2`/`dup3`.
    ///
    /// Standard streams clone trivially; a file clones the `Arc` node handle and
    /// copies the current offset (Linux `dup` shares the open-file description, so
    /// the offset is the same at duplication time); a directory clones its captured
    /// child list and cursor so the duplicate continues iterating from the same
    /// position.
    pub fn dup_clone(&self) -> OpenObject {
        match self {
            OpenObject::Console => OpenObject::Console,
            OpenObject::Stdin => OpenObject::Stdin,
            OpenObject::PipeRead(e) => OpenObject::PipeRead(Arc::clone(e)),
            OpenObject::PipeWrite(e) => OpenObject::PipeWrite(Arc::clone(e)),
            OpenObject::File { node, offset } => OpenObject::File {
                node: Arc::clone(node),
                offset: *offset,
            },
            OpenObject::Dir {
                path,
                children,
                index,
            } => OpenObject::Dir {
                path: path.clone(),
                children: children.clone(),
                index: *index,
            },
            OpenObject::Eventfd { val, semaphore } => OpenObject::Eventfd { val: Arc::clone(val), semaphore: *semaphore },
            OpenObject::Socket { rx, tx } => OpenObject::Socket { rx: Arc::clone(rx), tx: Arc::clone(tx) },
            OpenObject::UnixListener(l) => OpenObject::UnixListener(Arc::clone(l)),
            OpenObject::UnixSocketUnbound { nonblocking } => OpenObject::UnixSocketUnbound { nonblocking: *nonblocking },
            OpenObject::Epoll { interests } => OpenObject::Epoll { interests: Arc::clone(interests) },
        }
    }
}

/// A process's file-descriptor table.
///
/// Thin kernel-facing wrapper over the pure [`FdSlots`] bookkeeping: it fixes the
/// stored type to [`OpenObject`], pins the minimum allocatable descriptor at 3,
/// and maps the pure absent-fd error to [`Errno::EBADF`].
#[derive(Clone)]
pub struct FdTable {
    slots: FdSlots<OpenObject>,
    /// Descriptors flagged close-on-exec (O_CLOEXEC / FD_CLOEXEC).
    /// Swept by execve; libuv's fork+exec error-pipe protocol depends on it.
    cloexec: BTreeSet<u32>,
}

impl FdTable {
    /// Lowest descriptor a fresh `alloc` may return; 0/1/2 are reserved for the
    /// standard streams (R2.4).
    const FIRST_DYNAMIC_FD: usize = 3;

    /// Create a table with the standard streams pre-bound: fd 0 = stdin,
    /// fd 1 = console (stdout), fd 2 = console (stderr) (R2.2).
    pub fn with_standard_streams() -> Self {
        let mut initial: Vec<Option<OpenObject>> = Vec::with_capacity(Self::FIRST_DYNAMIC_FD);
        initial.push(Some(OpenObject::Stdin)); // fd 0
        initial.push(Some(OpenObject::Console)); // fd 1
        initial.push(Some(OpenObject::Console)); // fd 2
        Self {
            slots: FdSlots::from_slots(initial),
            cloexec: BTreeSet::new(),
        }
    }

    /// Allocate the lowest free descriptor `>= 3`, store `obj` there, and return
    /// the descriptor, growing the table as needed (R2.4).
    pub fn alloc(&mut self, obj: OpenObject) -> u32 {
        let fd = self.slots.alloc(Self::FIRST_DYNAMIC_FD, obj);
        self.cloexec.remove(&fd); // a recycled slot must not inherit the flag
        fd
    }

    /// Borrow the object referenced by `fd`, or `None` for an out-of-range/empty
    /// descriptor (caller maps `None` -> `EBADF`, R2.14).
    pub fn get(&self, fd: u32) -> Option<&OpenObject> {
        self.slots.get(fd)
    }

    /// Mutably borrow the object referenced by `fd`, or `None` for an
    /// out-of-range/empty descriptor (caller maps `None` -> `EBADF`, R2.14).
    pub fn get_mut(&mut self, fd: u32) -> Option<&mut OpenObject> {
        self.slots.get_mut(fd)
    }

    pub fn pipe(&mut self, nonblocking: bool) -> (u32, u32) {
        let state = Arc::new(Spinlock::new(PipeState {
            bytes: VecDeque::new(),
            readers: 1,
            writers: 1,
        }));
        let r = Arc::new(PipeEndpoint {
            state: Arc::clone(&state),
            read_end: true,
            nonblocking,
        });
        let w = Arc::new(PipeEndpoint {
            state,
            read_end: false,
            nonblocking,
        });
        let rfd = self.alloc(OpenObject::PipeRead(r));
        let wfd = self.alloc(OpenObject::PipeWrite(w));
        (rfd, wfd)
    }

    /// Close `fd`. Returns `Err(Errno::EBADF)` when the descriptor is absent or
    /// already closed, leaving the table unchanged; otherwise releases it and
    /// returns `Ok` (R2.6, R2.14).
    pub fn close(&mut self, fd: u32) -> Result<(), Errno> {
        let res = self.slots.close(fd).map_err(|_| Errno::EBADF);
        if res.is_ok() { self.cloexec.remove(&fd); }
        res
    }

    /// `dup` (32): duplicate `oldfd` into the lowest free descriptor `>= 3`,
    /// returning the new descriptor. `EBADF` if `oldfd` is not open.
    pub fn dup(&mut self, oldfd: u32) -> Result<u32, Errno> {
        self.dup_min(oldfd, Self::FIRST_DYNAMIC_FD as u32)
    }

    /// `fcntl(F_DUPFD)`: duplicate `oldfd` into the lowest free descriptor that is
    /// `>= min`, returning the new descriptor. `EBADF` if `oldfd` is not open.
    pub fn dup_min(&mut self, oldfd: u32, min: u32) -> Result<u32, Errno> {
        let dup = self.slots.get(oldfd).ok_or(Errno::EBADF)?.dup_clone();
        let fd = self.slots.alloc(min as usize, dup);
        self.cloexec.remove(&fd); // the duplicate starts without FD_CLOEXEC
        Ok(fd)
    }

    /// `dup2`/`dup3`: duplicate `oldfd` into the explicit descriptor `newfd`,
    /// closing whatever currently occupies `newfd` first. Returns `newfd`.
    /// `EBADF` if `oldfd` is not open.
    ///
    /// When `oldfd == newfd` the caller must enforce the `dup2`/`dup3` distinction
    /// (`dup2` returns `newfd` unchanged; `dup3` is `EINVAL`); this method assumes
    /// they differ.
    pub fn dup_to(&mut self, oldfd: u32, newfd: u32) -> Result<u32, Errno> {
        let dup = self.slots.get(oldfd).ok_or(Errno::EBADF)?.dup_clone();
        self.slots.set(newfd, dup);
        self.cloexec.remove(&newfd); // dup2/dup3 clear FD_CLOEXEC on the target
        Ok(newfd)
    }

    // ── close-on-exec bookkeeping ──

    /// Set or clear the FD_CLOEXEC flag for `fd`.
    pub fn set_cloexec(&mut self, fd: u32, on: bool) {
        if on { self.cloexec.insert(fd); } else { self.cloexec.remove(&fd); }
    }

    /// Whether `fd` carries the FD_CLOEXEC flag.
    pub fn is_cloexec(&self, fd: u32) -> bool { self.cloexec.contains(&fd) }

    /// Short human name for whatever occupies `fd`.
    pub fn describe_fd(&self, fd: u32) -> &'static str {
        match self.slots.get(fd) {
            None => "closed",
            Some(OpenObject::Stdin) => "stdin(kbd)",
            Some(OpenObject::Console) => "console",
            Some(OpenObject::PipeRead(_)) => "pipe-r",
            Some(OpenObject::PipeWrite(_)) => "pipe-w",
            Some(OpenObject::Socket { .. }) => "socket",
            Some(OpenObject::UnixListener(_)) => "unix-listener",
            Some(OpenObject::UnixSocketUnbound { .. }) => "unix-unbound",
            Some(OpenObject::Eventfd { .. }) => "eventfd",
            Some(OpenObject::File { .. }) => "file",
            Some(OpenObject::Dir { .. }) => "dir",
            Some(OpenObject::Epoll { .. }) => "epoll",
        }
    }

    /// Close every descriptor flagged close-on-exec (the execve sweep).
    pub fn close_cloexec(&mut self) {
        let fds = core::mem::take(&mut self.cloexec);
        for fd in fds { let _ = self.slots.close(fd); }
    }

    /// `socketpair(AF_UNIX, SOCK_STREAM)`: two cross-connected
    /// in-kernel byte queues. Each end reads from one queue and writes to the
    /// other, so BOTH fds are readable and writable (unlike a pipe). Closing
    /// one end makes the peer observe EOF on read and EPIPE on write.
    pub fn socketpair(&mut self, nonblocking: bool) -> (u32, u32) {
        let q_ab = Arc::new(Spinlock::new(PipeState { bytes: VecDeque::new(), readers: 1, writers: 1 }));
        let q_ba = Arc::new(Spinlock::new(PipeState { bytes: VecDeque::new(), readers: 1, writers: 1 }));
        let a = OpenObject::Socket {
            rx: Arc::new(PipeEndpoint { state: Arc::clone(&q_ba), read_end: true, nonblocking }),
            tx: Arc::new(PipeEndpoint { state: Arc::clone(&q_ab), read_end: false, nonblocking }),
        };
        let b = OpenObject::Socket {
            rx: Arc::new(PipeEndpoint { state: q_ab, read_end: true, nonblocking }),
            tx: Arc::new(PipeEndpoint { state: q_ba, read_end: false, nonblocking }),
        };
        let fa = self.alloc(a);
        let fb = self.alloc(b);
        // Which descriptor numbers the RPC channel ends get.
        crate::warn!("[DIAG] socketpair pid={} -> ({},{})",
            crate::task::scheduler::current_pid(), fa, fb);
        (fa, fb)
    }
}

// --- AF_UNIX listener plumbing -----------------------------------

/// Mutable half of a listening AF_UNIX socket.
pub struct UnixListenerInner {
    /// listen(2) has been called; connect(2) refuses otherwise.
    pub listening: bool,
    /// O_NONBLOCK: accept returns EAGAIN instead of blocking.
    pub nonblocking: bool,
    /// Fully connected server-side endpoint pairs queued by connect(2),
    /// waiting for accept(2).
    pub pending: VecDeque<(Arc<PipeEndpoint>, Arc<PipeEndpoint>)>,
}

/// A bound AF_UNIX stream server socket (uv_pipe server in nvim).
pub struct UnixListenerState {
    /// The sockaddr_un path this socket was bound to.
    pub path: String,
    pub inner: Spinlock<UnixListenerInner>,
}

impl UnixListenerState {
    pub fn new(path: String, nonblocking: bool) -> Self {
        Self { path, inner: Spinlock::new(UnixListenerInner { listening: false, nonblocking, pending: VecDeque::new() }) }
    }
}

/// Build the two endpoint pairs of a connected AF_UNIX stream —
/// (client side, server side) — without allocating fds. connect(2) pushes the
/// server pair into the listener queue and installs the client pair locally;
/// accept(2) later turns the server pair into a fresh fd.
pub fn socket_pair_endpoints(nonblocking_client: bool, nonblocking_server: bool)
    -> ((Arc<PipeEndpoint>, Arc<PipeEndpoint>), (Arc<PipeEndpoint>, Arc<PipeEndpoint>))
{
    let q_cs = Arc::new(Spinlock::new(PipeState { bytes: VecDeque::new(), readers: 1, writers: 1 }));
    let q_sc = Arc::new(Spinlock::new(PipeState { bytes: VecDeque::new(), readers: 1, writers: 1 }));
    let client = (
        Arc::new(PipeEndpoint { state: Arc::clone(&q_sc), read_end: true, nonblocking: nonblocking_client }),
        Arc::new(PipeEndpoint { state: Arc::clone(&q_cs), read_end: false, nonblocking: nonblocking_client }),
    );
    let server = (
        Arc::new(PipeEndpoint { state: q_cs, read_end: true, nonblocking: nonblocking_server }),
        Arc::new(PipeEndpoint { state: q_sc, read_end: false, nonblocking: nonblocking_server }),
    );
    (client, server)
}
