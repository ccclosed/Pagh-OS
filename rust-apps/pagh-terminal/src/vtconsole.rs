//! Kernel VT console shared text surface (proposed ABI v1) parser.
//!
//! The kernel's in-kernel VT (`src/drivers/vt.rs`) already owns the character
//! grid and the scrollback.  Desktop-B variant 1 does not rewrite the shell
//! into a userspace PTY: instead the kernel publishes a read-only snapshot of
//! the visible text grid on a character device, and `pagh-terminal` renders it
//! as a compositor surface.  Input is written back as VT bytes.
//!
//! This module implements the *host-testable* half: the layout constants, a
//! validating parser, and a reader for the `read()` fallback.  The full ABI
//! (mmap, ioctls, seqlock, security) is specified in `TERMINAL.md`; the kernel
//! side is intentionally not implemented here.
//!
//! Layout (little-endian):
//!
//! ```text
//! header (128 bytes)
//!   0 magic "PGVT"          16 cols u16, 18 rows u16
//!   4 version u16           20 cursor_x u16, 22 cursor_y u16
//!   6 header_len u16        24 cursor_visible u8, 25 cursor_style u8
//!   8 seq u32 (seqlock)     26 scroll_top u16, 28 scroll_bot u16
//!  12 flags u32             30 cell_w u16, 32 cell_h u16, 34 cell_size u16
//!   36 attrs_flags u16, 38 reserved
//!   40 cells_offset u32, 44 cells_bytes u32
//!   48 palette_offset u32, 52 palette_bytes u32
//!   56 total_bytes u32, 60 write_seq u32
//!   64..128 reserved (zero)
//!
//! cell (16 bytes)
//!   0 codepoint u32 (Unicode scalar)
//!   4 fg u32 0x00RRGGBB
//!   8 bg u32 0x00RRGGBB
//!  12 attrs u16 (same bit order as SGR: bold 1, dim 2, italic 4, underline 8,
//!     blink 16, reverse 32, hidden 64, strike 128, overline 256)
//!  14 width u8 (1 or 2)
//!  15 flags u8 (bit 0: wide continuation cell)
//! ```

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::cell::{Attrs, Cell, Rgb, CELL_WIDE, CELL_WIDE_CONT};
use crate::screen::{CursorStyle, Screen};

pub const MAGIC: u32 = 0x5456_4750; // "PGVT" little-endian
pub const VERSION: u16 = 1;
pub const HEADER_LEN: usize = 128;
pub const CELL_LEN: usize = 16;
pub const MAX_COLS: usize = 1000;
pub const MAX_ROWS: usize = 1000;

pub mod flags {
    /// Codepoints are Unicode scalars (v1 always sets this).
    pub const UNICODE_CELLS: u32 = 1 << 0;
    /// Cell colors are 24-bit RGB (v1 always sets this).
    pub const TRUE_COLOR: u32 = 1 << 1;
    /// Cursor fields are valid.
    pub const CURSOR_VALID: u32 = 1 << 2;
}

pub mod cell_flags {
    pub const WIDE_CONTINUATION: u8 = 1 << 0;
    pub const UNDERLINE_CONTINUATION: u8 = 1 << 1;
}

#[derive(Debug)]
pub enum VtError {
    BadMagic(u32),
    UnsupportedVersion(u16),
    BadHeader(&'static str),
    OddSequence(u32),
    Truncated { need: usize, have: usize },
    TooLarge,
    Io(io::Error),
}

impl fmt::Display for VtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VtError::BadMagic(m) => write!(f, "bad PGVT magic 0x{m:08x}"),
            VtError::UnsupportedVersion(v) => write!(f, "unsupported PGVT version {v}"),
            VtError::BadHeader(what) => write!(f, "invalid PGVT header: {what}"),
            VtError::OddSequence(seq) => write!(f, "PGVT snapshot is mid-update (seq {seq})"),
            VtError::Truncated { need, have } => {
                write!(f, "truncated PGVT snapshot: need {need}, have {have}")
            }
            VtError::TooLarge => write!(f, "PGVT grid exceeds the supported dimensions"),
            VtError::Io(e) => write!(f, "PGVT I/O error: {e}"),
        }
    }
}

impl std::error::Error for VtError {}

impl From<io::Error> for VtError {
    fn from(error: io::Error) -> VtError {
        VtError::Io(error)
    }
}

