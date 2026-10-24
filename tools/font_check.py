#!/usr/bin/env python3
"""Prove that the on-screen console glyphs are the committed font asset, pixel for pixel.

The kernel never parses a font: ``src/drivers/framebuffer.rs`` embeds
``assets/font8x18.bin`` — 95 coverage glyphs, ``cell_h`` rows of ``cell_w`` bytes,
0 = background and 255 = full ink — and blends each byte between the console's
background and foreground colour (``framebuffer::blend_px``).  The asset is
therefore *the* contract, and the only honest way to check the console typeface is
to compare what QEMU put on the screen against those bytes.

Three failure modes motivated this tool, and all three look fine to a human eye:

* the asset and the kernel constants drift apart (a regenerated font with a
  different cell, an edited ``CHAR_HEIGHT``) — the screen then shows a scaled or
  sheared glyph and every e2e test still passes, because they only grep serial
  text;
* the screen is not the asset at all: a stale ``iso_root/pagh.elf``, or a build
  that still embeds a different asset than the tree claims;
* the blend is wrong (coverage read from the wrong pixel, a row treated as shorter
  than ``cell_w``) — the text still reads, but every glyph is subtly the wrong
  shape, which no serial marker can catch.

Modes
-----

**Asset mode** (default, no QEMU): assert the layout contract — 95 glyphs
(0x20..0x7E) × ``cell_h`` × ``cell_w`` bytes = 95×18×8 = 13680 bytes, exactly one
blank glyph and it is space, partial coverage present (an all-or-nothing asset is
no longer antialiased), the ink bounding box of the whole set reported relative to
the derived baseline row — and cross-check the kernel's ``CHAR_WIDTH``/
``CHAR_HEIGHT`` constants, which are the other half of the contract.  ``--text``
renders a string from the asset bytes only (never from a TTF) as ASCII art and,
with ``--out``, as a PNG: that raster is the "expected" image the screenshot is
later compared against.

**Screenshot mode** (``--screenshot PNG --expect "text" --row R [--col C]``):
build the expected pixels by blending ``--bg``/``--fg`` with the asset coverage
and compare them with the screenshot pixel for pixel, printing the number of
differing pixels and exiting non-zero on any difference.  ``--find`` scans the
whole screen for the position where the text matches exactly — that is how the
row/col is *determined* rather than guessed.  ``--scan-all`` checks every console
cell above the status bar: each must be blank or exactly one asset glyph, which is
the difference between "the banner is right" and "the screen is the asset".

The comparison is deliberately exact: no tolerance, no "close enough" threshold,
and a mismatch is never downgraded to a warning.  When the screen disagrees, the
tool says how many pixels disagree and writes ``--diff-out`` so the disagreement
can be looked at.

Exit codes: 0 = every check passed / the pixels match; 1 = contract violation or a
non-zero pixel difference; 2 = usage or I/O error.

Requires Pillow (already a development-host tool); nothing here reaches the
kernel or CI's build job.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]

FIRST_CP = 0x20
LAST_CP = 0x7E
GLYPHS = LAST_CP - FIRST_CP + 1  # 95, the kernel's FONT_FIRST_CP..=FONT_LAST_CP

# The cell *width* is the asset's row length and the kernel's CHAR_WIDTH; the cell
# *height* is CHAR_HEIGHT. Both are cross-checked against the kernel source.
CELL_WIDTH = 8
DEFAULT_CELL_HEIGHT = 18
DEFAULT_ASSET = ROOT / "assets" / "font8x18.bin"
DEFAULT_KERNEL_SOURCE = ROOT / "src" / "drivers" / "framebuffer.rs"

# Console colours the screenshot is compared against. White on black is what the
# console draws by default; a themed console passes its own pair.
DEFAULT_BG = "000000"
DEFAULT_FG = "FFFFFF"


def die(msg: str) -> None:
    """Usage/IO failure: exit 2 so a caller can tell it from a real mismatch."""
    print(f"font_check: {msg}", file=sys.stderr)
    raise SystemExit(2)


def resolve(value: str) -> pathlib.Path:
    """A path relative to the repository root, like the other tools in this folder."""
    p = pathlib.Path(value)
    return p if p.is_absolute() else ROOT / p


def parse_hex(value: str) -> tuple[int, int, int]:
    v = value.lstrip("#")
    if len(v) != 6 or any(c not in "0123456789abcdefABCDEF" for c in v):
        die(f"--bg/--fg want RRGGBB, got {value!r}")
    return tuple(int(v[i : i + 2], 16) for i in (0, 2, 4))  # type: ignore[return-value]


def blend(bg: tuple[int, int, int], fg: tuple[int, int, int], cov: int) -> tuple[int, int, int]:
    """The kernel's ``blend_px``: coverage 0 leaves the background, 255 is the ink."""
    if cov == 0:
        return bg
    if cov == 255:
        return fg
    return tuple((b * (255 - cov) + f * cov + 127) // 255 for b, f in zip(bg, fg))  # type: ignore[return-value]


# --------------------------------------------------------------------------
# asset model
# --------------------------------------------------------------------------


class ContractError(Exception):
    """The asset itself violates the layout contract (exit 1, not a usage error)."""


def glyph_bytes_per_glyph(cell_w: int, cell_h: int) -> int:
    return cell_w * cell_h


def load_asset(path: pathlib.Path, cell_h: int, cell_w: int = CELL_WIDTH) -> bytes:
    """Read the asset and enforce the size contract before anything else.

    The size is the one part of the layout contract that cannot be checked
    afterwards, and the failure it catches is the expensive one: an asset of a
    different cell compared against this one produces a plausible-looking diff of
    thousands of pixels, when the real problem is the cell. So the message names
    the geometry the file actually encodes.
    """
    if not path.is_file():
        die(f"asset {path} not found")
    data = path.read_bytes()
    per_glyph = glyph_bytes_per_glyph(cell_w, cell_h)
    expect = GLYPHS * per_glyph
    if len(data) != expect:
        hint = ""
        if len(data) % GLYPHS == 0:
            per = len(data) // GLYPHS
            if per % cell_w == 0:
                hint = f" This file holds {GLYPHS} glyphs of {per // cell_w} rows."
            elif per % cell_h == 0:
                hint = f" This file holds {GLYPHS} glyphs of {per // cell_h} columns."
        raise ContractError(
            f"{path} is {len(data)} bytes, expected {expect} "
            f"({GLYPHS} glyphs x {cell_h} rows x {cell_w} bytes).{hint}"
        )
    return data


def glyph_coverage(data: bytes, ch: str, cell_h: int, cell_w: int = CELL_WIDTH) -> list[int]:
    """The ``cell_h * cell_w`` coverage bytes of one printable ASCII character."""
    cp = ord(ch)
    if not FIRST_CP <= cp <= LAST_CP:
        die(f"character {ch!r} (U+{cp:04X}) is outside the asset's 0x20..0x7E range")
    base = (cp - FIRST_CP) * glyph_bytes_per_glyph(cell_w, cell_h)
    return list(data[base : base + cell_w * cell_h])


def ink_bbox(cov: list[int], cell_h: int, cell_w: int = CELL_WIDTH):
    """``(x0, y0, x1, y1)`` of the inked pixels, inclusive, or ``None`` when blank."""
    xs: list[int] = []
    ys: list[int] = []
    for i, c in enumerate(cov):
        if c:
            xs.append(i % cell_w)
            ys.append(i // cell_w)
    if not xs:
        return None
    return min(xs), min(ys), max(xs), max(ys)


# --------------------------------------------------------------------------
# asset mode
# --------------------------------------------------------------------------


def check_asset(data: bytes, path: pathlib.Path, cell_h: int, kernel_src: pathlib.Path) -> bool:
    """The layout contract plus the kernel-constant cross-check. True when green."""
    ok = True
    print(f"asset     {path} ({len(data)} bytes)")
    print(
        f"contract  {GLYPHS} glyphs x {cell_h} rows x {CELL_WIDTH} coverage bytes "
        f"= {GLYPHS * glyph_bytes_per_glyph(CELL_WIDTH, cell_h)} bytes  OK"
    )

    blanks: list[str] = []
    touching_edges: list[str] = []
    top_edge: list[str] = []
    bottom_edge: list[str] = []
    bottoms: dict[int, int] = {}
    union = [CELL_WIDTH, cell_h, -1, -1]
    cov_values: set[int] = set()

    for i in range(GLYPHS):
        ch = chr(FIRST_CP + i)
        cov = glyph_coverage(data, ch, cell_h)
        cov_values.update(cov)
        box = ink_bbox(cov, cell_h)
        if box is None:
            blanks.append(ch)
            continue
        x0, y0, x1, y1 = box
        if x0 == 0 or x1 == CELL_WIDTH - 1:
            touching_edges.append(ch)
        if y0 == 0:
            top_edge.append(ch)
        if y1 == cell_h - 1:
            bottom_edge.append(ch)
        bottoms[y1 + 1] = bottoms.get(y1 + 1, 0) + 1
        union = [min(union[0], x0), min(union[1], y0), max(union[2], x1), max(union[3], y1)]

    if blanks == [" "]:
        print("blank     exactly one blank glyph: space (0x20)  OK")
    else:
        ok = False
        names = " ".join(repr(c) for c in blanks) if blanks else "(none)"
        print(f"blank     FAIL: blank glyphs are {names}; only space may be blank")

    # The baseline is not stored in the asset: the generator places the glyph origin
    # once for the whole set so a line of text cannot wobble. The row it settled on
    # is recoverable — it is the row below the last ink row of the glyphs that sit
    # *on* the baseline, i.e. the mode over all glyphs of (last ink row + 1).
    # Descenders do not move the mode.
    if bottoms:
        baseline, votes = max(bottoms.items(), key=lambda kv: (kv[1], -kv[0]))
        print(
            f"baseline  row {baseline} of {cell_h} (derived: {votes}/{GLYPHS - len(blanks)} "
            f"glyphs end their ink there)"
        )
        print(
            f"ink bbox  x {union[0]}..{union[2]} of 0..{CELL_WIDTH - 1}, "
            f"y {union[1]}..{union[3]} of 0..{cell_h - 1}"
            f"  ->  rel. baseline y {union[1] - baseline:+d}..{union[3] - baseline:+d}"
        )
        print(
            f"margins   top {union[1]} blank row(s), bottom {cell_h - 1 - union[3]} blank row(s); "
            f"deepest descender reaches {union[3] - baseline} row(s) below the baseline"
        )
    else:
        ok = False
        print("baseline  FAIL: the asset has no ink at all")

    # Coverage is what makes the text antialiased: an all-or-nothing asset means the
    # generator ran in a bitmap mode, or the ramp was flattened on the way in.
    midtones = len([v for v in cov_values if 0 < v < 255])
    if midtones:
        print(f"coverage  {midtones} partial coverage value(s) present  OK")
    else:
        ok = False
        print("coverage  FAIL: the asset has no partial coverage — every glyph edge is hard")

    print(
        f"edges     {len(touching_edges)} glyph(s) fill the full cell width: "
        f"{' '.join(repr(c) for c in touching_edges) if touching_edges else '(none)'}"
    )
    if top_edge or bottom_edge:
        print(
            f"edges     NOTE {len(top_edge) + len(bottom_edge)} glyph(s) reach a horizontal "
            f"border (top: {' '.join(repr(c) for c in top_edge) or '-'}; "
            f"bottom: {' '.join(repr(c) for c in bottom_edge) or '-'})"
        )
    else:
        print("edges     no glyph ink reaches the first or last cell row  OK")

    # Duplicate glyphs are legal in a font (two codepoints may share a design) but in
    # a generated 95-glyph ASCII asset they are far more likely to mean the
    # rasteriser failed for one of them.
    seen: dict[bytes, str] = {}
    dupes: list[str] = []
    per = glyph_bytes_per_glyph(CELL_WIDTH, cell_h)
    for i in range(GLYPHS):
        ch = chr(FIRST_CP + i)
        key = data[i * per : (i + 1) * per]
        if key in seen:
            dupes.append(f"{seen[key]!r}={ch!r}")
        else:
            seen[key] = ch
    print(
        f"distinct  {len(seen)} distinct glyphs of {GLYPHS}  OK"
        if not dupes
        else f"distinct  NOTE duplicate glyphs: {' '.join(dupes)}"
    )

    ok = check_kernel_constants(kernel_src, cell_h, path.name) and ok
    return ok


def check_kernel_constants(path: pathlib.Path, cell_h: int, asset_name: str) -> bool:
    """Cross-check ``CHAR_WIDTH``/``CHAR_HEIGHT`` — the font has two sources, not one.

    A regenerated asset with a different cell is harmless only if the kernel's
    constants move with it; the opposite case (constants changed, asset not) draws
    every glyph sheared. Parsing them here keeps that pair honest without a kernel
    build. A file that cannot be read or parsed is a *warning*, not a failure: this
    is a read-only cross-check of a source file other agents may be editing.
    """
    if not path.is_file():
        print(f"kernel    WARNING {path} not found; constants not cross-checked")
        return True
    text = path.read_text(errors="replace")
    width, height = const_usize(text, "CHAR_WIDTH"), const_usize(text, "CHAR_HEIGHT")
    if width is None or height is None:
        print(f"kernel    WARNING could not parse CHAR_WIDTH/CHAR_HEIGHT in {path.name}")
        return True
    print(f"kernel    {path.name}: CHAR_WIDTH={width}, CHAR_HEIGHT={height}")
    if width != CELL_WIDTH or height != cell_h:
        print(
            f"kernel    FAIL: the kernel draws {width}x{height} cells but the asset is "
            f"{CELL_WIDTH}x{cell_h} — regenerate one side to match the other"
        )
        return False
    if asset_name not in text:
        print(f"kernel    NOTE the embedded font is not {asset_name}")
    print("kernel    matches the asset cell  OK")
    return True


def const_usize(text: str, name: str):
    """Read ``pub const NAME: usize = N;`` out of a kernel source file."""
    m = re.search(rf"(?:pub )?const {name}: usize = (\d+);", text)
    return int(m.group(1)) if m else None


def ascii_art(text: str, data: bytes, cell_h: int, cell_w: int = CELL_WIDTH) -> str:
    """The expected raster as text: '#' full ink, '.' background, shades between."""
    shades = " .:-=+*#%@"
    lines = []
    for y in range(cell_h):
        line = []
        for ch in text:
            cov = glyph_coverage(data, ch, cell_h, cell_w)
            for x in range(cell_w):
                c = cov[y * cell_w + x]
                line.append(shades[min(len(shades) - 1, c * len(shades) // 256)])
        lines.append("".join(line))
    return "\n".join(lines)


def save_expected_png(
    text: str, data: bytes, cell_h: int, out: pathlib.Path, scale: int, bg, fg
) -> None:
    """Write the asset-rendered string exactly as the kernel's blitter would."""
    from PIL import Image

    img = Image.new("RGB", (len(text) * CELL_WIDTH, cell_h), bg)
    px = img.load()
    for i, ch in enumerate(text):
        cov = glyph_coverage(data, ch, cell_h)
        for y in range(cell_h):
            for x in range(CELL_WIDTH):
                px[i * CELL_WIDTH + x, y] = blend(bg, fg, cov[y * CELL_WIDTH + x])
    if scale > 1:
        img = img.resize((img.width * scale, img.height * scale), Image.NEAREST)
    out.parent.mkdir(parents=True, exist_ok=True)
    img.save(out)
    print(
        f"expected  {out} ({len(text)} chars, {len(text) * CELL_WIDTH}x{cell_h} px, "
        f"fg {fg[0]:02x}{fg[1]:02x}{fg[2]:02x}, scale {scale})"
    )


# --------------------------------------------------------------------------
# screenshot mode
# --------------------------------------------------------------------------


def expected_rows(text: str, data: bytes, cell_h: int, bg, fg) -> list[list[tuple[int, int, int]]]:
    """The expected pixels of a whole string: ``cell_h`` rows of blended pixels."""
    width = len(text) * CELL_WIDTH
    rows: list[list[tuple[int, int, int]]] = [[] for _ in range(cell_h)]
    for ch in text:
        cov = glyph_coverage(data, ch, cell_h)
        for y in range(cell_h):
            for x in range(CELL_WIDTH):
                rows[y].append(blend(bg, fg, cov[y * CELL_WIDTH + x]))
    assert all(len(r) == width for r in rows)
    return rows


def compare_at(img, text: str, data: bytes, row: int, col: int, cell_h: int, bg, fg):
    """(differing pixels, per-cell detail); ``None`` when the rect leaves the image."""
    width, height = img.size
    x0, y0 = col * CELL_WIDTH, row * cell_h
    if x0 + len(text) * CELL_WIDTH > width or y0 + cell_h > height:
        return None
    px = img.load()
    exp = expected_rows(text, data, cell_h, bg, fg)
    diff = 0
    cells = []
    for i, ch in enumerate(text):
        d = 0
        for y in range(cell_h):
            for x in range(CELL_WIDTH):
                if exp[y][i * CELL_WIDTH + x] != px[x0 + i * CELL_WIDTH + x, y0 + y]:
                    d += 1
        cells.append((ch, d))
        diff += d
    return diff, cells


def report_compare(img, text: str, data: bytes, row: int, col: int, cell_h: int, bg, fg):
    """Print the per-line verdict; return ``(differing pixels, ok)``."""
    result = compare_at(img, text, data, row, col, cell_h, bg, fg)
    if result is None:
        print(
            f"screen    FAIL: row {row} col {col} + {len(text)} cells x {cell_h} rows "
            f"leaves the {img.size[0]}x{img.size[1]} screenshot"
        )
        return -1, False
    diff, cells = result
    x0, y0 = col * CELL_WIDTH, row * cell_h
    print(f'screen    "{text}"')
    print(
        f"rect      row {row} col {col} -> x {x0}..{x0 + len(text) * CELL_WIDTH - 1}, "
        f"y {y0}..{y0 + cell_h - 1} ({len(text)} cells)"
    )
    print(f"pixels    {len(text) * CELL_WIDTH * cell_h} compared, diff {diff}")
    if diff:
        bad = [(i, c, d) for i, (c, d) in enumerate(cells) if d]
        shown = ", ".join(f"col+{i} {c!r}={d}px" for i, c, d in bad[:12])
        print(f"mismatch  {len(bad)} cell(s) differ: {shown}{' …' if len(bad) > 12 else ''}")
    return diff, diff == 0


def write_diff_png(
    img, text: str, data: bytes, row: int, col: int, cell_h: int, bg, fg, out, scale: int
):
    """Three stacked panels: expected, actual, differing pixels in red."""
    from PIL import Image

    x0, y0 = col * CELL_WIDTH, row * cell_h
    w, h = len(text) * CELL_WIDTH, cell_h
    exp = expected_rows(text, data, cell_h, bg, fg)
    expected = Image.new("RGB", (w, h), bg)
    ep = expected.load()
    for y in range(h):
        for x in range(w):
            ep[x, y] = exp[y][x]
    actual = img.crop((x0, y0, x0 + w, y0 + h)).convert("RGB")
    ap = actual.load()
    mask = Image.new("RGB", (w, h), (25, 25, 25))
    mp = mask.load()
    for y in range(h):
        for x in range(w):
            mp[x, y] = (255, 60, 60) if exp[y][x] != ap[x, y] else (90, 90, 90)
    panel = Image.new("RGB", (w, h * 3 + 8), (0, 0, 0))
    panel.paste(expected, (0, 0))
    panel.paste(actual, (0, h + 4))
    panel.paste(mask, (0, 2 * h + 8))
    if scale > 1:
        panel = panel.resize((panel.width * scale, panel.height * scale), Image.NEAREST)
    out.parent.mkdir(parents=True, exist_ok=True)
    panel.save(out)
    print(f"diff png  {out} (top: expected, middle: screen, bottom: red = differing)")


def glyph_masks(data: bytes, cell_h: int, bg) -> dict[tuple, list[tuple[str, list[int]]]]:
    """Ink mask (which pixels have any coverage) -> the glyphs that produce it.

    The console prints in several colours — white output, a green prompt, red
    errors — so a scan of the whole screen cannot compare blended pixels against
    one foreground colour. The ink *mask* is colour-independent and still pins the
    glyph: it is the shape the asset says, pixel for pixel.
    """
    masks: dict[tuple, list[tuple[str, list[int]]]] = {}
    for i in range(GLYPHS):
        ch = chr(FIRST_CP + i)
        cov = glyph_coverage(data, ch, cell_h)
        mask = tuple(c > 0 for c in cov)
        masks.setdefault(mask, []).append((ch, cov))
    return masks


def screen_cell(img, col: int, row: int, cell_h: int) -> tuple:
    px = img.load()
    x0, y0 = col * CELL_WIDTH, row * cell_h
    return tuple(tuple(px[x0 + x, y0 + y] for x in range(CELL_WIDTH)) for y in range(cell_h))


def scan_screen(
    img, data: bytes, cell_h: int, bg, fg, status_height: int, skip: set[tuple[int, int]]
) -> int:
    """Every console cell above the status bar must be blank or exactly one asset glyph.

    Two checks per cell: the ink mask must be the asset's (colour-independent, so the
    green prompt and red errors are covered too), and — when the cell is drawn in a
    single colour — every pixel must equal the blend of that colour with the asset's
    coverage. ``skip`` names cells the caller knows are overlays rather than console
    text: the blinking caret and the software mouse pointer are drawn on top of the
    grid, so their cells legitimately hold pixels no glyph could explain.
    """
    masks = glyph_masks(data, cell_h, bg)
    blank_mask = tuple(False for _ in range(cell_h * CELL_WIDTH))
    width, height = img.size
    cols, rows = width // CELL_WIDTH, height // cell_h
    n_blank = n_matched = 0
    seen: set[str] = set()
    unexplained: list[tuple[int, int, tuple, str]] = []
    for row in range(rows):
        if (row + 1) * cell_h > height - status_height:
            break
        for col in range(cols):
            if (row, col) in skip:
                continue
            cell = screen_cell(img, col, row, cell_h)
            flat = [cell[y][x] for y in range(cell_h) for x in range(CELL_WIDTH)]
            mask = tuple(px != bg for px in flat)
            if mask == blank_mask:
                n_blank += 1
                continue
            candidates = masks.get(mask)
            if not candidates:
                unexplained.append((row, col, cell, "ink shape is not any asset glyph"))
                continue
            # The ink colour is derived, not guessed from the brightest pixel: a glyph
            # whose coverage never reaches 255 (``=`` tops out below it) would
            # otherwise be measured against a dimmer foreground than the console used.
            # Taking the highest-coverage pixel and dividing out its coverage recovers
            # the colour the console actually drew with.
            ok = False
            for ch, cov in candidates:
                best = max(range(len(cov)), key=lambda i: cov[i])
                if cov[best] == 0:
                    continue
                fg_est = tuple(
                    min(255, round(flat[best][k] * 255 / cov[best])) for k in range(3)
                )
                if all(blend(bg, fg_est, cov[i]) == flat[i] for i in range(len(flat))):
                    ok = True
                    seen.add(ch)
                    break
            if ok:
                n_matched += 1
            else:
                unexplained.append((row, col, cell, f"matches the shape of {candidates[0][0]!r} "
                                                     f"but not its coverage in this colour"))
    total = n_blank + n_matched + len(unexplained)
    print(
        f"scan      {total} cells above the status bar ({rows} rows x {cols} cols, "
        f"bg {(bg[0] << 16) | (bg[1] << 8) | bg[2]:06x}, {len(skip)} overlay cell(s) skipped)"
    )
    print(
        f"scan      {n_blank} blank, {n_matched} cells matching an asset glyph "
        f"({len(seen)} distinct characters)"
    )
    if not unexplained:
        print("scan      no cell on the console holds pixels outside the asset  OK")
        return 0
    print(f"scan      FAIL {len(unexplained)} unexplained cell(s):")
    for row, col, cell, why in unexplained[:8]:
        flat = " ".join(f"{p[0]:02x}{p[1]:02x}{p[2]:02x}" for line in cell[:2] for p in line)
        print(f"            row {row} col {col}: {flat} … — {why}")
    if len(unexplained) > 8:
        print(f"            … and {len(unexplained) - 8} more")
    return 1


def find_text(img, text: str, data: bytes, cell_h: int, bg, fg, limit: int = 10):
    """Every grid position where the text matches the asset raster exactly."""
    exp = expected_rows(text, data, cell_h, bg, fg)
    width, height = img.size
    cols = width // CELL_WIDTH
    rows = height // cell_h
    px = img.load()
    hits: list[tuple[int, int]] = []
    for row in range(rows):
        for col in range(max(1, cols - len(text) + 1)):
            ok = True
            for y in range(cell_h):
                for x in range(len(text) * CELL_WIDTH):
                    if px[col * CELL_WIDTH + x, row * cell_h + y] != exp[y][x]:
                        ok = False
                        break
                if not ok:
                    break
            if ok:
                hits.append((row, col))
                if len(hits) >= limit:
                    return hits
    return hits


# --------------------------------------------------------------------------
# main
# --------------------------------------------------------------------------


def main() -> int:
    p = argparse.ArgumentParser(
        description="Verify the console font asset and the glyphs QEMU actually drew",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "examples:\n"
            "  python3 tools/font_check.py\n"
            '  python3 tools/font_check.py --text "Welcome to pagh OS Shell!" '
            "--out target/font_expected.png\n"
            '  python3 tools/font_check.py --screenshot target/shot.png '
            '--expect "Welcome to pagh OS Shell!" --find\n'
            "  python3 tools/font_check.py --screenshot target/shot.png --scan-all\n"
        ),
    )
    p.add_argument(
        "--asset", default=str(DEFAULT_ASSET), help=f"font asset (default {DEFAULT_ASSET.name})"
    )
    p.add_argument(
        "--cell-height",
        type=int,
        default=DEFAULT_CELL_HEIGHT,
        help="glyph cell height in pixels (default 18)",
    )
    p.add_argument(
        "--cell-width", type=int, default=CELL_WIDTH, help="glyph cell width in pixels (default 8)"
    )
    p.add_argument(
        "--kernel",
        default=str(DEFAULT_KERNEL_SOURCE),
        help="framebuffer source to cross-check CHAR_WIDTH/CHAR_HEIGHT",
    )
    p.add_argument(
        "--text", help="render this string from the asset bytes (ASCII art, and --out writes a PNG)"
    )
    p.add_argument("--out", help="PNG path for the asset-rendered string (with --text)")
    p.add_argument(
        "--scale", type=int, default=1, help="integer nearest-neighbour upscale for PNGs (default 1)"
    )
    p.add_argument("--screenshot", help="QEMU screendump PNG to compare against the asset")
    p.add_argument("--expect", help="the exact text the screenshot should show (with --screenshot)")
    p.add_argument("--row", type=int, help="grid row of the text: pixel y = row * cell height")
    p.add_argument(
        "--col",
        type=int,
        default=0,
        help="grid column of the text: pixel x = col * cell width (default 0)",
    )
    p.add_argument(
        "--find", action="store_true", help="scan the screenshot for positions where --expect matches"
    )
    p.add_argument(
        "--scan-all",
        action="store_true",
        help="check every console cell above the status bar against the asset",
    )
    p.add_argument(
        "--status-height",
        type=int,
        default=18,
        help="status-bar strip height in pixels to exclude from --scan-all (default 18)",
    )
    p.add_argument(
        "--bg",
        default=DEFAULT_BG,
        help=f"console background colour behind the text, RRGGBB (default {DEFAULT_BG})",
    )
    p.add_argument(
        "--fg", default=DEFAULT_FG, help=f"console ink colour, RRGGBB (default {DEFAULT_FG})"
    )
    p.add_argument(
        "--skip-cell",
        action="append",
        default=[],
        help="row,col of a cell that holds an overlay (caret, mouse pointer) rather than "
        "console text; repeatable, --scan-all only",
    )
    p.add_argument("--diff-out", help="write a PNG showing expected vs screen vs differing pixels")
    args = p.parse_args()

    if args.scale < 1:
        die("--scale must be >= 1")
    if args.cell_height < 1 or args.cell_width < 1:
        die("--cell-height/--cell-width must be >= 1")
    # A flag that silently does nothing is worse than no flag: these only mean
    # something in screenshot mode, so refuse them in asset mode instead of ignoring
    # them and printing a cheerful "ASSET OK".
    if not args.screenshot:
        for flag, given in (
            ("--expect", args.expect is not None),
            ("--find", args.find),
            ("--row", args.row is not None),
            ("--diff-out", args.diff_out is not None),
            ("--scan-all", args.scan_all),
        ):
            if given:
                die(f"{flag} only applies with --screenshot")

    cell_w = args.cell_width
    asset_path = resolve(args.asset)
    try:
        data = load_asset(asset_path, args.cell_height, cell_w)
    except ContractError as exc:
        print(f"font_check: {exc}", file=sys.stderr)
        return 1

    bg = parse_hex(args.bg)
    fg = parse_hex(args.fg)

    # ----- screenshot mode -------------------------------------------------
    if args.screenshot:
        if (args.row is not None or args.find) and not args.expect:
            die('--screenshot with --row/--find requires --expect "text"')
        if args.row is None and not args.find and not args.scan_all:
            die("--screenshot needs --row <grid_row>, --find to locate the text, or --scan-all")
        from PIL import Image

        shot = resolve(args.screenshot)
        if not shot.exists():
            die(f"screenshot {shot} not found")
        img = Image.open(shot).convert("RGB")
        print(f"asset     {asset_path} ({len(data)} bytes), cell {cell_w}x{args.cell_height}")
        print(f"shot      {shot} ({img.size[0]}x{img.size[1]})")

        failed = False
        if args.row is not None:
            d, ok = report_compare(
                img, args.expect, data, args.row, args.col, args.cell_height, bg, fg
            )
            if d >= 0 and args.diff_out:
                write_diff_png(
                    img,
                    args.expect,
                    data,
                    args.row,
                    args.col,
                    args.cell_height,
                    bg,
                    fg,
                    resolve(args.diff_out),
                    max(1, args.scale),
                )
            if d > 0 or not ok:
                failed = True
            else:
                print(f"font_check: SCREEN OK row={args.row} col={args.col} diff=0")
        if args.find:
            hits = find_text(img, args.expect, data, args.cell_height, bg, fg)
            if hits:
                print(
                    f"find      exact match at {len(hits)} position(s): "
                    + ", ".join(f"row {r} col {c}" for r, c in hits)
                )
                if args.row is None:
                    print(f"font_check: SCREEN OK row={hits[0][0]} col={hits[0][1]} diff=0")
            else:
                # No diff count here on purpose: `--find` has not compared anything
                # yet, it has only failed to find one position where the whole text
                # matches. Reporting a pixel count would be inventing a number.
                print("find      no position on the screen matches the expected text exactly")
                failed = True
        if args.scan_all:
            skip = set()
            for spec in args.skip_cell:
                parts = spec.split(",")
                if len(parts) != 2 or not all(x.strip().isdigit() for x in parts):
                    die(f"--skip-cell wants row,col; got {spec!r}")
                skip.add((int(parts[0]), int(parts[1])))
            if scan_screen(img, data, args.cell_height, bg, fg, args.status_height, skip):
                failed = True
        if failed:
            print("font_check: SCREEN FAIL (see above)", file=sys.stderr)
            return 1
        return 0

    # ----- asset mode ------------------------------------------------------
    ok = check_asset(data, asset_path, args.cell_height, resolve(args.kernel))
    if args.text:
        print(ascii_art(args.text, data, args.cell_height, cell_w))
        if args.out:
            save_expected_png(
                args.text, data, args.cell_height, resolve(args.out), args.scale, bg, fg
            )
    elif args.out:
        die("--out renders --text; pass both")
    if not ok:
        print("font_check: ASSET FAIL", file=sys.stderr)
        return 1
    print("font_check: ASSET OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())