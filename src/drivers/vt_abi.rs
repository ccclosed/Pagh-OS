//! Чистый ABI разделяемой VT-поверхности (`/dev/vtconsole0`).
//!
//! Контракт — `rust-apps/pagh-terminal/TERMINAL.md`, раздел «Kernel console
//! shared surface ABI v1». Модуль `core`-only: host-tests включают его через
//! `#[path]` и проверяют ровно те байты/смещения, которые публикует ядро, не
//! поднимая QEMU. Раскладка заголовка и клетки закреплена здесь, а не в
//! `vtsurface.rs`: расхождение writer'а с валидатором клиента должно падать на
//! хосте.
//!
//! Заголовок — 128 байт, клетка — 16 байт, little-endian. Заголовок несёт
//! seqlock `seq` (нечётный = идёт запись) и `write_seq` (счётчик завершённых
//! публикаций); клетки начинаются сразу за заголовком.
#![allow(dead_code)]

/// `magic` заголовка: ASCII `"PGVT"` в little-endian.
pub const MAGIC: u32 = 0x5456_4750;
/// Версия ABI (major).
pub const VERSION: u16 = 1;
/// Длина заголовка.
pub const HEADER_LEN: usize = 128;
/// Размер одной клетки.
pub const CELL_SIZE: usize = 16;
/// Смещение массива клеток.
pub const CELLS_OFFSET: u32 = HEADER_LEN as u32;
/// Ширина глиф-клетки текущего kernel-шрифта.
pub const CELL_WIDTH: u16 = 8;
/// Высота глиф-клетки текущего kernel-шрифта.
pub const CELL_HEIGHT: u16 = 18;
/// Пределы сетки из спецификации: 1000×1000.
pub const MAX_COLS: u16 = 1000;
pub const MAX_ROWS: u16 = 1000;
/// Жёсткий предел `total_bytes` (64 МиБ) — защита от 1000×1000×16+128 всё равно
/// меньше, но проверка живёт рядом с арифметикой.
pub const MAX_TOTAL_BYTES: u32 = 64 * 1024 * 1024;

/// `flags`: codepoint'ы — Unicode (этот writer публикует Latin-1/ASCII).
pub const FLAG_UNICODE: u32 = 1 << 0;
/// `flags`: цвета — truecolor `0x00RRGGBB`.
pub const FLAG_TRUECOLOR: u32 = 1 << 1;
/// `flags`: поля курсора валидны.
pub const FLAG_CURSOR_VALID: u32 = 1 << 2;

/// `attrs` клетки (биты из ABI).
pub const ATTR_BOLD: u16 = 1;
pub const ATTR_DIM: u16 = 2;
pub const ATTR_ITALIC: u16 = 4;
pub const ATTR_UNDERLINE: u16 = 8;
pub const ATTR_BLINK: u16 = 16;
pub const ATTR_REVERSE: u16 = 32;
pub const ATTR_HIDDEN: u16 = 64;
pub const ATTR_STRIKE: u16 = 128;
pub const ATTR_OVERLINE: u16 = 256;

/// `flags` клетки: продолжение wide-символа.
pub const CELL_FLAG_WIDE_CONT: u8 = 1 << 0;

/// `PGVT_IOC_INFO` (`_IOR('V', 0x01, struct pgvt_info)`, 32 байта).
pub const PGVT_IOC_INFO: u64 = 0x8020_5601;

/// Ошибка разбора/валидации байтов поверхности.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VtAbiError {
    /// Буфер короче заголовка/клеток.
    Truncated,
    /// Не тот `magic`.
    BadMagic,
    /// Неизвестная major-версия.
    UnsupportedVersion,
    /// `header_len` не 128.
    BadHeaderLen,
    /// Нулевая/слишком большая сетка.
    BadGeometry,
    /// `cell_size` не 16.
    BadCellSize,
    /// `cells_offset`/`cells_bytes`/`total_bytes` не сходятся.
    BadLayout,
    /// `seq` нечётный — запись в процессе.
    OddSequence,
    /// Курсор/scroll region вне сетки.
    BadCursor,
}