/// Parsed header of a shared-surface snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VtHeader {
    pub version: u16,
    pub header_len: u16,
    pub seq: u32,
    pub flags: u32,
    pub cols: u16,
    pub rows: u16,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub cursor_visible: bool,
    pub cursor_style: CursorStyle,
    pub scroll_top: u16,
    pub scroll_bot: u16,
    pub cell_width: u16,
    pub cell_height: u16,
    pub cell_size: u16,
    pub attrs_flags: u16,
    pub cells_offset: u32,
    pub cells_bytes: u32,
    pub palette_offset: u32,
    pub palette_bytes: u32,
    pub total_bytes: u32,
    pub write_seq: u32,
}

impl VtHeader {
    /// Decode and validate the fixed header from at least `HEADER_LEN` bytes.
    pub fn parse(bytes: &[u8]) -> Result<VtHeader, VtError> {
        if bytes.len() < HEADER_LEN {
            return Err(VtError::Truncated {
                need: HEADER_LEN,
                have: bytes.len(),
            });
        }
        let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        if magic != MAGIC {
            return Err(VtError::BadMagic(magic));
        }
        let version = read_u16(bytes, 4);
        if version != VERSION {
            return Err(VtError::UnsupportedVersion(version));
        }
        let header_len = read_u16(bytes, 6);
        if header_len as usize != HEADER_LEN {
            return Err(VtError::BadHeader("header_len must be 128"));
        }
        let seq = read_u32(bytes, 8);
        let flags = read_u32(bytes, 12);
        let cols = read_u16(bytes, 16);
        let rows = read_u16(bytes, 18);
        if cols == 0 || rows == 0 {
            return Err(VtError::BadHeader("cols/rows must be non-zero"));
        }
        if cols as usize > MAX_COLS || rows as usize > MAX_ROWS {
            return Err(VtError::TooLarge);
        }
        let cursor_x = read_u16(bytes, 20);
        let cursor_y = read_u16(bytes, 22);
        if cursor_x >= cols || cursor_y >= rows {
            return Err(VtError::BadHeader("cursor outside the grid"));
        }
        let cursor_style = match bytes[25] {
            0 => CursorStyle::Block,
            1 => CursorStyle::Underline,
            2 => CursorStyle::Bar,
            _ => CursorStyle::Block,
        };
        let scroll_top = read_u16(bytes, 26);
        let scroll_bot = read_u16(bytes, 28);
        if scroll_top >= rows || scroll_bot >= rows || scroll_top > scroll_bot {
            return Err(VtError::BadHeader("scroll region outside the grid"));
        }
        let cell_width = read_u16(bytes, 30);
        let cell_height = read_u16(bytes, 32);
        let cell_size = read_u16(bytes, 34);
        if cell_width == 0 || cell_height == 0 || cell_size as usize != CELL_LEN {
            return Err(VtError::BadHeader("unsupported cell geometry"));
        }
        let cells_offset = read_u32(bytes, 40);
        let cells_bytes = read_u32(bytes, 44);
        let expected_cells = (cols as usize)
            .checked_mul(rows as usize)
            .and_then(|n| n.checked_mul(CELL_LEN))
            .ok_or(VtError::TooLarge)?;
        if (cells_offset as usize) < header_len as usize || cells_bytes as usize != expected_cells {
            return Err(VtError::BadHeader("cell array size/offset mismatch"));
        }
        let total_bytes = read_u32(bytes, 56);
        if (total_bytes as usize) < cells_offset as usize + cells_bytes as usize {
            return Err(VtError::BadHeader(
                "total_bytes smaller than the cell array",
            ));
        }
        Ok(VtHeader {
            version,
            header_len,
            seq,
            flags,
            cols,
            rows,
            cursor_x,
            cursor_y,
            cursor_visible: bytes[24] != 0,
            cursor_style,
            scroll_top,
            scroll_bot,
            cell_width,
            cell_height,
            cell_size,
            attrs_flags: read_u16(bytes, 36),
            cells_offset,
            cells_bytes,
            palette_offset: read_u32(bytes, 48),
            palette_bytes: read_u32(bytes, 52),
            total_bytes,
            write_seq: read_u32(bytes, 60),
        })
    }

    pub fn cell_count(&self) -> usize {
        self.cols as usize * self.rows as usize
    }

    pub fn snapshot_len(&self) -> usize {
        self.total_bytes as usize
    }

    pub fn stable(&self) -> bool {
        self.seq % 2 == 0
    }
}

