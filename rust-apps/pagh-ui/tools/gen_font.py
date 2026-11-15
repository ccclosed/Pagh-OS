#!/usr/bin/env python3
"""Generate the embedded bitmap font for `pagh-ui`.

The toolkit must render text without any external crate and without a font
file at runtime: pagh desktop clients are statically linked musl binaries that
ship inside the ext2 image. This script rasterises a system font once and emits
`src/font_data.rs` (sorted glyph table + 4-bit alpha blob).

Usage:
    python3 tools/gen_font.py [--font FONT.ttf] [--size 13] > src/font_data.rs

The glyph bitmaps are generated from DejaVu Sans, which is distributed under
the permissive DejaVu Fonts License (a Bitstream Vera derivative); embedding
rasterised glyphs in a program is allowed by that license.
"""

from __future__ import annotations

import argparse
import sys

try:
    from PIL import Image, ImageDraw, ImageFont
except ImportError:  # pragma: no cover - build-time helper
    sys.exit("error: Pillow is required to regenerate the font (pip install pillow)")

DEFAULT_FONT = "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf"

# Characters the UI needs: printable ASCII, the Russian alphabet, and a few
# typographic marks used by the Files app (quotes, dashes, ellipsis, numero).
CHARS = (
    [chr(c) for c in range(0x20, 0x7F)]
    + ["\u00b7", "\u00ab", "\u00bb", "\u2013", "\u2014", "\u2026", "\u2116", "\u0401"]
    + [chr(c) for c in range(0x0410, 0x0450)]
    + ["\u0451"]
)

MAX_WIDTH = 16
CELL_H = 24


def rasterise(font: ImageFont.FreeTypeFont, ch: str) -> tuple[int, int, int, int, int, list[list[int]]]:
    """Return (left, top, width, height, advance, rows) for one character."""
    advance = int(round(font.getlength(ch)))
    image = Image.new("L", (max(advance, 1) + 16, CELL_H), 0)
    draw = ImageDraw.Draw(image)
    baseline = 16
    draw.text((8, baseline), ch, fill=255, font=font, anchor="ls")
    bbox = image.getbbox()
    if bbox is None:
        return 0, 0, 0, 0, advance, []
    x0, y0, x1, y1 = bbox
    left = x0 - 8
    top = y0 - baseline
    width = x1 - x0
    height = y1 - y0
    if width > MAX_WIDTH or height > CELL_H:
        raise SystemExit(f"glyph {ch!r} too large: {width}x{height}")
    rows: list[list[int]] = []
    for y in range(y0, y1):
        row = []
        for x in range(x0, x1):
            row.append(round(image.getpixel((x, y)) / 255 * 15))
        rows.append(row)
    return left, top, width, height, advance, rows


def pack(rows: list[list[int]]) -> bytes:
    out = bytearray()
    for row in rows:
        i = 0
        while i < len(row):
            hi = row[i] & 0xF
            lo = (row[i + 1] & 0xF) if i + 1 < len(row) else 0
            out.append((hi << 4) | lo)
            i += 2
    return bytes(out)


def rust_char(ch: str) -> str:
    o = ord(ch)
    if ch == "\\":
        return "'\\\\'"
    if ch == "'":
        return "'\\''"
    if 0x20 <= o < 0x7F:
        return f"'{ch}'"
    return f"'\\u{{{o:04x}}}'"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--font", default=DEFAULT_FONT)
    ap.add_argument("--size", type=int, default=13)
    args = ap.parse_args()

    font = ImageFont.truetype(args.font, args.size)
    ascent, descent = font.getmetrics()

    glyphs = []
    blob = bytearray()
    for ch in CHARS:
        left, top, width, height, advance, rows = rasterise(font, ch)
        data = pack(rows)
        offset = len(blob)
        blob.extend(data)
        glyphs.append((ord(ch), ch, left, top, width, height, advance, offset))

    # Keep the table sorted by code point for binary search.
    glyphs.sort(key=lambda g: g[0])

    w = sys.stdout.write
    w("//! Bitmap font for `pagh-ui`: glyph table + 4-bit alpha bitmap blob.\n")
    w("//!\n")
    w("//! GENERATED FILE — do not edit by hand. Regenerate with:\n")
    w("//!\n")
    w("//! ```text\n")
    w("//! python3 tools/gen_font.py > src/font_data.rs\n")
    w("//! ```\n")
    w("//!\n")
    w("//! Source font: DejaVu Sans, 13 px, antialiased to 4 bits per pixel. DejaVu\n")
    w("//! Sans is distributed under the DejaVu Fonts License (a permissive\n")
    w("//! Bitstream Vera derivative); embedding rasterised glyphs is permitted.\n")
    w("//! The table covers printable ASCII, the Russian alphabet (А-я, Ё, ё),\n")
    w("//! «» quotes, – and — dashes, … ellipsis, · middle dot and №.\n\n")
    w(f"/// Human readable font name.\npub const FONT_NAME: &str = \"DejaVu Sans {args.size}\";\n\n")
    w("/// Distance from the top of a text line down to the baseline, in pixels.\n")
    w(f"pub const ASCENT: i32 = {ascent};\n\n")
    w("/// Distance from the baseline to the bottom of a text line, in pixels.\n")
    w(f"pub const DESCENT: i32 = {descent};\n\n")
    w("/// Line box height used by the layout code, in pixels.\n")
    w(f"pub const LINE_HEIGHT: i32 = {ascent + descent + 1};\n\n")
    w(f"/// Widest glyph in the table, in pixels.\npub const MAX_WIDTH: usize = {MAX_WIDTH};\n\n")
    w("/// One glyph: ink bearings relative to the pen position/baseline, the ink\n")
    w("/// size, the pen advance, and an offset into [`ALPHA`].\n")
    w("#[derive(Clone, Copy)]\n")
    w("pub struct Glyph {\n")
    w("    pub ch: char,\n")
    w("    /// Horizontal ink offset relative to the pen position (pixels).\n")
    w("    pub left: i8,\n")
    w("    /// Vertical ink offset relative to the baseline (negative = above).\n")
    w("    pub top: i8,\n")
    w("    pub width: u8,\n")
    w("    pub height: u8,\n")
    w("    /// Pen advance in pixels.\n")
    w("    pub advance: u8,\n")
    w("    /// Byte offset into [`ALPHA`] of the first row.\n")
    w("    pub offset: u32,\n")
    w("}\n\n")
    w("/// Packed 4-bit alpha rows: `ceil(width / 2)` bytes per row, high nibble\n")
    w("/// first. Rows are stored top to bottom, `height` rows per glyph.\n")
    w("pub static ALPHA: &[u8] = &[\n")
    line = "   "
    for i, byte in enumerate(blob):
        line += f" 0x{byte:02x},"
        if (i + 1) % 16 == 0:
            w(line + "\n")
            line = "   "
    if line.strip():
        w(line + "\n")
    w("];\n\n")
    w("/// Glyph table sorted by code point (binary search in [`crate::font`]).\n")
    w("pub static GLYPHS: &[Glyph] = &[\n")
    for cp, ch, left, top, width, height, advance, offset in glyphs:
        w(
            "    Glyph { ch: %s, left: %d, top: %d, width: %d, height: %d, advance: %d, offset: %d },\n"
            % (rust_char(ch), left, top, width, height, advance, offset)
        )
    w("];\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())