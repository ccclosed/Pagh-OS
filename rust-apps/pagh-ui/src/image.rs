//! Сериализация поверхности: PNG (собственный энкодер) и PPM.
//!
//! PNG пишется без внешних зависимостей, но с настоящим сжатием: deflate с
//! фиксированными кодами Хаффмана и жадным LZ77 (~150 строк), CRC32 и Adler32.
//! UI-скриншоты сжимаются в 10–30 раз, поэтому демо-PNG можно класть в
//! репозиторий.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use crate::canvas::Canvas;
use crate::color::Color;
use crate::geom::{Point, Size};

/// Непрозрачное RGBA-изображение.
#[derive(Clone)]
pub struct Image {
    width: i32,
    height: i32,
    pixels: Vec<u8>,
}

impl Image {
    /// Копия поверхности как есть (альфа сохраняется).
    pub fn from_canvas(canvas: &Canvas) -> Image {
        Image {
            width: canvas.width(),
            height: canvas.height(),
            pixels: canvas.pixels().to_vec(),
        }
    }

    /// Поверхность, «сплющенная» на непрозрачный фон (для скриншотов).
    pub fn flatten(canvas: &Canvas, background: Color) -> Image {
        let mut pixels = Vec::with_capacity((canvas.width() * canvas.height() * 4) as usize);
        for y in 0..canvas.height() {
            for x in 0..canvas.width() {
                let px = canvas.pixel(Point::new(x, y)).blend_over(background);
                pixels.extend_from_slice(&[px.r, px.g, px.b, 255]);
            }
        }
        Image {
            width: canvas.width(),
            height: canvas.height(),
            pixels,
        }
    }

    /// Изображение из сырых RGBA-байтов.
    pub fn from_rgba(width: i32, height: i32, pixels: Vec<u8>) -> Image {
        assert_eq!(
            pixels.len(),
            (width * height * 4) as usize,
            "неверный размер RGBA-буфера"
        );
        Image {
            width,
            height,
            pixels,
        }
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    pub fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }

    pub fn as_rgba(&self) -> &[u8] {
        &self.pixels
    }

    pub fn pixel(&self, p: Point) -> Color {
        if p.x < 0 || p.y < 0 || p.x >= self.width || p.y >= self.height {
            return Color::TRANSPARENT;
        }
        let idx = ((p.y * self.width + p.x) * 4) as usize;
        Color::rgba(
            self.pixels[idx],
            self.pixels[idx + 1],
            self.pixels[idx + 2],
            self.pixels[idx + 3],
        )
    }

    /// Детерминированный 64-битный хеш (FNV-1a по размеру и пикселям).
    ///
    /// Используется в screenshot-тестах: рендер чисто целочисленный, поэтому
    /// хеш одинаков на любой платформе.
    pub fn fnv1a(&self) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in (self.width as u32)
            .to_le_bytes()
            .iter()
            .chain((self.height as u32).to_le_bytes().iter())
            .chain(self.pixels.iter())
        {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    /// Записать PNG в файл.
    pub fn save_png(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        self.write_png(&mut writer)
    }

    /// Записать PNG в произвольный поток.
    pub fn write_png(&self, out: &mut dyn Write) -> io::Result<()> {
        out.write_all(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])?;

        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&(self.width as u32).to_be_bytes());
        ihdr.extend_from_slice(&(self.height as u32).to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        write_chunk(out, b"IHDR", &ihdr)?;

        let mut raw = Vec::with_capacity(((self.width * 4 + 1) * self.height) as usize);
        for y in 0..self.height {
            raw.push(0); // filter type 0 (None)
            let row = (y * self.width * 4) as usize;
            raw.extend_from_slice(&self.pixels[row..row + (self.width * 4) as usize]);
        }
        let zlib = zlib_compress(&raw);
        write_chunk(out, b"IDAT", &zlib)?;
        write_chunk(out, b"IEND", &[])?;
        out.flush()
    }

    /// Записать PPM (P6) — формат, который читают QEMU-скрипты и ImageMagick.
    pub fn save_ppm(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        write!(writer, "P6\n{} {}\n255\n", self.width, self.height)?;
        let mut rgb = Vec::with_capacity((self.width * self.height * 3) as usize);
        for index in (0..self.pixels.len()).step_by(4) {
            rgb.extend_from_slice(&self.pixels[index..index + 3]);
        }
        writer.write_all(&rgb)?;
        writer.flush()
    }

