//! Разделяемая память клиента: `memfd_create` + `ftruncate` + `mmap`, передача
//! fd через AF_UNIX `SCM_RIGHTS` и низкоуровневые операции над поверхностью.
//!
//! Это клиентская половина `ShmSurface` из `docs/desktop-render-spec.md` §3 B5:
//! клиент создаёт memfd, отображает его `MAP_SHARED|PROT_READ|PROT_WRITE`,
//! рендерит прямо в mapping (без `Vec<u8>`-кадра) и отправляет fd композитору
//! одним `sendmsg` вместе с сообщением `SHM_OFFER`. Сервер делает свой `dup`
//! через `SCM_RIGHTS`, поэтому клиентский fd после отправки можно закрыть —
//! mapping продолжает жить.
//!
//! # Безопасность
//!
//! Вся unsafe-арифметика (mmap/munmap/sendmsg/recvmsg, разметка `cmsghdr`)
//! изолирована в этом модуле. API наружу — безопасный:
//! [`Memfd`], [`MappedBuffer`] (владеет mapping и размонтирует его в `Drop`),
//! [`ShmSurface`] (memfd + геометрия) и [`send_fd_payload`]/[`recv_fd_payload`].
//!
//! # Ограничения
//!
//! Поддержка `SCM_RIGHTS` есть в ядре Pagh-OS 2.24.0+ (см. `docs/desktop-b.md`
//! §7) и в Linux-хосте; если `memfd_create`/`sendmsg` не поддержаны, клиент
//! откатывается на полный кадр по сокету (fallback, см. `client`).

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::ptr::NonNull;

use crate::protocol::FrameFormat;

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 0x01;
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const MSG_NOSIGNAL: i32 = 0x4000;
const MSG_CMSG_CLOEXEC: i32 = 0x40000000;
const MFD_CLOEXEC: u32 = 1;
/// `CMSG_SPACE(4)` для одного `SCM_RIGHTS(fd)` на x86_64/Linux.
pub const SCM_RIGHTS_FD_SPACE: usize = 24;
/// Тестовый выключатель: `PAGH_UI_DISABLE_SHM=1` заставляет клиент считать, что
/// сервер не умеет `ShmSurface` (проверка fallback в e2e).
pub const DISABLE_ENV: &str = "PAGH_UI_DISABLE_SHM";

#[repr(C)]
struct IoVec {
    base: *mut std::ffi::c_void,
    len: usize,
}

#[repr(C)]
struct CMsgHdr {
    len: usize,
    level: i32,
    kind: i32,
}

#[repr(C)]
struct MsgHdr {
    name: *mut std::ffi::c_void,
    name_len: u32,
    iov: *mut IoVec,
    iov_len: usize,
    control: *mut std::ffi::c_void,
    control_len: usize,
    flags: i32,
}

extern "C" {
    fn memfd_create(name: *const std::ffi::c_char, flags: u32) -> i32;
    fn ftruncate(fd: i32, length: i64) -> i32;
    fn mmap(
        addr: *mut std::ffi::c_void,
        length: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, length: usize) -> i32;
    fn sendmsg(fd: i32, msg: *const MsgHdr, flags: i32) -> isize;
    fn recvmsg(fd: i32, msg: *mut MsgHdr, flags: i32) -> isize;
}

/// Выравнивание ancillary-записи (`CMSG_ALIGN`).
const fn cmsg_align(len: usize) -> usize {
    (len + std::mem::size_of::<usize>() - 1) & !(std::mem::size_of::<usize>() - 1)
}

/// `CMSG_SPACE(n)`: сколько байт нужно в control-буфере под cmsg с `n` байт
/// полезной нагрузки.
pub const fn cmsg_space(payload: usize) -> usize {
    cmsg_align(std::mem::size_of::<CMsgHdr>()) + cmsg_align(payload)
}

/// `CMSG_LEN(n)`: длина cmsg с `n` байт полезной нагрузки.
pub const fn cmsg_len(payload: usize) -> usize {
    std::mem::size_of::<CMsgHdr>() + payload
}

