//! Effectful Linux file-I/O syscall handlers (task 12.1).
//!
//! This is the **kernel-only** half of the `io` component. It wires the pure
//! planners in [`super::io`] (`plan_read`/`plan_lseek`) and [`super::stat`]
//! (`encode_stat`) to the running `Compat_Process`'s [`FdTable`], the kernel
//! console, and the VFS/ext2 file tree.
//!
//! It lives in its OWN file (not `io.rs`) on purpose: `io.rs` is `#[path]`-included
//! verbatim by the `host-tests` crate so its pure planners can be property-tested
//! on the host (R11.6). These handlers use kernel-only `memory`/`vfs`/`task` APIs
//! that do not exist on the host, so keeping them here leaves `io.rs` purely
//! host-testable while this file is compiled only as part of the kernel.
//!
//! ## User-pointer safety
//!
//! Every handler that takes a user pointer routes it through the single
//! [`super::check_user_ptr`] choke point (range check + page-presence walk) BEFORE
//! dereferencing it. During a syscall the active CR3 is the calling process's user
//! PML4, so a validated lower-half user pointer is directly accessible from ring 0.
//!
//! ## Locking discipline
//!
//! [`crate::task::compat::with_current_compat`] holds the `COMPAT_STATES` spinlock
//! (interrupts disabled) for the duration of its closure. Disk-backed VFS reads
//! can block waiting for a device interrupt, so these handlers never hold that lock
//! across a VFS call: they resolve the descriptor (cloning the `Arc<dyn VfsNode>`
//! and snapshotting the offset) under the lock, release it, perform the I/O, then
//! re-acquire briefly to commit the new offset.
#![allow(dead_code)]

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::task::compat;
use crate::task::fd::{OpenObject, PipeEndpoint, PipeReadResult, PipeWriteResult};
use crate::vfs::{self, VfsNode};

use super::check_user_ptr;
use super::dirent::{dirent_reclen, encode_dirent64, DT_DIR, DT_REG};
use super::errno::Errno;
use super::io::{plan_lseek, plan_read};
use super::stat::{encode_stat, LinuxStat, S_IFDIR, S_IFREG};

/// `st_mode` type bits for a character device (console/stdin), so `fstat` on a
/// standard stream reports a plausible (non-regular) type.
const S_IFCHR: u32 = 0o020000;

/// `st_mode` type bits for a socket (STAGE-15 socketpair ends).
const S_IFSOCK: u32 = 0o140000;

/// Largest byte count `read`/`write` accept, matching the Linux `int` cap in
/// R2.2 (0..=2_147_483_647). Larger requests are rejected with `EINVAL` rather
/// than attempting a multi-gigabyte kernel allocation.
const COUNT_MAX: u64 = 0x7FFF_FFFF;

/// `openat` "current working directory" sentinel dir fd.
const AT_FDCWD: u64 = (-100i64) as u64;

/// Default permission bits reported for an ext2-backed regular file.
const DEFAULT_FILE_PERMS: u32 = 0o644;

/// A descriptor resolved to an actionable target, decoupled from the
/// `COMPAT_STATES` lock so subsequent (possibly blocking) VFS I/O runs unlocked.
enum Resolved {
    /// fds 1/2 — the kernel console.
    Console,
    /// fd 0 — standard input (not writable).
    Stdin,
    /// An ext2-backed file: a cloned node handle and the offset at resolve time.
    File { node: Arc<dyn VfsNode>, offset: u64 },
    PipeRead(Arc<PipeEndpoint>),
    PipeWrite(Arc<PipeEndpoint>),
    /// STAGE-15: one end of an AF_UNIX socketpair (rx = incoming, tx = outgoing).
    Socket { rx: Arc<PipeEndpoint>, tx: Arc<PipeEndpoint> },
    /// An eventfd counter.
    Eventfd { val: Arc<crate::sync::spinlock::Spinlock<u64>>, semaphore: bool },
    /// An epoll instance (not directly readable/writable via read/write).
    Epoll,
    /// An open directory (not a byte stream): read/write/pread/pwrite are rejected.
    Dir,
}

/// Resolve `fd` for the current process, cloning the backing node so the caller
/// can drop the `COMPAT_STATES` lock before doing VFS I/O. Returns `None` when the
/// descriptor is absent/closed or the process has no compat state (→ `EBADF`).
fn resolve_fd(fd: u32) -> Option<Resolved> {
    compat::with_current_compat(|cs| {
        cs.fds.get(fd).map(|obj| match obj {
            OpenObject::Console => Resolved::Console,
            OpenObject::Stdin => Resolved::Stdin,
            OpenObject::PipeRead(e) => Resolved::PipeRead(Arc::clone(e)),
            OpenObject::PipeWrite(e) => Resolved::PipeWrite(Arc::clone(e)),
            OpenObject::Socket { rx, tx } => Resolved::Socket { rx: Arc::clone(rx), tx: Arc::clone(tx) },
            OpenObject::File { node, offset } => Resolved::File {
                node: Arc::clone(node),
                offset: *offset,
            },
            OpenObject::Dir { .. } => Resolved::Dir,
            OpenObject::Eventfd { val, semaphore } => Resolved::Eventfd { val: Arc::clone(val), semaphore: *semaphore },
            // STAGE-16: not byte streams — same read/write/seek rejections as epoll fds.
            OpenObject::UnixListener(_) => Resolved::Epoll,
            OpenObject::UnixSocketUnbound { .. } => Resolved::Epoll,
            OpenObject::Epoll { .. } => Resolved::Epoll,
        })
    })
    .flatten()
}

/// Clone the VFS node backing an open regular-file descriptor, for callers
/// outside this module that need the file bytes without holding the compat
/// lock (currently `mem_sys::sys_mmap`'s file-backed mappings). Returns `None`
/// for absent descriptors and anything that is not an ext2-backed `File`.
pub fn file_node_for_fd(fd: u32) -> Option<Arc<dyn VfsNode>> {
    match resolve_fd(fd) {
        Some(Resolved::File { node, .. }) => Some(node),
        _ => None,
    }
}

/// Commit a new offset for an open file descriptor (no-op if it is not currently
/// an open `File`). Used after a `read`/`write`/`lseek` advances the offset.
fn set_fd_offset(fd: u32, new_off: u64) {
    compat::with_current_compat(|cs| {
        if let Some(OpenObject::File { offset, .. }) = cs.fds.get_mut(fd) {
            *offset = new_off;
        }
    });
}

/// Write `slice` to the kernel console, reusing the exact serial-console path the
/// legacy `SYS_WRITE` uses (valid-UTF-8 prefix written as `&str`, trailing
/// invalid bytes emitted individually so output never panics).
fn console_write(slice: &[u8]) {
    use crate::drivers::Console;
    let console = crate::drivers::serial::console();
    // STAGE-13.7: mirror every stdout/stderr byte (and the stdin echo) to the
    // framebuffer console as well. Previously this wrote ONLY to serial, so a
    // Compat_Process was invisible in the QEMU graphical window: the shell
    // prints via kprintln!+fb_println!, but python's output bypassed the
    // framebuffer entirely. The fb writer already handles \n scrolling and
    // 0x08 backspace, so the line-editor echo renders correctly too.
    // STAGE-14 VT: route all compat stdout/stderr through the VT emulator.
    // STAGE-16.4 DIAG: if this counter grows but the screen stays black, the
    // VT renderer is the suspect; if it never grows, the UI client never draws.
    diag_count(&DIAG_CONSOLE_BYTES, "console bytes", slice.len() as u64, 8192);
    crate::drivers::vt::write(slice);
    match core::str::from_utf8(slice) {
        Ok(s) => { console.write_str(s); }
        Err(e) => {
            let valid_up_to = e.valid_up_to();
            // SAFETY: `from_utf8` guarantees `slice[..valid_up_to]` is valid UTF-8.
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

/// Copy `len` validated user bytes at `ptr` into an owned buffer.
///
/// PRECONDITION: `[ptr, ptr+len)` has already passed [`check_user_ptr`].
fn copy_in(ptr: u64, len: u64) -> Vec<u8> {
    let mut buf = vec![0u8; len as usize];
    if len > 0 {
        // SAFETY: the range was validated (in-range + every page mapped) and the
        // active CR3 is the calling process's user PML4, so the source is readable.
        unsafe {
            core::ptr::copy_nonoverlapping(ptr as *const u8, buf.as_mut_ptr(), len as usize);
        }
    }
    buf
}

/// Copy `src` out to the validated user buffer at `ptr`.
///
/// PRECONDITION: `[ptr, ptr+src.len())` has already passed [`check_user_ptr`].
fn copy_out(ptr: u64, src: &[u8]) {
    if !src.is_empty() {
        // SAFETY: validated range, active user CR3 — destination is writable.
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), ptr as *mut u8, src.len());
        }
    }
}

/// Read a NUL-terminated path string from user memory, validating each byte's
/// page before dereferencing it. Caps the path at 4096 bytes (`EINVAL` if longer
/// or not valid UTF-8).
fn read_user_cstr(ptr: u64) -> Result<String, Errno> {
    const PATH_MAX: usize = 4096;
    let mut bytes: Vec<u8> = Vec::new();
    let mut addr = ptr;
    for _ in 0..PATH_MAX {
        check_user_ptr(addr, 1)?;
        // SAFETY: the single byte at `addr` was just validated as mapped/in-range.
        let b = unsafe { *(addr as *const u8) };
        if b == 0 {
            return String::from_utf8(bytes).map_err(|_| Errno::EINVAL);
        }
        bytes.push(b);
        addr += 1;
    }
    Err(Errno::EINVAL)
}

fn read_pipe(e:&PipeEndpoint,dst:&mut[u8])->Result<usize,Errno>{loop{match e.read(dst){PipeReadResult::Data(n)=>return Ok(n),PipeReadResult::Eof=>return Ok(0),PipeReadResult::WouldBlock if e.nonblocking()=>return Err(Errno::EAGAIN),PipeReadResult::WouldBlock=>crate::task::scheduler::yield_current()}}}
fn write_pipe(e:&PipeEndpoint,src:&[u8])->Result<usize,Errno>{loop{match e.write(src){PipeWriteResult::Data(n)=>return Ok(n),PipeWriteResult::Broken=>return Err(Errno::EPIPE),PipeWriteResult::WouldBlock if e.nonblocking()=>return Err(Errno::EAGAIN),PipeWriteResult::WouldBlock=>crate::task::scheduler::yield_current()}}}