/// One cell decoded from the shared surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VtCell {
    pub ch: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub attrs: Attrs,
    pub width: u8,
    pub continuation: bool,
}

impl VtCell {
    pub fn to_cell(self) -> Cell {
        if self.continuation {
            Cell {
                ch: ' ',
                fg: crate::cell::Color::Rgb(self.fg.0, self.fg.1, self.fg.2),
                bg: crate::cell::Color::Rgb(self.bg.0, self.bg.1, self.bg.2),
                attrs: self.attrs,
                flags: CELL_WIDE_CONT,
            }
        } else {
            Cell {
                ch: self.ch,
                fg: crate::cell::Color::Rgb(self.fg.0, self.fg.1, self.fg.2),
                bg: crate::cell::Color::Rgb(self.bg.0, self.bg.1, self.bg.2),
                attrs: self.attrs,
                flags: if self.width == 2 { CELL_WIDE } else { 0 },
            }
        }
    }
}

impl VtCell {
    fn decode(bytes: &[u8; CELL_LEN]) -> VtCell {
        let cp = read_u32(bytes, 0);
        let ch = char::from_u32(cp)
            .filter(|c| !c.is_control())
            .unwrap_or(' ');
        let fg = unpack_rgb(read_u32(bytes, 4));
        let bg = unpack_rgb(read_u32(bytes, 8));
        let attrs = Attrs(read_u16(bytes, 12) & 0x01FF);
        let width = match bytes[14] {
            2 => 2,
            _ => 1,
        };
        let continuation = bytes[15] & cell_flags::WIDE_CONTINUATION != 0;
        VtCell {
            ch,
            fg,
            bg,
            attrs,
            width: if continuation { 1 } else { width },
            continuation,
        }
    }
}

/// A validated snapshot of the kernel VT grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VtSnapshot {
    pub header: VtHeader,
    pub cells: Vec<VtCell>,
}