/// Создать анонимный memfd.
pub fn create_memfd(name: &str) -> io::Result<OwnedFd> {
    let name = CString::new(name.replace('\0', "_"))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "memfd name contains NUL"))?;
    // SAFETY: имя — валидная C-строка, fd не переиспользуется, флаг CLOEXEC
    // не меняет видимость fd для композитора (fd передаётся явно).
    let fd = unsafe { memfd_create(name.as_ptr(), MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: memfd_create вернул новый дескриптор, владение переходит сюда.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Установить размер memfd (`ftruncate`).
pub fn set_len(fd: RawFd, length: usize) -> io::Result<()> {
    // SAFETY: fd валиден (владелец гарантирует), ftruncate не трогает память.
    let rc = unsafe { ftruncate(fd, length as i64) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Отображение файла/`memfd` в память; размонтируется в [`Drop`].
///
/// Объект владеет диапазоном `[ptr, ptr+len)` и не может быть скопирован,
/// поэтому случайное `munmap` дважды невозможно.
pub struct MappedBuffer {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: mapping — это обычная разделяемая память; перемещение владельца между
// потоками безопасно, как и для `Vec<u8>`.
unsafe impl Send for MappedBuffer {}

impl MappedBuffer {
    /// `mmap(MAP_SHARED, PROT_READ|PROT_WRITE)` поверх fd.
    pub fn map_shared(fd: RawFd, len: usize) -> io::Result<MappedBuffer> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty shared buffer",
            ));
        }
        // SAFETY: mmap пишет в новый диапазон; fd валиден, offset = 0.
        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(ptr as *mut u8)
            .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "mmap returned null"))?;
        Ok(MappedBuffer { ptr, len })
    }

    /// `mmap(MAP_SHARED, PROT_READ)` — серверная сторона `ShmSurface`
    /// (композитор не пишет в память клиента). Используется тестовым сервером.
    pub fn map_shared_readonly(fd: RawFd, len: usize) -> io::Result<MappedBuffer> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty shared buffer",
            ));
        }
        // SAFETY: mmap создаёт новое read-only отображение; fd валиден.
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, MAP_SHARED, fd, 0) };
        if ptr as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(ptr as *mut u8)
            .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "mmap returned null"))?;
        Ok(MappedBuffer { ptr, len })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Байты mapping только для чтения.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: диапазон выделен mmap и живёт, пока жив self; владение
        // уникально, изменяемая ссылка не может существовать одновременно.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// Байты mapping для записи (рендер прямо в разделяемую память).
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: см. `as_slice`; `&mut self` гарантирует уникальность.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for MappedBuffer {
    fn drop(&mut self) {
        // SAFETY: ptr/len получены из успешного mmap и ещё не размонтированы.
        unsafe {
            munmap(self.ptr.as_ptr() as *mut std::ffi::c_void, self.len);
        }
    }
}

/// Memfd с геометрией клиентской поверхности (клиентская половина `SHM_OFFER`).
#[derive(Debug)]
pub struct ShmSurface {
    fd: OwnedFd,
    width: u32,
    height: u32,
    stride: u32,
    format: FrameFormat,
}

