//! ShmSurface (PDP/1 type 14–16): memfd клиента, переданный одним SCM_RIGHTS fd.
//!
//! Реализация независимая, по Linux UAPI (`memfd_create`, `sendmsg`/`recvmsg`,
//! `SCM_RIGHTS`, `mmap`); без libc-крейта и без заимствования кода сторонних
//! композиторов. Сервер отображает клиентский memfd **read-only** и копирует из
//! него только повреждённые прямоугольники в свой back buffer; клиентская память
//! никогда не попадает в scanout напрямую.

use crate::protocol::Message;
use crate::surface::PixelFormat;
use std::ffi::{c_int, c_long, c_void, CString};
use std::fs::File;
use std::io::{self, Write};
use std::mem::size_of;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::ptr;
use std::slice;

const SYS_MEMFD_CREATE: c_long = 319;
const MFD_CLOEXEC: u32 = 1;
const PROT_READ: c_int = 1;
const MAP_SHARED: c_int = 1;
const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;
const SOL_SOCKET: c_int = 1;
const SCM_RIGHTS: c_int = 1;
const MSG_NOSIGNAL: c_int = 0x4000;

#[repr(C)]
struct IoVec {
    base: *mut c_void,
    len: usize,
}

#[repr(C)]
struct MsgHdr {
    name: *mut c_void,
    namelen: u32,
    iov: *mut IoVec,
    iovlen: usize,
    control: *mut c_void,
    controllen: usize,
    flags: c_int,
}

#[repr(C)]
struct CMsgHdr {
    len: usize,
    level: c_int,
    kind: c_int,
}

extern "C" {
    fn syscall(number: c_long, ...) -> c_long;
    fn sendmsg(fd: c_int, msg: *const MsgHdr, flags: c_int) -> isize;
    fn recvmsg(fd: c_int, msg: *mut MsgHdr, flags: c_int) -> isize;
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
}

fn cmsg_align(value: usize) -> usize {
    (value + size_of::<usize>() - 1) & !(size_of::<usize>() - 1)
}

fn cmsg_len(data: usize) -> usize {
    cmsg_align(size_of::<CMsgHdr>()) + data
}

fn cmsg_space(data: usize) -> usize {
    cmsg_align(cmsg_len(data))
}

/// Создать anonymous memfd (Linux `memfd_create`) с `MFD_CLOEXEC`.
pub fn memfd(name: &str) -> io::Result<OwnedFd> {
    let c_name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "имя memfd содержит NUL"))?;
    // SAFETY: syscall — тонкая обёртка Linux; имя живёт на время вызова, флаги
    // передаются как unsigned int в регистре.
    let fd = unsafe { syscall(SYS_MEMFD_CREATE, c_name.as_ptr(), MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: memfd_create вернул новый fd, которым ещё никто не владеет.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Создать memfd и выставить его размер `size` (ftruncate через `File::set_len`).
pub fn create_memfd(name: &str, size: usize) -> io::Result<OwnedFd> {
    let fd = memfd(name)?;
    let file = File::from(fd);
    file.set_len(size as u64)?;
    Ok(file.into())
}

/// Отобразить memfd только для чтения, не более `min_len` байт.
pub struct ShmMapping {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: mapping — обычная разделяемая память, принадлежащая объекту; сервер
// использует её в одном потоке, но `Send` нужен, чтобы хранить binding в Server.
unsafe impl Send for ShmMapping {}

impl ShmMapping {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr/len получены из успешного mmap и живут до Drop.
        unsafe { slice::from_raw_parts(self.ptr, self.len) }
    }

    /// Валидированный view на область `stride*height`; `None`, если mapping мал.
    pub fn view(
        &self,
        width: u32,
        height: u32,
        format: PixelFormat,
        stride: u32,
    ) -> Option<ShmView<'_>> {
        let needed = (stride as usize).checked_mul(height as usize)?;
        if needed > self.len {
            return None;
        }
        Some(ShmView {
            width,
            height,
            format,
            stride,
            data: &self.as_slice()[..needed],
        })
    }
}

impl Drop for ShmMapping {
    fn drop(&mut self) {
        // SAFETY: mapping создан mmap с этим len и ещё не снят.
        unsafe {
            let _ = munmap(self.ptr as *mut c_void, self.len);
        }
    }
}

/// Пиксели клиентского memfd, доступные только для чтения.
#[derive(Clone, Copy)]
pub struct ShmView<'a> {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub stride: u32,
    pub data: &'a [u8],
}