impl VtSnapshot {
    pub fn text(&self) -> String {
        let cols = self.header.cols as usize;
        let mut lines = Vec::with_capacity(self.header.rows as usize);
        for row in 0..self.header.rows as usize {
            let mut line = String::new();
            for col in 0..cols {
                let cell = self.cells[row * cols + col];
                if cell.continuation {
                    continue;
                }
                line.push(cell.ch);
            }
            lines.push(line.trim_end().to_string());
        }
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// Convert into the renderer's [`Screen`].
    pub fn to_screen(&self) -> Screen {
        let cells = self.cells.iter().map(|cell| cell.to_cell()).collect();
        Screen::from_cells(
            self.header.cols as usize,
            self.header.rows as usize,
            cells,
            (self.header.cursor_x as usize, self.header.cursor_y as usize),
            self.header.cursor_visible,
            self.header.cursor_style,
        )
    }
}

/// Parse a complete `read()` snapshot.  Rejects an odd seqlock value, which
/// means the kernel is in the middle of publishing a frame.
pub fn parse_snapshot(bytes: &[u8]) -> Result<VtSnapshot, VtError> {
    let header = VtHeader::parse(bytes)?;
    if !header.stable() {
        return Err(VtError::OddSequence(header.seq));
    }
    let end = header.cells_offset as usize + header.cells_bytes as usize;
    if bytes.len() < end {
        return Err(VtError::Truncated {
            need: end,
            have: bytes.len(),
        });
    }
    if end > header.snapshot_len() {
        return Err(VtError::BadHeader("cell array beyond total_bytes"));
    }
    let mut cells = Vec::with_capacity(header.cell_count());
    for chunk in bytes[header.cells_offset as usize..end].chunks_exact(CELL_LEN) {
        let array: &[u8; CELL_LEN] = chunk.try_into().unwrap();
        cells.push(VtCell::decode(array));
    }
    Ok(VtSnapshot { header, cells })
}

/// Reader for the proposed `/dev/vtconsole0` character device.
///
/// v1 allows two access modes: `mmap()` of `total_bytes` (preferred, described
/// in TERMINAL.md) and `read()` of one complete snapshot from offset 0.  This
/// reader implements the `read()` fallback so the client is testable on a host
/// with a plain file; the mmap path would only change how `bytes` is obtained.
pub struct VtConsoleReader {
    path: PathBuf,
    last_seq: Option<u32>,
}

impl VtConsoleReader {
    pub fn open(path: impl AsRef<Path>) -> io::Result<VtConsoleReader> {
        // Probe the path now so a missing device is reported at startup.  The
        // snapshot itself is read from a freshly opened handle each poll, which
        // works for a plain file and for a char device that returns one
        // snapshot per open() without requiring lseek.
        File::open(&path)?;
        Ok(VtConsoleReader {
            path: path.as_ref().to_path_buf(),
            last_seq: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read one full snapshot from a fresh handle.
    ///
    /// A read()-mode device may expose the seqlock to userspace (odd `seq` or
    /// `EAGAIN` while the kernel is copying), so transient failures are retried
    /// a few times before they are reported.
    pub fn read_snapshot(&mut self) -> Result<VtSnapshot, VtError> {
        let mut last = None;
        for _ in 0..8 {
            match self.read_snapshot_once() {
                Ok(snapshot) => return Ok(snapshot),
                Err(VtError::OddSequence(_)) => {
                    last = Some(VtError::BadHeader("snapshot stayed mid-update"));
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(VtError::Io(error))
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::TimedOut =>
                {
                    last = Some(VtError::Io(error));
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(error) => return Err(error),
            }
        }
        Err(last.unwrap_or(VtError::BadHeader("snapshot retry exhausted")))
    }

    fn read_snapshot_once(&mut self) -> Result<VtSnapshot, VtError> {
        let mut file = File::open(&self.path)?;
        let mut header_bytes = [0u8; HEADER_LEN];
        file.read_exact(&mut header_bytes)?;
        let header = VtHeader::parse(&header_bytes)?;
        let total = header.snapshot_len();
        if total > 64 * 1024 * 1024 {
            return Err(VtError::TooLarge);
        }
        let mut buffer = vec![0u8; total];
        buffer[..HEADER_LEN].copy_from_slice(&header_bytes);
        file.read_exact(&mut buffer[HEADER_LEN..])?;
        parse_snapshot(&buffer)
    }

    /// Return a snapshot only if the surface changed since the last poll.
    pub fn poll(&mut self) -> Result<Option<VtSnapshot>, VtError> {
        let snapshot = self.read_snapshot()?;
        if self.last_seq == Some(snapshot.header.seq) {
            return Ok(None);
        }
        self.last_seq = Some(snapshot.header.seq);
        Ok(Some(snapshot))
    }
}

/// Write terminal input bytes to the proposed `/dev/vtinput` device.
pub fn send_input(path: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.write_all(bytes)?;
    file.flush()
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn unpack_rgb(v: u32) -> Rgb {
    Rgb(
        ((v >> 16) & 0xFF) as u8,
        ((v >> 8) & 0xFF) as u8,
        (v & 0xFF) as u8,
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn snapshot_bytes(
        cols: u16,
        rows: u16,
        seq: u32,
        cells: &[(char, u32, u32, u16, u8, u8)],
        cursor: (u16, u16),
    ) -> Vec<u8> {
        let mut bytes = vec![0u8; HEADER_LEN];
        bytes[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        bytes[4..6].copy_from_slice(&VERSION.to_le_bytes());
        bytes[6..8].copy_from_slice(&(HEADER_LEN as u16).to_le_bytes());
        bytes[8..12].copy_from_slice(&seq.to_le_bytes());
        bytes[12..16].copy_from_slice(
            &(flags::UNICODE_CELLS | flags::TRUE_COLOR | flags::CURSOR_VALID).to_le_bytes(),
        );
        bytes[16..18].copy_from_slice(&cols.to_le_bytes());
        bytes[18..20].copy_from_slice(&rows.to_le_bytes());
        bytes[20..22].copy_from_slice(&cursor.0.to_le_bytes());
        bytes[22..24].copy_from_slice(&cursor.1.to_le_bytes());
        bytes[24] = 1;
        bytes[25] = 0;
        bytes[26..28].copy_from_slice(&0u16.to_le_bytes());
        bytes[28..30].copy_from_slice(&(rows - 1).to_le_bytes());
        bytes[30..32].copy_from_slice(&8u16.to_le_bytes());
        bytes[32..34].copy_from_slice(&18u16.to_le_bytes());
        bytes[34..36].copy_from_slice(&(CELL_LEN as u16).to_le_bytes());
        let cells_bytes = cols as usize * rows as usize * CELL_LEN;
        bytes[40..44].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        bytes[44..48].copy_from_slice(&(cells_bytes as u32).to_le_bytes());
        bytes[56..60].copy_from_slice(&((HEADER_LEN + cells_bytes) as u32).to_le_bytes());
        bytes[60..64].copy_from_slice(&seq.to_le_bytes());
        bytes.resize(HEADER_LEN + cells_bytes, 0);
        for (i, (ch, fg, bg, attrs, width, flags)) in cells.iter().enumerate() {
            let start = HEADER_LEN + i * CELL_LEN;
            bytes[start..start + 4].copy_from_slice(&(*ch as u32).to_le_bytes());
            bytes[start + 4..start + 8].copy_from_slice(&fg.to_le_bytes());
            bytes[start + 8..start + 12].copy_from_slice(&bg.to_le_bytes());
            bytes[start + 12..start + 14].copy_from_slice(&attrs.to_le_bytes());
            bytes[start + 14] = *width;
            bytes[start + 15] = *flags;
        }
        bytes
    }

    fn sample() -> Vec<u8> {
        snapshot_bytes(
            4,
            2,
            4,
            &[
                ('a', 0x00112233, 0x00000000, 1, 1, 0),
                ('б', 0x00FF0000, 0x00000000, 0, 1, 0),
                ('界', 0x00FFFFFF, 0x00000000, 0, 2, 0),
                (' ', 0x00FFFFFF, 0x00000000, 0, 1, 1),
                ('z', 0x00FFFFFF, 0x00000000, 0, 1, 0),
                (' ', 0, 0, 0, 1, 0),
                (' ', 0, 0, 0, 1, 0),
                (' ', 0, 0, 0, 1, 0),
            ],
            (3, 1),
        )
    }

    #[test]
    fn parses_header_cells_and_text() {
        let snapshot = parse_snapshot(&sample()).unwrap();
        assert_eq!(snapshot.header.cols, 4);
        assert_eq!(snapshot.header.rows, 2);
        assert_eq!(snapshot.text(), "aб界\nz");
        assert_eq!(snapshot.cells[0].attrs, Attrs::BOLD);
        assert_eq!(snapshot.cells[0].fg, Rgb(0x11, 0x22, 0x33));
        assert!(snapshot.cells[3].continuation);
        assert_eq!(snapshot.header.cursor_x, 3);
    }

    #[test]
    fn converts_to_screen() {
        let screen = parse_snapshot(&sample()).unwrap().to_screen();
        assert_eq!(screen.line_text(0), "aб界");
        assert_eq!(screen.line_text(1), "z");
        assert!(screen.cell(2, 0).is_wide());
        assert!(screen.cell(3, 0).is_wide_continuation());
    }

    #[test]
    fn rejects_bad_inputs() {
        let mut bytes = sample();
        bytes[0] ^= 0xFF;
        assert!(matches!(parse_snapshot(&bytes), Err(VtError::BadMagic(_))));

        let mut bytes = sample();
        bytes[8..12].copy_from_slice(&5u32.to_le_bytes());
        assert!(matches!(
            parse_snapshot(&bytes),
            Err(VtError::OddSequence(5))
        ));

        let mut bytes = sample();
        bytes[16..18].copy_from_slice(&0u16.to_le_bytes());
        assert!(matches!(parse_snapshot(&bytes), Err(VtError::BadHeader(_))));

        let bytes = sample();
        assert!(matches!(
            parse_snapshot(&bytes[..HEADER_LEN + 10]),
            Err(VtError::Truncated { .. })
        ));
    }

    #[test]
    fn reader_polls_only_when_seq_changes() {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "pagh-vtconsole-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&dir, sample()).unwrap();
        let mut reader = VtConsoleReader::open(&dir).unwrap();
        assert!(reader.poll().unwrap().is_some());
        assert!(reader.poll().unwrap().is_none());
        let next = snapshot_bytes(
            4,
            2,
            6,
            &[
                ('n', 0, 0, 0, 1, 0),
                ('e', 0, 0, 0, 1, 0),
                ('w', 0, 0, 0, 1, 0),
                (' ', 0, 0, 0, 1, 0),
                (' ', 0, 0, 0, 1, 0),
                (' ', 0, 0, 0, 1, 0),
                (' ', 0, 0, 0, 1, 0),
                (' ', 0, 0, 0, 1, 0),
            ],
            (0, 0),
        );
        std::fs::write(&dir, next).unwrap();
        assert_eq!(reader.poll().unwrap().unwrap().text(), "new");
        let _ = std::fs::remove_file(&dir);
    }
}
