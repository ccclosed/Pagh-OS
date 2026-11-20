//! Чистый ABI `fbdev`: кодирование `struct fb_var_screeninfo` и
//! `struct fb_fix_screeninfo` для `FBIOGET_VSCREENINFO`/`FBIOGET_FSCREENINFO`.
//!
//! Модуль `core`-only намеренно: host-tests включают его через `#[path]` и
//! сверяют размеры/смещения с настоящим Linux UAPI (`include/uapi/linux/fb.h`),
//! поэтому композитор получает ровно те байты, которые ждёт `fbset`/SDL/KMS-
//! совместимый код. Любая правка раскладки обязана падать в этих тестах, а не
//! на машине пользователя.
//!
//! Размеры (x86_64, Linux 6.x): `fb_var_screeninfo` = 160 байт,
//! `fb_fix_screeninfo` = 80 байт. `unsigned long` в UAPI 64-битный, поэтому
//! `smem_start`/`mmio_start` лежат по 8-байтовым смещениям 16 и 56.

/// `FBIOGET_VSCREENINFO` — прочитать `struct fb_var_screeninfo`.
pub const FBIOGET_VSCREENINFO: u64 = 0x4600;
/// `FBIOGET_FSCREENINFO` — прочитать `struct fb_fix_screeninfo`.
pub const FBIOGET_FSCREENINFO: u64 = 0x4602;

/// `sizeof(struct fb_var_screeninfo)` на x86_64.
pub const FB_VAR_SIZE: usize = 160;
/// `sizeof(struct fb_fix_screeninfo)` на x86_64.
pub const FB_FIX_SIZE: usize = 80;

/// `FB_TYPE_PACKED_PIXELS` — единственный тип, который отдаёт Limine.
pub const FB_TYPE_PACKED_PIXELS: u32 = 0;
/// `FB_VISUAL_TRUECOLOR`.
pub const FB_VISUAL_TRUECOLOR: u32 = 2;
/// `FB_ACCEL_NONE`.
pub const FB_ACCEL_NONE: u32 = 0;

/// `struct fb_bitfield` (offset/length/msb_right), входит в var four times.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FbBitfield {
    pub offset: u32,
    pub length: u32,
}

/// Минимальный `struct fb_var_screeninfo`, который заполняет ядро.
///
/// Поля, которых у Limine нет (тайминги, margins, rotate), остаются нулями:
/// userspace, которому нужен только размер/формат, читает `xres`/`yres`/
/// `bits_per_pixel`; `fbset`, меняющий режим, должен был бы получить `EINVAL`
/// на `FBIOPUT_VSCREENINFO`, а не выдуманные тайминги.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VarScreenInfo {
    pub xres: u32,
    pub yres: u32,
    pub xres_virtual: u32,
    pub yres_virtual: u32,
    pub xoffset: u32,
    pub yoffset: u32,
    pub bits_per_pixel: u32,
    pub grayscale: u32,
    pub red: FbBitfield,
    pub green: FbBitfield,
    pub blue: FbBitfield,
    pub transp: FbBitfield,
    pub nonstd: u32,
    pub activate: u32,
    pub height: u32,
    pub width: u32,
    pub accel_flags: u32,
    pub pixclock: u32,
    pub left_margin: u32,
    pub right_margin: u32,
    pub upper_margin: u32,
    pub lower_margin: u32,
    pub hsync_len: u32,
    pub vsync_len: u32,
    pub sync: u32,
    pub vmode: u32,
    pub rotate: u32,
    pub colorspace: u32,
}

