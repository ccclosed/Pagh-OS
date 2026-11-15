# Bundled font assets

The terminal renderer embeds two bitmap coverage assets generated from
open-source fonts by `tools/gen_font.py`:

| asset | source face | license |
|---|---|---|
| `assets/font8x18.bin` (1203 × 8×18 glyphs) | Fira Code Regular, 13 px em | SIL Open Font License 1.1 |
| `assets/font_wide8x18.bin` (487 × 16×18 glyphs) | Noto Sans Mono CJK SC Regular, 16 px em | SIL Open Font License 1.1 |

## Fira Code

Copyright (c) 2014-2021 The Fira Code Project Authors
(https://github.com/tonsky/FiraCode), with Reserved Font Name "Fira Code".
Licensed under the SIL Open Font License, Version 1.1 (see `OFL-1.1.txt`).

The packed asset is a rasterised, format-converted derivative ("Pagh Terminal
Bitmap"), distributed under OFL 1.1. It does not use the reserved font name in
its primary name.

## Noto Sans Mono CJK SC

Copyright 2014-2021 Adobe (http://www.adobe.com/), with Reserved Font Name
"Source". Noto is a trademark of Google Inc. Licensed under the SIL Open Font
License, Version 1.1 (see `OFL-1.1.txt`).

The packed asset is a rasterised, format-converted derivative ("Pagh Terminal
Wide Bitmap"), distributed under OFL 1.1.

The Rust source code in this directory is MIT-licensed (see the repository
`LICENSE`); the two bitmap assets remain under OFL 1.1.
