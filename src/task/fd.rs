//! Per-`Compat_Process` file-descriptor table (R2.4, R2.6, R2.14).
//!
//! Maps small integer fds to open objects, with 0/1/2 pre-bound to the standard
//! streams (R2.2) and fresh descriptors allocated as the lowest free index `>= 3`
//! (R2.4). The pure index-allocation and close bookkeeping lives in the
//! dependency-free [`super::fd_alloc`] module so it is host-testable for Property
//! 7; this module layers the kernel-only [`OpenObject`] (which embeds
//! `Arc<dyn VfsNode>`) and the shared [`Errno`] mapping on top of it.
#![allow(dead_code)]

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::x86_64::linux::epoll_sys::EpollEntry;
use crate::arch::x86_64::linux::errno::Errno;
use crate::sync::spinlock::Spinlock;
use crate::vfs::VfsNode;

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
    pub fn nonblocking(&self) -> bool {
        self.nonblocking
    }
    pub fn read(&self, dst: &mut [u8]) -> PipeReadResult {
        let mut s = self.state.lock();
        if !s.bytes.is_empty() {
            let n = core::cmp::min(dst.len(), s.bytes.len());
            for b in dst.iter_mut().take(n) {
                *b = s.bytes.pop_front().unwrap();
            }
            PipeReadResult::Data(n)
        } else if s.writers == 0 {
            PipeReadResult::Eof
        } else {
            PipeReadResult::WouldBlock
        }
    }
    pub fn write(&self, src: &[u8]) -> PipeWriteResult {
        let mut s = self.state.lock();
        if s.readers == 0 {
            return PipeWriteResult::Broken;
        }
        let room = PIPE_CAPACITY.saturating_sub(s.bytes.len());
        if room == 0 {
            return PipeWriteResult::WouldBlock;
        }
        let n = core::cmp::min(room, src.len());
        s.bytes.extend(src[..n].iter().copied());
        PipeWriteResult::Data(n)
    }
    pub fn read_ready(&self) -> bool {
        let s = self.state.lock();
        !s.bytes.is_empty() || s.writers == 0
    }
    pub fn write_ready(&self) -> bool {
        let s = self.state.lock();
        s.readers > 0 && s.bytes.len() < PIPE_CAPACITY
    }
    pub fn peer_closed(&self) -> bool {
        let s = self.state.lock();
        if self.read_end {
            s.writers == 0
        } else {
            s.readers == 0
        }
    }
    /// Clone this endpoint with a different O_NONBLOCK flag (fcntl
    /// F_SETFL). The clone registers as an extra reader/writer on the shared
    /// queue; the original's count drops when its last Arc is released.
    pub fn with_nonblocking(self: &Arc<Self>, on: bool) -> Arc<PipeEndpoint> {
        if self.nonblocking == on {
            return Arc::clone(self);
        }
        {
            let mut s = self.state.lock();
            if self.read_end {
                s.readers += 1
            } else {
                s.writers += 1
            }
        }
        Arc::new(PipeEndpoint {
            state: Arc::clone(&self.state),
            read_end: self.read_end,
            nonblocking: on,
        })
    }
}

/// Dropping the last endpoint Arc of a side decrements that side's count so
/// the opposite end observes EOF (`writers == 0`) / EPIPE (`readers == 0`).
/// Without this, counts only ever grew: a forked child's inherited write ends
/// (run-command's CLOEXEC notify pipe in particular) never released them and
/// the parent blocked in `read()` forever — git clone hung right after
/// spawning its remote helper.
impl Drop for PipeEndpoint {
    fn drop(&mut self) {
        let mut s = self.state.lock();
        if self.read_end {
            s.readers = s.readers.saturating_sub(1);
        } else {
            s.writers = s.writers.saturating_sub(1);
        }
    }
}

/// State of one `timerfd` instance.
///
/// Times are in scheduler ticks (the only clock available to the kernel), and
/// expiration is computed lazily on read/poll: no timer IRQ walks the fd table.
pub struct TimerfdState {
    /// Absolute tick at which the next expiration is due; `None` = disarmed.
    pub deadline: Option<u64>,
    /// Repetition interval in ticks; `0` = one-shot.
    pub interval: u64,
    /// Expirations already accumulated (returned by the next read).
    pub expirations: u64,
    /// Whether the descriptor was created with TFD_NONBLOCK.
    pub nonblocking: bool,
}

