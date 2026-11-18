//! Backend `/dev/fb0`: fbdev ioctl + `mmap(MAP_SHARED)` прямо в scanout.
//!
//! Kernel-ABI описан в `docs/desktop-b.md` (Pagh-OS 2.24.0):
//! `FBIOGET_VSCREENINFO`/`FBIOGET_FSCREENINFO`, формат пикселя задаётся масками
//! `var.red/green/blue`, stride — `fix.line_length`. Первый `open`/`mmap`
//! забирает экран у kernel-консоли (takeover); ABI односторонний — закрытие fd
//! экран не возвращает (это документировано в `docs/desktop-b.md` §3.1).
//!
//! Здесь только три FFI-вызова (`ioctl`, `mmap`, `munmap`) — без libc-крейта,
//! чтобы сохранить нулевые зависимости и static-musl сборку.

use crate::backend::Backend;
use crate::canvas::Canvas;
use crate::geometry::Rect;
use crate::timing::{self, Stage};
use std::ffi::{c_int, c_ulong, c_void};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// `FBIOGET_VSCREENINFO` (Linux UAPI).
pub const FBIOGET_VSCREENINFO: c_ulong = 0x4600;
/// `FBIOGET_FSCREENINFO` (Linux UAPI).
pub const FBIOGET_FSCREENINFO: c_ulong = 0x4602;

const FB_VAR_SIZE: usize = 160;
const FB_FIX_SIZE: usize = 80;
const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_SHARED: c_int = 1;
const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;

extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
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

/// `struct fb_bitfield` (offset/length; `msb_right` ядро оставляет нулём).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BitField {
    pub offset: u32,
    pub length: u32,
}

impl BitField {
    pub fn mask(self) -> u32 {
        if self.length == 0 {
            0
        } else {
            ((1u32 << self.length.min(32)) - 1) << self.offset
        }
    }
}

/// Разобранный `struct fb_var_screeninfo`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FbVarInfo {
    pub xres: u32,
    pub yres: u32,
    pub xres_virtual: u32,
    pub yres_virtual: u32,
    pub xoffset: u32,
    pub yoffset: u32,
    pub bits_per_pixel: u32,
    pub red: BitField,
    pub green: BitField,
    pub blue: BitField,
    pub transp: BitField,
}

/// Разобранный `struct fb_fix_screeninfo`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FbFixInfo {
    pub id: [u8; 16],
    pub smem_start: u64,
    pub smem_len: u32,
    pub type_: u32,
    pub visual: u32,
    pub line_length: u32,
}

impl FbVarInfo {
    /// Разобрать 160 байт ответа `FBIOGET_VSCREENINFO`.
    pub fn decode(buf: &[u8; FB_VAR_SIZE]) -> Self {
        let u32_at = |off: usize| u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        Self {
            xres: u32_at(0),
            yres: u32_at(4),
            xres_virtual: u32_at(8),
            yres_virtual: u32_at(12),
            xoffset: u32_at(16),
            yoffset: u32_at(20),
            bits_per_pixel: u32_at(24),
            red: BitField {
                offset: u32_at(32),
                length: u32_at(36),
            },
            green: BitField {
                offset: u32_at(44),
                length: u32_at(48),
            },
            blue: BitField {
                offset: u32_at(56),
                length: u32_at(60),
            },
            transp: BitField {
                offset: u32_at(68),
                length: u32_at(72),
            },
        }
    }
}

impl FbFixInfo {
    /// Разобрать 80 байт ответа `FBIOGET_FSCREENINFO`.
    pub fn decode(buf: &[u8; FB_FIX_SIZE]) -> Self {
        let u64_at = |off: usize| u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
        let u32_at = |off: usize| u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        let mut id = [0u8; 16];
        id.copy_from_slice(&buf[0..16]);
        Self {
            id,
            smem_start: u64_at(16),
            smem_len: u32_at(24),
            type_: u32_at(28),
            visual: u32_at(36),
            line_length: u32_at(48),
        }
    }

    pub fn id_string(&self) -> String {
        let end = self.id.iter().position(|b| *b == 0).unwrap_or(16);
        String::from_utf8_lossy(&self.id[..end]).into_owned()
    }
}

/// Геометрия и упаковка пикселей в scanout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FbLayout {
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
    pub stride: u32,
    pub smem_len: u32,
    pub red: BitField,
    pub green: BitField,
    pub blue: BitField,
}

