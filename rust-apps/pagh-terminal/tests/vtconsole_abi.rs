//! ABI-level tests for the proposed kernel VT console shared text surface
//! (`/dev/vtconsole0`, v1).  These construct the byte layout by hand so a
//! future kernel implementation can be checked against the documented offsets.

use pagh_terminal::cell::{Attrs, Rgb};
use pagh_terminal::screen::CursorStyle;
use pagh_terminal::vtconsole::{
    self, flags, parse_snapshot, VtError, VtHeader, CELL_LEN, HEADER_LEN, MAGIC, VERSION,
};

fn build(
    cols: u16,
    rows: u16,
    seq: u32,
    cells: &[(u32, u32, u32, u16, u8, u8)],
    cursor: (u16, u16),
    style: u8,
) -> Vec<u8> {
    let cells_bytes = cols as usize * rows as usize * CELL_LEN;
    let mut bytes = vec![0u8; HEADER_LEN + cells_bytes];
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
    bytes[25] = style;
    bytes[26..28].copy_from_slice(&0u16.to_le_bytes());
    bytes[28..30].copy_from_slice(&(rows - 1).to_le_bytes());
    bytes[30..32].copy_from_slice(&8u16.to_le_bytes());
    bytes[32..34].copy_from_slice(&18u16.to_le_bytes());
    bytes[34..36].copy_from_slice(&(CELL_LEN as u16).to_le_bytes());
    bytes[40..44].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
    bytes[44..48].copy_from_slice(&(cells_bytes as u32).to_le_bytes());
    bytes[56..60].copy_from_slice(&((HEADER_LEN + cells_bytes) as u32).to_le_bytes());
    bytes[60..64].copy_from_slice(&seq.to_le_bytes());
    for (i, (cp, fg, bg, attrs, width, cell_flags)) in cells.iter().enumerate() {
        let start = HEADER_LEN + i * CELL_LEN;
        bytes[start..start + 4].copy_from_slice(&cp.to_le_bytes());
        bytes[start + 4..start + 8].copy_from_slice(&fg.to_le_bytes());
        bytes[start + 8..start + 12].copy_from_slice(&bg.to_le_bytes());
        bytes[start + 12..start + 14].copy_from_slice(&attrs.to_le_bytes());
        bytes[start + 14] = *width;
        bytes[start + 15] = *cell_flags;
    }
    bytes
}

fn sample() -> Vec<u8> {
    build(
        5,
        2,
        8,
        &[
            ('P' as u32, 0x002FD4B0, 0x000E161B, Attrs::BOLD.0, 1, 0),
            ('a' as u32, 0x00D8E6E3, 0x000E161B, Attrs::empty().0, 1, 0),
            ('g' as u32, 0x00D8E6E3, 0x000E161B, Attrs::UNDERLINE.0, 1, 0),
            ('h' as u32, 0x00D8E6E3, 0x000E161B, 0, 1, 0),
            (' ' as u32, 0x00D8E6E3, 0x000E161B, 0, 1, 0),
            ('界' as u32, 0x00FFFFFF, 0x00000000, 0, 2, 0),
            (' ' as u32, 0x00FFFFFF, 0x00000000, 0, 1, 1),
            ('я' as u32, 0x00FF0000, 0x00000000, 0, 1, 0),
            (' ' as u32, 0, 0, 0, 1, 0),
            (' ' as u32, 0, 0, 0, 1, 0),
        ],
        (4, 1),
        2,
    )
}

#[test]
fn header_offsets_and_fields_match_the_documented_abi() {
    let bytes = sample();
    let header = VtHeader::parse(&bytes).unwrap();
    assert_eq!(header.version, 1);
    assert_eq!(header.header_len, 128);
    assert_eq!(header.seq, 8);
    assert_eq!((header.cols, header.rows), (5, 2));
    assert_eq!((header.cursor_x, header.cursor_y), (4, 1));
    assert!(header.cursor_visible);
    assert_eq!(header.cursor_style, CursorStyle::Bar);
    assert_eq!((header.scroll_top, header.scroll_bot), (0, 1));
    assert_eq!((header.cell_width, header.cell_height), (8, 18));
    assert_eq!(header.cell_size, 16);
    assert_eq!(header.cells_offset, 128);
    assert_eq!(header.cells_bytes, 2 * 5 * 16);
    assert_eq!(header.total_bytes, (HEADER_LEN + 160) as u32);
    assert_eq!(header.snapshot_len(), HEADER_LEN + 160);
    assert_eq!(header.cell_count(), 10);
    assert!(header.stable());
}