/// `read` (0): copy up to `count` bytes from the file at its current offset into
/// the user buffer, advance the offset by the bytes copied, and return that count
/// (R2.3). `EBADF` for an absent fd (R2.14); reads on the console/stdin return 0.
pub fn sys_read(fd: u64, buf: u64, count: u64) -> Result<u64, Errno> {
    if count > COUNT_MAX {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(buf, count)?;

    match resolve_fd(fd as u32) {
        None => Err(Errno::EBADF),
        // STAGE-13.7: stdin is interactive now — a blocking, line-buffered
        // read from the PS/2 keyboard (echo, backspace, ^D = EOF). Previously
        // this returned an instant EOF, so CPython silently exited with 0.
        Some(Resolved::Console) | Some(Resolved::Stdin) => {
            let raw = crate::task::compat::with_current_compat(|cs| cs.raw_mode).unwrap_or(false);
            if raw { read_stdin_raw(buf, count) } else { read_stdin_line(buf, count) }
        }
        Some(Resolved::Dir) => Err(Errno::EISDIR),
        Some(Resolved::PipeWrite(_)) => Err(Errno::EBADF),
        Some(Resolved::Eventfd { val, semaphore }) => {
            // eventfd read: blocks until val > 0, then returns 8-byte u64 and resets.
            if count < 8 { return Err(Errno::EINVAL); }
            check_user_ptr(buf, 8)?;
            loop {
                let mut v = val.lock();
                if *v > 0 {
                    let out = if semaphore { *v -= 1; 1u64 } else { let r=*v; *v=0; r };
                    drop(v);
                    // SAFETY: validated above
                    unsafe { core::ptr::write_unaligned(buf as *mut u64, out); }
                    return Ok(8);
                }
                drop(v);
                crate::task::scheduler::yield_current();
            }
        }
        Some(Resolved::Epoll) => Err(Errno::EINVAL),
        Some(Resolved::PipeRead(e)) => { let mut data=vec![0u8;count as usize];let n=read_pipe(&e,&mut data)?;copy_out(buf,&data[..n]);Ok(n as u64) }
        Some(Resolved::Socket { rx, .. }) => { let mut data=vec![0u8;count as usize];let n=read_pipe(&rx,&mut data)?;if n>0{diag_count(&DIAG_SOCK_R,"sock read",n as u64,16384);}copy_out(buf,&data[..n]);Ok(n as u64) }
        Some(Resolved::File { node, offset }) => {
            let size = node.size();
            let (copied, _) = plan_read(size, offset, count);
            if copied == 0 {
                return Ok(0);
            }
            let mut kbuf = vec![0u8; copied as usize];
            let n = node.read(offset, &mut kbuf).map_err(|e| {
                // STAGE-13.7: a real VFS/ext2 read failure is not "invalid
                // argument" — report EIO and log what actually broke.
                crate::error!(
                    "[linux] file read failed: {:?} ino={} off={} len={}",
                    e, node.fs_ino(), offset, copied
                );
                Errno::EIO
            })?;
            copy_out(buf, &kbuf[..n]);
            let new_off = offset + n as u64;
            set_fd_offset(fd as u32, new_off);
            Ok(n as u64)
        }
    }
}

/// `write` (1): write `count` user bytes to the descriptor. fds 1/2 (console)
/// emit to the kernel console and return `count` (R2.2); a file descriptor writes
/// at its offset and advances it; stdin is not writable (`EBADF`); an absent fd is
/// `EBADF` (R2.14).
pub fn sys_write(fd: u64, buf: u64, count: u64) -> Result<u64, Errno> {
    if count > COUNT_MAX {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(buf, count)?;

    match resolve_fd(fd as u32) {
        None => Err(Errno::EBADF),
        Some(Resolved::Stdin) => Err(Errno::EBADF),
        Some(Resolved::Dir) => Err(Errno::EISDIR),
        Some(Resolved::Eventfd { val, .. }) => {
            if count < 8 { return Err(Errno::EINVAL); }
            check_user_ptr(buf, 8)?;
            // SAFETY: validated above
            let add = unsafe { core::ptr::read_unaligned(buf as *const u64) };
            *val.lock() = val.lock().saturating_add(add);
            Ok(8)
        }
        Some(Resolved::Epoll) => Err(Errno::EINVAL),
        Some(Resolved::PipeRead(_)) => Err(Errno::EBADF),
        Some(Resolved::PipeWrite(e)) => {let data=copy_in(buf,count);Ok(write_pipe(&e,&data)? as u64)}
        Some(Resolved::Socket { tx, .. }) => {let data=copy_in(buf,count);let n=write_pipe(&tx,&data)?;if n>0{diag_count(&DIAG_SOCK_W,"sock write",n as u64,16384);}Ok(n as u64)}
        Some(Resolved::Console) => {
            let data = copy_in(buf, count);
            console_write(&data);
            Ok(count)
        }
        Some(Resolved::File { node, offset }) => {
            let data = copy_in(buf, count);
            let n = node.write(offset, &data).map_err(|_| Errno::EINVAL)?;
            set_fd_offset(fd as u32, offset + n as u64);
            Ok(n as u64)
        }
    }
}

/// `writev` (20): gather-write up to `iovcnt` `iovec` entries. fds 1/2 emit each
/// buffer to the console; a file descriptor writes them in order advancing its
/// offset; returns the total bytes written (R2.2). Each `iov_base` is validated
/// through the pointer choke point before being read.
pub fn sys_writev(fd: u64, iov: u64, iovcnt: u64) -> Result<u64, Errno> {
    // struct iovec { void *iov_base; size_t iov_len; } — 16 bytes on x86_64.
    const IOV_SIZE: u64 = 16;
    const IOV_MAX: u64 = 1024;
    if iovcnt == 0 {
        return Ok(0);
    }
    if iovcnt > IOV_MAX {
        return Err(Errno::EINVAL);
    }
    // Validate the iovec array itself before reading any entry.
    check_user_ptr(iov, iovcnt * IOV_SIZE)?;

    let target = resolve_fd(fd as u32).ok_or(Errno::EBADF)?;
    if matches!(target, Resolved::Stdin) {
        return Err(Errno::EBADF);
    }
    if matches!(target, Resolved::Dir) { return Err(Errno::EISDIR); }
    if matches!(target, Resolved::Eventfd { .. } | Resolved::Epoll) { return Err(Errno::EINVAL); }
    if matches!(target, Resolved::PipeRead(_)) { return Err(Errno::EBADF); }

    // Track a running offset for the file case; commit it once at the end.
    let mut file_off = match &target {
        Resolved::File { offset, .. } => *offset,
        _ => 0,
    };
    let mut total: u64 = 0;

    for i in 0..iovcnt {
        let entry = iov + i * IOV_SIZE;
        // SAFETY: the whole iovec array range was validated above.
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
                console_write(&data);
                total += len;
            }
            Resolved::PipeWrite(e) => {let n=write_pipe(e,&data)?;total+=n as u64;if n<data.len(){break;}}
            Resolved::File { node, .. } => {
                let n = node.write(file_off, &data).map_err(|_| Errno::EINVAL)?;
                file_off += n as u64;
                total += n as u64;
            }
            Resolved::Socket { tx, .. } => {let n=write_pipe(tx,&data)?;total+=n as u64;if n<data.len(){break;}}
            // Stdin/PipeRead/Dir/Eventfd/Epoll are rejected by the guards above.
            _ => unreachable!(),
        }
    }

    if matches!(target, Resolved::File { .. }) {
        set_fd_offset(fd as u32, file_off);
    }
    Ok(total)
}

/// STAGE-16.11: `readv` (19) — vectored read. Delegates to `sys_read` on the
/// first non-empty iovec: a short count is a legal readv result and callers
/// (glibc stdio, libuv) loop for the rest, so this inherits sys_read's per-fd
/// blocking semantics without duplicating them.
pub fn sys_readv(fd: u64, iov: u64, iovcnt: u64) -> Result<u64, Errno> {
    const IOV_SIZE: u64 = 16;
    const IOV_MAX: u64 = 1024;
    if iovcnt == 0 { return Ok(0); }
    if iovcnt > IOV_MAX { return Err(Errno::EINVAL); }
    check_user_ptr(iov, iovcnt * IOV_SIZE)?;
    for i in 0..iovcnt {
        let entry = iov + i * IOV_SIZE;
        // SAFETY: the whole iovec array range was validated above.
        let base = unsafe { *(entry as *const u64) };
        let len = unsafe { *((entry + 8) as *const u64) };
        if len == 0 { continue; }
        return sys_read(fd, base, len);
    }
    Ok(0)
}

/// STAGE-16.11: `preadv` (295) — positional vectored read; first non-empty
/// iovec via `sys_pread64` (the descriptor offset is not advanced).
pub fn sys_preadv(fd: u64, iov: u64, iovcnt: u64, offset: u64) -> Result<u64, Errno> {
    const IOV_SIZE: u64 = 16;
    const IOV_MAX: u64 = 1024;
    if iovcnt == 0 { return Ok(0); }
    if iovcnt > IOV_MAX { return Err(Errno::EINVAL); }
    check_user_ptr(iov, iovcnt * IOV_SIZE)?;
    for i in 0..iovcnt {
        let entry = iov + i * IOV_SIZE;
        // SAFETY: the whole iovec array range was validated above.
        let base = unsafe { *(entry as *const u64) };
        let len = unsafe { *((entry + 8) as *const u64) };
        if len == 0 { continue; }
        return sys_pread64(fd, base, len, offset);
    }
    Ok(0)
}

