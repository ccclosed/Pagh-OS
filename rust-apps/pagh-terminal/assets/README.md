# Generated font assets

Do not edit by hand. Regenerate with:

```sh
python3 tools/gen_font.py --allow-clip
```

`font8x18.bin` stores 1203 narrow glyphs, `font_wide8x18.bin` stores 487 wide
glyphs. Each glyph is `cell_h × cell_w` bytes of 8-bit coverage (0 = background,
255 = full ink), row-major. `src/font_index.rs` lists the sorted codepoints; the
array position is the glyph index. `font_meta.txt` records the source font
hashes and the measured baseline. `src/unicode_tables.rs` is generated from the
same run and provides the East-Asian-wide / zero-width range tables.

`--check` verifies that the committed assets, index and tables match the
generator output byte for byte.