#[test]
fn cells_decode_codepoints_colors_attrs_and_wide_flags() {
    let snapshot = parse_snapshot(&sample()).unwrap();
    assert_eq!(snapshot.text(), "Pagh\n界я");
    assert_eq!(snapshot.cells[0].fg, Rgb(0x2f, 0xd4, 0xb0));
    assert_eq!(snapshot.cells[0].bg, Rgb(0x0e, 0x16, 0x1b));
    assert_eq!(snapshot.cells[0].attrs, Attrs::BOLD);
    assert_eq!(snapshot.cells[2].attrs, Attrs::UNDERLINE);
    assert_eq!(snapshot.cells[5].width, 2);
    assert!(snapshot.cells[6].continuation);
    assert_eq!(snapshot.cells[7].ch, 'я');
    assert_eq!(snapshot.header.cursor_style, CursorStyle::Bar);
}

#[test]
fn snapshot_converts_to_renderable_screen() {
    let screen = parse_snapshot(&sample()).unwrap().to_screen();
    assert_eq!(screen.cols(), 5);
    assert_eq!(screen.rows(), 2);
    assert_eq!(screen.line_text(0), "Pagh");
    assert_eq!(screen.line_text(1), "界я");
    assert!(screen.cell(0, 1).is_wide());
    assert!(screen.cell(1, 1).is_wide_continuation());
    assert_eq!(screen.cursor.x, 4);
    assert_eq!(screen.cursor.y, 1);
}

#[test]
fn rejects_mid_update_snapshots() {
    let mut bytes = sample();
    bytes[8..12].copy_from_slice(&9u32.to_le_bytes());
    assert!(matches!(
        parse_snapshot(&bytes),
        Err(VtError::OddSequence(9))
    ));
}

#[test]
fn rejects_wrong_magic_version_and_geometry() {
    let mut bytes = sample();
    bytes[0..4].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
    assert!(matches!(parse_snapshot(&bytes), Err(VtError::BadMagic(_))));

    let mut bytes = sample();
    bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
    assert!(matches!(
        parse_snapshot(&bytes),
        Err(VtError::UnsupportedVersion(2))
    ));

    let mut bytes = sample();
    bytes[6..8].copy_from_slice(&64u16.to_le_bytes());
    assert!(matches!(parse_snapshot(&bytes), Err(VtError::BadHeader(_))));

    let mut bytes = sample();
    bytes[34..36].copy_from_slice(&15u16.to_le_bytes());
    assert!(matches!(parse_snapshot(&bytes), Err(VtError::BadHeader(_))));

    let mut bytes = sample();
    bytes[20..22].copy_from_slice(&5u16.to_le_bytes());
    assert!(matches!(parse_snapshot(&bytes), Err(VtError::BadHeader(_))));

    let mut bytes = sample();
    bytes[8..12].copy_from_slice(&6u32.to_le_bytes());
    bytes[44..48].copy_from_slice(&0u32.to_le_bytes());
    assert!(matches!(parse_snapshot(&bytes), Err(VtError::BadHeader(_))));
}

#[test]
fn rejects_truncated_snapshots_without_panicking() {
    let bytes = sample();
    for cut in [0, 4, 64, HEADER_LEN - 1, HEADER_LEN + 1, bytes.len() - 1] {
        let result = parse_snapshot(&bytes[..cut]);
        assert!(result.is_err(), "cut at {cut} unexpectedly parsed");
    }
}

#[test]
fn reader_reads_snapshot_file_and_send_input_writes() {
    let base = std::env::temp_dir().join(format!(
        "pagh-term-vt-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let snapshot_path = base.with_extension("surface");
    let input_path = base.with_extension("input");
    std::fs::write(&snapshot_path, sample()).unwrap();
    let mut reader = vtconsole::VtConsoleReader::open(&snapshot_path).unwrap();
    let snapshot = reader.poll().unwrap().unwrap();
    assert_eq!(snapshot.text(), "Pagh\n界я");
    assert!(reader.poll().unwrap().is_none());

    std::fs::write(&input_path, b"").unwrap();
    vtconsole::send_input(&input_path, b"\x1b[A\r").unwrap();
    assert_eq!(std::fs::read(&input_path).unwrap(), b"\x1b[A\r");
    let _ = std::fs::remove_file(&snapshot_path);
    let _ = std::fs::remove_file(&input_path);
}

#[test]
fn dangling_wide_continuation_is_normalized() {
    // Continuation without a preceding wide cell must not render as a glyph.
    let mut cells = vec![(' ' as u32, 0u32, 0u32, 0u16, 1u8, 0u8); 4];
    cells[0] = (' ' as u32, 0, 0, 0, 1, 1);
    let bytes = build(4, 1, 2, &cells, (0, 0), 0);
    let screen = parse_snapshot(&bytes).unwrap().to_screen();
    assert!(!screen.cell(0, 0).is_wide_continuation());
}