/// STAGE-16.11: `pwritev` (296) — positional vectored write; first non-empty
/// iovec via `sys_pwrite64`.
pub fn sys_pwritev(fd: u64, iov: u64, iovcnt: u64, offset: u64) -> Result<u64, Errno> {
    const IOV_SIZE: u64 = 16;
    const IOV_MAX: u64 = 1024;
    if iovcnt == 0 { return Ok(0); }
    if iovcnt > IOV_MAX { return Err(Errno::EINVAL); }
    check_user_ptr(iov, iovcnt * IOV_SIZE)?;
    for i in 0..iovcnt {
        let entry = iov + i * IOV_SIZE;
        // SAFETY: the whole iovec array range was validated above.
        let base = unsafe { *(entry as *const u64) };
        let len = unsafe { *((entry + 8) as *const u64) };
        if len == 0 { continue; }
        return sys_pwrite64(fd, base, len, offset);
    }
    Ok(0)
}

/// STAGE-16.11: `fsync`/`fdatasync` (74/75). nvim fsyncs the ShaDa file on
/// write; the ext2 WAL already journals every write at syscall time (ordered
/// mode), so there is no dirty cache to flush — success on any valid fd.
pub fn sys_fsync(fd: u64) -> Result<u64, Errno> {
    match resolve_fd(fd as u32) {
        None => Err(Errno::EBADF),
        Some(_) => Ok(0),
    }
}

/// Read the current process's cwd (absolute), defaulting to `/` when there is no
/// compat state (a native task driving these handlers in a test harness).
fn current_cwd() -> String {
    compat::with_current_compat(|cs| cs.cwd.clone()).unwrap_or_else(|| String::from("/"))
}

/// Normalize `path`, resolving it against the current working directory when it is
/// relative, and collapsing `.`/`..`/empty components into a clean absolute path.
///
/// `..` at the root stays at the root; the result always begins with `/` and never
/// has a trailing slash (except the bare root `/`). This is the single place
/// relative `open`/`openat`/`access`/`chdir`/`statfs`/`readlink` paths become
/// absolute before hitting [`vfs::lookup_path`] (Feature: linux-binary-compat).
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
    out
}

/// Build the [`OpenObject`] for an already-resolved absolute path, allocating a
/// fresh descriptor for it. A directory becomes an [`OpenObject::Dir`] carrying a
/// snapshot of its children (for `getdents64`); a file becomes an
/// [`OpenObject::File`] at offset 0.
fn open_resolved(abs: &str) -> Result<u64, Errno> {
    let node = vfs::lookup_path(abs).map_err(|_| Errno::ENOENT)?;
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
            offset: 0,
        }
    };
    let fd = compat::with_current_compat(|cs| cs.fds.alloc(obj));
    match fd {
        Some(fd) => Ok(fd as u64),
        // No compat state (native task) — nowhere to record the descriptor.
        None => Err(Errno::EBADF),
    }
}

// STAGE-16.2: creation flags for open/openat (octal values from fcntl.h).
const O_CREAT_FL: u64 = 0o100;
const O_EXCL_FL: u64 = 0o200;
const O_TRUNC_FL: u64 = 0o1000;

/// Resolve a user path (against the cwd if relative) and allocate a fresh
/// descriptor for it (R2.4). STAGE-16.2: honors O_CREAT/O_EXCL (creates a
/// regular file via the parent directory's `create_file` - i.e. works on the
/// /tmp ramfs; read-only trees still refuse) and best-effort O_TRUNC.
/// `ENOENT` if the path is absent and O_CREAT is unset (R2.5). Shared by
/// `open`/`openat`.
fn open_path(path: &str, flags: u64) -> Result<u64, Errno> {
    let abs = resolve_path(path);
    let trimmed = abs.trim_end_matches('/');
    let lookup_target = if trimmed.is_empty() { "/" } else { trimmed };
    match vfs::lookup_path(lookup_target) {
        Ok(node) => {
            if flags & O_CREAT_FL != 0 && flags & O_EXCL_FL != 0 {
                return Err(Errno::EEXIST);
            }
            if flags & O_TRUNC_FL != 0 && !node.is_directory() {
                // Best effort: nodes without truncate support (plain ext2
                // files in this minimal layer) keep their contents.
                let _ = node.truncate(0);
            }
        }
        Err(_) => {
            if flags & O_CREAT_FL == 0 {
                return Err(Errno::ENOENT);
            }
            let (parent, name) = match trimmed.rfind('/') {
                Some(0) => ("/", &trimmed[1..]),
                Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
                None => return Err(Errno::ENOENT),
            };
            if name.is_empty() {
                return Err(Errno::EINVAL);
            }
            let dir = vfs::lookup_path(parent).map_err(|_| Errno::ENOENT)?;
            dir.create_file(name).map_err(|e| {
                crate::warn!("[linux] open(O_CREAT) failed: {:?} parent={} name={}", e, parent, name);
                Errno::EIO
            })?;
        }
    }
    open_resolved(&abs)
}

/// `open` (2): open an existing ext2 path (resolved against the cwd if relative),
/// allocating the lowest fd ≥ 3 (R2.4); `ENOENT` if the path is absent (R2.5).
/// Directories open as a directory descriptor usable with `getdents64`.
pub fn sys_open(path: u64, flags: u64, _mode: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    let fd = open_path(&p, flags)?;
    // STAGE-15: honor O_CLOEXEC now that execve sweeps flagged descriptors.
    if flags & O_CLOEXEC != 0 {
        compat::with_current_compat(|cs| cs.fds.set_cloexec(fd as u32, true));
    }
    Ok(fd)
}

/// `openat` (257): like `open`. `AT_FDCWD` (and any dirfd in this minimal layer)
/// resolves the path against the process cwd; absolute paths ignore the dirfd.
pub fn sys_openat(dirfd: u64, path: u64, flags: u64, _mode: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    // Absolute paths ignore dirfd; relative paths resolve against the cwd (the
    // only directory base this minimal layer tracks), which covers AT_FDCWD.
    let _ = dirfd;
    let fd = open_path(&p, flags)?;
    // STAGE-15: honor O_CLOEXEC now that execve sweeps flagged descriptors.
    if flags & O_CLOEXEC != 0 {
        compat::with_current_compat(|cs| cs.fds.set_cloexec(fd as u32, true));
    }
    Ok(fd)
}

/// `close` (3): release the descriptor, or `EBADF` if it is not open (R2.6, R2.14).
pub fn sys_close(fd: u64) -> Result<u64, Errno> {
    let res = compat::with_current_compat(|cs| cs.fds.close(fd as u32));
    match res {
        Some(Ok(())) => Ok(0),
        Some(Err(e)) => Err(e),
        None => Err(Errno::EBADF),
    }
}

/// `lseek` (8): reposition a file descriptor's offset per `whence`/`offset`,
/// returning the new absolute offset (R2.7) or `EINVAL` for a bad whence/negative
/// result (R2.15). Console/stdin are not seekable (`EINVAL`); absent fd is `EBADF`.
pub fn sys_lseek(fd: u64, offset: u64, whence: u64) -> Result<u64, Errno> {
    match resolve_fd(fd as u32) {
        None => Err(Errno::EBADF),
        // STAGE-13.8 ERRNO FIX: the console is not seekable — ESPIPE, like a
        // pipe. CPython probes lseek on fds 0/1/2 at startup to decide
        // buffering; EINVAL spammed the diag log three times per start.
        Some(Resolved::Console) | Some(Resolved::Stdin) => Err(Errno::ESPIPE),
        Some(Resolved::PipeRead(_)) | Some(Resolved::PipeWrite(_)) => Err(Errno::ESPIPE),
        Some(Resolved::Socket { .. }) => Err(Errno::ESPIPE),
        Some(Resolved::Eventfd { .. }) | Some(Resolved::Epoll) => Err(Errno::ESPIPE),
        // A directory descriptor supports rewinding/positioning its dents cursor:
        // SEEK_SET sets the cursor index, returning it. Other whences are EINVAL.
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
        Some(Resolved::File { node, offset: cur }) => {
            let size = node.size();
            let new_off = plan_lseek(whence as u32, cur, size, offset as i64)?;
            set_fd_offset(fd as u32, new_off);
            Ok(new_off)
        }
    }
}

/// `st_dev` reported for all VFS-backed files (arbitrary but nonzero; 8:0).
const STAT_DEV_VFS: u64 = 0x0800;

