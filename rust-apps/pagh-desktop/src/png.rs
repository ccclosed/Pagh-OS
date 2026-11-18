//! Минимальный кодировщик PNG (truecolor, zlib stored-блоки) и PPM.
//!
//! Внешних зависимостей нет: deflate-поток собирается из несжатых stored-блоков, что
//! делает файлы больше, но полностью детерминированными и без сторонних крейтов —
//! важно для статической сборки под musl и для тестов без сети.

use crate::canvas::Canvas;
use crate::color;
use std::fs;
use std::io;
use std::path::Path;

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Закодировать холст как PNG RGB8.
///
/// Строки фильтруются Up-фильтром (кроме первой), затем zlib-поток сжимается
/// встроенным deflate-компрессором (fixed Huffman + LZ77).
pub fn encode_png(canvas: &Canvas) -> Vec<u8> {
    let width = canvas.width();
    let height = canvas.height();
    let pixels = canvas.pixels();
    let stride = width as usize * 3;

    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    let mut previous = vec![0u8; stride];
    let mut current = vec![0u8; stride];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let px = pixels[y * width as usize + x];
            current[x * 3] = color::red(px);
            current[x * 3 + 1] = color::green(px);
            current[x * 3 + 2] = color::blue(px);
        }
        if y == 0 {
            raw.push(0); // filter: None
            raw.extend_from_slice(&current);
        } else {
            raw.push(2); // filter: Up
            for i in 0..stride {
                raw.push(current[i].wrapping_sub(previous[i]));
            }
        }
        std::mem::swap(&mut previous, &mut current);
    }
    let idat = crate::deflate::zlib_compress(&raw);

    let mut out = Vec::with_capacity(idat.len() + 128);
    out.extend_from_slice(&PNG_SIGNATURE);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(2); // color type: truecolor RGB
    ihdr.push(0); // compression
    ihdr.push(0); // filter
    ihdr.push(0); // interlace
    write_chunk(&mut out, b"IHDR", &ihdr);
    write_chunk(&mut out, b"IDAT", &idat);
    write_chunk(&mut out, b"IEND", &[]);
    out
}

/// Записать холст в PNG.
pub fn write_png(path: impl AsRef<Path>, canvas: &Canvas) -> io::Result<()> {
    let data = encode_png(canvas);
    if let Some(parent) = path.as_ref().parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(path, data)
}