/// Минимальный `struct fb_fix_screeninfo`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixScreenInfo {
    /// 16 байт, NUL-padded: у Linux это `"paghfb"`/`"VESA VGA"` и т.п.
    pub id: [u8; 16],
    pub smem_start: u64,
    pub smem_len: u32,
    pub type_: u32,
    pub type_aux: u32,
    pub visual: u32,
    pub line_length: u32,
}

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// Закодировать `fb_var_screeninfo` побайтно (little-endian, x86_64).
pub fn encode_var(v: &VarScreenInfo) -> [u8; FB_VAR_SIZE] {
    let mut b = [0u8; FB_VAR_SIZE];
    put_u32(&mut b, 0, v.xres);
    put_u32(&mut b, 4, v.yres);
    put_u32(&mut b, 8, v.xres_virtual);
    put_u32(&mut b, 12, v.yres_virtual);
    put_u32(&mut b, 16, v.xoffset);
    put_u32(&mut b, 20, v.yoffset);
    put_u32(&mut b, 24, v.bits_per_pixel);
    put_u32(&mut b, 28, v.grayscale);
    // struct fb_bitfield: offset, length, msb_right (всегда 0).
    put_u32(&mut b, 32, v.red.offset);
    put_u32(&mut b, 36, v.red.length);
    put_u32(&mut b, 44, v.green.offset);
    put_u32(&mut b, 48, v.green.length);
    put_u32(&mut b, 56, v.blue.offset);
    put_u32(&mut b, 60, v.blue.length);
    put_u32(&mut b, 68, v.transp.offset);
    put_u32(&mut b, 72, v.transp.length);
    put_u32(&mut b, 80, v.nonstd);
    put_u32(&mut b, 84, v.activate);
    put_u32(&mut b, 88, v.height);
    put_u32(&mut b, 92, v.width);
    put_u32(&mut b, 96, v.accel_flags);
    put_u32(&mut b, 100, v.pixclock);
    put_u32(&mut b, 104, v.left_margin);
    put_u32(&mut b, 108, v.right_margin);
    put_u32(&mut b, 112, v.upper_margin);
    put_u32(&mut b, 116, v.lower_margin);
    put_u32(&mut b, 120, v.hsync_len);
    put_u32(&mut b, 124, v.vsync_len);
    put_u32(&mut b, 128, v.sync);
    put_u32(&mut b, 132, v.vmode);
    put_u32(&mut b, 136, v.rotate);
    put_u32(&mut b, 140, v.colorspace);
    // reserved[4] (144..160) уже нули.
    b
}

/// Закодировать `fb_fix_screeninfo` побайтно (little-endian, x86_64).
pub fn encode_fix(f: &FixScreenInfo) -> [u8; FB_FIX_SIZE] {
    let mut b = [0u8; FB_FIX_SIZE];
    b[0..16].copy_from_slice(&f.id);
    put_u64(&mut b, 16, f.smem_start);
    put_u32(&mut b, 24, f.smem_len);
    put_u32(&mut b, 28, f.type_);
    put_u32(&mut b, 32, f.type_aux);
    put_u32(&mut b, 36, f.visual);
    // xpanstep/ypanstep/ywrapstep (40..46) — у packed framebuffer нули.
    put_u32(&mut b, 48, f.line_length);
    // mmio_start (56, unsigned long), mmio_len (64), accel (68), capabilities
    // (72), reserved[2] (74..78) и хвостовой padding — нули: MMIO-базы отдельно
    // от smem_start у Limine нет.
    put_u32(&mut b, 68, FB_ACCEL_NONE);
    b
}