/// Nonzero fallback inode for synthetic nodes without filesystem identity:
/// FNV-1a of the node name with the top bit set, so it can never collide with
/// a real ext2 inode number (those fit in 32 bits) and never reads as zero.
fn synth_ino(name: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        h = (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h | (1 << 63)
}

/// Fill a [`LinuxStat`] for a node and copy it to the validated user buffer.
fn write_stat(node: &Arc<dyn VfsNode>, statbuf: u64) -> Result<u64, Errno> {
    let mode = if node.is_directory() {
        S_IFDIR | 0o700
    } else {
        S_IFREG | DEFAULT_FILE_PERMS
    };
    let mut stat = encode_stat(node.size(), mode);
    // STAGE-13.7 FIX: file identity matters. glibc's ld.so deduplicates loaded
    // shared objects by the (st_dev, st_ino) pair from `fstat`, and the main
    // executable's link_map carries an all-zero file id. With st_dev/st_ino
    // left zeroed for every file, each freshly opened library "matched" the
    // main binary, the loader reused its link_map instead of mapping libc, and
    // startup died with "no version information available" spam followed by
    // undefined `__libc_start_main, version GLIBC_2.34` (exit 127).
    stat.st_dev = STAT_DEV_VFS;
    stat.st_ino = match node.fs_ino() {
        0 => synth_ino(node.name()),
        ino => ino,
    };
    stat.st_nlink = 1;
    stat.st_blocks = (stat.st_size + 511) / 512;
    write_stat_struct(&stat, statbuf);
    Ok(0)
}

/// Copy a fully-built [`LinuxStat`] to the validated user buffer.
fn write_stat_struct(stat: &LinuxStat, statbuf: u64) {
    let bytes = unsafe {
        core::slice::from_raw_parts(
            stat as *const LinuxStat as *const u8,
            core::mem::size_of::<LinuxStat>(),
        )
    };
    copy_out(statbuf, bytes);
}

/// `fstat` (5): populate the user `struct stat` for an open descriptor (R2.8).
/// `EBADF` for an absent fd (R2.14). Console/stdin report a character device.
pub fn sys_fstat(fd: u64, statbuf: u64) -> Result<u64, Errno> {
    check_user_ptr(statbuf, core::mem::size_of::<LinuxStat>() as u64)?;
    match resolve_fd(fd as u32) {
        None => Err(Errno::EBADF),
        Some(Resolved::Socket { .. }) => {
            // STAGE-15: socketpair ends report S_IFSOCK — libuv's
            // uv_guess_handle checks the fd type of stdio descriptors.
            let stat = encode_stat(0, S_IFSOCK | 0o666);
            write_stat_struct(&stat, statbuf);
            Ok(0)
        }
        Some(Resolved::Console) | Some(Resolved::Stdin)
        | Some(Resolved::PipeRead(_)) | Some(Resolved::PipeWrite(_))
        | Some(Resolved::Eventfd { .. }) | Some(Resolved::Epoll) => {
            let stat = encode_stat(0, S_IFCHR | 0o620);
            write_stat_struct(&stat, statbuf);
            Ok(0)
        }
        Some(Resolved::Dir) => {
            let stat = encode_stat(0, S_IFDIR | 0o700);
            write_stat_struct(&stat, statbuf);
            Ok(0)
        }
        Some(Resolved::File { node, .. }) => write_stat(&node, statbuf),
    }
}

/// `newfstatat` (262): stat a path (absolute, or `AT_FDCWD`-relative) into the
/// user `struct stat` (R2.8); `ENOENT` if absent (R2.5).
pub fn sys_newfstatat(_dirfd: u64, path: u64, statbuf: u64, _flags: u64) -> Result<u64, Errno> {
    check_user_ptr(statbuf, core::mem::size_of::<LinuxStat>() as u64)?;
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    let node = vfs::lookup_path(&abs).map_err(|_| Errno::ENOENT)?;
    write_stat(&node, statbuf)
}

/// `ioctl` (16): console/stdin answer the core tty queries (`TCGETS`,
/// `TCSETS*`, `TIOCGWINSZ`) so `isatty()` reports a terminal. STAGE-13.7:
/// blanket `EINVAL` made CPython treat stdin as a non-tty pipe, read an
/// instant EOF and exit 0 without ever showing a prompt. Other descriptors
/// still report `EINVAL`; an absent fd is `EBADF`.
pub fn sys_ioctl(fd: u64, request: u64, arg: u64) -> Result<u64, Errno> {
    // STAGE-13.8 CLOEXEC FIX: FIOCLEX/FIONCLEX set/clear the close-on-exec
    // flag on an fd. pagh has no execve yet, so the flag has no observable
    // effect; report success on any valid fd instead of EINVAL. CPython's
    // _Py_set_inheritable() issues ioctl(fd, FIOCLEX) for every module file
    // importlib opens and treats any errno other than ENOTTY/EACCES as a
    // fatal I/O error — this was the importlib get_data OSError (Errno 22)
    // that aborted startup with "Failed to import encodings module".
    const FIONCLEX: u64 = 0x5450;
    const FIOCLEX: u64 = 0x5451;
    if request == FIOCLEX || request == FIONCLEX {
        return match resolve_fd(fd as u32) {
            None => Err(Errno::EBADF),
            Some(_) => Ok(0),
        };
    }
    // STAGE-16.9: FIONBIO. libuv's uv__nonblock on Linux is ioctl(FIONBIO),
    // NOT fcntl(F_SETFL). uv_pipe_open() calls it on the dup'd socketpair end
    // of the embedded-nvim RPC channel; answering ENOTTY made pipe_open fail,
    // so the channel never reached the event loop (the black-screen hang).
    // Mirrors the F_SETFL O_NONBLOCK arm below.
    const FIONBIO: u64 = 0x5421;
    if request == FIONBIO {
        check_user_ptr(arg, 4)?;
        let on = unsafe { core::ptr::read_unaligned(arg as *const u32) } != 0;
        crate::warn!("[DIAG] ioctl FIONBIO pid={} fd={} on={}",
            crate::task::scheduler::current_pid(), fd, on);
        return compat::with_current_compat(|cs| {
            let obj = cs.fds.get_mut(fd as u32).ok_or(Errno::EBADF)?;
            match obj {
                OpenObject::PipeRead(e) => *e = e.with_nonblocking(on),
                OpenObject::PipeWrite(e) => *e = e.with_nonblocking(on),
                OpenObject::Socket { rx, tx } => {
                    let nrx = rx.with_nonblocking(on);
                    let ntx = tx.with_nonblocking(on);
                    *rx = nrx; *tx = ntx;
                }
                OpenObject::UnixListener(l) => l.inner.lock().nonblocking = on,
                OpenObject::UnixSocketUnbound { nonblocking } => *nonblocking = on,
                OpenObject::Stdin => { STDIN_NONBLOCK.store(on, core::sync::atomic::Ordering::Relaxed); }
                _ => {}
            }
            Ok(0)
        })
        .unwrap_or(Err(Errno::EBADF));
    }
    match resolve_fd(fd as u32) {
        None => Err(Errno::EBADF),
        Some(Resolved::Console) | Some(Resolved::Stdin) => tty_ioctl(request, arg),
        // STAGE-13.8 ERRNO FIX: a tty request on a non-tty fd is "inappropriate
        // ioctl for device", not "invalid argument". glibc isatty() expects
        // ENOTTY; EINVAL also tripped the EINVAL diag on every isatty probe
        // CPython makes (TCGETS on fd 3) and flooded the serial log.
        Some(_) => {
            // STAGE-16.9: tty probes (isatty and friends) on non-tty fds are
            // routine noise; any OTHER unknown request is a compat-surface gap
            // worth seeing on screen.
            if !(0x5401..=0x5420).contains(&request) {
                crate::warn!("[DIAG] ioctl pid={} fd={} req=0x{:x} -> ENOTTY",
                    crate::task::scheduler::current_pid(), fd, request);
            }
            Err(Errno::ENOTTY)
        }
    }
}

// ─── tty surface (stage 13.7) ────────────────────────────────────────────────

/// `TCGETS`: read terminal attributes (`struct termios`).
const TCGETS: u64 = 0x5401;
/// `TCSETS` / `TCSETSW` / `TCSETSF`: set terminal attributes (accepted, ignored).
const TCSETS: u64 = 0x5402;
const TCSETSW: u64 = 0x5403;
const TCSETSF: u64 = 0x5404;
/// `TIOCGWINSZ`: read the window size (`struct winsize`).
const TIOCGWINSZ: u64 = 0x5413;
const TIOCSWINSZ: u64 = 0x5414;
/// STAGE-16.14: terminal foreground process group (bash job control).
const TIOCGPGRP: u64 = 0x540F;
const TIOCSPGRP: u64 = 0x5410;

/// Byte size of the Linux `struct termios` (4×u32 + c_line + c_cc[19]).
const TERMIOS_SIZE: usize = 36;

/// Build a plausible cooked-mode `struct termios` byte image: ICRNL|IXON,
/// OPOST|ONLCR, B38400|CS8|CREAD, ISIG|ICANON|ECHO|ECHOE|ECHOK|IEXTEN plus the
/// standard control characters (^C, ^\, DEL, ^U, ^D-as-VEOF, ...).
fn build_termios() -> [u8; TERMIOS_SIZE] {
    let mut t = [0u8; TERMIOS_SIZE];
    let c_iflag: u32 = 0x0500; // ICRNL | IXON
    let c_oflag: u32 = 0x0005; // OPOST | ONLCR
    let c_cflag: u32 = 0x00BF; // B38400 | CS8 | CREAD
    // STAGE 16.16: report the REAL tty state instead of a hardcoded cooked
    // image. tcgetattr previously always claimed ICANON|ECHO, so a program
    // that saves attributes, switches to raw and restores them (bash readline
    // does this around every command) read back a lie.
    let (raw_now, echo_now) = crate::task::compat::with_current_compat(|cs| (cs.raw_mode, cs.echo))
        .unwrap_or((false, true));
    let mut c_lflag: u32 = 0x8A3B; // ISIG|ICANON|ECHO|ECHOE|ECHOK|ECHOCTL|ECHOKE|IEXTEN
    if raw_now { c_lflag &= !0x0002u32; } // ICANON
    if !echo_now { c_lflag &= !0x0008u32; } // ECHO
    t[0..4].copy_from_slice(&c_iflag.to_le_bytes());
    t[4..8].copy_from_slice(&c_oflag.to_le_bytes());
    t[8..12].copy_from_slice(&c_cflag.to_le_bytes());
    t[12..16].copy_from_slice(&c_lflag.to_le_bytes());
    t[16] = 0; // c_line
    // c_cc: VINTR VQUIT VERASE VKILL VEOF VTIME VMIN VSWTC VSTART VSTOP VSUSP
    //       VEOL VREPRINT VDISCARD VWERASE VLNEXT VEOL2 (rest zero)
    let c_cc: [u8; 19] = [3, 28, 127, 21, 4, 0, 1, 0, 17, 19, 26, 0, 18, 15, 23, 22, 0, 0, 0];
    t[17..36].copy_from_slice(&c_cc);
    t
}

/// Console/stdin ioctls: just enough tty surface for `isatty()` to say yes
/// (glibc issues `TCGETS`) and for programs to learn an 80×25 window.
fn tty_ioctl(request: u64, arg: u64) -> Result<u64, Errno> {
    match request {
        TCGETS => {
            check_user_ptr(arg, TERMIOS_SIZE as u64)?;
            copy_out(arg, &build_termios());
            Ok(0)
        }
        // STAGE-14 RAW TTY: if ICANON is cleared, switch to raw mode so reads
        // return one byte at a time (nvim/less/vim need this).
        TCSETS | TCSETSW | TCSETSF => {
            if arg != 0 {
                let _ = check_user_ptr(arg, TERMIOS_SIZE as u64);
                // c_lflag is at offset 12; ICANON = 0x02
                let c_lflag = unsafe { core::ptr::read_unaligned((arg + 12) as *const u32) };
                let raw = c_lflag & 0x02 == 0; // ICANON cleared => raw mode
                // STAGE 16.16: track ECHO (0x08) too, so raw-mode reads know
                // whether the kernel must echo typed characters itself.
                let echo = c_lflag & 0x08 != 0;
                crate::task::compat::with_current_compat(|cs| { if cs.raw_mode != raw || cs.echo != echo { crate::warn!("[DIAG] tty: raw_mode={} echo={} pid={}", raw, echo, crate::task::scheduler::current_pid()); } cs.raw_mode = raw; cs.echo = echo; });
            }
            Ok(0)
        }
        TIOCGWINSZ => {
            check_user_ptr(arg, 8)?;
            let (cols, rows) = crate::drivers::vt::dimensions();
            // struct winsize { ws_row: u16, ws_col: u16, ws_xpixel: u16, ws_ypixel: u16 }
            let mut ws = [0u8; 8];
            ws[0..2].copy_from_slice(&rows.to_le_bytes());
            ws[2..4].copy_from_slice(&cols.to_le_bytes());
            copy_out(arg, &ws);
            Ok(0)
        }
        // Window-size writes are accepted and ignored (readline issues
        // TIOCSWINSZ during startup to propagate its computed size).
        TIOCSWINSZ => Ok(0),
        // STAGE-16.14: report the caller as the foreground process group and
        // accept ownership changes silently. Without these, bash printed
        // "cannot set terminal process group (-1)" + "no job control in this
        // shell" on every start.
        TIOCGPGRP => {
            check_user_ptr(arg, 4)?;
            let pg = crate::task::compat::current_tgid() as u32;
            copy_out(arg, &pg.to_le_bytes());
            Ok(0)
        }
        TIOCSPGRP => Ok(0),
        _ => Err(Errno::ENOTTY),
    }
}

// STAGE-13.8 SELECT: bytes of a cooked line the previous read(2) did not
// consume — readline drains input one byte per read after select().
static STDIN_PENDING: crate::sync::spinlock::Spinlock<alloc::vec::Vec<u8>> =
    crate::sync::spinlock::Spinlock::new(alloc::vec::Vec::new());

// STAGE-16.3: libuv puts the raw tty into O_NONBLOCK and multiplexes it with
// epoll; a read that blocks in-kernel stalls nvim's whole TUI event loop.
static STDIN_NONBLOCK: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// STAGE-16.3: true when a raw-mode stdin read can make progress right now:
/// leftover bytes, queued VT query replies, or an unread keyboard scancode.
pub(crate) fn stdin_input_available() -> bool {
    !STDIN_PENDING.lock().is_empty()
        || crate::drivers::vt::has_input_responses()
        || crate::drivers::ps2_kbd::has_pending()
}

// STAGE-16.4 DIAG: black-screen telemetry. Counts bytes through the three
// pipeline segments (client stdout -> VT, RPC socketpair reads/writes) and
// logs the first byte plus every `step` bytes after, so one glance at the
// screen shows WHERE the nvim pipeline stalls.
static DIAG_CONSOLE_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static DIAG_SOCK_R: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static DIAG_SOCK_W: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static DIAG_STDIN_EAGAIN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn diag_count(ctr: &core::sync::atomic::AtomicU64, label: &str, n: u64, step: u64) {
    let old = ctr.fetch_add(n, core::sync::atomic::Ordering::Relaxed);
    let new = old + n;
    if old == 0 || old / step != new / step {
        crate::warn!("[DIAG] {} pid={} total={}", label,
            crate::task::scheduler::current_pid(), new);
    }
}
/// Hard cap so a pathological paste cannot grow the cooked line unbounded.
const STDIN_LINE_MAX: usize = 4096;

/// Blocking, line-buffered stdin read from the PS/2 keyboard (stage 13.7).
///
/// Cooked-tty semantics: printable characters echo as they are typed, Backspace
/// erases, Tab expands to four spaces (one byte per rendered column keeps the
/// erase bookkeeping trivial), Enter terminates the line (returned including
/// the trailing `\n`), and Ctrl+D on an empty line reports EOF (`Ok(0)`).
/// Blocks by yielding to the scheduler between polls, so the shell's
/// foreground wait and the timer keep running while a binary sits in `read`.

// STAGE-14 RAW TTY: in raw mode (ICANON cleared) return one byte immediately;
// map special keys to ANSI escape sequences so nvim's terminal layer works.
fn read_stdin_raw(buf: u64, count: u64) -> Result<u64, Errno> {
    use crate::shell::keys::{Decoder, KeyEvent};
    if count == 0 { return Ok(0); }
    // STAGE-16.3: queue any pending VT query replies (DA1/DSR/DECRQM/OSC)
    // so nvim's terminal interrogation gets its answers on stdin.
    {
        let resp = crate::drivers::vt::take_input_responses();
        if !resp.is_empty() {
            crate::warn!("[DIAG] stdin: injected {} query-reply bytes", resp.len());
            STDIN_PENDING.lock().extend_from_slice(&resp);
        }
    }
    // Drain pending bytes first (raw mode can also leave leftovers).
    {
        let mut pending = STDIN_PENDING.lock();
        if !pending.is_empty() {
            let n = core::cmp::min(count as usize, pending.len());
            let head: alloc::vec::Vec<u8> = pending.drain(..n).collect();
            copy_out(buf, &head);
            return Ok(n as u64);
        }
    }
    let mut decoder = Decoder::new();
    // STAGE-16.3: once a scancode was consumed, wait a bounded number of
    // polls so multi-byte sequences (E0-prefixed arrows) complete, but a
    // lone key-release cannot park a nonblocking reader forever.
    let mut consumed_any = false;
    let mut spins = 0u32;
    loop {
        let sc = crate::drivers::get_char("keyboard").and_then(|kbd| kbd.read_char());
        let sc = match sc {
            Some(b) => b,
            None => {
                // STAGE-16.3: honor O_NONBLOCK - blocking here stalled
                // nvim's TUI loop before the first frame was drawn.
                if STDIN_NONBLOCK.load(core::sync::atomic::Ordering::Relaxed)
                    && (!consumed_any || spins > 200)
                {
                    if !DIAG_STDIN_EAGAIN.swap(true, core::sync::atomic::Ordering::Relaxed) {
                        crate::warn!("[DIAG] stdin: nonblocking read -> first EAGAIN (uv loop is polling)");
                    }
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
        // Encode KeyEvent as bytes (ANSI sequences for special keys)
        let bytes: alloc::vec::Vec<u8> = match ev {
            KeyEvent::Char(c) => {
                let mut tmp = [0u8; 4];
                alloc::vec::Vec::from(c.encode_utf8(&mut tmp).as_bytes())
            }
            KeyEvent::Enter => alloc::vec![b'\r'],
            KeyEvent::Backspace => alloc::vec![0x7F], // DEL
            KeyEvent::Escape => alloc::vec![0x1B],
            KeyEvent::Tab => alloc::vec![b'\t'],
            KeyEvent::Up    => alloc::vec![0x1B, b'[', b'A'],
            KeyEvent::Down  => alloc::vec![0x1B, b'[', b'B'],
            KeyEvent::Right => alloc::vec![0x1B, b'[', b'C'],
            KeyEvent::Left  => alloc::vec![0x1B, b'[', b'D'],
            KeyEvent::Home  => alloc::vec![0x1B, b'[', b'H'],
            KeyEvent::End   => alloc::vec![0x1B, b'[', b'F'],
            KeyEvent::PageUp   => alloc::vec![0x1B, b'[', b'5', b'~'],
            KeyEvent::PageDown => alloc::vec![0x1B, b'[', b'6', b'~'],
            KeyEvent::Delete   => alloc::vec![0x1B, b'[', b'3', b'~'],
            KeyEvent::Ctrl(c)  => alloc::vec![(c as u8) & 0x1F],
        };
        if bytes.is_empty() { continue; }
        // STAGE 16.16: a real tty keeps echoing in raw mode unless ECHO is
        // cleared. bash's readline runs without a terminfo database here, so
        // it does NOT redraw the input line itself and relied on that missing
        // kernel echo - typed characters only showed up when Enter finally
        // flushed the line. nvim and full readline clear ECHO, so they cannot
        // double-echo. Escape sequences (0x1B...) are never echoed.
        if crate::task::compat::with_current_compat(|cs| cs.echo).unwrap_or(false) {
            match bytes[0] {
                b'\r' | b'\n' => console_write(b"\r\n"),
                0x7F | 0x08 => console_write(b"\x08 \x08"),
                b if b >= 0x20 => console_write(&bytes),
                _ => {}
            }
        }
        let n = core::cmp::min(count as usize, bytes.len());
        copy_out(buf, &bytes[..n]);
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
            copy_out(buf, &head);
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
                // No pending scancode: let other tasks (and the IRQ that feeds
                // the ring buffer) run instead of burning the time slice.
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
            break; // pathological line length: hand back what we have
        }
    }
    let n = core::cmp::min(count as usize, line.len());
    copy_out(buf, &line[..n]);
    if n < line.len() {
        // Stash the tail: readline asks for one byte per read(2).
        STDIN_PENDING.lock().extend_from_slice(&line[n..]);
    }
    Ok(n as u64)
}

/// `access` (21): succeed (return 0) when the path exists on the VFS/ext2 tree,
/// else `ENOENT` (R2.5). The requested access mode is not enforced in this layer.
pub fn sys_access(path: u64, _mode: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    vfs::lookup_path(&abs).map_err(|_| Errno::ENOENT)?;
    Ok(0)
}

/// `mkdir` (83): create a directory on the mounted VFS/ext2 tree (STAGE-13.8).
/// CPython probes it for pyc cache directories; ENOSYS was tolerated but
/// logged an "unsupported syscall" warning on every interpreter start.
/// STAGE-16.13 `rename` (82). The VFS trait has no native rename, so this is
/// an emulation at the syscall layer: copy the file contents to the target
/// path, then unlink the source. Not atomic (irrelevant for a single-user
/// kernel) and files only: directory renames report EACCES with a WARN so a
/// future stage can add real dirent renaming to ext2.
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
    let node = vfs::lookup_path(oldabs).map_err(|_| Errno::ENOENT)?;
    if node.is_directory() {
        crate::warn!("[linux] rename: directory rename not supported: {} -> {}", oldabs, newabs);
        return Err(Errno::EACCES);
    }
    if flags & RENAME_NOREPLACE != 0 && vfs::lookup_path(newabs).is_ok() {
        return Err(Errno::EEXIST);
    }
    // Read the whole source file.
    let size = node.size() as usize;
    let mut data = ::alloc::vec::Vec::new();
    data.resize(size, 0u8);
    let mut off = 0usize;
    while off < size {
        let n = node.read(off as u64, &mut data[off..]).map_err(|_| Errno::EIO)?;
        if n == 0 { break; }
        off += n;
    }
    data.truncate(off);
    // Create/replace the target and write the contents.
    let (nparent, nname) = match newabs.rfind('/') {
        Some(0) => ("/", &newabs[1..]),
        Some(i) => (&newabs[..i], &newabs[i + 1..]),
        None => return Err(Errno::ENOENT),
    };
    if nname.is_empty() { return Err(Errno::EINVAL); }
    let ndir = vfs::lookup_path(nparent).map_err(|_| Errno::ENOENT)?;
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
        if n == 0 { return Err(Errno::EIO); }
        woff += n;
    }
    // Unlink the source.
    let (oparent, oname) = match oldabs.rfind('/') {
        Some(0) => ("/", &oldabs[1..]),
        Some(i) => (&oldabs[..i], &oldabs[i + 1..]),
        None => return Err(Errno::ENOENT),
    };
    let odir = vfs::lookup_path(oparent).map_err(|_| Errno::ENOENT)?;
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

/// `renameat` (264): dirfds are ignored (paths resolve against the cwd, which
/// covers AT_FDCWD in this minimal layer, matching openat).
pub fn sys_renameat(_olddirfd: u64, oldpath: u64, _newdirfd: u64, newpath: u64) -> Result<u64, Errno> {
    let o = read_user_cstr(oldpath)?;
    let n = read_user_cstr(newpath)?;
    rename_paths(&o, &n, 0)
}

/// `renameat2` (316): like renameat; only RENAME_NOREPLACE is honored.
pub fn sys_renameat2(_olddirfd: u64, oldpath: u64, _newdirfd: u64, newpath: u64, flags: u64) -> Result<u64, Errno> {
    let o = read_user_cstr(oldpath)?;
    let n = read_user_cstr(newpath)?;
    rename_paths(&o, &n, flags)
}

pub fn sys_mkdir(path: u64, _mode: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    let trimmed = abs.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(Errno::EEXIST); // "/" always exists
    }
    if vfs::lookup_path(trimmed).is_ok() {
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
    let dir = vfs::lookup_path(parent).map_err(|_| Errno::ENOENT)?;
    dir.create_dir(name).map_err(|e| {
        crate::error!("[linux] mkdir failed: {:?} parent={} name={}", e, parent, name);
        Errno::EIO
    })?;
    Ok(0)
}

// Keep AT_FDCWD referenced so the intent (dir-fd handling) is documented even
// though absolute paths make it inert in this minimal layer.
const _: u64 = AT_FDCWD;

// ───────────────────── directory / path / fd (linux-binary-compat) ─────────────────────

/// `getdents64` (217): serialize directory entries from an open directory fd into
/// the user buffer as packed `struct linux_dirent64`, advancing a per-fd cursor.
/// Returns the number of bytes written, `0` at end of directory. `EBADF` for an
/// absent fd, `ENOTDIR` for a non-directory fd, `EINVAL` if the buffer is too small
/// for even the first remaining entry.
///
/// The directory's children were snapshotted at `open` time, so this runs entirely
/// under the `COMPAT_STATES` lock (no blocking VFS call) using `get_mut` to advance
/// the cursor. Each record's `d_ino`/`d_off` are synthesized (the VFS exposes no
/// inode numbers): `d_ino` is the 1-based child position, `d_off` the next cursor.
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
                let d_type = if child.is_directory() { DT_DIR } else { DT_REG };
                let d_ino = (*index as u64) + 1;
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
            copy_out(buf, &out);
            Ok(out.len() as u64)
        }
    }
}

