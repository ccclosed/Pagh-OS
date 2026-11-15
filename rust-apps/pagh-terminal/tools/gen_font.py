#!/usr/bin/env python3
"""Rasterise the terminal's bitmap font from Fira Code + Noto Sans CJK.

The terminal renderer does not parse fonts at runtime.  This script bakes two
flat coverage assets and a Rust codepoint index:

* ``assets/font8x18.bin``       — 8x18 glyphs, one byte of coverage per pixel,
  row-major, one glyph per codepoint listed in ``src/font_index.rs``.
* ``assets/font_wide8x18.bin``  — 16x18 glyphs for East-Asian-wide characters
  (CJK punctuation, kana, fullwidth forms, and the curated ideographs below),
  same coverage encoding.
* ``src/font_index.rs``         — sorted codepoint arrays.  A codepoint's array
  position is also its glyph index, so lookup is a binary search and no offset
  table is needed.

Coverage (0 = background, 255 = full ink) rather than 1-bit pixels keeps the
terminal looking like the host's Fira Code at the same 13 px em; the kernel
console uses the same face and the same 8x18 cell (``assets/font8x18.bin`` at
the repository root), so the desktop terminal and the kernel VT read as one
font family.

The cell size is the contract; the pixel size is derived from it.  Fira Code's
advance is 1200/1950 em, so an 8 px cell is exactly 13 px.  The baseline is
measured from rendered ASCII ink (not from ``getbbox`` relative to the
ascender) and kept for every glyph in the set.

Licensing: the generated assets are derived from Fira Code (SIL OFL 1.1) and
Noto Sans CJK (SIL OFL 1.1).  The notices and the OFL text are in
``fonts/LICENSES.md`` and ``fonts/OFL-1.1.txt``; the assets are distributed
under OFL 1.1, the Rust code under MIT.

Requires Pillow + fontTools (host-only; never part of the crate build).

Usage::

    python3 tools/gen_font.py                 # regenerate assets + index
    python3 tools/gen_font.py --check         # fail if committed assets differ
    python3 tools/gen_font.py --preview /tmp/font.png
"""

from __future__ import annotations

import argparse
import hashlib
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
ASSETS = ROOT / "assets"
INDEX = ROOT / "src" / "font_index.rs"
UNICODE = ROOT / "src" / "unicode_tables.rs"

CELL_W = 8
CELL_H = 18
WIDE_W = 16
# Noto Sans Mono CJK SC: face 7 in the Debian TTC.  Mono CJK keeps ideographs
# exactly one em wide, which is what the 16 px double cell needs.
CJK_FACE_INDEX = 7

# Font lookup: the committed Fira Code face first, then the usual distro paths.
FIRA_CANDIDATES = (
    "/usr/share/fonts/fira-code/FiraCode-Regular.ttf",
    "/usr/share/fonts/truetype/fira-code/FiraCode-Regular.ttf",
    "/usr/share/fonts/TTF/FiraCode-Regular.ttf",
    "~/.local/share/fonts/FiraCode-Regular.ttf",
)
CJK_CANDIDATES = (
    "/usr/share/fonts/google-noto-sans-cjk-fonts/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
)

# Narrow (single-cell) codepoint ranges.  The generator drops codepoints the
# face does not have and codepoints whose Unicode East-Asian width is Wide or
# Fullwidth (those belong in the wide asset).
NARROW_RANGES = (
    (0x0020, 0x007E),  # printable ASCII
    (0x00A0, 0x00FF),  # Latin-1 supplement
    (0x0100, 0x017F),  # Latin extended-A
    (0x0180, 0x024F),  # Latin extended-B
    (0x0370, 0x03FF),  # Greek
    (0x0400, 0x04FF),  # Cyrillic + supplement
    (0x0500, 0x052F),  # Cyrillic supplement
    (0x2000, 0x206F),  # general punctuation
    (0x2070, 0x209F),  # super/subscripts
    (0x20A0, 0x20BF),  # currency
    (0x2100, 0x214F),  # letterlike symbols
    (0x2150, 0x218F),  # number forms
    (0x2190, 0x21FF),  # arrows
    (0x2200, 0x22FF),  # mathematical operators
    (0x2300, 0x23FF),  # technical
    (0x2460, 0x24FF),  # enclosed alphanumerics
    (0x2500, 0x257F),  # box drawing
    (0x2580, 0x259F),  # block elements
    (0x25A0, 0x25FF),  # geometric shapes
    (0x2600, 0x26FF),  # miscellaneous symbols
    (0x2700, 0x27BF),  # dingbats
    (0x2800, 0x28FF),  # braille patterns
    (0x2B00, 0x2BFF),  # miscellaneous symbols and arrows
    (0x2E80, 0x2EFF),  # CJK radicals supplement (narrow ones only)
)