impl TimerfdState {
    /// Expirations due at `now`, without mutating (used by poll/epoll).
    pub fn pending(&self, now: u64) -> u64 {
        if self.expirations > 0 {
            return self.expirations;
        }
        match self.deadline {
            Some(deadline) if now >= deadline => {
                if self.interval == 0 {
                    1
                } else {
                    (now - deadline) / self.interval + 1
                }
            }
            _ => 0,
        }
    }

    /// Consume all due expirations, advancing the deadline.
    pub fn take(&mut self, now: u64) -> u64 {
        let due = self.pending(now);
        if due > 0 {
            self.expirations = 0;
            if self.interval == 0 {
                self.deadline = None;
            } else if let Some(deadline) = self.deadline {
                let elapsed_periods = (now.saturating_sub(deadline)) / self.interval + 1;
                self.deadline = Some(deadline + elapsed_periods * self.interval);
            }
        }
        due
    }
}

/// An object a file descriptor can refer to.
///
/// Standard streams resolve to the kernel console / stdin; everything else is an
/// ext2-backed file reached through the VFS, carrying its own read/write offset.
pub enum OpenObject {
    /// The kernel console (pre-bound to fds 1 and 2 for stdout/stderr).
    Console,
    /// An AF_INET TCP socket over the kernel's own TCP stack.
    InetTcp(Arc<crate::arch::x86_64::linux::inet_sock::InetTcp>),
    /// An AF_INET UDP socket over the kernel's own UDP stack.
    InetUdp(Arc<crate::arch::x86_64::linux::inet_sock::InetUdp>),
    /// Standard input (pre-bound to fd 0).
    Stdin,
    /// An open ext2-backed file and its current byte offset.
    File {
        /// The VFS node backing this descriptor.
        node: Arc<dyn VfsNode>,
        /// Absolute path the descriptor was opened under (for `/proc/.../fd`).
        path: String,
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
    /// A `timerfd` instance; state is shared with poll/epoll readers.
    Timerfd(Arc<Spinlock<TimerfdState>>),
    /// A `signalfd` instance: the mask of signals it consumes.
    Signalfd {
        mask: Arc<Spinlock<u64>>,
    },
    /// An eventfd counter (EFD_SEMAPHORE if semaphore=true).
    Eventfd {
        val: Arc<Spinlock<u64>>,
        semaphore: bool,
    },
    /// One end of an AF_UNIX stream socketpair — a cross-connected
    /// pair of pipe endpoints (rx = this end's incoming bytes, tx = outgoing).
    Socket {
        rx: Arc<PipeEndpoint>,
        tx: Arc<PipeEndpoint>,
    },
    /// A bound/listening AF_UNIX server socket.
    UnixListener(Arc<UnixListenerState>),
    /// Socket(2) created but not yet bound or connected.
    UnixSocketUnbound {
        nonblocking: bool,
    },
    /// An epoll instance with its interest list.
    Epoll {
        interests: Arc<Spinlock<Vec<EpollEntry>>>,
    },
    Dir {
        /// Absolute path the directory was opened under (used by `fchdir`).
        path: String,
        /// Child nodes captured at open time.
        children: Vec<Arc<dyn VfsNode>>,
        /// Index of the next child `getdents64` will emit.
        index: usize,
    },
}

/// Linux `O_RDONLY` (access mode 0).
pub const O_RDONLY: u64 = 0;
/// Linux `O_WRONLY`.
pub const O_WRONLY: u64 = 1;
/// Linux `O_RDWR`.
pub const O_RDWR: u64 = 2;
/// Linux `O_NONBLOCK` (x86_64).
pub const O_NONBLOCK: u64 = 0o4000;
/// Linux `O_CLOEXEC` (x86_64). Tracked separately in [`FdTable::cloexec`] so
/// `FD_CLOEXEC` can be toggled without rewriting the open flags.
pub const O_CLOEXEC: u64 = 0o2000000;

/// Open-time flags Linux strips from `file->f_flags` after `open(2)` returns
/// (`fs/open.c::do_dentry_open`: `f->f_flags &= ~(O_CREAT | O_EXCL | O_NOCTTY |
/// O_TRUNC | …)`). `/proc/<pid>/fdinfo/<n>` reports `f_flags`, so these must not
/// leak into `flags:` even though the caller passed them to `open`.
const O_CREAT_FLAG: u64 = 0o100;
const O_EXCL_FLAG: u64 = 0o200;
const O_NOCTTY_FLAG: u64 = 0o400;
const O_TRUNC_FLAG: u64 = 0o1000;
const FDINFO_OPEN_TIME_FLAGS: u64 = O_CREAT_FLAG | O_EXCL_FLAG | O_NOCTTY_FLAG | O_TRUNC_FLAG;

/// Default open/status flags for an object created without an explicit open
/// flag set. These are the access modes the object actually has: stdio is
/// one-directional, pipes are one-directional, every socket/event object is
/// read-write. The open path overrides this with the caller's real flags via
/// [`FdTable::alloc_with_flags`].
fn default_object_flags(obj: &OpenObject) -> u64 {
    match obj {
        OpenObject::Stdin | OpenObject::PipeRead(_) => O_RDONLY,
        OpenObject::Console | OpenObject::PipeWrite(_) => O_WRONLY,
        OpenObject::File { .. } | OpenObject::Dir { .. } => O_RDONLY,
        OpenObject::InetTcp(_)
        | OpenObject::InetUdp(_)
        | OpenObject::Socket { .. }
        | OpenObject::UnixListener(_)
        | OpenObject::UnixSocketUnbound { .. }
        | OpenObject::Timerfd(_)
        | OpenObject::Signalfd { .. }
        | OpenObject::Eventfd { .. }
        | OpenObject::Epoll { .. } => O_RDWR,
    }
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
            OpenObject::InetTcp(t) => OpenObject::InetTcp(Arc::clone(t)),
            OpenObject::InetUdp(u) => OpenObject::InetUdp(Arc::clone(u)),
            OpenObject::PipeRead(e) => OpenObject::PipeRead(Arc::clone(e)),
            OpenObject::PipeWrite(e) => OpenObject::PipeWrite(Arc::clone(e)),
            OpenObject::File { node, path, offset } => OpenObject::File {
                node: Arc::clone(node),
                path: path.clone(),
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
            OpenObject::Timerfd(state) => OpenObject::Timerfd(Arc::clone(state)),
            OpenObject::Signalfd { mask } => OpenObject::Signalfd {
                mask: Arc::clone(mask),
            },
            OpenObject::Eventfd { val, semaphore } => OpenObject::Eventfd {
                val: Arc::clone(val),
                semaphore: *semaphore,
            },
            OpenObject::Socket { rx, tx } => OpenObject::Socket {
                rx: Arc::clone(rx),
                tx: Arc::clone(tx),
            },
            OpenObject::UnixListener(l) => OpenObject::UnixListener(Arc::clone(l)),
            OpenObject::UnixSocketUnbound { nonblocking } => OpenObject::UnixSocketUnbound {
                nonblocking: *nonblocking,
            },
            OpenObject::Epoll { interests } => OpenObject::Epoll {
                interests: Arc::clone(interests),
            },
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
    /// The `f_flags` each descriptor was opened with (without `O_CLOEXEC`,
    /// which lives in `cloexec`). This is the real source for `/proc/.../fdinfo`
    /// `flags:`; `dup`/`dup2` copy it, `close` drops it, and the
    /// `F_SETFL`/`FIONBIO` paths update `O_NONBLOCK` when they change it.
    flags: BTreeMap<u32, u64>,
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
        let mut flags = BTreeMap::new();
        flags.insert(0, O_RDONLY);
        flags.insert(1, O_WRONLY);
        flags.insert(2, O_WRONLY);
        Self {
            slots: FdSlots::from_slots(initial),
            cloexec: BTreeSet::new(),
            flags,
        }
    }

    /// Allocate the lowest free descriptor `>= 3`, store `obj` there, and return
    /// the descriptor, growing the table as needed (R2.4). The recorded open
    /// flags are the object's real access mode ([`default_object_flags`]); paths
    /// that know the caller's `open(2)` flags use [`Self::alloc_with_flags`].
    pub fn alloc(&mut self, obj: OpenObject) -> u32 {
        let flags = default_object_flags(&obj);
        self.alloc_with_flags(obj, flags)
    }

    /// Allocate a descriptor and record the exact open/status flags the
    /// descriptor carries. `O_CLOEXEC` is stripped here because it is tracked
    /// in `cloexec` ([`Self::fdinfo_flags`] recombines the two), and the
    /// creation-only flags Linux clears from `f_flags` are removed for the
    /// same reason.
    pub fn alloc_with_flags(&mut self, obj: OpenObject, flags: u64) -> u32 {
        let fd = self.slots.alloc(Self::FIRST_DYNAMIC_FD, obj);
        self.cloexec.remove(&fd); // a recycled slot must not inherit the flag
        self.flags
            .insert(fd, flags & !(O_CLOEXEC | FDINFO_OPEN_TIME_FLAGS));
        fd
    }

    /// Recorded open/status flags for `fd` (without `O_CLOEXEC`), or `None`
    /// when the descriptor is absent.
    pub fn open_flags(&self, fd: u32) -> Option<u64> {
        self.flags.get(&fd).copied()
    }

    /// The `f_flags` value `/proc/<pid>/fdinfo/<fd>` reports: recorded open
    /// flags plus `O_CLOEXEC` when `FD_CLOEXEC` is set on this descriptor.
    pub fn fdinfo_flags(&self, fd: u32) -> Option<u64> {
        let mut flags = self.open_flags(fd)?;
        if self.cloexec.contains(&fd) {
            flags |= O_CLOEXEC;
        }
        Some(flags)
    }

    /// Set or clear `O_NONBLOCK` in the recorded descriptor flags after an
    /// `fcntl(F_SETFL)`/`ioctl(FIONBIO)` actually changed the object's state.
    pub fn set_nonblocking_flag(&mut self, fd: u32, on: bool) {
        if let Some(flags) = self.flags.get_mut(&fd) {
            if on {
                *flags |= O_NONBLOCK;
            } else {
                *flags &= !O_NONBLOCK;
            }
        }
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
        let extra = if nonblocking { O_NONBLOCK } else { 0 };
        let rfd = self.alloc_with_flags(OpenObject::PipeRead(r), O_RDONLY | extra);
        let wfd = self.alloc_with_flags(OpenObject::PipeWrite(w), O_WRONLY | extra);
        (rfd, wfd)
    }

    /// The descriptor table's current slot capacity, reported as `FDSize:` in
    /// `/proc/self/status` (Linux reports the allocated table size, not the
    /// number of open descriptors).
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Close `fd`. Returns `Err(Errno::EBADF)` when the descriptor is absent or
    /// already closed, leaving the table unchanged; otherwise releases it and
    /// returns `Ok` (R2.6, R2.14).
    pub fn close(&mut self, fd: u32) -> Result<(), Errno> {
        let res = self.slots.close(fd).map_err(|_| Errno::EBADF);
        if res.is_ok() {
            self.cloexec.remove(&fd);
            self.flags.remove(&fd);
        }
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
        let flags = self
            .flags
            .get(&oldfd)
            .copied()
            .unwrap_or_else(|| default_object_flags(&dup));
        // F-r3-life-1: `min` is caller-controlled (`fcntl(F_DUPFD, arg)`). The
        // syscall layer rejects `arg >= NOFILE_MAX` with EINVAL, but the table
        // itself refuses to grow past `FD_TABLE_LIMIT` so no path can turn a
        // huge minimum into a multi-hundred-MiB allocation (EMFILE).
        let fd = self
            .slots
            .try_alloc(min as usize, super::fd_alloc::FD_TABLE_LIMIT, dup)
            .map_err(|_| Errno::EMFILE)?;
        self.cloexec.remove(&fd); // the duplicate starts without FD_CLOEXEC
        self.flags.insert(fd, flags);
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
        let flags = self
            .flags
            .get(&oldfd)
            .copied()
            .unwrap_or_else(|| default_object_flags(&dup));
        self.slots.set(newfd, dup);
        self.cloexec.remove(&newfd); // dup2/dup3 clear FD_CLOEXEC on the target
        self.flags.insert(newfd, flags);
        Ok(newfd)
    }

    // ── close-on-exec bookkeeping ──

    /// Set or clear the FD_CLOEXEC flag for `fd`.
    pub fn set_cloexec(&mut self, fd: u32, on: bool) {
        if on {
            self.cloexec.insert(fd);
        } else {
            self.cloexec.remove(&fd);
        }
    }

    /// Whether `fd` carries the FD_CLOEXEC flag.
    pub fn is_cloexec(&self, fd: u32) -> bool {
        self.cloexec.contains(&fd)
    }

    /// Short human name for whatever occupies `fd`.
    pub fn describe_fd(&self, fd: u32) -> &'static str {
        match self.slots.get(fd) {
            None => "closed",
            Some(OpenObject::Stdin) => "stdin(kbd)",
            Some(OpenObject::Console) => "console",
            Some(OpenObject::PipeRead(_)) => "pipe-r",
            Some(OpenObject::PipeWrite(_)) => "pipe-w",
            Some(OpenObject::Socket { .. }) => "socket",
            Some(OpenObject::InetTcp(_)) => "inet-tcp",
            Some(OpenObject::InetUdp(_)) => "inet-udp",
            Some(OpenObject::UnixListener(_)) => "unix-listener",
            Some(OpenObject::UnixSocketUnbound { .. }) => "unix-unbound",
            Some(OpenObject::Timerfd(_)) => "timerfd",
            Some(OpenObject::Signalfd { .. }) => "signalfd",
            Some(OpenObject::Eventfd { .. }) => "eventfd",
            Some(OpenObject::File { .. }) => "file",
            Some(OpenObject::Dir { .. }) => "dir",
            Some(OpenObject::Epoll { .. }) => "epoll",
        }
    }