/// `getcwd` (79): write the process's current working directory (NUL-terminated)
/// into the user buffer, returning the number of bytes written including the NUL.
/// `ERANGE` if the buffer is too small to hold the path plus its terminator.
pub fn sys_getcwd(buf: u64, size: u64) -> Result<u64, Errno> {
    let cwd = current_cwd();
    let bytes = cwd.as_bytes();
    let need = bytes.len() + 1; // include NUL terminator
    if size < need as u64 {
        return Err(Errno::ERANGE);
    }
    check_user_ptr(buf, need as u64)?;
    // Copy the path then the NUL terminator.
    copy_out(buf, bytes);
    // SAFETY: byte at buf+bytes.len() is within the validated `need` range.
    unsafe {
        *((buf + bytes.len() as u64) as *mut u8) = 0;
    }
    Ok(need as u64)
}

/// `chdir` (80): resolve `path` (against the cwd if relative), verify it is an
/// existing directory, and set it as the process cwd. `ENOENT` if absent,
/// `ENOTDIR` if it is not a directory.
pub fn sys_chdir(path: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    let node = vfs::lookup_path(&abs).map_err(|_| Errno::ENOENT)?;
    if !node.is_directory() {
        return Err(Errno::ENOTDIR);
    }
    compat::with_current_compat(|cs| cs.cwd = abs).ok_or(Errno::EBADF)?;
    Ok(0)
}