    /// Сохранить PNG, создав каталоги.
    pub fn save_png_creating_dirs(&self, path: impl AsRef<Path>) -> io::Result<()> {
        create_parent_dir(&path)?;
        self.save_png(path)
    }

    /// Сохранить PPM, создав каталоги.
    pub fn save_ppm_creating_dirs(&self, path: impl AsRef<Path>) -> io::Result<()> {
        create_parent_dir(&path)?;
        self.save_ppm(path)
    }
}

fn create_parent_dir(path: &impl AsRef<Path>) -> io::Result<()> {
    if let Some(parent) = path.as_ref().parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

fn write_chunk(out: &mut dyn Write, kind: &[u8; 4], data: &[u8]) -> io::Result<()> {
    out.write_all(&(data.len() as u32).to_be_bytes())?;
    out.write_all(kind)?;
    out.write_all(data)?;
    let mut crc = Crc32::new();
    crc.update(kind);
    crc.update(data);
    out.write_all(&crc.finish().to_be_bytes())
}

/// zlib-контейнер: deflate с фиксированными кодами Хаффмана + Adler32.
///
/// Кодировщик жадный: для каждой позиции ищет самое длинное совпадение в
/// скользящем окне 32 КиБ (одна запись хеш-таблицы на 3-байтовый ключ) и
/// пишет пару `длина/расстояние`, иначе — литерал. Этого достаточно для
/// скриншотов; полноценный lazy-matching и динамические коды не нужны.
fn zlib_compress(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() / 4 + 64);
    out.extend_from_slice(&[0x78, 0x01]);
    out.extend_from_slice(&deflate_fixed(raw));
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

/// Поток бит, упакованный в байты (младший бит — первый).
struct BitWriter {
    bytes: Vec<u8>,
    bit: u32,
    count: u32,
}

impl BitWriter {
    fn new() -> BitWriter {
        BitWriter {
            bytes: Vec::new(),
            bit: 0,
            count: 0,
        }
    }

    /// Записать `count` младших бит значения (LSB first).
    fn bits(&mut self, value: u32, count: u32) {
        for index in 0..count {
            self.bit |= ((value >> index) & 1) << self.count;
            self.count += 1;
            if self.count == 8 {
                self.bytes.push(self.bit as u8);
                self.bit = 0;
                self.count = 0;
            }
        }
    }

    /// Записать код Хаффмана: старший бит кода идёт первым.
    fn code(&mut self, code: u32, length: u32) {
        for index in (0..length).rev() {
            self.bits((code >> index) & 1, 1);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.count > 0 {
            self.bytes.push(self.bit as u8);
        }
        self.bytes
    }
}

/// Код Хаффмана для литерала/длины (RFC 1951, §3.2.6).
fn write_literal_code(bits: &mut BitWriter, symbol: u32) {
    match symbol {
        0..=143 => bits.code(0x30 + symbol, 8),
        144..=255 => bits.code(0x190 + symbol - 144, 9),
        256..=279 => bits.code(symbol - 256, 7),
        _ => bits.code(0xC0 + symbol - 280, 8),
    }
}

/// Таблицы длин: (базовое значение, число дополнительных бит).
const LENGTH_BASE: [(u32, u32); 29] = [
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 0),
    (11, 1),
    (13, 1),
    (15, 1),
    (17, 1),
    (19, 2),
    (23, 2),
    (27, 2),
    (31, 2),
    (35, 3),
    (43, 3),
    (51, 3),
    (59, 3),
    (67, 4),
    (83, 4),
    (99, 4),
    (115, 4),
    (131, 5),
    (163, 5),
    (195, 5),
    (227, 5),
    (258, 0),
];

/// Таблицы расстояний: (базовое значение, число дополнительных бит).
const DISTANCE_BASE: [(u32, u32); 30] = [
    (1, 0),
    (2, 0),
    (3, 0),
    (4, 0),
    (5, 1),
    (7, 1),
    (9, 2),
    (13, 2),
    (17, 3),
    (25, 3),
    (33, 4),
    (49, 4),
    (65, 5),
    (97, 5),
    (129, 6),
    (193, 6),
    (257, 7),
    (385, 7),
    (513, 8),
    (769, 8),
    (1025, 9),
    (1537, 9),
    (2049, 10),
    (3073, 10),
    (4097, 11),
    (6145, 11),
    (8193, 12),
    (12289, 12),
    (16385, 13),
    (24577, 13),
];

fn write_match(bits: &mut BitWriter, length: u32, distance: u32) {
    // Длина: код 257..285.
    for (index, (base, extra)) in LENGTH_BASE.iter().enumerate() {
        let next_base = LENGTH_BASE
            .get(index + 1)
            .map(|(next, _)| *next)
            .unwrap_or(259);
        if length >= *base && (length < next_base || index == LENGTH_BASE.len() - 1) {
            write_literal_code(bits, 257 + index as u32);
            if *extra > 0 {
                bits.bits(length - base, *extra);
            }
            break;
        }
    }
    // Расстояние: код 0..29.
    for (index, (base, extra)) in DISTANCE_BASE.iter().enumerate() {
        let next_base = DISTANCE_BASE
            .get(index + 1)
            .map(|(next, _)| *next)
            .unwrap_or(32769);
        if distance >= *base && distance < next_base {
            bits.code(index as u32, 5);
            if *extra > 0 {
                bits.bits(distance - base, *extra);
            }
            break;
        }
    }
}

/// Максимальное расстояние совпадения.
const MAX_DISTANCE: usize = 32768;

/// Один блок deflate с фиксированными кодами Хаффмана и жадным LZ77.
fn deflate_fixed(raw: &[u8]) -> Vec<u8> {
    use std::collections::HashMap;

    let mut bits = BitWriter::new();
    bits.bits(1, 1); // BFINAL
    bits.bits(1, 2); // BTYPE = 01 (фиксированные коды)

    // Хеш 3-байтовых последовательностей -> последняя позиция.
    let mut table: HashMap<u32, usize> = HashMap::new();
    let key_at = |position: usize| -> u32 {
        (raw[position] as u32)
            | ((raw[position + 1] as u32) << 8)
            | ((raw[position + 2] as u32) << 16)
    };

    let mut position = 0usize;
    while position < raw.len() {
        let mut best_length = 0usize;
        let mut best_distance = 0usize;
        if position + 3 <= raw.len() {
            let key = key_at(position);
            if let Some(&previous) = table.get(&key) {
                let distance = position - previous;
                if distance <= MAX_DISTANCE {
                    let limit = (raw.len() - position).min(258);
                    let mut length = 0usize;
                    while length < limit && raw[previous + length] == raw[position + length] {
                        length += 1;
                    }
                    if length >= 3 {
                        best_length = length;
                        best_distance = distance;
                    }
                }
            }
            // Текущая позиция становится кандидатом для будущих совпадений.
            table.insert(key, position);
        }
        if best_length >= 3 {
            write_match(&mut bits, best_length as u32, best_distance as u32);
            // Обновляем таблицу внутри совпадения, чтобы не терять контекст.
            let end = position + best_length;
            for offset in 1..best_length {
                let candidate = position + offset;
                if candidate + 3 <= raw.len() && candidate < end {
                    table.insert(key_at(candidate), candidate);
                }
            }
            position = end;
        } else {
            write_literal_code(&mut bits, raw[position] as u32);
            position += 1;
        }
    }
    write_literal_code(&mut bits, 256); // конец блока
    bits.finish()
}

/// CRC32 (IEEE 802.3), используемый чанками PNG.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(data);
    crc.finish()
}