    /// Close every descriptor flagged close-on-exec (the execve sweep).
    pub fn close_cloexec(&mut self) {
        let fds = core::mem::take(&mut self.cloexec);
        for fd in fds {
            let _ = self.slots.close(fd);
            self.flags.remove(&fd);
        }
    }

    /// `socketpair(AF_UNIX, SOCK_STREAM)`: two cross-connected
    /// in-kernel byte queues. Each end reads from one queue and writes to the
    /// other, so BOTH fds are readable and writable (unlike a pipe). Closing
    /// one end makes the peer observe EOF on read and EPIPE on write.
    pub fn socketpair(&mut self, nonblocking: bool) -> (u32, u32) {
        let q_ab = Arc::new(Spinlock::new(PipeState {
            bytes: VecDeque::new(),
            readers: 1,
            writers: 1,
        }));
        let q_ba = Arc::new(Spinlock::new(PipeState {
            bytes: VecDeque::new(),
            readers: 1,
            writers: 1,
        }));
        let a = OpenObject::Socket {
            rx: Arc::new(PipeEndpoint {
                state: Arc::clone(&q_ba),
                read_end: true,
                nonblocking,
            }),
            tx: Arc::new(PipeEndpoint {
                state: Arc::clone(&q_ab),
                read_end: false,
                nonblocking,
            }),
        };
        let b = OpenObject::Socket {
            rx: Arc::new(PipeEndpoint {
                state: q_ab,
                read_end: true,
                nonblocking,
            }),
            tx: Arc::new(PipeEndpoint {
                state: q_ba,
                read_end: false,
                nonblocking,
            }),
        };
        let extra = if nonblocking { O_NONBLOCK } else { 0 };
        let fa = self.alloc_with_flags(a, O_RDWR | extra);
        let fb = self.alloc_with_flags(b, O_RDWR | extra);
        // Which descriptor numbers the RPC channel ends get.

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
        Self {
            path,
            inner: Spinlock::new(UnixListenerInner {
                listening: false,
                nonblocking,
                pending: VecDeque::new(),
            }),
        }
    }
}

