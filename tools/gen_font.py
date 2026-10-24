#!/usr/bin/env python3
"""Rasterise a monospaced TrueType font into the pagh framebuffer font asset.

The kernel does not parse fonts. ``src/drivers/framebuffer.rs`` embeds one flat
asset holding an **8-bit coverage** glyph for every printable ASCII codepoint —
``cell_h`` rows of ``cell_w`` bytes each, 0 = background, 255 = full ink — and
blends that coverage between the console's background and foreground colour
(``framebuffer::blend_px``).  Coverage rather than a bitmap is what makes the
console text look like the same face in a terminal on the host: a 1-bit asset at
an 8 px advance is exactly the "rough" look this format replaced.

The cell size is the contract, and the font size is *derived* from it — never the
other way round.  A monospaced face has a fixed advance width in font units; the
pixel size that makes that advance equal the cell width is::

    px = cell_w / (advance_units / units_per_em)

For Fira Code — the committed asset's face and the maintainer's terminal font —
the advance is 1200/1950 = 0.6154 em, so an 8 px cell means em 13.00 px exactly:
the advance is exactly 8.00 px and the face lands on the cell grid with no
fraction.  (DejaVu Sans Mono's 0.602 em only reaches 7.83 px at em 13, which is
why it is the fallback and not the default.)  ``--check`` fails the run when any
glyph's ink leaves the cell, so a future font swap cannot silently shear off
letter edges.

Two vertical details matter as much as the size:

* the glyph origin sits on a baseline, and that baseline is placed **once for the
  whole set** so the deepest descender clears the cell bottom (``--valign fit``).
  Per-glyph centring would make a line of text wobble; the font's own
  ascent/descent block (``--valign metrics``) is centred on the *em box*, not on
  the ink, so it leaves the descender row empty and the text riding high.
* the extents are measured from **rendered pixels**, not from
  ``font.getbbox``/``ImageFont.getbbox``: those report coordinates relative to the
  ascender line, not the baseline, and using them shifts the whole font by one
  ascent (the first version of this tool "rendered" 95 blank cells that way).

Usage::

    # regenerate the committed asset (the defaults everything is tuned for)
    python3 tools/gen_font.py --out assets/font8x18.bin

    # prove the committed asset still matches the pipeline
    python3 tools/gen_font.py --out assets/font8x18.bin --check

    # look at it, shaded as the kernel blends it
    python3 tools/gen_font.py --out assets/font8x18.bin --png /tmp/font.png --png-scale 4

Font lookup: ``--font`` wins; otherwise Fira Code locations are tried (system
dirs, then flatpak/Steam runtimes), then DejaVu Sans Mono.  The source file's
SHA-256, family and version are printed either way — the asset is only
reproducible if the face is identified.

Requires fontTools and Pillow; both are development-host tools and never reach the
kernel or CI's build job.
"""

from __future__ import annotations

import argparse
import hashlib
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]

FIRST_CP = 0x20
LAST_CP = 0x7E
GLYPHS = LAST_CP - FIRST_CP + 1  # 95, the kernel's FONT_FIRST_CP..=FONT_LAST_CP

# Font paths tried when --font is absent. Fira Code first: the committed asset is
# that face, and its 0.6154 em advance is what makes an 8 px cell exact.
FONT_CANDIDATES = (
    "/usr/share/fonts/fira-code/FiraCode-Regular.ttf",
    "/usr/share/fonts/truetype/fira-code/FiraCode-Regular.ttf",
    "/usr/share/fonts/TTF/FiraCode-Regular.ttf",
    "~/.local/share/fonts/FiraCode-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "~/.local/share/fonts/DejaVuSansMono.ttf",
)
# Directories searched recursively when the face is not installed system-wide:
# flatpak and Steam runtimes bundle it under a content hash, so the depth varies
# and only a recursive search is reliable.
FONT_SEARCH_ROOTS = ("~/.local/share/flatpak/runtime", "~/.local/share/Steam")
FONT_SEARCH_NAMES = ("FiraCode-Regular.ttf", "DejaVuSansMono.ttf")


def find_font(explicit: str | None) -> pathlib.Path:
    """Return the TTF to rasterise, or exit with every place that was searched."""
    if explicit:
        p = pathlib.Path(explicit).expanduser()
        if not p.is_file():
            sys.exit(f"gen_font: --font {p} is not a file")
        return p
    seen: list[str] = []
    for cand in FONT_CANDIDATES:
        p = pathlib.Path(cand).expanduser()
        seen.append(str(p))
        if p.is_file():
            return p
    for root in FONT_SEARCH_ROOTS:
        base = pathlib.Path(root).expanduser()
        seen.append(str(base))
        if not base.is_dir():
            continue
        for name in FONT_SEARCH_NAMES:
            for hit in sorted(base.rglob(name)):
                if hit.is_file():
                    return hit
    sys.exit(
        "gen_font: no console font found; pass --font /path/to/FiraCode-Regular.ttf\n"
        "  searched:\n    " + "\n    ".join(seen)
    )