/// `struct pgvt_info` ответа ioctl.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PgvtInfo {
    pub abi_version: u32,
    pub total_bytes: u32,
    pub cols: u32,
    pub rows: u32,
    pub cell_size: u32,
    pub header_len: u32,
    pub reserved: [u32; 2],
}

/// Поля заголовка, которые реально читает/пишет v1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub seq: u32,
    pub flags: u32,
    pub cols: u16,
    pub rows: u16,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub cursor_visible: u8,
    pub cursor_style: u8,
    pub scroll_top: u16,
    pub scroll_bot: u16,
    pub cell_width: u16,
    pub cell_height: u16,
    pub cell_size: u16,
    pub attrs_flags: u16,
    pub reserved0: u16,
    pub cells_offset: u32,
    pub cells_bytes: u32,
    pub palette_offset: u32,
    pub palette_bytes: u32,
    pub total_bytes: u32,
    pub write_seq: u32,
}

impl Header {
    /// Заголовок для сетки `cols × rows` в начальном (чётном) состоянии.
    pub fn new(cols: u16, rows: u16) -> Option<Self> {
        let cells_bytes = cells_bytes(cols, rows)?;
        Some(Header {
            seq: 0,
            flags: FLAG_UNICODE | FLAG_TRUECOLOR | FLAG_CURSOR_VALID,
            cols,
            rows,
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: 1,
            cursor_style: 0,
            scroll_top: 0,
            scroll_bot: rows.saturating_sub(1),
            cell_width: CELL_WIDTH,
            cell_height: CELL_HEIGHT,
            cell_size: CELL_SIZE as u16,
            attrs_flags: 0,
            reserved0: 0,
            cells_offset: CELLS_OFFSET,
            cells_bytes,
            palette_offset: 0,
            palette_bytes: 0,
            total_bytes: CELLS_OFFSET + cells_bytes,
            write_seq: 0,
        })
    }
}

/// `cols * rows * 16` с проверками границ.
pub fn cells_bytes(cols: u16, rows: u16) -> Option<u32> {
    if cols == 0 || rows == 0 || cols > MAX_COLS || rows > MAX_ROWS {
        return None;
    }
    let cells = (cols as u64) * (rows as u64) * (CELL_SIZE as u64);
    let total = CELLS_OFFSET as u64 + cells;
    if total > MAX_TOTAL_BYTES as u64 {
        return None;
    }
    Some(cells as u32)
}

/// `total_bytes` для сетки.
pub fn total_bytes(cols: u16, rows: u16) -> Option<u32> {
    cells_bytes(cols, rows).map(|c| CELLS_OFFSET + c)
}

/// Одна клетка в проводе.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    pub codepoint: u32,
    pub fg: u32,
    pub bg: u32,
    pub attrs: u16,
    pub width: u8,
    pub flags: u8,
}

impl Cell {
    /// ASCII/Latin-1 байт kernel-VT → клетка поверхности.
    pub fn from_byte(ch: u8, fg: u32, bg: u32) -> Self {
        Cell {
            // 0 — «пробел» по ABI; kernel VT хранит настоящий пробел (0x20).
            codepoint: if ch == 0 { b' ' as u32 } else { ch as u32 },
            fg: fg & 0x00FF_FFFF,
            bg: bg & 0x00FF_FFFF,
            attrs: 0,
            width: 1,
            flags: 0,
        }
    }
}

fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn get_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}
fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

