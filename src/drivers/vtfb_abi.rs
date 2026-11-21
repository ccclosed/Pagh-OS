//! Чистый ABI пиксельной поверхности терминальной сессии (`/dev/vtfb1`).
//!
//! Контракт фиксирован координатором и не меняется этим модулем:
//!
//! * `ioctl(fd, VTFBIOGET_INFO = 0x4700, &info)` — 24 байта little-endian:
//!   `struct { u32 width; u32 height; u32 stride; u32 bpp; u64 generation; }`.
//! * `mmap(NULL, stride*height, PROT_READ, MAP_SHARED, fd, 0)` отображает
//!   пиксели сессии (кадры PMM поверхности).
//! * `read(2)` fallback — те же 24 байта заголовка, затем пиксели; поэтому
//!   логический размер файла — `24 + stride*height`.
//!
//! `generation` инкрементируется на каждый flush сессии: клиент по нему
//! понимает, что поверхность изменилась, и не блитит её зря.
//!
//! Модуль `core`-only и без состояния — host-свойство P66 исполняет ровно тот
//! исходник, который компилирует ядро (кодирование/декодирование 24 байт и план
//! `read`'а — единственная часть ABI, проверяемая без QEMU).

/// `ioctl`-запрос «дай информацию о поверхности» (значение из контракта).
pub const VTFBIOGET_INFO: u64 = 0x4700;

/// Длина заголовка в `read(2)` и размер структуры `ioctl`.
pub const VTFB_INFO_SIZE: usize = 24;

/// Информация о пиксельной поверхности.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VtfbInfo {
    /// Ширина в пикселях.
    pub width: u32,
    /// Высота в пикселях.
    pub height: u32,
    /// Байт на строку (шаг); равен `width * bpp`.
    pub stride: u32,
    /// Бит на пиксель (32).
    pub bpp: u32,
    /// Счётчик завершённых flush'ей сессии.
    pub generation: u64,
}

/// Закодировать структуру в 24 байта little-endian (раскладка `#[repr(C)]`
/// без хвостового паддинга: 5 полей по 4/8 байт).
pub fn encode_info(info: &VtfbInfo) -> [u8; VTFB_INFO_SIZE] {
    let mut out = [0u8; VTFB_INFO_SIZE];
    out[0..4].copy_from_slice(&info.width.to_le_bytes());
    out[4..8].copy_from_slice(&info.height.to_le_bytes());
    out[8..12].copy_from_slice(&info.stride.to_le_bytes());
    out[12..16].copy_from_slice(&info.bpp.to_le_bytes());
    out[16..24].copy_from_slice(&info.generation.to_le_bytes());
    out
}

/// Декодировать 24 байта; `None`, если их меньше.
///
/// Нужен только тестам и будущему клиенту на стороне ядра, но живёт здесь,
/// чтобы кодирование и декодирование не разъехались.
pub fn decode_info(bytes: &[u8]) -> Option<VtfbInfo> {
    if bytes.len() < VTFB_INFO_SIZE {
        return None;
    }
    let u32_at = |at: usize| -> u32 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    };
    let mut gen = [0u8; 8];
    gen.copy_from_slice(&bytes[16..24]);
    Some(VtfbInfo {
        width: u32_at(0),
        height: u32_at(4),
        stride: u32_at(8),
        bpp: u32_at(12),
        generation: u64::from_le_bytes(gen),
    })
}

/// Размер пиксельной части поверхности в байтах (`stride * height`).
pub fn pixel_bytes(info: &VtfbInfo) -> u64 {
    (info.stride as u64).saturating_mul(info.height as u64)
}

/// Логический размер `read(2)`-файла: заголовок плюс пиксели.
pub fn read_size(info: &VtfbInfo) -> u64 {
    VTFB_INFO_SIZE as u64 + pixel_bytes(info)
}

/// План одного `read(2)`: `(байт_заголовка, пиксельное_смещение, байт_пикселей)`.
///
/// Заголовок лежит в `[0, 24)`, пиксели — после него. Запрос может начинаться
/// где угодно (клиент читает поверхность кусками), поэтому план явно говорит,
/// сколько байт заголовка ещё осталось скопировать, с какого пиксельного
/// смещения продолжать и сколько пиксельных байт приходится на этот запрос.
/// Сумма двух длин всегда равна `copied = min(count, total - offset)`.
pub fn read_plan(info: &VtfbInfo, offset: u64, count: u64) -> (u64, u64, u64) {
    let total = read_size(info);
    if offset >= total || count == 0 {
        return (0, 0, 0);
    }
    let copied = count.min(total - offset);
    let header_end = (offset + copied).min(VTFB_INFO_SIZE as u64);
    let header_bytes = header_end.saturating_sub(offset);
    let pixel_offset = offset.saturating_sub(VTFB_INFO_SIZE as u64);
    let pixel_bytes = copied - header_bytes;
    (header_bytes, pixel_offset, pixel_bytes)
}