impl FbLayout {
    pub fn from_info(var: &FbVarInfo, fix: &FbFixInfo) -> io::Result<Self> {
        if var.xres == 0 || var.yres == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("fbdev: пустой размер {}x{}", var.xres, var.yres),
            ));
        }
        if !matches!(var.bits_per_pixel, 16 | 24 | 32) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("fbdev: неподдерживаемый bpp {}", var.bits_per_pixel),
            ));
        }
        if fix.line_length == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fbdev: line_length=0",
            ));
        }
        let minimum_stride = var
            .xres
            .checked_mul(var.bits_per_pixel / 8)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "fbdev: stride overflow"))?;
        if fix.line_length < minimum_stride {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "fbdev: line_length {} меньше минимального {}",
                    fix.line_length, minimum_stride
                ),
            ));
        }
        let needed = (fix.line_length as u64)
            .checked_mul(var.yres as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "fbdev: smem overflow"))?;
        if (fix.smem_len as u64) < needed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "fbdev: smem_len {} меньше {} (stride*yres)",
                    fix.smem_len, needed
                ),
            ));
        }
        for (name, field) in [("red", var.red), ("green", var.green), ("blue", var.blue)] {
            if field.length > 8 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("fbdev: {name}.length={} > 8", field.length),
                ));
            }
            if field.length > 0
                && u64::from(field.offset) + u64::from(field.length) > u64::from(var.bits_per_pixel)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("fbdev: {name} маска выходит за bpp"),
                ));
            }
        }
        Ok(Self {
            width: var.xres,
            height: var.yres,
            bpp: var.bits_per_pixel,
            stride: fix.line_length,
            smem_len: fix.smem_len,
            red: var.red,
            green: var.green,
            blue: var.blue,
        })
    }

    pub fn bytes_per_pixel(&self) -> usize {
        (self.bpp / 8) as usize
    }

    pub fn stride_usize(&self) -> usize {
        self.stride as usize
    }

    /// Упаковать `0x00RRGGBB` в слово scanout по маскам из `var`.
    pub fn pack(&self, color: u32) -> u32 {
        let field = |channel: u32, bits: BitField| -> u32 {
            if bits.length == 0 {
                return 0;
            }
            let scaled = if bits.length >= 8 {
                channel
            } else {
                channel >> (8 - bits.length)
            };
            (scaled & ((1 << bits.length) - 1)) << bits.offset
        };
        field((color >> 16) & 0xFF, self.red)
            | field((color >> 8) & 0xFF, self.green)
            | field(color & 0xFF, self.blue)
    }

    /// Обратное преобразование для selfcheck/чтения кадра.
    pub fn unpack(&self, raw: u32) -> u32 {
        let field = |bits: BitField| -> u32 {
            if bits.length == 0 {
                return 0;
            }
            let value = (raw >> bits.offset) & ((1 << bits.length) - 1);
            if bits.length >= 8 {
                value
            } else {
                // Расширение 5/6-битного канала до 8 бит (как в rgb565_to_rgb).
                (value * 255) / ((1 << bits.length) - 1)
            }
        };
        (field(self.red) << 16) | (field(self.green) << 8) | field(self.blue)
    }

    fn bounds(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && (x as u32) < self.width && (y as u32) < self.height
    }

    /// Прочитать пиксель scanout (для `--fbdev-selfcheck`).
    pub fn read_pixel(&self, src: &[u8], x: i32, y: i32) -> Option<u32> {
        if !self.bounds(x, y) {
            return None;
        }
        let bytes = self.bytes_per_pixel();
        let off = y as usize * self.stride_usize() + x as usize * bytes;
        if off + bytes > src.len() {
            return None;
        }
        let mut raw = [0u8; 4];
        raw[..bytes].copy_from_slice(&src[off..off + bytes]);
        Some(self.unpack(u32::from_le_bytes(raw)))
    }

    /// Скопировать только повреждённые прямоугольники холста в scanout.
    ///
    /// Горячий путь — построчный: для 32 bpp без преобразования каналов
    /// (`red=16:8`, `green=8:8`, `blue=0:8`) строка копируется словами; для
    /// 16/24 bpp и нестандартных масок упаковка выполняется по строке, без
    /// `set_pixel`/`write_pixel` на каждый пиксель.
    pub fn blit_damage(&self, dst: &mut [u8], canvas: &Canvas, damage: &[Rect]) -> usize {
        let _stage = timing::Scope::new(Stage::Copy);
        let screen = Rect::new(0, 0, self.width as i32, self.height as i32);
        let canvas_rect = canvas.bounds();
        let bytes = self.bytes_per_pixel();
        let mut pixels = 0usize;
        for rect in damage {
            let Some(rect) = rect
                .intersection(screen)
                .and_then(|rect| rect.intersection(canvas_rect))
            else {
                continue;
            };
            for y in rect.y..rect.bottom() {
                let dst_off = y as usize * self.stride_usize() + rect.x as usize * bytes;
                let dst_end = dst_off + rect.w as usize * bytes;
                if dst_end > dst.len() {
                    continue;
                }
                let src_off = y as usize * canvas.width() as usize + rect.x as usize;
                let src = &canvas.pixels()[src_off..src_off + rect.w as usize];
                self.copy_row(&mut dst[dst_off..dst_end], src);
                pixels += rect.w as usize;
            }
        }
        pixels
    }

    /// Признак «scanout уже в XRGB8888 с теми же масками» — можно копировать
    /// слова без `pack`.
    fn is_xrgb8888(&self) -> bool {
        self.bpp == 32
            && self.red.offset == 16
            && self.red.length == 8
            && self.green.offset == 8
            && self.green.length == 8
            && self.blue.offset == 0
            && self.blue.length == 8
    }

    fn copy_row(&self, dst: &mut [u8], src: &[u32]) {
        match self.bpp {
            32 => {
                if self.is_xrgb8888() {
                    // Внутренний холст хранит 0xAARRGGBB; в scanout XRGB
                    // альфа-байт нулевой, как у `pack`.
                    for (d, s) in dst.chunks_exact_mut(4).zip(src.iter()) {
                        d.copy_from_slice(&(*s & 0x00FF_FFFF).to_le_bytes());
                    }
                } else {
                    for (d, s) in dst.chunks_exact_mut(4).zip(src.iter()) {
                        d.copy_from_slice(&self.pack(*s).to_le_bytes());
                    }
                }
            }
            24 => {
                for (d, s) in dst.chunks_exact_mut(3).zip(src.iter()) {
                    d.copy_from_slice(&self.pack(*s).to_le_bytes()[..3]);
                }
            }
            16 => {
                for (d, s) in dst.chunks_exact_mut(2).zip(src.iter()) {
                    d.copy_from_slice(&self.pack(*s).to_le_bytes()[..2]);
                }
            }
            _ => {
                // FbLayout::from_info отбрасывает остальные bpp; ветка нужна
                // только для исчерпывающести match.
            }
        }
    }
}