/// Закодировать заголовок в 128 байт.
pub fn encode_header(h: &Header) -> [u8; HEADER_LEN] {
    let mut b = [0u8; HEADER_LEN];
    put_u32(&mut b, 0, MAGIC);
    put_u16(&mut b, 4, VERSION);
    put_u16(&mut b, 6, HEADER_LEN as u16);
    put_u32(&mut b, 8, h.seq);
    put_u32(&mut b, 12, h.flags);
    put_u16(&mut b, 16, h.cols);
    put_u16(&mut b, 18, h.rows);
    put_u16(&mut b, 20, h.cursor_x);
    put_u16(&mut b, 22, h.cursor_y);
    b[24] = h.cursor_visible;
    b[25] = h.cursor_style;
    put_u16(&mut b, 26, h.scroll_top);
    put_u16(&mut b, 28, h.scroll_bot);
    put_u16(&mut b, 30, h.cell_width);
    put_u16(&mut b, 32, h.cell_height);
    put_u16(&mut b, 34, h.cell_size);
    put_u16(&mut b, 36, h.attrs_flags);
    put_u16(&mut b, 38, h.reserved0);
    put_u32(&mut b, 40, h.cells_offset);
    put_u32(&mut b, 44, h.cells_bytes);
    put_u32(&mut b, 48, h.palette_offset);
    put_u32(&mut b, 52, h.palette_bytes);
    put_u32(&mut b, 56, h.total_bytes);
    put_u32(&mut b, 60, h.write_seq);
    // 64..128 reserved — нули.
    b
}

/// Разобрать и провалидировать заголовок.
pub fn parse_header(bytes: &[u8]) -> Result<Header, VtAbiError> {
    if bytes.len() < HEADER_LEN {
        return Err(VtAbiError::Truncated);
    }
    if get_u32(bytes, 0) != MAGIC {
        return Err(VtAbiError::BadMagic);
    }
    if get_u16(bytes, 4) != VERSION {
        return Err(VtAbiError::UnsupportedVersion);
    }
    if get_u16(bytes, 6) as usize != HEADER_LEN {
        return Err(VtAbiError::BadHeaderLen);
    }
    let seq = get_u32(bytes, 8);
    if seq & 1 != 0 {
        return Err(VtAbiError::OddSequence);
    }
    let h = Header {
        seq,
        flags: get_u32(bytes, 12),
        cols: get_u16(bytes, 16),
        rows: get_u16(bytes, 18),
        cursor_x: get_u16(bytes, 20),
        cursor_y: get_u16(bytes, 22),
        cursor_visible: bytes[24],
        cursor_style: bytes[25],
        scroll_top: get_u16(bytes, 26),
        scroll_bot: get_u16(bytes, 28),
        cell_width: get_u16(bytes, 30),
        cell_height: get_u16(bytes, 32),
        cell_size: get_u16(bytes, 34),
        attrs_flags: get_u16(bytes, 36),
        reserved0: get_u16(bytes, 38),
        cells_offset: get_u32(bytes, 40),
        cells_bytes: get_u32(bytes, 44),
        palette_offset: get_u32(bytes, 48),
        palette_bytes: get_u32(bytes, 52),
        total_bytes: get_u32(bytes, 56),
        write_seq: get_u32(bytes, 60),
    };
    if h.cell_size as usize != CELL_SIZE {
        return Err(VtAbiError::BadCellSize);
    }
    let want_cells = cells_bytes(h.cols, h.rows).ok_or(VtAbiError::BadGeometry)?;
    if h.cells_offset < HEADER_LEN as u32
        || h.cells_bytes != want_cells
        || h.total_bytes < h.cells_offset + h.cells_bytes
        || h.total_bytes > MAX_TOTAL_BYTES
    {
        return Err(VtAbiError::BadLayout);
    }
    if h.cursor_x >= h.cols
        || h.cursor_y >= h.rows
        || h.scroll_top > h.scroll_bot
        || h.scroll_bot >= h.rows
    {
        return Err(VtAbiError::BadCursor);
    }
    Ok(h)
}

/// Закодировать одну клетку.
pub fn encode_cell(c: &Cell) -> [u8; CELL_SIZE] {
    let mut b = [0u8; CELL_SIZE];
    put_u32(&mut b, 0, c.codepoint);
    put_u32(&mut b, 4, c.fg);
    put_u32(&mut b, 8, c.bg);
    put_u16(&mut b, 12, c.attrs);
    b[14] = c.width;
    b[15] = c.flags;
    b
}