# Wide (double-cell) codepoint ranges rendered from Noto Sans CJK.
WIDE_RANGES = (
    (0x3000, 0x303F),  # CJK punctuation
    (0x3040, 0x309F),  # hiragana
    (0x30A0, 0x30FF),  # katakana
    (0xFF01, 0xFF60),  # fullwidth ASCII + punctuation
)

# Common ideographs used by the demos/screenshots and by people typing a few
# Chinese words.  Full CJK coverage would be ~5 MB; missing wide codepoints are
# rendered as a two-cell placeholder box and are still advanced by two cells.
CURATED_WIDE = (
    "你好世界中文终端宽字符测试欢迎使用帕格操作系统文件系统内核桌面窗口命令目录"
    "用户密码网络设置模拟器表头行列屏幕字体颜色滚动选择复制粘贴根目录主文件夹"
    "新建立即删除重命名属性打开保存关闭最小化最大化恢复窗口管理器面板状态栏"
    "火狐浏览器编辑器终端模拟器截图键盘鼠标触控板显示器分辨率主题壁纸"
    "一二三四五六七八九十百千万亿年月日时分秒上下左右前后内外大小"
)


def find_font(explicit: str | None, candidates: tuple[str, ...], label: str) -> pathlib.Path:
    if explicit:
        p = pathlib.Path(explicit).expanduser()
        if not p.is_file():
            sys.exit(f"gen_font: --{label} {p} is not a file")
        return p
    for cand in candidates:
        p = pathlib.Path(cand).expanduser()
        if p.is_file():
            return p
    sys.exit(f"gen_font: no {label} font found; pass --{label}-font /path/to/font")


def font_facts(
    path: pathlib.Path,
    cps: tuple[int, ...] = tuple(range(0x20, 0x7F)),
    face_index: int = 0,
):
    from fontTools.ttLib import TTFont

    f = TTFont(str(path), lazy=True, fontNumber=face_index)
    upm = f["head"].unitsPerEm
    cmap = f.getBestCmap()
    hmtx = f["hmtx"]
    advances = {hmtx[cmap[cp]][0] for cp in cps if cp in cmap}
    name = f["name"]
    family = name.getDebugName(1) or path.stem
    version = name.getDebugName(5) or "?"
    return upm, advances, family, cmap, version