/// Записать холст в PPM (P6) — простой формат для отладки.
pub fn write_ppm(path: impl AsRef<Path>, canvas: &Canvas) -> io::Result<()> {
    if let Some(parent) = path.as_ref().parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut out = format!("P6\n{} {}\n255\n", canvas.width(), canvas.height()).into_bytes();
    for px in canvas.pixels() {
        out.push(color::red(*px));
        out.push(color::green(*px));
        out.push(color::blue(*px));
    }
    fs::write(path, out)
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut crc_input = Vec::with_capacity(data.len() + 4);
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// Декодированное PNG-изображение: RGBA8, строки сверху вниз.
pub struct DecodedPng {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl DecodedPng {
    /// Альфа пикселя; за границами — 0.
    pub fn alpha(&self, x: u32, y: u32) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.rgba[((y * self.width + x) as usize) * 4 + 3]
    }
}

/// Декодировать PNG в RGBA8.
///
/// Поддержано то, что нужно готовым иконкам и скриншотам: 8 бит на канал,
/// без интерлейса, color type 0/2/4/6 (серый/RGB/серый+альфа/RGBA). Все пять
/// PNG-фильтров (None/Sub/Up/Average/Paeth) и все три deflate-формы.
///
/// `None` — файл не PNG или использует неподдержанную форму. Вызывающий
/// (модуль иконок) в этом случае просто ничего не рисует: падать из-за
/// испорченного ассета композитор не должен.
pub fn decode_png(data: &[u8]) -> Option<DecodedPng> {
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

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        let idx = ((crc ^ *byte as u32) & 0xFF) as usize;
        crc = CRC_TABLE[idx] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

static CRC_TABLE: [u32; 256] = build_crc_table();

const fn build_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0usize;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::rgb;
    use crate::geometry::Rect;

    fn read_chunks(png: &[u8]) -> Vec<(String, Vec<u8>)> {
        assert_eq!(&png[..8], &PNG_SIGNATURE);
        let mut chunks = Vec::new();
        let mut off = 8;
        while off + 8 <= png.len() {
            let len = u32::from_be_bytes(png[off..off + 4].try_into().unwrap()) as usize;
            let kind = String::from_utf8(png[off + 4..off + 8].to_vec()).unwrap();
            let data = png[off + 8..off + 8 + len].to_vec();
            let crc = u32::from_be_bytes(png[off + 8 + len..off + 12 + len].try_into().unwrap());
            let mut crc_input = kind.as_bytes().to_vec();
            crc_input.extend_from_slice(&data);
            assert_eq!(crc, crc32(&crc_input), "CRC чанка {kind}");
            chunks.push((kind, data));
            off += 12 + len;
        }
        assert_eq!(off, png.len());
        chunks
    }

    fn decode_rgb(width: usize, height: usize, raw: &[u8]) -> Vec<u8> {
        assert_eq!(raw.len(), (width * 3 + 1) * height);
        let stride = width * 3;
        let mut out = vec![0u8; stride * height];
        let mut previous = vec![0u8; stride];
        for y in 0..height {
            let base = y * (stride + 1);
            let filter = raw[base];
            let row = &raw[base + 1..base + 1 + stride];
            let mut current = vec![0u8; stride];
            match filter {
                0 => current.copy_from_slice(row),
                2 => {
                    for i in 0..stride {
                        current[i] = previous[i].wrapping_add(row[i]);
                    }
                }
                other => panic!("неожиданный filter {other}"),
            }
            out[y * stride..(y + 1) * stride].copy_from_slice(&current);
            previous = current;
        }
        out
    }

    #[test]
    fn png_structure_and_pixels() {
        let mut canvas = Canvas::new(3, 2, rgb(10, 20, 30));
        canvas.put(1, 1, rgb(200, 100, 50));
        let png = encode_png(&canvas);
        let chunks = read_chunks(&png);
        assert_eq!(chunks[0].0, "IHDR");
        let ihdr = &chunks[0].1;
        assert_eq!(u32::from_be_bytes(ihdr[0..4].try_into().unwrap()), 3);
        assert_eq!(u32::from_be_bytes(ihdr[4..8].try_into().unwrap()), 2);
        assert_eq!(ihdr[8], 8);
        assert_eq!(ihdr[9], 2);
        let idat = chunks.iter().find(|(k, _)| k == "IDAT").unwrap();
        let raw = crate::deflate::inflate_zlib(&idat.1).expect("inflate");
        let rgb = decode_rgb(3, 2, &raw);
        assert_eq!(&rgb[0..3], &[10, 20, 30]);
        assert_eq!(&rgb[3..6], &[10, 20, 30]);
        assert_eq!(&rgb[9 + 3..9 + 6], &[200, 100, 50]);
        assert_eq!(chunks.last().unwrap().0, "IEND");
    }

    #[test]
    fn gradient_png_is_compressed() {
        let mut canvas = Canvas::new(1280, 720, rgb(0, 0, 0));
        canvas.fill_gradient(Rect::new(0, 0, 1280, 720), rgb(23, 39, 49), rgb(8, 14, 19));
        let png = encode_png(&canvas);
        assert!(
            png.len() < 300_000,
            "PNG-градиент должен сжиматься, получено {} байт",
            png.len()
        );
    }

    #[test]
    fn writes_files() {
        let dir = std::env::temp_dir().join(format!("pagh-png-test-{}", std::process::id()));
        let png = dir.join("a.png");
        let ppm = dir.join("a.ppm");
        let canvas = Canvas::new(4, 4, rgb(1, 2, 3));
        write_png(&png, &canvas).unwrap();
        write_ppm(&ppm, &canvas).unwrap();
        assert_eq!(fs::read(&png).unwrap()[..8], PNG_SIGNATURE);
        let ppm_data = fs::read(&ppm).unwrap();
        assert!(ppm_data.starts_with(b"P6\n4 4\n255\n"));
        fs::remove_dir_all(dir).ok();
    }
}