/// Разобрать клетку (для host-тестов/будущего kernel-читателя).
pub fn parse_cell(bytes: &[u8]) -> Result<Cell, VtAbiError> {
    if bytes.len() < CELL_SIZE {
        return Err(VtAbiError::Truncated);
    }
    Ok(Cell {
        codepoint: get_u32(bytes, 0),
        fg: get_u32(bytes, 4),
        bg: get_u32(bytes, 8),
        attrs: get_u16(bytes, 12),
        width: bytes[14],
        flags: bytes[15],
    })
}

/// Закодировать `struct pgvt_info`.
pub fn encode_info(i: &PgvtInfo) -> [u8; 32] {
    let mut b = [0u8; 32];
    put_u32(&mut b, 0, i.abi_version);
    put_u32(&mut b, 4, i.total_bytes);
    put_u32(&mut b, 8, i.cols);
    put_u32(&mut b, 12, i.rows);
    put_u32(&mut b, 16, i.cell_size);
    put_u32(&mut b, 20, i.header_len);
    put_u32(&mut b, 24, i.reserved[0]);
    put_u32(&mut b, 28, i.reserved[1]);
    b
}

/// Seqlock: writer начал публикацию (чётный → нечётный).
pub fn seq_begin(seq: u32) -> u32 {
    seq.wrapping_add(1)
}

/// Seqlock: writer завершил публикацию (нечётный → чётный).
pub fn seq_end(seq: u32) -> u32 {
    seq.wrapping_add(1)
}