/// `fchdir` (81): set the process cwd to the path the directory fd was opened
/// under. `EBADF` for an absent fd, `ENOTDIR` if the fd is not a directory.
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

/// `dup` (32): duplicate `oldfd` into the lowest free descriptor, returning it.
/// `EBADF` if `oldfd` is not open.
pub fn sys_dup(oldfd: u64) -> Result<u64, Errno> {
    let r = compat::with_current_compat(|cs| cs.fds.dup(oldfd as u32))
        .unwrap_or(Err(Errno::EBADF))
        .map(|fd| fd as u64);
    // STAGE-16.8 DIAG: nvim dup()s its stdio before hiding it behind stderr;
    // an EBADF here means fd 0/1 were already gone when the server started.
    match &r {
        Ok(fd) => crate::warn!("[DIAG] dup pid={} oldfd={} -> {}",
            crate::task::scheduler::current_pid(), oldfd, fd),
        Err(_) => crate::warn!("[DIAG] dup pid={} oldfd={} -> EBADF",
            crate::task::scheduler::current_pid(), oldfd),
    }
    r
}

/// `dup2` (33): duplicate `oldfd` into the explicit descriptor `newfd`, closing
/// whatever occupies `newfd` first. If `oldfd == newfd` and `oldfd` is valid, it is
/// returned unchanged (no close); `EBADF` if `oldfd` is invalid.
pub fn sys_dup2(oldfd: u64, newfd: u64) -> Result<u64, Errno> {
    // STAGE-16.7 DIAG: stdio rewiring during spawn is exactly where a lost
    // nvim RPC channel would hide; dup2/dup3 are rare enough to log each call.
    crate::warn!("[DIAG] dup2 pid={} oldfd={} newfd={}",
        crate::task::scheduler::current_pid(), oldfd, newfd);
    compat::with_current_compat(|cs| {
        // `oldfd` must be valid regardless.
        if cs.fds.get(oldfd as u32).is_none() {
            return Err(Errno::EBADF);
        }
        if oldfd == newfd {
            return Ok(newfd);
        }
        cs.fds.dup_to(oldfd as u32, newfd as u32).map(|fd| fd as u64)
    })
    .unwrap_or(Err(Errno::EBADF))
}

/// `dup3` (292): like `dup2` but `oldfd == newfd` is an error (`EINVAL`) and the
/// only accepted flag is `O_CLOEXEC` (ignored here). `EBADF` if `oldfd` is invalid.
pub fn sys_dup3(oldfd: u64, newfd: u64, flags: u64) -> Result<u64, Errno> {
    crate::warn!("[DIAG] dup3 pid={} oldfd={} newfd={} flags=0x{:x}",
        crate::task::scheduler::current_pid(), oldfd, newfd, flags);
    if oldfd == newfd {
        return Err(Errno::EINVAL);
    }
    if flags & !O_CLOEXEC != 0 {
        return Err(Errno::EINVAL);
    }
    compat::with_current_compat(|cs| {
        let fd = cs.fds.dup_to(oldfd as u32, newfd as u32)?;
        // STAGE-15: dup3's only flag — mark the new descriptor close-on-exec.
        if flags & O_CLOEXEC != 0 { cs.fds.set_cloexec(fd, true); }
        Ok(fd as u64)
    })
    .unwrap_or(Err(Errno::EBADF))
}

// fcntl commands.
const F_DUPFD: u64 = 0;
const F_GETFD: u64 = 1;
const F_SETFD: u64 = 2;
const F_GETFL: u64 = 3;
const F_SETFL: u64 = 4;
const F_DUPFD_CLOEXEC: u64 = 1030;