/// Отобразить memfd; `min_len` — минимум `stride*height`, проверяется по fstat.
pub fn map_readonly(fd: OwnedFd, min_len: usize) -> io::Result<ShmMapping> {
    let file = File::from(fd);
    let len = file.metadata()?.len();
    if len < min_len as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("memfd {} байт, нужно {min_len}", len),
        ));
    }
    // SAFETY: fd валиден (принадлежит file), len — размер файла; mmap read-only
    // MAP_SHARED не даёт серверу писать в клиентскую память.
    let ptr = unsafe {
        mmap(
            ptr::null_mut(),
            min_len,
            PROT_READ,
            MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if ptr == MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(ShmMapping {
        ptr: ptr as *mut u8,
        len: min_len,
    })
}

/// Отправить сообщение вместе с ровно одним fd через SCM_RIGHTS.
pub fn send_message_with_fd(stream: &UnixStream, message: &Message, fd: RawFd) -> io::Result<()> {
    let bytes = message.encode();
    send_bytes_with_fd(stream, &bytes, fd)
}

/// Отправить байты с одним ancillary fd (первый `sendmsg`, остаток — `write`).
pub fn send_bytes_with_fd(stream: &UnixStream, bytes: &[u8], fd: RawFd) -> io::Result<()> {
    let mut iov = IoVec {
        base: bytes.as_ptr() as *mut c_void,
        len: bytes.len(),
    };
    let mut control = [0u8; 64];
    let header = CMsgHdr {
        len: cmsg_len(size_of::<c_int>()),
        level: SOL_SOCKET,
        kind: SCM_RIGHTS,
    };
    // SAFETY: control — локальный [u8;64], записываем cmsghdr и fd в его начало.
    unsafe {
        ptr::write_unaligned(control.as_mut_ptr() as *mut CMsgHdr, header);
        ptr::write_unaligned(
            control.as_mut_ptr().add(cmsg_align(size_of::<CMsgHdr>())) as *mut c_int,
            fd,
        );
    }
    loop {
        let mut msg = MsgHdr {
            name: ptr::null_mut(),
            namelen: 0,
            iov: &mut iov,
            iovlen: 1,
            control: control.as_mut_ptr() as *mut c_void,
            controllen: cmsg_space(size_of::<c_int>()),
            flags: 0,
        };
        // SAFETY: msg указывает на живые iov/control на время вызова.
        let sent = unsafe { sendmsg(stream.as_raw_fd(), &mut msg, MSG_NOSIGNAL) };
        if sent < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        let sent = sent as usize;
        if sent == 0 {
            continue;
        }
        if sent < bytes.len() {
            // fd уже доставлен с первым байтом sendmsg; остаток — обычной записью.
            let mut writer = stream;
            writer.write_all(&bytes[sent..])?;
        }
        return Ok(());
    }
}

/// Принять байты и ancillary fds; несколько fd в одном cmsg отклоняются.
pub fn recv_with_fds(stream: &UnixStream, buffer: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = IoVec {
        base: buffer.as_mut_ptr() as *mut c_void,
        len: buffer.len(),
    };
    let mut control = [0u8; 128];
    let mut msg = MsgHdr {
        name: ptr::null_mut(),
        namelen: 0,
        iov: &mut iov,
        iovlen: 1,
        control: control.as_mut_ptr() as *mut c_void,
        controllen: control.len(),
        flags: 0,
    };
    // SAFETY: msg/iov/control живут на время вызова, буферы достаточного размера.
    let received = unsafe { recvmsg(stream.as_raw_fd(), &mut msg, 0) };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    let mut offset = 0usize;
    while offset + size_of::<CMsgHdr>() <= msg.controllen {
        // SAFETY: заголовок лежит в control-буфере; читаем unaligned.
        let header = unsafe { ptr::read_unaligned(control.as_ptr().add(offset) as *const CMsgHdr) };
        if header.len < size_of::<CMsgHdr>() || offset + header.len > msg.controllen {
            break;
        }
        if header.level == SOL_SOCKET && header.kind == SCM_RIGHTS {
            let data_len = header.len - size_of::<CMsgHdr>();
            if data_len == 0 || data_len % size_of::<c_int>() != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "некорректный SCM_RIGHTS cmsg",
                ));
            }
            let count = data_len / size_of::<c_int>();
            if count != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("SCM_RIGHTS: ожидался один fd, получено {count}"),
                ));
            }
            // SAFETY: fd лежит сразу за cmsghdr в control-буфере.
            let fd = unsafe {
                ptr::read_unaligned(
                    control
                        .as_ptr()
                        .add(offset + cmsg_align(size_of::<CMsgHdr>()))
                        as *const c_int,
                )
            };
            // SAFETY: SCM_RIGHTS передал нам владение новым fd.
            fds.push(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        offset += cmsg_align(header.len);
    }
    Ok((received as usize, fds))
}

/// Принятый SHM-ресурс окна на сервере.
pub struct ShmBinding {
    pub client_id: u64,
    pub window_id: u32,
    pub generation: u32,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub stride: u32,
    pub mapping: ShmMapping,
}

impl ShmBinding {
    pub fn view(&self) -> Option<ShmView<'_>> {
        self.mapping
            .view(self.width, self.height, self.format, self.stride)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn memfd_can_be_mapped_readonly() {
        let fd = create_memfd("pagh-shm-test", 64).expect("memfd");
        let mapping = map_readonly(fd, 64).expect("mmap");
        assert_eq!(mapping.len(), 64);
        assert_eq!(mapping.as_slice().len(), 64);
    }

    #[test]
    fn small_memfd_is_rejected() {
        let fd = create_memfd("pagh-shm-small", 16).expect("memfd");
        assert!(map_readonly(fd, 64).is_err());
    }

    #[test]
    fn scm_rights_roundtrip() {
        let (a, b) = UnixStream::pair().expect("socketpair");
        let fd = create_memfd("pagh-shm-roundtrip", 4096).expect("memfd");
        send_bytes_with_fd(&a, b"PDS1test", fd.as_raw_fd()).expect("sendmsg");
        let mut buffer = [0u8; 16];
        let (n, fds) = recv_with_fds(&b, &mut buffer).expect("recvmsg");
        assert_eq!(&buffer[..n], b"PDS1test");
        assert_eq!(fds.len(), 1);
        let mapping = map_readonly(fds.into_iter().next().unwrap(), 4096).expect("mmap");
        assert_eq!(mapping.len(), 4096);
    }
}