/// Build the two endpoint pairs of a connected AF_UNIX stream —
/// (client side, server side) — without allocating fds. connect(2) pushes the
/// server pair into the listener queue and installs the client pair locally;
/// accept(2) later turns the server pair into a fresh fd.
pub fn socket_pair_endpoints(
    nonblocking_client: bool,
    nonblocking_server: bool,
) -> (
    (Arc<PipeEndpoint>, Arc<PipeEndpoint>),
    (Arc<PipeEndpoint>, Arc<PipeEndpoint>),
) {
    let q_cs = Arc::new(Spinlock::new(PipeState {
        bytes: VecDeque::new(),
        readers: 1,
        writers: 1,
    }));
    let q_sc = Arc::new(Spinlock::new(PipeState {
        bytes: VecDeque::new(),
        readers: 1,
        writers: 1,
    }));
    let client = (
        Arc::new(PipeEndpoint {
            state: Arc::clone(&q_sc),
            read_end: true,
            nonblocking: nonblocking_client,
        }),
        Arc::new(PipeEndpoint {
            state: Arc::clone(&q_cs),
            read_end: false,
            nonblocking: nonblocking_client,
        }),
    );
    let server = (
        Arc::new(PipeEndpoint {
            state: q_cs,
            read_end: true,
            nonblocking: nonblocking_server,
        }),
        Arc::new(PipeEndpoint {
            state: q_sc,
            read_end: false,
            nonblocking: nonblocking_server,
        }),
    );
    (client, server)
}
