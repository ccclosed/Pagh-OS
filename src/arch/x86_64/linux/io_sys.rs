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
use crate::task::fd::OpenObject;
use crate::vfs::{self, VfsNode};

use super::check_user_ptr;
use super::dirent::{dirent_reclen, encode_dirent64, DT_DIR, DT_REG};
use super::errno::Errno;
use super::io::{plan_lseek, plan_read};
use super::stat::{encode_stat, LinuxStat, S_IFDIR, S_IFREG};

/// `st_mode` type bits for a character device (console/stdin), so `fstat` on a
/// standard stream reports a plausible (non-regular) type.
const S_IFCHR: u32 = 0o020000;

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
            OpenObject::File { node, offset } => Resolved::File {
                node: Arc::clone(node),
                offset: *offset,
            },
            OpenObject::Dir { .. } => Resolved::Dir,
        })
    })
    .flatten()
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
    match core::str::from_utf8(slice) {
        Ok(s) => console.write_str(s),
        Err(e) => {
            let valid_up_to = e.valid_up_to();
            // SAFETY: `from_utf8` guarantees `slice[..valid_up_to]` is valid UTF-8.
            let prefix = unsafe { core::str::from_utf8_unchecked(&slice[..valid_up_to]) };
            console.write_str(prefix);
            for &byte in &slice[valid_up_to..] {
                let mut tmp = [0u8; 4];
                console.write_str((byte as char).encode_utf8(&mut tmp));
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
        // Reading the console / stdin yields EOF in this minimal model.
        Some(Resolved::Console) | Some(Resolved::Stdin) => Ok(0),
        Some(Resolved::Dir) => Err(Errno::EISDIR),
        Some(Resolved::File { node, offset }) => {
            let size = node.size();
            let (copied, _) = plan_read(size, offset, count);
            if copied == 0 {
                return Ok(0);
            }
            let mut kbuf = vec![0u8; copied as usize];
            let n = node.read(offset, &mut kbuf).map_err(|_| Errno::EINVAL)?;
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
    if matches!(target, Resolved::Dir) {
        return Err(Errno::EISDIR);
    }

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
            Resolved::File { node, .. } => {
                let n = node.write(file_off, &data).map_err(|_| Errno::EINVAL)?;
                file_off += n as u64;
                total += n as u64;
            }
            Resolved::Stdin => unreachable!(),
            Resolved::Dir => unreachable!(),
        }
    }

    if matches!(target, Resolved::File { .. }) {
        set_fd_offset(fd as u32, file_off);
    }
    Ok(total)
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

/// Resolve a user path (against the cwd if relative) and allocate a fresh
/// descriptor for it, or `ENOENT` if the path does not exist (R2.4, R2.5). Shared
/// by `open`/`openat`.
fn open_path(path: &str) -> Result<u64, Errno> {
    let abs = resolve_path(path);
    open_resolved(&abs)
}

/// `open` (2): open an existing ext2 path (resolved against the cwd if relative),
/// allocating the lowest fd ≥ 3 (R2.4); `ENOENT` if the path is absent (R2.5).
/// Directories open as a directory descriptor usable with `getdents64`.
pub fn sys_open(path: u64, _flags: u64, _mode: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    open_path(&p)
}

/// `openat` (257): like `open`. `AT_FDCWD` (and any dirfd in this minimal layer)
/// resolves the path against the process cwd; absolute paths ignore the dirfd.
pub fn sys_openat(dirfd: u64, path: u64, _flags: u64, _mode: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    // Absolute paths ignore dirfd; relative paths resolve against the cwd (the
    // only directory base this minimal layer tracks), which covers AT_FDCWD.
    let _ = dirfd;
    open_path(&p)
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
        Some(Resolved::Console) | Some(Resolved::Stdin) => Err(Errno::EINVAL),
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

/// Fill a [`LinuxStat`] for a node and copy it to the validated user buffer.
fn write_stat(node: &Arc<dyn VfsNode>, statbuf: u64) -> Result<u64, Errno> {
    let mode = if node.is_directory() {
        S_IFDIR | 0o755
    } else {
        S_IFREG | DEFAULT_FILE_PERMS
    };
    let stat = encode_stat(node.size(), mode);
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
        Some(Resolved::Console) | Some(Resolved::Stdin) => {
            let stat = encode_stat(0, S_IFCHR | 0o620);
            write_stat_struct(&stat, statbuf);
            Ok(0)
        }
        Some(Resolved::Dir) => {
            let stat = encode_stat(0, S_IFDIR | 0o755);
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

/// `ioctl` (16): no terminal/device ioctls are supported, so a valid descriptor
/// reports `EINVAL` (programs treat this as "not a tty"); an absent fd is `EBADF`.
pub fn sys_ioctl(fd: u64, _request: u64, _arg: u64) -> Result<u64, Errno> {
    match resolve_fd(fd as u32) {
        None => Err(Errno::EBADF),
        Some(_) => Err(Errno::EINVAL),
    }
}

/// `access` (21): succeed (return 0) when the path exists on the VFS/ext2 tree,
/// else `ENOENT` (R2.5). The requested access mode is not enforced in this layer.
pub fn sys_access(path: u64, _mode: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
    let abs = resolve_path(&p);
    vfs::lookup_path(&abs).map_err(|_| Errno::ENOENT)?;
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
    compat::with_current_compat(|cs| cs.fds.dup(oldfd as u32))
        .unwrap_or(Err(Errno::EBADF))
        .map(|fd| fd as u64)
}

/// `dup2` (33): duplicate `oldfd` into the explicit descriptor `newfd`, closing
/// whatever occupies `newfd` first. If `oldfd == newfd` and `oldfd` is valid, it is
/// returned unchanged (no close); `EBADF` if `oldfd` is invalid.
pub fn sys_dup2(oldfd: u64, newfd: u64) -> Result<u64, Errno> {
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
pub fn sys_dup3(oldfd: u64, newfd: u64, _flags: u64) -> Result<u64, Errno> {
    if oldfd == newfd {
        return Err(Errno::EINVAL);
    }
    compat::with_current_compat(|cs| {
        if cs.fds.get(oldfd as u32).is_none() {
            return Err(Errno::EBADF);
        }
        cs.fds.dup_to(oldfd as u32, newfd as u32).map(|fd| fd as u64)
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
///   * `F_DUPFD`/`F_DUPFD_CLOEXEC` → duplicate `fd` into the lowest free
///     descriptor `>= arg` (close-on-exec is not tracked, so the CLOEXEC form is
///     equivalent here).
///   * `F_GETFD`/`F_SETFD` → close-on-exec flag is not tracked; report/accept 0.
///   * `F_GETFL` → report `O_RDONLY` (0).
///   * `F_SETFL` → accept and return 0.
///   * anything else → `EINVAL`.
pub fn sys_fcntl(fd: u64, cmd: u64, arg: u64) -> Result<u64, Errno> {
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => compat::with_current_compat(|cs| {
            cs.fds.dup_min(fd as u32, arg as u32).map(|f| f as u64)
        })
        .unwrap_or(Err(Errno::EBADF)),
        F_GETFD | F_SETFD => {
            // Validate the fd exists; flags themselves are not tracked.
            if resolve_fd(fd as u32).is_none() {
                return Err(Errno::EBADF);
            }
            Ok(0)
        }
        F_GETFL | F_SETFL => {
            if resolve_fd(fd as u32).is_none() {
                return Err(Errno::EBADF);
            }
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// `readlink` (89): no symbolic links exist in this filesystem, so a path that
/// resolves to an existing node is "not a symlink" (`EINVAL`) and an absent path
/// is `ENOENT`. The output buffer is never written.
pub fn sys_readlink(path: u64, _buf: u64, _bufsiz: u64) -> Result<u64, Errno> {
    let p = read_user_cstr(path)?;
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
        Some(Resolved::Console) | Some(Resolved::Stdin) => Err(Errno::ESPIPE),
        Some(Resolved::Dir) => Err(Errno::EISDIR),
        Some(Resolved::File { node, .. }) => {
            let size = node.size();
            let (copied, _) = plan_read(size, offset, count);
            if copied == 0 {
                return Ok(0);
            }
            let mut kbuf = vec![0u8; copied as usize];
            let n = node.read(offset, &mut kbuf).map_err(|_| Errno::EINVAL)?;
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