impl ShmSurface {
    /// Создать memfd ровно под `stride * height` и заполнить нулями (ftruncate).
    ///
    /// `stride` по умолчанию — `width * bytes_per_pixel`; сервер принимает
    /// `stride >= width*bpp` (дополнительное выравнивание), клиент его не
    /// использует.
    pub fn create(
        name: &str,
        width: u32,
        height: u32,
        format: FrameFormat,
    ) -> io::Result<ShmSurface> {
        if width == 0 || height == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared surface must be non-empty",
            ));
        }
        let bpp = format.bytes_per_pixel() as u32;
        let stride = width
            .checked_mul(bpp)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "stride overflow"))?;
        let size = (stride as u64)
            .checked_mul(height as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "size overflow"))?;
        if size > usize::MAX as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared surface too large",
            ));
        }
        let fd = create_memfd(name)?;
        set_len(fd.as_raw_fd(), size as usize)?;
        Ok(ShmSurface {
            fd,
            width,
            height,
            stride,
            format,
        })
    }

    /// memfd прямо из готового дескриптора (используется тестами/серверным
    /// харнессом; клиент создаёт через [`ShmSurface::create`]).
    pub fn from_fd(fd: OwnedFd, width: u32, height: u32, stride: u32) -> io::Result<ShmSurface> {
        let format = FrameFormat::from_wire(1).unwrap_or(FrameFormat::Xrgb8888);
        let bpp = format.bytes_per_pixel() as u32;
        if stride < width.saturating_mul(bpp) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stride smaller than width",
            ));
        }
        Ok(ShmSurface {
            fd,
            width,
            height,
            stride,
            format,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn stride(&self) -> u32 {
        self.stride
    }

    pub fn format(&self) -> FrameFormat {
        self.format
    }

    /// Размер буфера, который ждёт сервер: `stride * height`.
    pub fn byte_len(&self) -> usize {
        self.stride as usize * self.height as usize
    }

    /// `mmap(MAP_SHARED, PROT_READ|PROT_WRITE)` этой поверхности.
    pub fn map(&self) -> io::Result<MappedBuffer> {
        MappedBuffer::map_shared(self.fd(), self.byte_len())
    }
}

/// Отправить `payload` вместе с ровно одним fd через `SCM_RIGHTS`.
///
/// Сообщение должно быть отдельным `sendmsg` (не батчить с другими кадрами):
/// ancillary-данные относятся к первому байту потока. Функция пишет весь
/// payload (для коротких протокольных сообщений — один syscall).
pub fn send_fd_payload(stream: &UnixStream, payload: &[u8], fd: RawFd) -> io::Result<()> {
    if payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SCM_RIGHTS message needs a payload",
        ));
    }
    let mut control = [0u8; SCM_RIGHTS_FD_SPACE];
    // SAFETY: control — выровненный по usize массив; CMsgHdr/IoVec/MsgHdr —
    // repr(C), поля заполняются по Linux ABI.
    unsafe {
        let header = CMsgHdr {
            len: cmsg_len(4),
            level: SOL_SOCKET,
            kind: SCM_RIGHTS,
        };
        std::ptr::write_unaligned(control.as_mut_ptr() as *mut CMsgHdr, header);
        std::ptr::copy_nonoverlapping(
            fd.to_ne_bytes().as_ptr(),
            control.as_mut_ptr().add(std::mem::size_of::<CMsgHdr>()),
            std::mem::size_of::<RawFd>(),
        );

        let iov = IoVec {
            base: payload.as_ptr() as *mut std::ffi::c_void,
            len: payload.len(),
        };
        let msg = MsgHdr {
            name: std::ptr::null_mut(),
            name_len: 0,
            iov: &iov as *const IoVec as *mut IoVec,
            iov_len: 1,
            control: control.as_mut_ptr() as *mut std::ffi::c_void,
            control_len: control.len(),
            flags: 0,
        };

        let mut sent = 0usize;
        let mut msg = msg;
        while sent < payload.len() {
            let iov = IoVec {
                base: payload[sent..].as_ptr() as *mut std::ffi::c_void,
                len: payload.len() - sent,
            };
            msg.iov = &iov as *const IoVec as *mut IoVec;
            msg.iov_len = 1;
            if sent == 0 {
                msg.control = control.as_mut_ptr() as *mut std::ffi::c_void;
                msg.control_len = control.len();
            } else {
                // fd уже привязан к первым байтам: добиваем хвост без cmsg.
                msg.control = std::ptr::null_mut();
                msg.control_len = 0;
            }
            let written = sendmsg(stream.as_raw_fd(), &msg, MSG_NOSIGNAL);
            if written < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "sendmsg wrote zero bytes",
                ));
            }
            sent += written as usize;
        }
    }
    Ok(())
}

