//! Минимальный декодер deflate/zlib для готовых PNG-иконок.
//!
//! Внешних зависимостей нет. Декодеру нужны все три формы блоков (stored,
//! fixed и dynamic Хаффман): PNG-ассеты сжаты обычными инструментами, которые
//! почти всегда выбирают dynamic Huffman (BTYPE=10). Логика повторяет
//! `rust-apps/pagh-desktop/src/deflate.rs` (та же MIT-лицензия проекта) в
//! минимальном объёме — только разжатие; компрессор здесь не нужен.

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

/// Разжать zlib-поток (RFC 1950/1951): stored, fixed и dynamic Хаффман.
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