/// `fcntl` (72): the descriptor-management subset.
///   * `F_DUPFD` → duplicate `fd` into the lowest free descriptor `>= arg`.
///   * `F_DUPFD_CLOEXEC` → same, and mark the duplicate close-on-exec.
///   * `F_GETFD`/`F_SETFD` → STAGE-15: FD_CLOEXEC is tracked for real now
///     (libuv marks its fork error pipe with it and execve must sweep it).
///   * `F_GETFL` → O_RDWR plus O_NONBLOCK when the descriptor is nonblocking.
///   * `F_SETFL` → STAGE-16: O_NONBLOCK is applied for real to pipes, sockets
///     and listeners (libuv reads until EAGAIN, so this must work); other
///     flags are accepted and ignored.
///   * anything else → `EINVAL`.
pub fn sys_fcntl(fd: u64, cmd: u64, arg: u64) -> Result<u64, Errno> {
    // STAGE-16.8 DIAG: libuv's child-init shuffles stdio fds with F_DUPFD.
    if cmd == F_DUPFD || cmd == F_DUPFD_CLOEXEC {
        crate::warn!("[DIAG] fcntl_dupfd pid={} fd={} min={} cloexec={}",
            crate::task::scheduler::current_pid(), fd, arg, cmd == F_DUPFD_CLOEXEC);
    }
    const FD_CLOEXEC: u64 = 1;
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => compat::with_current_compat(|cs| {
            let newfd = cs.fds.dup_min(fd as u32, arg as u32)?;
            cs.fds.set_cloexec(newfd, cmd == F_DUPFD_CLOEXEC);
            Ok(newfd as u64)
        })
        .unwrap_or(Err(Errno::EBADF)),
        F_GETFD => compat::with_current_compat(|cs| {
            if cs.fds.get(fd as u32).is_none() {
                return Err(Errno::EBADF);
            }
            Ok(if cs.fds.is_cloexec(fd as u32) { FD_CLOEXEC } else { 0 })
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
                        // STAGE-16.3: stdin reports its real nonblocking state.
                        OpenObject::Stdin => STDIN_NONBLOCK.load(core::sync::atomic::Ordering::Relaxed),
                        _ => false,
                    };
                    Ok(if nb { O_RDWR_FL | O_NONBLOCK_FL } else { O_RDWR_FL })
                } else {
                    let on = arg & O_NONBLOCK_FL != 0;
                    match obj {
                        OpenObject::PipeRead(e) => *e = e.with_nonblocking(on),
                        OpenObject::PipeWrite(e) => *e = e.with_nonblocking(on),
                        OpenObject::Socket { rx, tx } => {
                            let nrx = rx.with_nonblocking(on);
                            let ntx = tx.with_nonblocking(on);
                            *rx = nrx; *tx = ntx;
                        }
                        OpenObject::UnixListener(l) => l.inner.lock().nonblocking = on,
                        OpenObject::UnixSocketUnbound { nonblocking } => *nonblocking = on,
                        // STAGE-16.3: libuv flips O_NONBLOCK on the raw tty.
                        OpenObject::Stdin => { crate::warn!("[DIAG] fcntl: stdin O_NONBLOCK={}", on); STDIN_NONBLOCK.store(on, core::sync::atomic::Ordering::Relaxed); }
                        _ => {}
                    }
                    Ok(0)
                }
            })
            .unwrap_or(Err(Errno::EBADF))
        }
        _ => Err(Errno::EINVAL),
    }
}

/// `readlink` (89): no symbolic links exist in this filesystem, so a path that
/// resolves to an existing node is "not a symlink" (`EINVAL`) and an absent path
/// is `ENOENT`. STAGE-15 exception: `/proc/self/exe` resolves to the exec'd
/// image path (libuv's uv_exepath — nvim's progpath — reads it, and the forked
/// child re-execs that path to start the embedded server).
pub fn sys_readlink(path: u64, buf: u64, bufsiz: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    if p == "/proc/self/exe" {
        let exe = compat::with_current_compat(|cs| cs.exe_path.clone()).unwrap_or_default();
        if exe.is_empty() {
            return Err(Errno::ENOENT);
        }
        let bytes = exe.as_bytes();
        let n = core::cmp::min(bytes.len() as u64, bufsiz);
        check_user_ptr(buf, n)?;
        copy_out(buf, &bytes[..n as usize]);
        return Ok(n);
    }
    let abs = resolve_path(&p);
    match vfs::lookup_path(&abs) {
        Ok(_) => Err(Errno::EINVAL),
        Err(_) => Err(Errno::ENOENT),
    }
}

/// `readlinkat` (267): like `readlink`; the dirfd is ignored (paths resolve
/// absolute / against the cwd).
pub fn sys_readlinkat(_dirfd: u64, path: u64, buf: u64, bufsiz: u64) -> Result<u64, Errno> {
    sys_readlink(path, buf, bufsiz)
}

/// `pread64` (17): read up to `count` bytes from `fd` at the absolute `offset`
/// WITHOUT advancing the descriptor's own offset. `EBADF` for an absent fd;
/// `ESPIPE` for a non-seekable stream (console/stdin); `EISDIR` for a directory.
pub fn sys_pread64(fd: u64, buf: u64, count: u64, offset: u64) -> Result<u64, Errno> {
    if count > COUNT_MAX {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(buf, count)?;
    match resolve_fd(fd as u32) {
        None => Err(Errno::EBADF),
        Some(Resolved::Console) | Some(Resolved::Stdin)
        | Some(Resolved::PipeRead(_)) | Some(Resolved::PipeWrite(_))
        | Some(Resolved::Socket { .. })
        | Some(Resolved::Eventfd { .. }) | Some(Resolved::Epoll) => Err(Errno::ESPIPE),
        Some(Resolved::Dir) => Err(Errno::EISDIR),
        Some(Resolved::File { node, .. }) => {
            let size = node.size();
            let (copied, _) = plan_read(size, offset, count);
            if copied == 0 {
                return Ok(0);
            }
            let mut kbuf = vec![0u8; copied as usize];
            let n = node.read(offset, &mut kbuf).map_err(|e| {
                // STAGE-13.7: a real VFS/ext2 read failure is not "invalid
                // argument" — report EIO and log what actually broke.
                crate::error!(
                    "[linux] file read failed: {:?} ino={} off={} len={}",
                    e, node.fs_ino(), offset, copied
                );
                Errno::EIO
            })?;
            copy_out(buf, &kbuf[..n]);
            // NOTE: the descriptor offset is intentionally NOT updated.
            Ok(n as u64)
        }
    }
}

/// `pwrite64` (18): write `count` bytes to `fd` at the absolute `offset` WITHOUT
/// advancing the descriptor's own offset. Console writes ignore the offset and
/// emit to the console; `ESPIPE` for stdin; `EISDIR` for a directory; `EBADF` for
/// an absent fd.
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
        Some(Resolved::Eventfd { .. }) | Some(Resolved::Epoll) => Err(Errno::ESPIPE),
        Some(Resolved::Dir) => Err(Errno::EISDIR),
        Some(Resolved::Console) => {
            let data = copy_in(buf, count);
            console_write(&data);
            Ok(count)
        }
        Some(Resolved::File { node, .. }) => {
            let data = copy_in(buf, count);
            let n = node.write(offset, &data).map_err(|_| Errno::EINVAL)?;
            // NOTE: the descriptor offset is intentionally NOT updated.
            Ok(n as u64)
        }
    }
}

/// The x86_64 Linux `struct statfs` (subset populated with plausible values).
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

/// ext2 superblock magic, reported in `f_type` so `df`-class probes recognize it.
const EXT2_SUPER_MAGIC: i64 = 0xEF53;

/// Build a plausible `statfs` snapshot from the PMM frame counts (used as a stand-
/// in for filesystem capacity, which the VFS does not expose cheaply).
fn build_statfs() -> LinuxStatfs {
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

/// Copy a built `statfs` to the validated user buffer.
fn write_statfs(buf: u64) {
    let sf = build_statfs();
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &sf as *const LinuxStatfs as *const u8,
            core::mem::size_of::<LinuxStatfs>(),
        )
    };
    copy_out(buf, bytes);
}

/// `statfs` (137): fill the user `struct statfs` with plausible values for the
/// path (which must exist, else `ENOENT`). Returns 0.
pub fn sys_statfs(path: u64, buf: u64) -> Result<u64, Errno> {
    check_user_ptr(buf, core::mem::size_of::<LinuxStatfs>() as u64)?;
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    vfs::lookup_path(&abs).map_err(|_| Errno::ENOENT)?;
    write_statfs(buf);
    Ok(0)
}

/// `fstatfs` (138): fill the user `struct statfs` for an open descriptor (which
/// must be valid, else `EBADF`). Returns 0.
pub fn sys_fstatfs(fd: u64, buf: u64) -> Result<u64, Errno> {
    check_user_ptr(buf, core::mem::size_of::<LinuxStatfs>() as u64)?;
    if resolve_fd(fd as u32).is_none() {
        return Err(Errno::EBADF);
    }
    write_statfs(buf);
    Ok(0)
}


const O_NONBLOCK:u64=0x800; const O_CLOEXEC:u64=0x80000;
const POLLIN:i16=0x001; const POLLOUT:i16=0x004; const POLLERR:i16=0x008; const POLLHUP:i16=0x010; const POLLNVAL:i16=0x020;
pub fn sys_pipe(pipefd:u64)->Result<u64,Errno>{sys_pipe2(pipefd,0)}
pub fn sys_pipe2(pipefd:u64,flags:u64)->Result<u64,Errno>{if flags&!(O_NONBLOCK|O_CLOEXEC)!=0{return Err(Errno::EINVAL)}check_user_ptr(pipefd,8)?;let pair=compat::with_current_compat(|cs|{let p=cs.fds.pipe(flags&O_NONBLOCK!=0);if flags&O_CLOEXEC!=0{cs.fds.set_cloexec(p.0,true);cs.fds.set_cloexec(p.1,true);}p}).ok_or(Errno::EBADF)?;let words=[pair.0,pair.1];let bytes=unsafe{core::slice::from_raw_parts(words.as_ptr()as*const u8,8)};copy_out(pipefd,bytes);Ok(0)}
fn poll_revents(fd:i32,events:i16)->i16{if fd<0{return 0}match resolve_fd(fd as u32){None=>POLLNVAL,Some(Resolved::PipeRead(e))=>{let mut o=if e.read_ready(){events&POLLIN}else{0};if e.peer_closed(){o|=POLLHUP}o},Some(Resolved::PipeWrite(e))=>{let mut o=if e.write_ready(){events&POLLOUT}else{0};if e.peer_closed(){o|=POLLERR}o},Some(Resolved::Socket{rx,tx})=>{let mut o=0i16;if rx.read_ready(){o|=events&POLLIN}if tx.write_ready(){o|=events&POLLOUT}if rx.peer_closed(){o|=POLLHUP}o},Some(Resolved::Stdin)=>{if stdin_input_available(){events&POLLIN}else{0}},Some(Resolved::Console)=>events&POLLOUT,Some(Resolved::Eventfd{val,..})=>{let v=val.lock();if*v>0{events&POLLIN}else{0}},Some(Resolved::Epoll)=>0,Some(Resolved::File{..})|Some(Resolved::Dir)=>events&(POLLIN|POLLOUT)}}
pub fn sys_poll(fds:u64,nfds:u64,timeout:u64)->Result<u64,Errno>{const SZ:u64=8;if nfds>1024{return Err(Errno::EINVAL)}check_user_ptr(fds,nfds.checked_mul(SZ).ok_or(Errno::EINVAL)?)?;let ms=timeout as i64;let deadline=if ms<0{None}else{Some(crate::task::scheduler::ticks().saturating_add((ms as u64).saturating_add(9)/10))};loop{let mut ready=0;for i in 0..nfds{let p=fds+i*SZ;let fd=unsafe{*(p as*const i32)};let ev=unsafe{*((p+4)as*const i16)};let rev=poll_revents(fd,ev);unsafe{*((p+6)as*mut i16)=rev}if rev!=0{ready+=1}}if ready!=0||ms==0{return Ok(ready)}if let Some(end)=deadline{if crate::task::scheduler::ticks()>=end{return Ok(0)}}crate::task::scheduler::yield_current()}}