def font_facts(path: pathlib.Path):
    """(units_per_em, advance_units, family, style, version) for a monospaced face."""
    from fontTools.ttLib import TTFont

    f = TTFont(str(path), lazy=True)
    upm = f["head"].unitsPerEm
    cmap = f.getBestCmap()
    hmtx = f["hmtx"]
    advances = {hmtx[cmap[cp]][0] for cp in range(FIRST_CP, LAST_CP + 1) if cp in cmap}
    if len(advances) != 1:
        sys.exit(
            f"gen_font: {path.name} is not monospaced over 0x20..0x7E "
            f"(found {len(advances)} distinct advances)"
        )
    name = f["name"]
    family = name.getDebugName(1) or path.stem
    style = name.getDebugName(2) or ""
    version = name.getDebugName(5) or "?"
    return upm, advances.pop(), family, style, version


def px_for_cell(cell_w: int, upm: int, advance: int) -> int:
    """Largest integer pixel size whose advance still fits the cell (floored, ≥ 1)."""
    return max(1, int(cell_w * upm / advance))


def glyph_bytes_per_glyph(cell_w: int, cell_h: int) -> int:
    """Bytes one glyph occupies: `cell_h` rows of `cell_w` coverage bytes."""
    return cell_h * cell_w


def glyph_pixel(data: bytes, cell_w: int, cell_h: int, cp: int, dx: int, dy: int) -> int:
    """One coverage byte of one glyph, read exactly as the kernel reads it.

    The asset is `95 * cell_h * cell_w` bytes: one byte per pixel, row-major, left
    to right. The glyph index is the codepoint **minus 0x20**; reading a row as
    anything other than `cell_w` bytes is the one mistake that stays silent — it
    shifts every row of every glyph after the first.
    """
    base = (cp - FIRST_CP) * glyph_bytes_per_glyph(cell_w, cell_h)
    return data[base + dy * cell_w + dx]


