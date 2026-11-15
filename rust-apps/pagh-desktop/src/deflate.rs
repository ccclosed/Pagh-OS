//! Минимальный deflate-компрессор (fixed Huffman + LZ77) для PNG-энкодера.
//!
//! Внешних зависимостей нет. Сжатие нужно только для скриншотов/отладочных кадров;
//! формат — zlib (RFC 1950) поверх deflate fixed-Huffman (RFC 1951, BTYPE=01).
//! Скорость важнее степени сжатия: используется жадный LZ77 с хеш-цепочкой.

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

#[cfg(test)]
pub(crate) fn inflate_fixed(zlib: &[u8]) -> Vec<u8> {
    assert_eq!(zlib[0], 0x78);
    let mut reader = BitReader::new(&zlib[2..]);
    assert_eq!(reader.read_lsb(1), 1);
    assert_eq!(reader.read_lsb(2), 1);
    let mut out = Vec::new();
    loop {
        let symbol = read_symbol(&mut reader);
        match symbol {
            0..=255 => out.push(symbol as u8),
            256 => break,
            257..=285 => {
                let index = (symbol - 257) as usize;
                let extra = LENGTH_EXTRA[index];
                let length = LENGTH_BASE[index] as usize
                    + if extra > 0 {
                        reader.read_lsb(extra as usize) as usize
                    } else {
                        0
                    };
                let dist_symbol = read_symbol_distance(&mut reader);
                let extra = DIST_EXTRA[dist_symbol] as usize;
                let distance = DIST_BASE[dist_symbol] as usize
                    + if extra > 0 {
                        reader.read_lsb(extra) as usize
                    } else {
                        0
                    };
                let start = out
                    .len()
                    .checked_sub(distance)
                    .expect("distance за пределы");
                for i in 0..length {
                    let byte = out[start + i];
                    out.push(byte);
                }
            }
            other => panic!("недопустимый символ {other}"),
        }
    }
    out
}

#[cfg(test)]
fn read_symbol(reader: &mut BitReader<'_>) -> u16 {
    let prefix = reader.read_code(7);
    if prefix <= 23 {
        return 256 + prefix as u16;
    }
    if prefix <= 99 {
        let code = (prefix << 1) | reader.read_code(1);
        if code <= 0xBF {
            return (code as u16).wrapping_sub(0x30);
        }
        return 280 + (code as u16 - 0xC0);
    }
    let code = (prefix << 2) | reader.read_code(2);
    144 + (code as u16 - 0x190)
}

#[cfg(test)]
fn read_symbol_distance(reader: &mut BitReader<'_>) -> usize {
    reader.read_code(5) as usize
}

#[cfg(test)]
struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

#[cfg(test)]
impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn read_bit(&mut self) -> u32 {
        let byte = self.data[self.bit / 8];
        let bit = (byte >> (self.bit % 8)) & 1;
        self.bit += 1;
        bit as u32
    }

    /// Чтение кода Хаффмана: первый бит — старший (как в deflate).
    fn read_code(&mut self, count: usize) -> u32 {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | self.read_bit();
        }
        value
    }

    /// Чтение extra-битов: младший бит первым (как в deflate).
    fn read_lsb(&mut self, count: usize) -> u32 {
        let mut value = 0;
        for i in 0..count {
            value |= self.read_bit() << i;
        }
        value
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
        assert_eq!(inflate_fixed(&compressed), sample);
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
        assert_eq!(inflate_fixed(&compressed), data);
    }

    #[test]
    fn empty_and_single_byte() {
        assert_eq!(inflate_fixed(&zlib_compress(&[])), Vec::<u8>::new());
        assert_eq!(inflate_fixed(&zlib_compress(&[7])), vec![7]);
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
        assert_eq!(inflate_fixed(&compressed), data);
    }
}
