//! Минимальный deflate-компрессор и совместимый декодер для PNG.
//!
//! Внешних зависимостей нет. Сжатие нужно для скриншотов/отладочных кадров;
//! формат — zlib (RFC 1950) поверх deflate (RFC 1951). Энкодер пишет только
//! fixed-Huffman (BTYPE=01, скорость важнее степени сжатия), декодер понимает
//! все три формы (stored/fixed/dynamic), потому что готовые PNG-иконки приходят
//! сжатыми сторонними инструментами.

const WINDOW: usize = 32_768;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const HASH_BITS: usize = 15;
const HASH_SIZE: usize = 1 << HASH_BITS;
const MAX_CHAIN: usize = 96;

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12_289, 16_385, 24_577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Сжать данные в zlib-поток.
pub fn zlib_compress(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    out.push(0x78); // CM=deflate, CINFO=32K
    out.push(0x01); // FCHECK, без словаря
    out.extend_from_slice(&deflate_fixed(data));
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn deflate_fixed(data: &[u8]) -> Vec<u8> {
    let mut writer = BitWriter::new();
    writer.write_bits(1, 1); // BFINAL = 1
    writer.write_bits(1, 2); // BTYPE = 01 (fixed Huffman)

    let mut head = vec![usize::MAX; HASH_SIZE];
    let mut prev = vec![usize::MAX; data.len()];
    let mut pos = 0usize;

    while pos < data.len() {
        let mut best_len = 0usize;
        let mut best_dist = 0usize;
        if pos + MIN_MATCH <= data.len() {
            let hash = hash3(&data[pos..pos + 3]);
            let mut candidate = head[hash];
            let mut chain = 0usize;
            let limit = pos.saturating_sub(WINDOW);
            while candidate != usize::MAX && candidate >= limit && chain < MAX_CHAIN {
                let max_len = MAX_MATCH.min(data.len() - pos);
                let mut len = 0usize;
                while len < max_len && data[candidate + len] == data[pos + len] {
                    len += 1;
                }
                if len > best_len {
                    best_len = len;
                    best_dist = pos - candidate;
                    if len == max_len {
                        break;
                    }
                }
                candidate = prev[candidate];
                chain += 1;
            }
            prev[pos] = head[hash];
            head[hash] = pos;
        }

        if best_len >= MIN_MATCH {
            emit_length(&mut writer, best_len);
            emit_distance(&mut writer, best_dist);
            // Позиции внутри совпадения тоже добавляем в хеш-таблицу, чтобы
            // находить дальнейшие повторения (жадный, но не совсем слепой LZ).
            let end = (pos + best_len).min(data.len());
            let mut p = pos + 1;
            while p + MIN_MATCH <= end {
                let hash = hash3(&data[p..p + 3]);
                prev[p] = head[hash];
                head[hash] = p;
                p += 1;
            }
            pos = end;
        } else {
            emit_literal(&mut writer, data[pos] as u16);
            pos += 1;
        }
    }
    emit_literal(&mut writer, 256); // end of block
    writer.align();
    writer.finish()
}

fn hash3(bytes: &[u8]) -> usize {
    (((bytes[0] as usize) << 10) ^ ((bytes[1] as usize) << 5) ^ (bytes[2] as usize))
        & (HASH_SIZE - 1)
}

fn emit_literal(writer: &mut BitWriter, symbol: u16) {
    let (code, bits) = match symbol {
        0..=143 => (0x30 + symbol as u32, 8),
        144..=255 => (0x190 + (symbol as u32 - 144), 9),
        256..=279 => (symbol as u32 - 256, 7),
        _ => (0xC0 + (symbol as u32 - 280), 8),
    };
    writer.write_huffman(code, bits);
}

fn emit_length(writer: &mut BitWriter, length: usize) {
    let index = LENGTH_BASE
        .iter()
        .enumerate()
        .rev()
        .find(|(_, base)| **base as usize <= length)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let symbol = 257 + index as u16;
    emit_literal(writer, symbol);
    let extra = LENGTH_EXTRA[index];
    if extra > 0 {
        let base = LENGTH_BASE[index] as usize;
        writer.write_bits((length - base) as u32, extra);
    }
}

fn emit_distance(writer: &mut BitWriter, distance: usize) {
    let index = DIST_BASE
        .iter()
        .enumerate()
        .rev()
        .find(|(_, base)| **base as usize <= distance)
        .map(|(i, _)| i)
        .unwrap_or(0);
    writer.write_huffman(index as u32, 5);
    let extra = DIST_EXTRA[index];
    if extra > 0 {
        let base = DIST_BASE[index] as usize;
        writer.write_bits((distance - base) as u32, extra);
    }
}

/// Бит-райтер в порядке deflate: биты упаковываются от младшего к старшему,
/// а коды Хаффмана записываются старшим битом вперёд (поэтому reversal).
struct BitWriter {
    out: Vec<u8>,
    buffer: u64,
    bits: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            buffer: 0,
            bits: 0,
        }
    }

    fn write_bits(&mut self, value: u32, count: u8) {
        debug_assert!(count <= 32);
        if count == 0 {
            return;
        }
        let masked = if count == 32 {
            value as u64
        } else {
            (value as u64) & ((1u64 << count) - 1)
        };
        self.buffer |= masked << self.bits;
        self.bits += count;
        while self.bits >= 8 {
            self.out.push((self.buffer & 0xFF) as u8);
            self.buffer >>= 8;
            self.bits -= 8;
        }
    }

    fn write_huffman(&mut self, code: u32, bits: u8) {
        self.write_bits(reverse_bits(code, bits), bits);
    }

    fn align(&mut self) {
        if self.bits > 0 {
            self.out.push((self.buffer & 0xFF) as u8);
            self.buffer = 0;
            self.bits = 0;
        }
    }

    fn finish(self) -> Vec<u8> {
        debug_assert_eq!(self.bits, 0);
        self.out
    }
}