/// NUL-padded 16-байтовый `id` для [`FixScreenInfo`].
pub fn padded_id(name: &[u8]) -> [u8; 16] {
    let mut id = [0u8; 16];
    let n = core::cmp::min(name.len(), 15); // 16-й байт — терминатор
    id[..n].copy_from_slice(&name[..n]);
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Смещения, снятые с настоящего `linux/fb.h` (`gcc -offsetof`, x86_64).
    #[test]
    fn var_offsets_match_linux_uapi() {
        let v = VarScreenInfo {
            xres: 0x1111_1111,
            yres: 0x2222_2222,
            xres_virtual: 0x3333_3333,
            yres_virtual: 0x4444_4444,
            xoffset: 0x5555_5555,
            yoffset: 0x6666_6666,
            bits_per_pixel: 0x7777_7777,
            grayscale: 0x8888_8888,
            red: FbBitfield {
                offset: 0x9999_9999,
                length: 0xAAAA_AAAA,
            },
            green: FbBitfield {
                offset: 0xBBBB_BBBB,
                length: 0xCCCC_CCCC,
            },
            blue: FbBitfield {
                offset: 0xDDDD_DDDD,
                length: 0xEEEE_EEEE,
            },
            transp: FbBitfield {
                offset: 0x0101_0101,
                length: 0x0202_0202,
            },
            nonstd: 0x0303_0303,
            activate: 0x0404_0404,
            height: 0x0505_0505,
            width: 0x0606_0606,
            accel_flags: 0x0707_0707,
            pixclock: 0x0808_0808,
            left_margin: 0x0909_0909,
            right_margin: 0x0A0A_0A0A,
            upper_margin: 0x0B0B_0B0B,
            lower_margin: 0x0C0C_0C0C,
            hsync_len: 0x0D0D_0D0D,
            vsync_len: 0x0E0E_0E0E,
            sync: 0x0F0F_0F0F,
            vmode: 0x1010_1010,
            rotate: 0x1111_1111,
            colorspace: 0x1212_1212,
        };
        let b = encode_var(&v);
        assert_eq!(b.len(), 160);
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        assert_eq!(u32_at(0), v.xres);
        assert_eq!(u32_at(4), v.yres);
        assert_eq!(u32_at(8), v.xres_virtual);
        assert_eq!(u32_at(12), v.yres_virtual);
        assert_eq!(u32_at(16), v.xoffset);
        assert_eq!(u32_at(20), v.yoffset);
        assert_eq!(u32_at(24), v.bits_per_pixel);
        assert_eq!(u32_at(28), v.grayscale);
        assert_eq!(u32_at(32), v.red.offset);
        assert_eq!(u32_at(36), v.red.length);
        assert_eq!(u32_at(40), 0, "fb_bitfield.msb_right");
        assert_eq!(u32_at(44), v.green.offset);
        assert_eq!(u32_at(48), v.green.length);
        assert_eq!(u32_at(56), v.blue.offset);
        assert_eq!(u32_at(60), v.blue.length);
        assert_eq!(u32_at(68), v.transp.offset);
        assert_eq!(u32_at(80), v.nonstd);
        assert_eq!(u32_at(84), v.activate);
        assert_eq!(u32_at(88), v.height);
        assert_eq!(u32_at(92), v.width);
        assert_eq!(u32_at(96), v.accel_flags);
        assert_eq!(u32_at(100), v.pixclock);
        assert_eq!(u32_at(104), v.left_margin);
        assert_eq!(u32_at(108), v.right_margin);
        assert_eq!(u32_at(112), v.upper_margin);
        assert_eq!(u32_at(116), v.lower_margin);
        assert_eq!(u32_at(120), v.hsync_len);
        assert_eq!(u32_at(124), v.vsync_len);
        assert_eq!(u32_at(128), v.sync);
        assert_eq!(u32_at(132), v.vmode);
        assert_eq!(u32_at(136), v.rotate);
        assert_eq!(u32_at(140), v.colorspace);
        for o in (144..160).step_by(4) {
            assert_eq!(u32_at(o), 0, "reserved[{o}]");
        }
    }

    #[test]
    fn fix_offsets_match_linux_uapi() {
        let f = FixScreenInfo {
            id: padded_id(b"paghfb"),
            smem_start: 0x1122_3344_5566_7788,
            smem_len: 0x99AA_BBCC,
            type_: FB_TYPE_PACKED_PIXELS,
            type_aux: 0,
            visual: FB_VISUAL_TRUECOLOR,
            line_length: 0x1234_5678,
        };
        let b = encode_fix(&f);
        assert_eq!(b.len(), 80);
        assert_eq!(&b[0..8], b"paghfb\0\0");
        assert_eq!(
            u64::from_le_bytes(b[16..24].try_into().unwrap()),
            f.smem_start
        );
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        assert_eq!(u32_at(24), f.smem_len);
        assert_eq!(u32_at(28), FB_TYPE_PACKED_PIXELS);
        assert_eq!(u32_at(32), 0);
        assert_eq!(u32_at(36), FB_VISUAL_TRUECOLOR);
        assert_eq!(u32_at(40), 0, "xpanstep");
        assert_eq!(u32_at(42) & 0xffff, 0, "ypanstep low half");
        assert_eq!(u32_at(44) & 0xffff, 0, "ywrapstep low half");
        assert_eq!(u32_at(48), f.line_length);
        assert_eq!(u64::from_le_bytes(b[56..64].try_into().unwrap()), 0);
        assert_eq!(u32_at(64), 0);
        assert_eq!(u32_at(68), 0);
        assert_eq!(u32_at(72) & 0xffff, 0, "capabilities");
        assert_eq!(u32_at(72) >> 16, 0, "reserved[0]");
        assert_eq!(u32_at(76) & 0xffff, 0, "reserved[1]");
    }

    #[test]
    fn ioctl_numbers_are_the_uapi_ones() {
        assert_eq!(FBIOGET_VSCREENINFO, 0x4600);
        assert_eq!(FBIOGET_FSCREENINFO, 0x4602);
    }
}