def rasterise(
    font_path: pathlib.Path,
    px: int,
    cell_w: int,
    cell_h: int,
    valign: str,
    bottom_margin: int,
    gamma: float = 1.0,
    alpha_floor: int = 0,
    alpha_ceil: int = 255,
):
    """Render 0x20..0x7E into `cell_w`×`cell_h` coverage rows.

    Returns ``(rows, info)``: ``rows[cp - 0x20]`` is ``cell_h * cell_w`` coverage
    bytes in row-major order; ``info`` carries the layout it settled on (measured
    ink extents relative to the baseline, the baseline row, and any glyph whose ink
    escapes the cell).

    ``gamma`` shapes the coverage ramp before it is stored (above 1 darkens the
    strokes, below 1 lightens them — the maintainer's terminal runs this face at
    weight 250, so a light touch of gamma is what makes the two read as one font),
    ``alpha_floor`` drops coverage at or below that value to zero (kills the faint
    noise the rasteriser leaves in the corners of a cell) and ``alpha_ceil`` snaps
    coverage at or above it to full ink (keeps stems solid, not translucent).
    """
    from PIL import Image, ImageDraw, ImageFont

    font = ImageFont.truetype(str(font_path), px)
    ascent, descent = font.getmetrics()
    band = px * 4 + 16  # measuring canvas: room for accents above and descenders below

    def render(ch: str, baseline_y: int, height: int, width: int):
        """One glyph on a canvas whose y = `baseline_y` is the baseline."""
        img = Image.new("L", (width, height), 0)
        ImageDraw.Draw(img).text((width // 2, baseline_y), ch, font=font, fill=255)
        return img

    # Pass 1: measure the real ink extents, in pixels, relative to the baseline.
    # PIL's `getbbox` reports coordinates relative to the *ascender line*, not the
    # baseline, so it cannot be used for this; rendering and reading the pixels
    # cannot be off by an ascent.
    ref_base = band // 2
    ink_top = 0
    ink_bottom = 0
    seen = False
    for cp in range(FIRST_CP, LAST_CP + 1):
        img = render(chr(cp), ref_base, band, max(cell_w, 8) + px * 4)
        box = img.getbbox()
        if box is None:
            continue  # blank glyph (space): no ink to align
        top, bottom = box[1] - ref_base, box[3] - ref_base
        ink_top = min(ink_top, top) if seen else top
        ink_bottom = max(ink_bottom, bottom) if seen else bottom
        seen = True
    if not seen:
        sys.exit("gen_font: font has no ink in the printable ASCII range")

    span = ink_bottom - ink_top
    if span > cell_h:
        sys.exit(
            f"gen_font: the ink of 0x20..0x7E spans {span}px, which cannot fit a "
            f"{cell_h}px cell even with no margin; raise --cell-h or lower --size"
        )

    if valign == "fit":
        # Anchor the descender: at least `bottom_margin` blank rows below the ink,
        # the rest of the slack goes on top. This is stable across fonts — "centre
        # the ink" would depend on whether an accent happens to be in the set.
        baseline = cell_h - bottom_margin - ink_bottom
    else:  # "metrics": the font's own ascent/descent block, centred in the cell
        baseline = (cell_h - (ascent + descent)) // 2 + ascent
        baseline = max(baseline, ink_bottom + bottom_margin)
        baseline = min(baseline, cell_h - 1 + ink_top)

    huge = px * 8 + 64  # canvas big enough for any bearing at this size
    rows: list[list[int]] = []
    clipped: list[tuple[str, int, int]] = []
    clipped_v: list[tuple[str, int, int]] = []
    for cp in range(FIRST_CP, LAST_CP + 1):
        ch = chr(cp)
        img = render(ch, huge // 2, huge, huge)
        # Crop to the cell: the cell's top row is `baseline` rows above the
        # baseline, and its first column is the pen origin (the glyph is positioned
        # by origin, not by ink — that is what keeps a monospaced line in columns).
        left = huge // 2
        top = huge // 2 - baseline
        cell = img.crop((left, top, left + cell_w, top + cell_h))
        cov: list[int] = []
        for y in range(cell_h):
            for x in range(cell_w):
                v = cell.getpixel((x, y))
                if gamma != 1.0:
                    v = round(255.0 * (v / 255.0) ** gamma)
                if v <= alpha_floor:
                    v = 0
                elif v >= alpha_ceil:
                    v = 255
                cov.append(v)
        # Ink escaping the cell is only visible in the uncropped render.
        full = img.getbbox()
        if full is not None:
            l, r = full[0] - left, full[2] - left
            t, b = full[1] - (huge // 2), full[3] - (huge // 2)
            if l < 0 or r > cell_w:
                clipped.append((ch, l, r))
            if t < -baseline or b > cell_h - baseline:
                clipped_v.append((ch, t, b))
        rows.append(cov)
    info = {
        "ascent": ascent,
        "descent": descent,
        "baseline": baseline,
        "ink_top": ink_top,
        "ink_bottom": ink_bottom,
        "bottom_margin": cell_h - baseline - ink_bottom,
        "top_margin": baseline + ink_top,
        "clipped": clipped,
        "clipped_v": clipped_v,
    }
    return rows, info


def render_bytes(rows: list[list[int]]) -> bytes:
    return bytes(b for glyph in rows for b in glyph)


def preview(png: pathlib.Path, sample: str, cell_w: int, cell_h: int, data: bytes, scale: int) -> None:
    """Render the asset the way the kernel will, so the result can be looked at.

    The kernel blends each coverage byte between its background and foreground
    colour, so the preview shades the same way: what this PNG shows is the bitmap
    the kernel will blit, not a second rendering of the TTF that could disagree
    with it.
    """
    from PIL import Image

    width = max(1, len(sample) * cell_w)
    img = Image.new("RGB", (width + 4, cell_h), (0, 0, 0))
    px = img.load()
    for i, ch in enumerate(sample):
        cp = ord(ch)
        if not FIRST_CP <= cp <= LAST_CP:
            continue
        for dy in range(cell_h):
            for dx in range(cell_w):
                c = glyph_pixel(data, cell_w, cell_h, cp, dx, dy)
                px[2 + i * cell_w + dx, dy] = (c, c, c)
    if scale > 1:
        img = img.resize((img.width * scale, img.height * scale), Image.NEAREST)
    out = png if png.is_absolute() else ROOT / png
    out.parent.mkdir(parents=True, exist_ok=True)
    img.save(out)
    print(f"  preview  {out} ({len(sample)} chars, {img.width}x{img.height} px, scale {scale})")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument(
        "--font",
        help="TTF/TTC to rasterise (default: Fira Code Regular, then DejaVu Sans Mono)",
    )
    ap.add_argument("--out", required=True, help="asset to write (e.g. assets/font8x18.bin)")
    ap.add_argument(
        "--cell",
        type=int,
        default=8,
        help="cell width in pixels; the font size is derived from it (default 8)",
    )
    ap.add_argument("--cell-h", type=int, default=18, help="cell height in pixels (default 18)")
    ap.add_argument(
        "--size",
        type=int,
        help="override the derived pixel size (the derived one is the largest that fits)",
    )
    ap.add_argument(
        "--valign",
        choices=("fit", "metrics"),
        default="fit",
        help="fit: anchor the baseline so the descenders clear the cell bottom (default); "
        "metrics: centre the font's own ascent/descent block",
    )
    ap.add_argument(
        "--bottom-margin",
        type=int,
        default=1,
        help="blank pixel rows kept under the deepest descender with --valign fit (default 1)",
    )
    ap.add_argument(
        "--gamma",
        type=float,
        default=1.0,
        help="coverage ramp shaping: >1 darkens the strokes, <1 lightens them (default 1.0)",
    )
    ap.add_argument(
        "--alpha-floor",
        type=int,
        default=0,
        help="coverage at or below this becomes 0, dropping rasteriser noise (default 0)",
    )
    ap.add_argument(
        "--alpha-ceil",
        type=int,
        default=255,
        help="coverage at or above this becomes 255, keeping stems solid (default 255)",
    )
    ap.add_argument("--check", action="store_true", help="fail if the asset is not byte-identical")
    ap.add_argument(
        "--png", help="also write a preview PNG of this asset, shaded as the kernel blends it"
    )
    ap.add_argument("--png-sample", default=None, help="text to draw in the preview PNG")
    ap.add_argument("--png-scale", type=int, default=1, help="integer upscale for the preview")
    ap.add_argument(
        "--allow-clip",
        action="store_true",
        help="write the asset even if glyphs overflow the cell",
    )
    args = ap.parse_args()
    if not 1 <= args.cell <= 64:
        ap.error("--cell must be 1..64")

    font_path = find_font(args.font)
    raw = font_path.read_bytes()
    sha = hashlib.sha256(raw).hexdigest()
    upm, advance, family, style, version = font_facts(font_path)
    px = args.size or px_for_cell(args.cell, upm, advance)
    eff = px * advance / upm

    print(f"gen_font: {family} {style} ({version})")
    print(f"  source   {font_path}")
    print(f"  sha256   {sha}")
    print(f"  metrics  upm={upm} advance={advance} ({advance / upm:.4f} em)")
    print(
        f"  size     cell {args.cell}x{args.cell_h} -> em {px}px, "
        f"advance {eff:.2f}px, {args.cell - eff:+.2f}px slack"
    )
    print(
        f"  coverage gamma {args.gamma}, floor {args.alpha_floor}, ceil {args.alpha_ceil}; "
        f"{glyph_bytes_per_glyph(args.cell, args.cell_h)} byte(s) per glyph"
    )

    rows, info = rasterise(
        font_path,
        px,
        args.cell,
        args.cell_h,
        args.valign,
        args.bottom_margin,
        args.gamma,
        args.alpha_floor,
        args.alpha_ceil,
    )
    data = render_bytes(rows)
    expect = GLYPHS * glyph_bytes_per_glyph(args.cell, args.cell_h)
    if len(data) != expect:
        sys.exit(f"gen_font: produced {len(data)} bytes, expected {expect}")

    print(
        f"  layout   baseline row {info['baseline']} of {args.cell_h}, ink "
        f"{info['ink_top']:+d}..{info['ink_bottom']:+d} rel. baseline "
        f"(margins top={info['top_margin']} bottom={info['bottom_margin']})"
    )
    if info["clipped"]:
        chars = " ".join(repr(c) for c, _, _ in info["clipped"])
        msg = (
            f"gen_font: {len(info['clipped'])} glyph(s) overflow the {args.cell}px cell: {chars}\n"
            f"  use a smaller --size or a wider --cell"
        )
        if not args.allow_clip:
            sys.exit(msg)
        print(f"  WARNING {msg}")
    if info["clipped_v"]:
        chars = " ".join(repr(c) for c, _, _ in info["clipped_v"])
        msg = (
            f"gen_font: {len(info['clipped_v'])} glyph(s) overflow the {args.cell_h}px cell "
            f"vertically: {chars}\n  raise --cell-h or lower --size"
        )
        if not args.allow_clip:
            sys.exit(msg)
        print(f"  WARNING {msg}")

    out = pathlib.Path(args.out)
    if not out.is_absolute():
        out = ROOT / out
    if args.check:
        if not out.is_file():
            sys.exit(f"gen_font: --check: {out} does not exist")
        have = out.read_bytes()
        if have != data:
            sys.exit(
                f"gen_font: {out} does not match this pipeline "
                f"({len(have)} bytes vs {len(data)})"
            )
        print(f"  check    {out.relative_to(ROOT)} matches ({len(data)} bytes)")
    else:
        out.parent.mkdir(parents=True, exist_ok=True)
        changed = (not out.is_file()) or out.read_bytes() != data
        out.write_bytes(data)
        print(
            f"  write    {out.relative_to(ROOT)} ({len(data)} bytes"
            f"{', changed' if changed else ', unchanged'})"
        )

    if args.png:
        sample = args.png_sample or "Welcome to pagh OS Shell! 0123456789 {}[]<>;:.,|/\\"
        preview(pathlib.Path(args.png), sample, args.cell, args.cell_h, data, args.png_scale)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())