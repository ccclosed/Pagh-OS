//! Минимальный декодер PNG (RGBA8) для готовых иконок.
//!
//! Логика повторяет `rust-apps/pagh-desktop/src/png.rs` (та же MIT-лицензия
//! проекта) в объёме, нужном иконкам: 8 бит на канал, без интерлейса,
//! color type 0/2/4/6 (серый/RGB/серый+альфа/RGBA), все пять PNG-фильтров.
//! Свой deflate — в [`crate::deflate`], внешних зависимостей нет, поэтому
//! статическая musl-сборка остаётся самодостаточной.
//!
//! `None` — файл не PNG или использует неподдержанную форму. Вызывающий
//! (модуль иконок) в этом случае просто ничего не рисует: падать из-за
//! испорченного ассета файловый менеджер не должен.

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Декодированное PNG-изображение: RGBA8, строки сверху вниз.
pub(crate) struct DecodedPng {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
}

/// Декодировать PNG в RGBA8.
pub(crate) fn decode_png(data: &[u8]) -> Option<DecodedPng> {
    if data.len() < 8 || data[..8] != PNG_SIGNATURE {
        return None;
    }
    let mut position = 8usize;
    let mut header: Option<(u32, u32, u8, u8, u8)> = None;
    let mut compressed = Vec::new();
    while position + 12 <= data.len() {
        let length = u32::from_be_bytes(data[position..position + 4].try_into().ok()?) as usize;
        let kind = &data[position + 4..position + 8];
        if position + 12 + length > data.len() {
            return None;
        }
        let body = &data[position + 8..position + 8 + length];
        match kind {
            b"IHDR" => {
                if length < 13 {
                    return None;
                }
                let width = u32::from_be_bytes(body[0..4].try_into().ok()?);
                let height = u32::from_be_bytes(body[4..8].try_into().ok()?);
                // compression == 0 (deflate), filter == 0 (adaptive), interlace == 0.
                if body[10] != 0 || body[11] != 0 {
                    return None;
                }
                header = Some((width, height, body[8], body[9], body[12]));
            }
            b"IDAT" => compressed.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        position += 12 + length;
    }

    let (width, height, bit_depth, color_type, interlace) = header?;
    if width == 0 || height == 0 || bit_depth != 8 || interlace != 0 {
        return None;
    }
    let channels = match color_type {
        0 => 1, // градации серого
        2 => 3, // RGB
        4 => 2, // серый + альфа
        6 => 4, // RGBA
        _ => return None,
    };
    let stride = width as usize * channels;
    let raw = crate::deflate::inflate_zlib(&compressed)?;
    if raw.len() < (stride + 1) * height as usize {
        return None;
    }

    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    let mut previous = vec![0u8; stride];
    let mut current = vec![0u8; stride];
    for row in 0..height as usize {
        let start = row * (stride + 1);
        let filter = raw[start];
        current.copy_from_slice(&raw[start + 1..start + 1 + stride]);
        for index in 0..stride {
            let left = if index >= channels {
                current[index - channels]
            } else {
                0
            };
            let above = previous[index];
            let upper_left = if index >= channels {
                previous[index - channels]
            } else {
                0
            };
            let predictor = match filter {
                0 => 0,
                1 => left,
                2 => above,
                3 => ((u16::from(left) + u16::from(above)) / 2) as u8,
                4 => paeth(left, above, upper_left),
                _ => return None,
            };
            current[index] = current[index].wrapping_add(predictor);
        }
        for column in 0..width as usize {
            let source = &current[column * channels..(column + 1) * channels];
            let target = &mut rgba[(row * width as usize + column) * 4..][..4];
            match channels {
                1 => {
                    target[0] = source[0];
                    target[1] = source[0];
                    target[2] = source[0];
                    target[3] = 255;
                }
                2 => {
                    target[0] = source[0];
                    target[1] = source[0];
                    target[2] = source[0];
                    target[3] = source[1];
                }
                3 => {
                    target[0] = source[0];
                    target[1] = source[1];
                    target[2] = source[2];
                    target[3] = 255;
                }
                _ => target.copy_from_slice(source),
            }
        }
        std::mem::swap(&mut previous, &mut current);
    }
    Some(DecodedPng {
        width,
        height,
        rgba,
    })
}

/// Предиктор Paeth (PNG spec §9.4): ближайший к `left + above - upper_left`.
fn paeth(left: u8, above: u8, upper_left: u8) -> u8 {
    let estimate = i32::from(left) + i32::from(above) - i32::from(upper_left);
    let da = (estimate - i32::from(left)).abs();
    let db = (estimate - i32::from(above)).abs();
    let dc = (estimate - i32::from(upper_left)).abs();
    if da <= db && da <= dc {
        left
    } else if db <= dc {
        above
    } else {
        upper_left
    }
}