def sha256(path: pathlib.Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def render_glyph(font, ch: str, cell_w: int, cell_h: int, baseline: int):
    """Return cell_h*cell_w coverage bytes for `ch`, cropped at the pen origin."""
    from PIL import Image, ImageDraw

    huge = 220
    origin = huge // 2
    img = Image.new("L", (huge, huge), 0)
    ImageDraw.Draw(img).text((origin, origin), ch, font=font, fill=255, anchor="ls")
    top = origin - baseline
    cell = img.crop((origin, top, origin + cell_w, top + cell_h))
    return bytes(cell.getpixel((x, y)) for y in range(cell_h) for x in range(cell_w))


def measure_ascii_baseline(font, cell_w: int, cell_h: int) -> tuple[int, int, int, int]:
    """Return (baseline, ink_top, ink_bottom, bottom_margin) measured from ink."""
    from PIL import Image, ImageDraw

    band, origin = 160, 80
    ink_top, ink_bottom, seen = 0, 0, False
    for cp in range(0x20, 0x7F):
        img = Image.new("L", (band, band), 0)
        ImageDraw.Draw(img).text((origin, origin), chr(cp), font=font, fill=255, anchor="ls")
        box = img.getbbox()
        if not box:
            continue
        top, bottom = box[1] - origin, box[3] - origin
        ink_top = min(ink_top, top) if seen else top
        ink_bottom = max(ink_bottom, bottom) if seen else bottom
        seen = True
    if not seen:
        sys.exit("gen_font: the font has no ink in printable ASCII")
    span = ink_bottom - ink_top
    if span > cell_h:
        sys.exit(f"gen_font: ASCII ink spans {span}px, taller than the {cell_h}px cell")
    baseline = cell_h - 1 - max(0, ink_bottom)
    return baseline, ink_top, ink_bottom, cell_h - baseline


def eaw_wide(ch: str) -> bool:
    import unicodedata

    return unicodedata.east_asian_width(ch) in ("W", "F")


def build() -> tuple[bytes, bytes, str, dict]:
    from PIL import Image, ImageDraw, ImageFont

    fira_path = find_font(None, FIRA_CANDIDATES, "fira")
    cjk_path = find_font(None, CJK_CANDIDATES, "cjk")

    fira_upm, fira_advances, fira_family, fira_cmap, fira_version = font_facts(fira_path)
    if len(fira_advances) != 1:
        sys.exit(f"gen_font: {fira_path} is not monospaced over ASCII: {sorted(fira_advances)}")
    px = max(1, int(CELL_W * fira_upm / fira_advances.pop()))
    fira = ImageFont.truetype(str(fira_path), px)
    baseline, ink_top, ink_bottom, bottom_margin = measure_ascii_baseline(fira, CELL_W, CELL_H)

    narrow_cps: list[int] = []
    for first, last in NARROW_RANGES:
        for cp in range(first, last + 1):
            ch = chr(cp)
            if cp not in fira_cmap or eaw_wide(ch):
                continue
            narrow_cps.append(cp)
    narrow_cps = sorted(set(narrow_cps))

    narrow = bytearray()
    clipped: list[tuple[int, int, int]] = []
    for cp in narrow_cps:
        cov = render_glyph(fira, chr(cp), CELL_W, CELL_H, baseline)
        narrow.extend(cov)

    # Wide glyphs: Noto Sans CJK for CJK/kana/fullwidth.  Its ideograph advance
    # is one em, hence a 16 px cell at em 16; the same baseline row as the
    # narrow face keeps mixed CJK/Latin lines on one line.
    cjk_upm, _, cjk_family, cjk_cmap, cjk_version = font_facts(
        cjk_path, (0x4E00, 0x3002, 0x3042, 0x30A2, 0xFF21), face_index=CJK_FACE_INDEX
    )
    wide_px = max(1, int(WIDE_W * cjk_upm / cjk_upm))
    cjk = ImageFont.truetype(str(cjk_path), wide_px, index=CJK_FACE_INDEX)
    wide_cps: list[int] = []
    for first, last in WIDE_RANGES:
        for cp in range(first, last + 1):
            if cp in cjk_cmap and eaw_wide(chr(cp)):
                wide_cps.append(cp)
    for ch in CURATED_WIDE:
        cp = ord(ch)
        if cp in cjk_cmap and eaw_wide(ch):
            wide_cps.append(cp)
    wide_cps = sorted(set(wide_cps))

    wide = bytearray()
    for cp in wide_cps:
        wide.extend(render_glyph(cjk, chr(cp), WIDE_W, CELL_H, baseline))

    # Report glyphs whose ink leaves the cell; this is the one silent failure
    # mode of a bitmap font.  Keep it fatal unless --allow-clip is passed.
    for cp in narrow_cps:
        img = Image.new("L", (64, 64), 0)
        ImageDraw.Draw(img).text((32, 32), chr(cp), font=fira, fill=255, anchor="ls")
        box = img.getbbox()
        if not box:
            continue
        l, r = box[0] - 32, box[2] - 32
        t, b = box[1] - 32, box[3] - 32
        if l < 0 or r > CELL_W or t < -baseline or b > CELL_H - baseline:
            clipped.append((cp, l, r))

    unicode_tables = render_unicode_tables()
    meta = {
        "fira": (fira_path, fira_family, fira_version, sha256(fira_path), px),
        "cjk": (cjk_path, cjk_family, cjk_version, sha256(cjk_path), wide_px),
        "baseline": baseline,
        "ink_top": ink_top,
        "ink_bottom": ink_bottom,
        "bottom_margin": bottom_margin,
        "narrow": len(narrow_cps),
        "wide": len(wide_cps),
        "clipped": clipped,
        "unicode_tables": unicode_tables,
        "unicode_version": __import__("unicodedata").unidata_version,
    }
    return bytes(narrow), bytes(wide), render_index(narrow_cps, wide_cps), meta


def render_unicode_tables() -> str:
    """Generate sorted East-Asian-wide and zero-width codepoint ranges.

    Those two predicates are all the parser needs for "simple wide": combining
    marks and format controls occupy no cell, East-Asian Wide/Fullwidth
    codepoints occupy two, everything else one.  Ambiguous-width characters are
    one cell, which is the common terminal default.
    """
    import unicodedata

    def ranges(pred) -> list[tuple[int, int]]:
        out: list[tuple[int, int]] = []
        start = None
        last = -2
        for cp in range(0x110000):
            ch = chr(cp)
            if unicodedata.category(ch) == "Cn":
                hit = False
            else:
                hit = pred(ch)
            if hit:
                if start is None:
                    start = cp
                last = cp
            elif start is not None:
                out.append((start, last))
                start = None
        if start is not None:
            out.append((start, last))
        return out

    wide = ranges(lambda ch: unicodedata.east_asian_width(ch) in ("W", "F"))
    zero = ranges(lambda ch: unicodedata.category(ch) in ("Mn", "Me", "Cf"))

    def table(name: str, rs: list[tuple[int, int]]) -> str:
        lines = ["#[rustfmt::skip]", f"pub static {name}: &[(u32, u32)] = &["]
        row: list[str] = []
        for a, b in rs:
            row.append(f"(0x{a:04X}, 0x{b:04X})")
            if len(row) == 5:
                lines.append("    " + ", ".join(row) + ",")
                row = []
        if row:
            lines.append("    " + ", ".join(row) + ",")
        lines.append("];")
        return "\n".join(lines)

    return (
        "// Generated by tools/gen_font.py from Python unicodedata "
        f"{unicodedata.unidata_version} — do not edit by hand.\n"
        "//\n"
        "// WIDE: East_Asian_Width W/F (two cells).  ZERO_WIDTH: Unicode\n"
        "// categories Mn/Me/Cf (combining marks and format controls, no cell).\n"
        "// Ranges are sorted and non-overlapping.\n\n"
        + table("WIDE", wide)
        + "\n\n"
        + table("ZERO_WIDTH", zero)
        + "\n"
    )


def render_index(narrow_cps: list[int], wide_cps: list[int]) -> str:
    def one(name: str, cps: list[int]) -> str:
        lines = ["#[rustfmt::skip]", f"pub static {name}: &[u32] = &["]
        row: list[str] = []
        for cp in cps:
            row.append(f"0x{cp:04X}")
            if len(row) == 12:
                lines.append("    " + ", ".join(row) + ",")
                row = []
        if row:
            lines.append("    " + ", ".join(row) + ",")
        lines.append("];")
        return "\n".join(lines)

    return (
        "// Generated by tools/gen_font.py — do not edit by hand.\n"
        "//\n"
        "// The array position is the glyph index in the matching asset; both\n"
        "// arrays are sorted ascending and looked up with binary_search.\n\n"
        + one("NARROW_CODEPOINTS", narrow_cps)
        + "\n\n"
        + one("WIDE_CODEPOINTS", wide_cps)
        + "\n"
    )


def write_outputs(narrow: bytes, wide: bytes, index: str, meta: dict) -> None:
    ASSETS.mkdir(parents=True, exist_ok=True)
    (ASSETS / "font8x18.bin").write_bytes(narrow)
    (ASSETS / "font_wide8x18.bin").write_bytes(wide)
    INDEX.write_text(index)
    UNICODE.write_text(meta["unicode_tables"])
    facts = (
        "Generated by tools/gen_font.py. Do not edit.\n"
        f"narrow glyphs: {meta['narrow']} x {CELL_W}x{CELL_H} coverage bytes\n"
        f"wide glyphs:   {meta['wide']} x {WIDE_W}x{CELL_H} coverage bytes\n"
        f"baseline row:  {meta['baseline']} (ink bottom {meta['ink_bottom']}, "
        f"bottom margin {meta['bottom_margin']})\n"
        f"source 1: {meta['fira'][0]}\n"
        f"  family {meta['fira'][1]} {meta['fira'][2]}; em {meta['fira'][4]} px; "
        f"sha256 {meta['fira'][3]}\n"
        f"source 2: {meta['cjk'][0]}\n"
        f"  family {meta['cjk'][1]} {meta['cjk'][2]}; em {meta['cjk'][4]} px; "
        f"sha256 {meta['cjk'][3]}\n"
        f"clipped narrow glyphs: {len(meta['clipped'])}\n"
        f"unicode tables: Unicode {meta['unicode_version']}\n"
    )
    (ASSETS / "font_meta.txt").write_text(facts)


def preview(path: pathlib.Path, meta: dict) -> None:
    """Render a small sample exactly like the Rust renderer blends coverage."""
    from PIL import Image, ImageDraw, ImageFont

    narrow = (ASSETS / "font8x18.bin").read_bytes()
    wide = (ASSETS / "font_wide8x18.bin").read_bytes()
    index = (ROOT / "src" / "font_index.rs").read_text()
    narrow_cps, wide_cps = parse_index(index)
    fira_path = pathlib.Path(meta["fira"][0])
    cjk_path = pathlib.Path(meta["cjk"][0])
    fira = ImageFont.truetype(str(fira_path), meta["fira"][4])
    cjk = ImageFont.truetype(str(cjk_path), meta["cjk"][4], index=CJK_FACE_INDEX)
    baseline = meta["baseline"]

    def glyph(ch: str):
        cp = ord(ch)
        if cp in narrow_cps:
            i = narrow_cps.index(cp)
            cov = narrow[i * CELL_W * CELL_H : (i + 1) * CELL_W * CELL_H]
            return CELL_W, cov
        if cp in wide_cps:
            i = wide_cps.index(cp)
            cov = wide[i * WIDE_W * CELL_H : (i + 1) * WIDE_W * CELL_H]
            return WIDE_W, cov
        return 0, b""

    sample = [
        "Pagh OS terminal — Fira Code 8x18 + Noto CJK",
        "Привет, мир! Ёё 0123456789 {}[]<>",
        "┌─────────┬─────────┐  → ✓ • ≤ ≥",
        "│ colors  │ 256     │  ▓▓▒▒░░██",
        "└─────────┴─────────┘  界 語 かな カナ",
        "你好，世界！ 全角ＡＢＣ１２３",
    ]
    scale = 3
    pad = 6
    width = max(sum(2 if eaw_wide(c) else 1 for c in line) for line in sample) * CELL_W + pad * 2
    height = len(sample) * CELL_H + pad * 2
    img = Image.new("RGB", (width, height), (14, 22, 28))
    pix = img.load()
    fg, bg = (216, 230, 227), (14, 22, 28)
    y = pad
    for line in sample:
        x = pad
        for ch in line:
            w, cov = glyph(ch)
            if w == 0:
                x += CELL_W
                continue
            for row in range(CELL_H):
                for col in range(w):
                    a = cov[row * w + col]
                    if a:
                        pix[x + col, y + row] = tuple(
                            int(bg[i] + (fg[i] - bg[i]) * a / 255) for i in range(3)
                        )
            x += w
        y += CELL_H
    img = img.resize((width * scale, height * scale), Image.NEAREST)
    img.save(path)
    print(f"preview: {path} ({img.width}x{img.height})")


def parse_index(text: str) -> tuple[list[int], list[int]]:
    import re

    def one(name: str) -> list[int]:
        m = re.search(rf"pub static {name}: &\[u32\] = &\[(.*?)\];", text, re.S)
        if not m:
            sys.exit(f"parse_index: {name} missing")
        return [int(v, 16) for v in re.findall(r"0x([0-9A-Fa-f]+)", m.group(1))]

    return one("NARROW_CODEPOINTS"), one("WIDE_CODEPOINTS")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--check", action="store_true", help="fail if generated files differ")
    ap.add_argument("--allow-clip", action="store_true", help="write even if glyph ink clips")
    ap.add_argument("--preview", type=pathlib.Path, help="write a shaded PNG preview")
    args = ap.parse_args()

    narrow, wide, index, meta = build()
    clipped = meta["clipped"]
    if clipped and not args.allow_clip:
        print(f"gen_font: {len(clipped)} narrow glyphs clip the cell; first: {clipped[:8]}", file=sys.stderr)
        print("gen_font: pass --allow-clip only after checking the preview", file=sys.stderr)
        return 2

    if args.check:
        expected = {
            ASSETS / "font8x18.bin": narrow,
            ASSETS / "font_wide8x18.bin": wide,
            INDEX: index.encode(),
            UNICODE: meta["unicode_tables"].encode(),
        }
        bad = [p for p, data in expected.items() if not p.is_file() or p.read_bytes() != data]
        if bad:
            print("gen_font: generated output differs from committed files:", file=sys.stderr)
            for p in bad:
                print(f"  {p.relative_to(ROOT)}", file=sys.stderr)
            return 1
        print(
            f"gen_font: OK — {meta['narrow']} narrow + {meta['wide']} wide glyphs, "
            f"baseline {meta['baseline']}, em {meta['fira'][4]}px"
        )
        return 0

    write_outputs(narrow, wide, index, meta)
    print(
        f"gen_font: wrote {meta['narrow']} narrow ({CELL_W}x{CELL_H}) + "
        f"{meta['wide']} wide ({WIDE_W}x{CELL_H}) glyphs; baseline {meta['baseline']}"
    )
    if args.preview:
        preview(args.preview, meta)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())