/// Принять ли снимок по паре `seq` до/после копирования.
pub fn seq_consistent(before: u32, after: u32) -> bool {
    before == after && before & 1 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_offsets_match_terminal_md() {
        let h = Header::new(160, 43).unwrap();
        let b = encode_header(&h);
        assert_eq!(b.len(), 128);
        assert_eq!(&b[0..4], &MAGIC.to_le_bytes(), "magic \"PGVT\"");
        assert_eq!(&b[4..6], &1u16.to_le_bytes());
        assert_eq!(&b[6..8], &128u16.to_le_bytes());
        assert_eq!(&b[40..44], &128u32.to_le_bytes(), "cells_offset");
        assert_eq!(&b[44..48], &(160u32 * 43 * 16).to_le_bytes(), "cells_bytes");
        assert_eq!(
            &b[56..60],
            &(128u32 + 160 * 43 * 16).to_le_bytes(),
            "total_bytes"
        );
        // Поля курсора/полей из таблицы ABI.
        assert_eq!(&b[16..18], &160u16.to_le_bytes(), "cols");
        assert_eq!(&b[18..20], &43u16.to_le_bytes(), "rows");
        assert_eq!(b[24], 1, "cursor_visible");
        assert_eq!(b[25], 0, "cursor_style=block");
        assert_eq!(&b[26..28], &0u16.to_le_bytes(), "scroll_top");
        assert_eq!(&b[28..30], &42u16.to_le_bytes(), "scroll_bot");
        assert_eq!(&b[30..32], &8u16.to_le_bytes(), "cell_width");
        assert_eq!(&b[32..34], &18u16.to_le_bytes(), "cell_height");
        assert_eq!(&b[34..36], &16u16.to_le_bytes(), "cell_size");
        assert_eq!(&b[60..64], &0u32.to_le_bytes(), "write_seq");
        assert!(b[64..128].iter().all(|&x| x == 0), "reserved zero");
    }

    #[test]
    fn header_roundtrip_and_validation() {
        let h = Header::new(100, 30).unwrap();
        let b = encode_header(&h);
        assert_eq!(parse_header(&b), Ok(h));
        // Truncation / magic / version / layout rejects.
        assert_eq!(parse_header(&b[..127]), Err(VtAbiError::Truncated));
        let mut bad = b;
        bad[0] = 0;
        assert_eq!(parse_header(&bad), Err(VtAbiError::BadMagic));
        let mut bad = b;
        bad[4] = 2;
        assert_eq!(parse_header(&bad), Err(VtAbiError::UnsupportedVersion));
        let mut bad = b;
        bad[44..48].copy_from_slice(&123u32.to_le_bytes());
        assert_eq!(parse_header(&bad), Err(VtAbiError::BadLayout));
        let mut bad = b;
        bad[8..12].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(parse_header(&bad), Err(VtAbiError::OddSequence));
    }

    #[test]
    fn geometry_limits_and_total_bytes() {
        assert_eq!(total_bytes(80, 25), Some(128 + 80 * 25 * 16));
        assert_eq!(cells_bytes(0, 1), None);
        assert_eq!(cells_bytes(1, 0), None);
        assert_eq!(cells_bytes(1001, 1), None);
        assert_eq!(cells_bytes(1000, 1000), Some(16_000_000));
    }

    #[test]
    fn cell_offsets_and_attributes() {
        let c = Cell {
            codepoint: 'A' as u32,
            fg: 0x0012_3456,
            bg: 0x00AB_CDEF,
            attrs: ATTR_BOLD | ATTR_UNDERLINE | ATTR_OVERLINE,
            width: 2,
            flags: 0,
        };
        let b = encode_cell(&c);
        assert_eq!(b.len(), 16);
        assert_eq!(&b[0..4], &0x41u32.to_le_bytes(), "codepoint");
        assert_eq!(&b[4..8], &0x0012_3456u32.to_le_bytes(), "fg");
        assert_eq!(&b[8..12], &0x00AB_CDEFu32.to_le_bytes(), "bg");
        assert_eq!(&b[12..14], &(1u16 | 8 | 256).to_le_bytes(), "attrs");
        assert_eq!(b[14], 2, "width");
        assert_eq!(b[15], 0, "flags");
        assert_eq!(parse_cell(&b), Ok(c));
        assert_eq!(parse_cell(&b[..15]), Err(VtAbiError::Truncated));
    }

    #[test]
    fn zero_codepoint_is_a_space_and_colors_stay_24_bit() {
        let c = Cell::from_byte(0, 0xFFFF_FFFF, 0xFF00_0000);
        assert_eq!(c.codepoint, b' ' as u32);
        assert_eq!(c.fg, 0x00FF_FFFF);
        assert_eq!(c.bg, 0x0000_0000);
        let c = Cell::from_byte(b'x', 0x0000_00FF, 0x0000_FF00);
        assert_eq!(c.codepoint, b'x' as u32);
        assert_eq!(c.width, 1);
    }

    #[test]
    fn seqlock_parity_and_consistency() {
        let mut s = 0u32;
        s = seq_begin(s);
        assert_eq!(s & 1, 1, "writer active is odd");
        assert!(!seq_consistent(0, s), "odd snapshot rejected");
        s = seq_end(s);
        assert_eq!(s & 1, 0, "writer done is even");
        assert_eq!(s, 2);
        assert!(seq_consistent(s, s));
        assert!(!seq_consistent(s, s.wrapping_add(1)));
        assert_eq!(seq_begin(u32::MAX - 1) & 1, 1, "wrap keeps parity");
    }

    #[test]
    fn ioctl_info_layout() {
        let i = PgvtInfo {
            abi_version: 1,
            total_bytes: 128 + 80 * 25 * 16,
            cols: 80,
            rows: 25,
            cell_size: 16,
            header_len: 128,
            reserved: [0, 0],
        };
        let b = encode_info(&i);
        assert_eq!(b.len(), 32);
        assert_eq!(&b[0..4], &1u32.to_le_bytes());
        assert_eq!(&b[16..20], &16u32.to_le_bytes());
        assert_eq!(&b[20..24], &128u32.to_le_bytes());
        assert_eq!(PGVT_IOC_INFO, 0x8020_5601);
    }
}