/// Потоковый CRC32.
pub struct Crc32 {
    value: u32,
}

impl Crc32 {
    pub fn new() -> Crc32 {
        Crc32 { value: 0xFFFF_FFFF }
    }

    pub fn update(&mut self, data: &[u8]) {
        for byte in data {
            let index = ((self.value ^ *byte as u32) & 0xFF) as usize;
            self.value = CRC_TABLE[index] ^ (self.value >> 8);
        }
    }

    pub fn finish(&self) -> u32 {
        self.value ^ 0xFFFF_FFFF
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Crc32::new()
    }
}

/// Adler32 из zlib.
pub fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for byte in data {
        a = (a + *byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
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

static CRC_TABLE: [u32; 256] = crc_table();

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_png(bytes: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
        assert_eq!(
            &bytes[0..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );
        let mut chunks = Vec::new();
        let mut pos = 8;
        while pos < bytes.len() {
            let len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
            let kind = &bytes[pos + 4..pos + 8];
            let data = bytes[pos + 8..pos + 8 + len].to_vec();
            let crc = u32::from_be_bytes(bytes[pos + 8 + len..pos + 12 + len].try_into().unwrap());
            assert_eq!(crc, crc32(&bytes[pos + 4..pos + 8 + len]), "CRC чанка");
            let name = std::str::from_utf8(kind).unwrap().to_string();
            let name: &'static str = Box::leak(name.into_boxed_str());
            chunks.push((name, data));
            pos += 12 + len;
        }
        chunks
    }

    /// Минимальный inflate для тестов: stored и fixed-Huffman блоки.
    fn inflate(zlib: &[u8]) -> Vec<u8> {
        assert_eq!(&zlib[..2], &[0x78, 0x01], "zlib header");
        assert_eq!(
            u32::from_be_bytes(zlib[zlib.len() - 4..].try_into().unwrap()),
            adler32(&inflate_raw(&zlib[2..zlib.len() - 4])),
            "adler32"
        );
        inflate_raw(&zlib[2..zlib.len() - 4])
    }

    fn inflate_raw(deflate: &[u8]) -> Vec<u8> {
        struct Bits<'a> {
            data: &'a [u8],
            pos: usize,
        }
        impl Bits<'_> {
            fn bit(&mut self) -> u32 {
                let byte = self.pos >> 3;
                let bit = (self.data[byte] >> (self.pos & 7)) & 1;
                self.pos += 1;
                bit as u32
            }
            fn bits(&mut self, count: u32) -> u32 {
                let mut value = 0;
                for index in 0..count {
                    value |= self.bit() << index;
                }
                value
            }
            fn align(&mut self) {
                self.pos = (self.pos + 7) & !7;
            }
        }
        // Таблицы фиксированных кодов: (длина, значение, символ).
        let mut literals: Vec<(u32, u32, u32)> = Vec::new();
        for symbol in 0..=143u32 {
            literals.push((8, 0x30 + symbol, symbol));
        }
        for symbol in 144..=255u32 {
            literals.push((9, 0x190 + symbol - 144, symbol));
        }
        for symbol in 256..=279u32 {
            literals.push((7, symbol - 256, symbol));
        }
        for symbol in 280..=287u32 {
            literals.push((8, 0xC0 + symbol - 280, symbol));
        }
        let lengths = super::LENGTH_BASE;
        let distances = super::DISTANCE_BASE;
        let mut bits = Bits {
            data: deflate,
            pos: 0,
        };
        let mut out: Vec<u8> = Vec::new();
        loop {
            let final_block = bits.bit() == 1;
            let block_type = bits.bits(2);
            match block_type {
                0 => {
                    bits.align();
                    let len = bits.bits(16);
                    let _nlen = bits.bits(16);
                    for _ in 0..len {
                        out.push(bits.bits(8) as u8);
                    }
                }
                1 => loop {
                    // Декодирование кодов Хаффмана: старший бит идёт первым.
                    let mut code = 0u32;
                    let mut symbol = None;
                    for length in 1..=9u32 {
                        code = (code << 1) | bits.bit();
                        if let Some((_, _, value)) = literals
                            .iter()
                            .find(|(bits_len, value, _)| *bits_len == length && *value == code)
                        {
                            symbol = Some(*value);
                            break;
                        }
                    }
                    let symbol = symbol.expect("известный код Хаффмана");
                    match symbol {
                        0..=255 => out.push(symbol as u8),
                        256 => break,
                        _ => {
                            let index = (symbol - 257) as usize;
                            let (base, extra) = lengths[index];
                            let length = base + bits.bits(extra);
                            let mut distance_code = 0u32;
                            for _ in 0..5 {
                                distance_code = (distance_code << 1) | bits.bit();
                            }
                            assert!(distance_code < 30, "недопустимый код расстояния");
                            let (distance_base, distance_extra) = distances[distance_code as usize];
                            let distance = distance_base + bits.bits(distance_extra);
                            for _ in 0..length {
                                let byte = out[out.len() - distance as usize];
                                out.push(byte);
                            }
                        }
                    }
                },
                other => panic!("неподдерживаемый тип блока deflate: {other}"),
            }
            if final_block {
                break;
            }
        }
        out
    }

    #[test]
    fn inflate_decodes_reference_zlib_stream() {
        // Поток сгенерирован zlib.compressobj(..., Z_FIXED) для b"abcabcabcabcabcabc".
        let reference = [
            0x78u8, 0x01, 0x4b, 0x4c, 0x4a, 0x4e, 0x44, 0x45, 0x00, 0x41, 0x7c, 0x06, 0xe5,
        ];
        assert_eq!(inflate(&reference), b"abcabcabcabcabcabc".to_vec());
    }

    #[test]
    fn deflate_roundtrips_through_inflate() {
        let samples: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"a".to_vec(),
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_vec(),
            b"hello hello hello hello hello".to_vec(),
            (0..5000u32).map(|index| (index / 37) as u8).collect(),
            (0..2000u32)
                .map(|index| {
                    let mut seed = index.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    (seed >> 16) as u8
                })
                .collect(),
        ];
        for sample in samples {
            let compressed = zlib_compress(&sample);
            assert_eq!(
                inflate(&compressed),
                sample,
                "roundtrip {} байт",
                sample.len()
            );
        }
    }

    #[test]
    fn png_roundtrip_structure() {
        let mut canvas = Canvas::new(32, 16);
        canvas.fill(crate::geom::Rect::new(4, 4, 8, 8), Color::rgb(1, 2, 3));
        let image = Image::flatten(&canvas, Color::BLACK);
        let mut bytes = Vec::new();
        image.write_png(&mut bytes).unwrap();
        let chunks = parse_png(&bytes);
        assert_eq!(chunks[0].0, "IHDR");
        assert_eq!(chunks.last().unwrap().0, "IEND");
        assert_eq!(chunks[0].1[0..4], 32u32.to_be_bytes());
        assert_eq!(chunks[0].1[4..8], 16u32.to_be_bytes());
        // Раскодированный IDAT должен совпасть со сканами без фильтра.
        let raw = inflate(&chunks[1].1);
        assert_eq!(raw.len(), ((32 * 4 + 1) * 16) as usize);
        for y in 0..16usize {
            assert_eq!(raw[y * (32 * 4 + 1)], 0, "фильтр строки {y}");
        }
        let row = (4 * (32 * 4 + 1) + 1 + 4 * 4) as usize;
        assert_eq!(&raw[row..row + 3], &[1, 2, 3]);
    }

    #[test]
    fn png_large_image_is_compressed_and_decodes() {
        // Большое изображение: deflate должен сжимать плоские области.
        let canvas = Canvas::filled(300, 300, Color::rgb(0, 128, 255));
        let image = Image::flatten(&canvas, Color::BLACK);
        let mut bytes = Vec::new();
        image.write_png(&mut bytes).unwrap();
        let chunks = parse_png(&bytes);
        let idat = &chunks[1].1;
        let raw_len = (300 * 4 + 1) * 300;
        assert!(
            idat.len() < raw_len / 10,
            "сжатие: {} из {}",
            idat.len(),
            raw_len
        );
        let raw = inflate(idat);
        assert_eq!(raw.len(), raw_len);
        assert_eq!(&raw[1..4], &[0, 128, 255]);
    }

    #[test]
    fn ppm_header_and_size() {
        let canvas = Canvas::filled(3, 2, Color::rgb(10, 20, 30));
        let image = Image::flatten(&canvas, Color::BLACK);
        let dir = std::env::temp_dir().join("pagh-ui-ppm-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.ppm");
        image.save_ppm(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..11], b"P6\n3 2\n255\n");
        assert_eq!(bytes.len(), 11 + 3 * 2 * 3);
        assert_eq!(&bytes[11..14], &[10, 20, 30]);
    }

    #[test]
    fn checksum_depends_on_pixels() {
        let a = Image::flatten(&Canvas::filled(4, 4, Color::BLACK), Color::BLACK);
        let b = Image::flatten(&Canvas::filled(4, 4, Color::rgb(1, 0, 0)), Color::BLACK);
        assert_ne!(a.fnv1a(), b.fnv1a());
        assert_eq!(a.fnv1a(), a.clone().fnv1a());
    }

    #[test]
    fn flatten_makes_opaque() {
        let mut canvas = Canvas::new(2, 2);
        canvas.fill(
            crate::geom::Rect::new(0, 0, 1, 1),
            Color::rgba(255, 0, 0, 128),
        );
        let image = Image::flatten(&canvas, Color::BLACK);
        assert_eq!(image.pixel(Point::new(1, 1)).a, 255);
        assert_eq!(image.pixel(Point::new(0, 0)).a, 255);
        assert!(image.pixel(Point::new(0, 0)).r > 100);
    }
}