fn reverse_bits(value: u32, bits: u8) -> u32 {
    let mut result = 0;
    for i in 0..bits {
        result = (result << 1) | ((value >> i) & 1);
    }
    result
}

fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for byte in data {
        a = (a + *byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

/// Разжать zlib-поток (RFC 1950/1951): stored, fixed и dynamic Хаффман.
///
/// Декодер нужен готовым PNG-иконкам: ассеты сжаты обычными инструментами,
/// которые почти всегда выбирают dynamic Huffman (BTYPE=10). Компрессор выше
/// пишет только fixed, поэтому энкодеру этот путь не нужен, а декодеру — нужен.
///
/// `None` — поток повреждён или использует невыразимую форму; вызывающий
/// (PNG-декодер) в этом случае просто не рисует иконку.
pub(crate) fn inflate_zlib(zlib: &[u8]) -> Option<Vec<u8>> {
    if zlib.len() < 6 {
        return None;
    }
    // CMF/FLG: метод deflate, окно не больше 32K, FCHECK сходится, без словаря.
    let (cmf, flg) = (zlib[0], zlib[1]);
    if cmf & 0x0F != 8 || (u16::from(cmf) * 256 + u16::from(flg)) % 31 != 0 || flg & 0x20 != 0 {
        return None;
    }
    // Хвост zlib — Adler32 (4 байта); декодеру он не нужен.
    let mut reader = BitReader::new(&zlib[2..zlib.len() - 4]);
    let mut out = Vec::new();
    loop {
        let last = reader.bits(1)?;
        match reader.bits(2)? {
            0 => stored_block(&mut reader, &mut out)?,
            1 => {
                let (literals, distances) = fixed_tables();
                inflate_block(&mut reader, &mut out, &literals, &distances)?;
            }
            2 => {
                let (literals, distances) = dynamic_tables(&mut reader)?;
                inflate_block(&mut reader, &mut out, &literals, &distances)?;
            }
            _ => return None,
        }
        if last == 1 {
            return Some(out);
        }
    }
}

/// Блок без сжатия: выровнять поток, прочитать LEN/NLEN и копировать байты.
fn stored_block(reader: &mut BitReader<'_>, out: &mut Vec<u8>) -> Option<()> {
    reader.align();
    let len = reader.byte()? as usize | (reader.byte()? as usize) << 8;
    let nlen = reader.byte()? as usize | (reader.byte()? as usize) << 8;
    if len ^ 0xFFFF != nlen {
        return None;
    }
    for _ in 0..len {
        out.push(reader.byte()?);
    }
    Some(())
}

/// Таблицы fixed-Хаффмана (RFC 1951 §3.2.6).
fn fixed_tables() -> (Huffman, Huffman) {
    let mut literals = [0u8; 288];
    literals[0..144].fill(8);
    literals[144..256].fill(9);
    literals[256..280].fill(7);
    literals[280..288].fill(8);
    let distances = [5u8; 30];
    (
        Huffman::from_lengths(&literals).expect("fixed-таблица литералов"),
        Huffman::from_lengths(&distances).expect("fixed-таблица расстояний"),
    )
}

/// Таблицы dynamic-блока: длины кодов переданы в начале блока.
fn dynamic_tables(reader: &mut BitReader<'_>) -> Option<(Huffman, Huffman)> {
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let hlit = reader.bits(5)? as usize + 257;
    let hdist = reader.bits(5)? as usize + 1;
    let hclen = reader.bits(4)? as usize + 4;
    if hlit > 286 || hdist > 30 {
        return None;
    }
    let mut code_lengths = [0u8; 19];
    for &slot in ORDER.iter().take(hclen) {
        code_lengths[slot] = reader.bits(3)? as u8;
    }
    let code_length_table = Huffman::from_lengths(&code_lengths)?;

    let mut lengths = Vec::with_capacity(hlit + hdist);
    while lengths.len() < hlit + hdist {
        match code_length_table.decode(reader)? {
            symbol @ 0..=15 => lengths.push(symbol as u8),
            16 => {
                let previous = *lengths.last()?;
                for _ in 0..3 + reader.bits(2)? {
                    lengths.push(previous);
                }
            }
            17 => lengths.extend(std::iter::repeat(0).take(3 + reader.bits(3)? as usize)),
            18 => lengths.extend(std::iter::repeat(0).take(11 + reader.bits(7)? as usize)),
            _ => return None,
        }
        if lengths.len() > hlit + hdist {
            return None;
        }
    }
    let literals = Huffman::from_lengths(&lengths[..hlit])?;
    let distances = Huffman::from_lengths(&lengths[hlit..])?;
    Some((literals, distances))
}

/// Раскодировать один Хаффман-блок до символа конца блока (256).
fn inflate_block(
    reader: &mut BitReader<'_>,
    out: &mut Vec<u8>,
    literals: &Huffman,
    distances: &Huffman,
) -> Option<()> {
    loop {
        match literals.decode(reader)? {
            symbol @ 0..=255 => out.push(symbol as u8),
            256 => return Some(()),
            symbol @ 257..=285 => {
                let index = (symbol - 257) as usize;
                let length = LENGTH_BASE[index] as usize
                    + if LENGTH_EXTRA[index] > 0 {
                        reader.bits(u32::from(LENGTH_EXTRA[index]))? as usize
                    } else {
                        0
                    };
                let distance_symbol = distances.decode(reader)? as usize;
                if distance_symbol >= 30 {
                    return None;
                }
                let distance = DIST_BASE[distance_symbol] as usize
                    + if DIST_EXTRA[distance_symbol] > 0 {
                        reader.bits(u32::from(DIST_EXTRA[distance_symbol]))? as usize
                    } else {
                        0
                    };
                if distance == 0 || distance > out.len() {
                    return None;
                }
                let start = out.len() - distance;
                // Копирование по одному байту корректно перекрывает расстояния
                // меньше длины серии (LZ77 это разрешает).
                for offset in 0..length {
                    out.push(out[start + offset]);
                }
            }
            _ => return None,
        }
    }
}

/// Читатель битов deflate: младший бит байта — первый.
struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn bits(&mut self, count: u32) -> Option<u32> {
        let mut value = 0u32;
        for index in 0..count {
            let byte = *self.data.get(self.bit / 8)?;
            value |= u32::from((byte >> (self.bit % 8)) & 1) << index;
            self.bit += 1;
        }
        Some(value)
    }

    fn align(&mut self) {
        self.bit = (self.bit + 7) & !7;
    }

    fn byte(&mut self) -> Option<u8> {
        let byte = *self.data.get(self.bit / 8)?;
        self.bit += 8;
        Some(byte)
    }
}

/// Каноническая таблица Хаффмана: счётчики длин + символы по порядку кодов.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn from_lengths(lengths: &[u8]) -> Option<Self> {
        let mut counts = [0u16; 16];
        for &length in lengths {
            if length as usize >= counts.len() {
                return None;
            }
            if length > 0 {
                counts[length as usize] += 1;
            }
        }
        // Переполненный набор кодов не может быть префиксным.
        let mut left: i32 = 1;
        for length in 1..=15 {
            left <<= 1;
            left -= i32::from(counts[length]);
            if left < 0 {
                return None;
            }
        }
        let mut offsets = [0u16; 16];
        let mut total = 0u16;
        for length in 1..=15 {
            offsets[length] = total;
            total += counts[length];
        }
        let mut symbols = vec![0u16; total as usize];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length > 0 {
                let slot = offsets[length as usize] as usize;
                symbols[slot] = symbol as u16;
                offsets[length as usize] += 1;
            }
        }
        Some(Self { counts, symbols })
    }

    /// Каноническое декодирование (puff-стиль): по одному биту до совпадения.
    fn decode(&self, reader: &mut BitReader<'_>) -> Option<u16> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for length in 1..=15 {
            code |= reader.bits(1)? as i32;
            let count = i32::from(self.counts[length]);
            if code - first < count {
                return self.symbols.get((index + code - first) as usize).copied();
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compress_and_decompress_roundtrip() {
        let mut sample = Vec::new();
        for i in 0..5000 {
            sample.push((i % 17) as u8);
        }
        sample.extend_from_slice(b"hello hello hello hello pagh-desktop");
        let compressed = zlib_compress(&sample);
        assert!(
            compressed.len() < sample.len(),
            "{} >= {}",
            compressed.len(),
            sample.len()
        );
        assert_eq!(inflate_zlib(&compressed).expect("inflate"), sample);
    }

    #[test]
    fn compress_repetitive_data_small() {
        let data = vec![0xAB; 100_000];
        let compressed = zlib_compress(&data);
        assert!(
            compressed.len() < 2000,
            "RLE/LZ должен сжать серию, получено {}",
            compressed.len()
        );
        assert_eq!(inflate_zlib(&compressed).expect("inflate"), data);
    }

    #[test]
    fn empty_and_single_byte() {
        assert_eq!(
            inflate_zlib(&zlib_compress(&[])).expect("inflate"),
            Vec::<u8>::new()
        );
        assert_eq!(
            inflate_zlib(&zlib_compress(&[7])).expect("inflate"),
            vec![7]
        );
    }

    #[test]
    fn random_data_roundtrip() {
        let mut state = 0x1234_5678u32;
        let mut data = Vec::new();
        for _ in 0..20_000 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            data.push((state >> 24) as u8);
        }
        let compressed = zlib_compress(&data);
        assert_eq!(inflate_zlib(&compressed).expect("inflate"), data);
    }
}