/// Принять данные и fd'ы из `SCM_RIGHTS` (серверная половина B5; используется
/// тестовым сервером и не нужна обычному клиенту).
///
/// Возвращает `(прочитано_байт, принятые_fd)`. `MSG_CMSG_CLOEXEC` выставляет
/// `FD_CLOEXEC` на принятые дескрипторы (Linux-совместимо).
pub fn recv_fd_payload(
    stream: &UnixStream,
    payload: &mut [u8],
) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut control = [0u8; 64];
    // SAFETY: все указатели валидны на время syscall; структуры repr(C).
    let received;
    {
        let iov = IoVec {
            base: payload.as_mut_ptr() as *mut std::ffi::c_void,
            len: payload.len(),
        };
        let mut msg = MsgHdr {
            name: std::ptr::null_mut(),
            name_len: 0,
            iov: &iov as *const IoVec as *mut IoVec,
            iov_len: 1,
            control: control.as_mut_ptr() as *mut std::ffi::c_void,
            control_len: control.len(),
            flags: 0,
        };
        loop {
            // SAFETY: msg/iov/control живут до конца вызова.
            let result = unsafe { recvmsg(stream.as_raw_fd(), &mut msg, MSG_CMSG_CLOEXEC) };
            if result >= 0 {
                received = result;
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if msg.flags & 0x20 != 0 {
            // MSG_CTRUNC: control-буфер обрезан — fd'ы могли потеряться.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SCM_RIGHTS control buffer truncated",
            ));
        }
    }

    let mut fds = Vec::new();
    // SAFETY: control заполнен ядром; обходим cmsg по CMSG_ALIGN.
    unsafe {
        let mut offset = 0usize;
        while offset + std::mem::size_of::<CMsgHdr>() <= control.len() {
            let header = std::ptr::read_unaligned(control.as_ptr().add(offset) as *const CMsgHdr);
            if header.len < std::mem::size_of::<CMsgHdr>() || offset + header.len > control.len() {
                break;
            }
            if header.level == SOL_SOCKET && header.kind == SCM_RIGHTS {
                let payload_len = header.len - std::mem::size_of::<CMsgHdr>();
                let mut consumed = 0usize;
                while consumed + std::mem::size_of::<RawFd>() <= payload_len {
                    let mut raw = 0i32;
                    std::ptr::copy_nonoverlapping(
                        control
                            .as_ptr()
                            .add(offset + std::mem::size_of::<CMsgHdr>() + consumed),
                        &mut raw as *mut i32 as *mut u8,
                        std::mem::size_of::<RawFd>(),
                    );
                    if raw >= 0 {
                        fds.push(OwnedFd::from_raw_fd(raw));
                    }
                    consumed += std::mem::size_of::<RawFd>();
                }
            }
            offset += cmsg_align(header.len);
        }
    }
    Ok((received as usize, fds))
}

/// Поддерживает ли окружение клиента разделяемую память.
///
/// Проверяет `memfd_create` + `ftruncate` + `mmap` на минимальном размере и
/// учитывает тестовый выключатель [`DISABLE_ENV`]. Реальное решение всё равно
/// принимает `welcome.flags`: сервер обязан объявить bit 2.
pub fn shm_supported() -> bool {
    if std::env::var_os(DISABLE_ENV).is_some_and(|value| !value.is_empty() && value != "0") {
        return false;
    }
    let Ok(fd) = create_memfd("pagh-ui-probe") else {
        return false;
    };
    if set_len(fd.as_raw_fd(), 4096).is_err() {
        return false;
    }
    MappedBuffer::map_shared(fd.as_raw_fd(), 4096).is_ok()
}

/// Записать RGBA-пиксель в буфер формата PDP/1.
pub fn store_pixel(format: FrameFormat, dst: &mut [u8], rgba: [u8; 4]) {
    let [r, g, b, _a] = rgba;
    match format {
        FrameFormat::Xrgb8888 => {
            dst[0] = b;
            dst[1] = g;
            dst[2] = r;
            dst[3] = 0;
        }
        FrameFormat::Rgb565 => {
            let value = ((r as u16 >> 3) << 11) | ((g as u16 >> 2) << 5) | (b as u16 >> 3);
            dst[..2].copy_from_slice(&value.to_le_bytes());
        }
    }
}