// ---------------------------------------------------------------------------
// STAGE-13.8 SELECT: minimal select/pselect6/ppoll (nr 23/270/271). GNU
// readline (loaded by the CPython REPL now that libreadline is installed)
// waits for stdin with pselect6 before every byte; ENOSYS left it spinning
// right after the first `>>>` prompt. The cooked-tty stdin always reports
// readable — the following read(2) blocks line-buffered anyway, and leftover
// bytes are served from STDIN_PENDING one read at a time.
// ---------------------------------------------------------------------------
const FDSET_WORDS: usize = 16; // 1024 fds, matching FD_SETSIZE

fn select_ready(fd: u64, want_write: bool) -> bool {
    match resolve_fd(fd as u32) {
        None => true, // report ready; the following operation returns EBADF
        Some(Resolved::Stdin) => !want_write,
        Some(Resolved::Console) => true,
        Some(Resolved::PipeRead(e)) => !want_write && (e.read_ready() || e.peer_closed()),
        Some(Resolved::PipeWrite(e)) => want_write && (e.write_ready() || e.peer_closed()),
        Some(Resolved::Socket { rx, tx }) => if want_write { tx.write_ready() || tx.peer_closed() } else { rx.read_ready() || rx.peer_closed() },
        Some(Resolved::Eventfd { val, .. }) => !want_write && { let v=val.lock(); *v>0 },
        Some(Resolved::Epoll) => false,
        Some(Resolved::File { .. }) | Some(Resolved::Dir) => true,
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

fn store_fdset(ptr: u64, nfds: u64, set: &[u64; FDSET_WORDS]) {
    if ptr == 0 {
        return;
    }
    let words = ((nfds + 63) / 64) as usize;
    for w in 0..words {
        unsafe { *((ptr + (w as u64) * 8) as *mut u64) = set[w] };
    }
}

fn do_select(nfds: u64, readfds: u64, writefds: u64, exceptfds: u64, timeout_ms: Option<i64>) -> Result<u64, Errno> {
    let nfds = nfds.min(1024);
    let want_r = load_fdset(readfds, nfds)?;
    let want_w = load_fdset(writefds, nfds)?;
    let _ = load_fdset(exceptfds, nfds)?; // exceptional conditions never fire here
    let deadline = match timeout_ms {
        None => None,
        Some(ms) if ms <= 0 => Some(0), // scan once, then time out
        Some(ms) => Some(crate::task::scheduler::ticks().saturating_add(((ms as u64).saturating_add(9)) / 10)),
    };
    loop {
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
            store_fdset(readfds, nfds, &got_r);
            store_fdset(writefds, nfds, &got_w);
            store_fdset(exceptfds, nfds, &[0u64; FDSET_WORDS]);
            return Ok(ready);
        }
        crate::task::scheduler::yield_current();
    }
}

/// `pselect6` (270): the timeout is a `struct timespec`; the sigmask is
/// ignored (no signal delivery yet).
pub fn sys_pselect6(nfds: u64, readfds: u64, writefds: u64, exceptfds: u64, timeout: u64, _sigmask: u64) -> Result<u64, Errno> {
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

/// `select` (23): the timeout is a `struct timeval`.
pub fn sys_select(nfds: u64, readfds: u64, writefds: u64, exceptfds: u64, timeout: u64) -> Result<u64, Errno> {
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

/// `ppoll` (271): `poll` with a `struct timespec` timeout; sigmask ignored.
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

// ─── STAGE-15: socketpair (53) + statx (332) ───────────────────────────────────────────

const AF_UNIX: u64 = 1;
const SOCK_STREAM: u64 = 1;
const SOCK_TYPE_MASK: u64 = 0xf;
const SOCK_NONBLOCK: u64 = 0x800;
const SOCK_CLOEXEC: u64 = 0x80000;

/// `socketpair` (53): AF_UNIX/SOCK_STREAM only — the channel libuv builds for
/// the `nvim --embed` msgpack-rpc server, dup2'd onto the child's stdio.
/// Backed by two cross-connected in-kernel byte queues (FdTable::socketpair),
/// so both descriptors are readable and writable.
pub fn sys_socketpair(domain: u64, sock_type: u64, protocol: u64, sv: u64) -> Result<u64, Errno> {
    if domain != AF_UNIX || protocol != 0 {
        return Err(Errno::EINVAL);
    }
    if sock_type & SOCK_TYPE_MASK != SOCK_STREAM
        || sock_type & !(SOCK_TYPE_MASK | SOCK_NONBLOCK | SOCK_CLOEXEC) != 0
    {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(sv, 8)?;
    let pair = compat::with_current_compat(|cs| {
        let p = cs.fds.socketpair(sock_type & SOCK_NONBLOCK != 0);
        if sock_type & SOCK_CLOEXEC != 0 {
            cs.fds.set_cloexec(p.0, true);
            cs.fds.set_cloexec(p.1, true);
        }
        p
    })
    .ok_or(Errno::EBADF)?;
    let words = [pair.0, pair.1];
    // SAFETY: two u32 fds → exactly the 8 bytes socketpair writes to sv.
    let bytes = unsafe { core::slice::from_raw_parts(words.as_ptr() as *const u8, 8) };
    copy_out(sv, bytes);
    Ok(0)
}

const STATX_BASIC_STATS: u32 = 0x7ff;
const AT_EMPTY_PATH: u64 = 0x1000;
const STATX_SIZE_BYTES: u64 = 256;

/// Fill a zeroed `struct statx` (256 bytes) with the basic fields pagh tracks.
/// Timestamps stay zero — the legacy `stat` path reports the same.
fn write_statx(buf: u64, size: u64, mode: u32, ino: u64) {
    // SAFETY: the caller validated `buf` for STATX_SIZE_BYTES.
    unsafe {
        core::ptr::write_bytes(buf as *mut u8, 0, STATX_SIZE_BYTES as usize);
        core::ptr::write_unaligned(buf as *mut u32, STATX_BASIC_STATS); // stx_mask
        core::ptr::write_unaligned((buf + 0x04) as *mut u32, 4096); // stx_blksize
        core::ptr::write_unaligned((buf + 0x10) as *mut u32, 1); // stx_nlink
        core::ptr::write_unaligned((buf + 0x1c) as *mut u16, mode as u16); // stx_mode
        core::ptr::write_unaligned((buf + 0x20) as *mut u64, ino); // stx_ino
        core::ptr::write_unaligned((buf + 0x28) as *mut u64, size); // stx_size
        core::ptr::write_unaligned((buf + 0x30) as *mut u64, (size + 511) / 512); // stx_blocks
    }
}

/// `statx` (332): translated onto the existing stat plumbing. Supports plain
/// path lookups (absolute or cwd-relative; any dirfd is treated as AT_FDCWD,
/// like `openat`) and the `AT_EMPTY_PATH` fd form glibc uses for fstat-style
/// queries. libuv's uv_fs_fstat probes statx first and only falls back on a
/// clean ENOSYS from the dispatcher — which it no longer needs to.
pub fn sys_statx(dirfd: u64, path: u64, flags: u64, _mask: u64, buf: u64) -> Result<u64, Errno> {
    check_user_ptr(buf, STATX_SIZE_BYTES)?;
    let p = read_user_cstr(path)?;
    if p.is_empty() && flags & AT_EMPTY_PATH != 0 {
        return match resolve_fd(dirfd as u32) {
            None => Err(Errno::EBADF),
            Some(Resolved::File { node, .. }) => {
                let ino = match node.fs_ino() { 0 => synth_ino(node.name()), ino => ino };
                write_statx(buf, node.size(), S_IFREG | DEFAULT_FILE_PERMS, ino);
                Ok(0)
            }
            Some(Resolved::Dir) => { write_statx(buf, 0, S_IFDIR | 0o700, 1); Ok(0) }
            Some(Resolved::Socket { .. }) => { write_statx(buf, 0, S_IFSOCK | 0o666, 1); Ok(0) }
            Some(_) => { write_statx(buf, 0, S_IFCHR | 0o620, 1); Ok(0) }
        };
    }
    let abs = resolve_path(&p);
    let node = vfs::lookup_path(&abs).map_err(|_| Errno::ENOENT)?;
    let mode = if node.is_directory() { S_IFDIR | 0o700 } else { S_IFREG | DEFAULT_FILE_PERMS };
    let ino = match node.fs_ino() { 0 => synth_ino(node.name()), ino => ino };
    write_statx(buf, node.size(), mode, ino);
    Ok(0)
}