/// Открытый `/dev/fb0` с отображением scanout.
pub struct FbdevBackend {
    file: File,
    layout: FbLayout,
    var: FbVarInfo,
    fix: FbFixInfo,
    mapping: *mut u8,
    map_len: usize,
    frames: u64,
    path: PathBuf,
}

// SAFETY: сырой указатель — это отображение физического scanout, которым владеет
// этот объект. `Backend: Send` нужен, потому что сервер перемещается в поток
// `Server::run`; никакого разделения между потоками нет (мьютексов/Arc на
// mapping не существует), поэтому Send безопасен.
unsafe impl Send for FbdevBackend {}

impl FbdevBackend {
    /// Открыть `/dev/fb0`, прочитать оба ioctl и отобразить scanout.
    ///
    /// Первый успешный `open` включает kernel takeover (консоль перестаёт
    /// перерисовывать экран); это происходит до mmap и не откатывается.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        let fd = file.as_raw_fd();

        let mut var_buf = [0u8; FB_VAR_SIZE];
        // SAFETY: fd жив, буфер ровно 160 байт — sizeof(fb_var_screeninfo) по ABI;
        // ioctl пишет не больше этого размера.
        let rc = unsafe { ioctl(fd, FBIOGET_VSCREENINFO, var_buf.as_mut_ptr() as *mut c_void) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut fix_buf = [0u8; FB_FIX_SIZE];
        // SAFETY: то же для 80-байтного fb_fix_screeninfo.
        let rc = unsafe { ioctl(fd, FBIOGET_FSCREENINFO, fix_buf.as_mut_ptr() as *mut c_void) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }

        let var = FbVarInfo::decode(&var_buf);
        let fix = FbFixInfo::decode(&fix_buf);
        let layout = FbLayout::from_info(&var, &fix)?;
        let map_len = fix.smem_len as usize;
        // SAFETY: mmap MAP_SHARED на /dev/fb0; len ровно smem_len (kernel
        // проверяет offset+len <= smem_len и разрешает непаговый len по
        // docs/desktop-b.md §3.3). Возвращённый указатель проверяем на MAP_FAILED.
        let mapping = unsafe {
            mmap(
                std::ptr::null_mut(),
                map_len,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                fd,
                0,
            )
        };
        if mapping == MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            file,
            layout,
            var,
            fix,
            mapping: mapping as *mut u8,
            map_len,
            frames: 0,
            path,
        })
    }

    pub fn layout(&self) -> &FbLayout {
        &self.layout
    }

    pub fn var(&self) -> &FbVarInfo {
        &self.var
    }

    pub fn fix(&self) -> &FbFixInfo {
        &self.fix
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Прочитать пиксель из scanout (диагностика/selfcheck).
    pub fn sample(&self, x: i32, y: i32) -> Option<u32> {
        // SAFETY: mapping/map_len принадлежат объекту и живут до Drop; slice
        // только читается, границы пикселя проверяет FbLayout.
        let slice = unsafe { std::slice::from_raw_parts(self.mapping, self.map_len) };
        self.layout.read_pixel(slice, x, y)
    }

    /// Строка для стартового лога: id, геометрия, маски.
    pub fn describe(&self) -> String {
        format!(
            "{} {}x{} bpp={} stride={} smem_len={} phys={:#x} rgb=({},{},{})",
            self.fix.id_string(),
            self.layout.width,
            self.layout.height,
            self.layout.bpp,
            self.layout.stride,
            self.layout.smem_len,
            self.fix.smem_start,
            self.layout.red.offset,
            self.layout.green.offset,
            self.layout.blue.offset,
        )
    }
}