/// Прочитать пиксель как straight-alpha RGBA.
pub fn load_pixel(format: FrameFormat, src: &[u8]) -> [u8; 4] {
    match format {
        FrameFormat::Xrgb8888 => [src[2], src[1], src[0], 255],
        FrameFormat::Rgb565 => {
            let value = u16::from_le_bytes([src[0], src[1]]);
            let r = ((value >> 11) & 0x1f) as u8;
            let g = ((value >> 5) & 0x3f) as u8;
            let b = (value & 0x1f) as u8;
            [
                (r << 3) | (r >> 2),
                (g << 2) | (g >> 4),
                (b << 3) | (b >> 2),
                255,
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    #[test]
    fn memfd_create_and_map_roundtrip() {
        let surface = ShmSurface::create("pagh-ui-test", 64, 32, FrameFormat::Xrgb8888).unwrap();
        assert_eq!(surface.stride(), 64 * 4);
        assert_eq!(surface.byte_len(), 64 * 4 * 32);
        let mut mapping = surface.map().unwrap();
        mapping.as_mut_slice()[0] = 0xab;
        mapping.as_mut_slice()[1] = 0xcd;
        assert_eq!(mapping.as_slice()[0], 0xab);
        assert_eq!(mapping.as_slice()[1], 0xcd);
        assert_eq!(mapping.len(), 64 * 4 * 32);
    }

    #[test]
    fn px565_surface_stride_is_two_bytes() {
        let surface = ShmSurface::create("pagh-ui-test-565", 10, 3, FrameFormat::Rgb565).unwrap();
        assert_eq!(surface.stride(), 20);
        assert_eq!(surface.byte_len(), 60);
        assert!(surface.map().is_ok());
    }

    #[test]
    fn scm_rights_carries_shared_mapping() {
        let (client, server) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let surface = ShmSurface::create("pagh-ui-scm", 16, 4, FrameFormat::Xrgb8888).unwrap();
        let mut mapping = surface.map().unwrap();
        mapping.as_mut_slice()[0] = 1;
        mapping.as_mut_slice()[1] = 2;
        mapping.as_mut_slice()[2] = 3;
        mapping.as_mut_slice()[3] = 4;

        let fd = surface.fd();
        let payload = b"SHM_OFFER";
        let handle = std::thread::spawn(move || {
            let mut buffer = [0u8; 32];
            let (read, fds) = recv_fd_payload(&server, &mut buffer).unwrap();
            assert_eq!(&buffer[..read], payload);
            assert_eq!(fds.len(), 1);
            let mut received = MappedBuffer::map_shared(fds[0].as_raw_fd(), 16 * 4 * 4).unwrap();
            assert_eq!(&received.as_slice()[..4], &[1, 2, 3, 4]);
            received.as_mut_slice()[4] = 9;
            received
        });
        send_fd_payload(&client, payload, fd).unwrap();
        let received = handle.join().unwrap();
        assert_eq!(received.len(), 16 * 4 * 4);
        assert_eq!(mapping.as_slice()[4], 9, "shared page visible to sender");
    }

    #[test]
    fn send_requires_payload() {
        let (client, _server) = UnixStream::pair().unwrap();
        let surface = ShmSurface::create("pagh-ui-empty", 1, 1, FrameFormat::Xrgb8888).unwrap();
        assert!(send_fd_payload(&client, &[], surface.fd()).is_err());
    }

    #[test]
    fn pixel_store_and_load_are_symmetric() {
        for format in [FrameFormat::Xrgb8888, FrameFormat::Rgb565] {
            let mut bytes = [0u8; 4];
            store_pixel(format, &mut bytes, [10, 20, 30, 255]);
            let [r, g, b, a] = load_pixel(format, &bytes);
            assert_eq!(a, 255);
            if format == FrameFormat::Xrgb8888 {
                assert_eq!((r, g, b), (10, 20, 30));
            } else {
                assert!((r as i32 - 10).abs() <= 8);
                assert!((g as i32 - 20).abs() <= 4);
                assert!((b as i32 - 30).abs() <= 8);
            }
        }
    }

    #[test]
    fn unsupported_environment_returns_false_when_disabled() {
        // Не трогаем реальный env глобально: проверяем сам гейт по переменной
        // через отдельный процесс нельзя, поэтому проверяем только, что
        // выключение не делает probe паникующим.
        let saved = std::env::var_os(DISABLE_ENV);
        std::env::set_var(DISABLE_ENV, "1");
        assert!(!shm_supported());
        match saved {
            Some(value) => std::env::set_var(DISABLE_ENV, value),
            None => std::env::remove_var(DISABLE_ENV),
        }
    }

    #[test]
    fn mapped_buffer_rejects_empty() {
        assert!(MappedBuffer::map_shared(-1, 0).is_err());
    }

    #[test]
    fn surface_byte_len_matches_ftruncate() {
        let surface = ShmSurface::create("pagh-ui-len", 7, 5, FrameFormat::Xrgb8888).unwrap();
        let mut mapping = surface.map().unwrap();
        assert_eq!(mapping.len(), surface.byte_len());
        mapping.as_mut_slice().last_mut().map(|b| *b = 7);
        let mut buf = Vec::new();
        // memfd виден через /proc/self/fd; небольшой sanity check, что fd живой.
        let path = format!("/proc/self/fd/{}", surface.fd());
        let mut file = std::fs::File::open(path).unwrap();
        let _ = file.read_to_end(&mut buf);
        assert_eq!(buf.len(), surface.byte_len());
        assert_eq!(buf[buf.len() - 1], 7);
        let _ = std::io::stdout().flush();
    }
}
