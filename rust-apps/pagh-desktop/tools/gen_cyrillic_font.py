#!/usr/bin/env python3
"""Сгенерировать таблицу кириллицы для шрифта композитора (8×16, 1 бит).

Композитор рисует интерфейс встроенным моноширинным шрифтом Terminus 16
(ASCII 32..126). Клиенты Pagh — русскоязычные приложения, поэтому заголовки
окон и меню требуют кириллицы. Этот скрипт растрирует нужные символы из
DejaVu Sans Mono 13 px в ячейку 8×16 и печатает Rust-таблицу, которую
`src/font.rs` использует через `font::glyph()`.

Использование:
    python3 tools/gen_cyrillic_font.py > /tmp/cyrillic.rs
    # затем вставить содержимое в src/font.rs
"""

from __future__ import annotations

import sys

try:
    from PIL import Image, ImageDraw, ImageFont
except ImportError:  # pragma: no cover
    sys.exit("нужен Pillow (pip install pillow)")

FONT = "/usr/share/fonts/google-noto/NotoSansMono-Regular.ttf"
CELL_W, CELL_H = 8, 16
BASELINE = 12

CHARS = (
    [chr(c) for c in range(0x0410, 0x0430)]   # А-Я
    + [chr(c) for c in range(0x0430, 0x0450)]  # а-я
    + ["Ё", "ё", "«", "»", "—", "–", "…", "·", "№"]
)


def rust_char(ch: str) -> str:
    o = ord(ch)
    if ch == "\\":
        return "'\\\\'"
    if ch == "'":
        return "'\\''"
    return f"'{ch}'" if o < 0x10000 else f"'\\u{{{o:04x}}}'"


def main() -> int:
    font = ImageFont.truetype(FONT, 13)
    out = sys.stdout.write
    out("/// Дополнительные глифы (кириллица и типографские знаки), 8x16, 1 бит.\n")
    out("///\n")
    out("/// Сгенерировано `tools/gen_cyrillic_font.py` из Noto Sans Mono 13 px;\n")
    out("/// ASCII-часть остаётся в [`FONT_8X16`]. Все глифы моноширинные (8 px).\n")
    out("#[rustfmt::skip]\n")
    out("pub static EXTRA_GLYPHS: &[(char, [u8; 16])] = &[\n")
    for ch in sorted(CHARS, key=ord):
        image = Image.new("L", (CELL_W, CELL_H), 0)
        draw = ImageDraw.Draw(image)
        draw.text((0, BASELINE), ch, fill=255, font=font, anchor="ls")
        rows = []
        for y in range(CELL_H):
            bits = 0
            for x in range(CELL_W):
                if image.getpixel((x, y)) >= 128:
                    bits |= 0x80 >> x
            rows.append(bits)
        body = ", ".join(f"0x{value:02x}" for value in rows)
        out(f"    ({rust_char(ch)}, [{body}]),\n")
    out("];\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