impl std::fmt::Debug for FbdevBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FbdevBackend")
            .field("path", &self.path)
            .field("layout", &self.layout)
            .field("frames", &self.frames)
            .finish()
    }
}

impl Backend for FbdevBackend {
    fn size(&self) -> (u32, u32) {
        (self.layout.width, self.layout.height)
    }

    fn present(&mut self, canvas: &Canvas, damage: &[Rect]) -> io::Result<()> {
        // SAFETY: mapping/map_len валидны до Drop; blit_damage проверяет
        // границы каждого пикселя и не пишет за len.
        let slice = unsafe { std::slice::from_raw_parts_mut(self.mapping, self.map_len) };
        self.layout.blit_damage(slice, canvas, damage);
        self.frames += 1;
        Ok(())
    }

    fn frames(&self) -> u64 {
        self.frames
    }

    fn kind(&self) -> crate::backend::BackendKind {
        crate::backend::BackendKind::Fbdev
    }
}

impl Drop for FbdevBackend {
    fn drop(&mut self) {
        // SAFETY: mapping был получен mmap с этим map_len и ещё не снят.
        unsafe {
            let _ = munmap(self.mapping as *mut c_void, self.map_len);
        }
        // fd закрывается полем file; takeover по ABI не возвращается.
        let _ = &self.file;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::rgb;

    fn var_bytes(
        xres: u32,
        yres: u32,
        bpp: u32,
        red: (u32, u32),
        green: (u32, u32),
        blue: (u32, u32),
    ) -> [u8; 160] {
        let mut buf = [0u8; 160];
        let put = |buf: &mut [u8; 160], off: usize, value: u32| {
            buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
        };
        put(&mut buf, 0, xres);
        put(&mut buf, 4, yres);
        put(&mut buf, 8, xres);
        put(&mut buf, 12, yres);
        put(&mut buf, 24, bpp);
        put(&mut buf, 32, red.0);
        put(&mut buf, 36, red.1);
        put(&mut buf, 44, green.0);
        put(&mut buf, 48, green.1);
        put(&mut buf, 56, blue.0);
        put(&mut buf, 60, blue.1);
        buf
    }

    fn fix_bytes(stride: u32, smem_len: u32) -> [u8; 80] {
        let mut buf = [0u8; 80];
        buf[0..6].copy_from_slice(b"paghfb");
        buf[16..24].copy_from_slice(&0x1234_5000u64.to_le_bytes());
        buf[24..28].copy_from_slice(&smem_len.to_le_bytes());
        buf[36..40].copy_from_slice(&2u32.to_le_bytes());
        buf[48..52].copy_from_slice(&stride.to_le_bytes());
        buf
    }

    fn layout_32() -> FbLayout {
        let var = FbVarInfo::decode(&var_bytes(4, 3, 32, (16, 8), (8, 8), (0, 8)));
        let fix = FbFixInfo::decode(&fix_bytes(16, 48));
        FbLayout::from_info(&var, &fix).unwrap()
    }

    #[test]
    fn decodes_uapi_offsets() {
        let var = FbVarInfo::decode(&var_bytes(1280, 720, 32, (16, 8), (8, 8), (0, 8)));
        assert_eq!((var.xres, var.yres, var.bits_per_pixel), (1280, 720, 32));
        assert_eq!(
            var.red,
            BitField {
                offset: 16,
                length: 8
            }
        );
        let fix = FbFixInfo::decode(&fix_bytes(5120, 5120 * 720));
        assert_eq!(fix.id_string(), "paghfb");
        assert_eq!(fix.line_length, 5120);
        assert_eq!(fix.smem_start, 0x1234_5000);
        assert_eq!(fix.smem_len, 5120 * 720);
    }

    #[test]
    fn packs_xrgb8888_and_rgb565() {
        let layout = layout_32();
        assert_eq!(layout.pack(rgb(0x12, 0x34, 0x56)), 0x0012_3456);
        let var = FbVarInfo::decode(&var_bytes(4, 3, 16, (11, 5), (5, 6), (0, 5)));
        let fix = FbFixInfo::decode(&fix_bytes(8, 24));
        let layout16 = FbLayout::from_info(&var, &fix).unwrap();
        assert_eq!(layout16.pack(rgb(0xFF, 0, 0)), 0xF800);
        assert_eq!(layout16.pack(rgb(0, 0xFF, 0)), 0x07E0);
        assert_eq!(layout16.pack(rgb(0, 0, 0xFF)), 0x001F);
    }

    #[test]
    fn blits_damage_with_stride_padding() {
        let layout = layout_32();
        let mut dst = vec![0u8; 48];
        let mut canvas = Canvas::new(4, 3, rgb(0, 0, 0));
        canvas.put(1, 1, rgb(0xAA, 0xBB, 0xCC));
        let written = layout.blit_damage(&mut dst, &canvas, &[Rect::new(0, 0, 4, 3)]);
        assert_eq!(written, 12);
        // Pixel (1,1): row 1 offset 16 + 4 = 20 -> B,G,R,X.
        assert_eq!(&dst[20..24], &[0xCC, 0xBB, 0xAA, 0x00]);
        assert_eq!(layout.read_pixel(&dst, 1, 1), Some(0x00AA_BBCC));
        assert_eq!(layout.read_pixel(&dst, 4, 1), None);
        // Damage вне экрана не пишет.
        assert_eq!(
            layout.blit_damage(&mut dst, &canvas, &[Rect::new(-5, -5, 2, 2)]),
            0
        );
    }

    #[test]
    fn rejects_bad_layout() {
        let var = FbVarInfo::decode(&var_bytes(10, 10, 32, (16, 8), (8, 8), (0, 8)));
        let fix = FbFixInfo::decode(&fix_bytes(8, 400));
        assert!(FbLayout::from_info(&var, &fix).is_err());
        let var = FbVarInfo::decode(&var_bytes(10, 10, 12, (16, 8), (8, 8), (0, 8)));
        let fix = FbFixInfo::decode(&fix_bytes(40, 400));
        assert!(FbLayout::from_info(&var, &fix).is_err());
        let var = FbVarInfo::decode(&var_bytes(10, 10, 32, (16, 8), (8, 8), (0, 8)));
        let fix = FbFixInfo::decode(&fix_bytes(40, 100));
        assert!(FbLayout::from_info(&var, &fix).is_err());
    }

    #[test]
    fn open_missing_fbdev_fails_cleanly() {
        let err = FbdevBackend::open("/dev/definitely-no-fb0").unwrap_err();
        assert!(matches!(
            err.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
        ));
    }
}
