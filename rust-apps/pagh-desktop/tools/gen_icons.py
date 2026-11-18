#!/usr/bin/env python3
"""Пересборка `assets/icons/*.png` из официальных Fluent-SVG (MIT).

Зачем инструмент в репозитории: PNG — запечённые ассеты, и без скрипта нельзя
ни проверить, ни повторить их качество. Скрипт фиксирует ровно тот конвейер,
который дал текущие файлы, и пишет рядом `assets/icons/manifest.json`
(глиф, нативный размер-мастер, фаза, инструменты, лицензия).

Что было не так у прежней пылевой сборки (cache/pagh-icons/work/build.py):

  * `pick()` брал нативный размер Fluent «ближайший снизу» и рендерил ЕГО
    дизайн в целевой размер. Для глифов без нативного целевого размера это
    означало апскейл дизайна (Window Console во Fluent есть только в 20 px,
    поэтому terminal-24 — растянутая двадцатка), а не отрисовку вектора в
    свой размер.
  * 1–1.5 px штрихи Fluent лежат на нецелых координатах, поэтому cairo
    размазывал их на две строки по ~50 %: `Subtract 16/20` получался серой
    полосой (max alpha 136), `Dismiss 16` — бледным крестом (234), а у
    `Plug Connected` не было ни одного непрозрачного пикселя (246).

Конвейер здесь:

  * мастер — нативный размер Fluent, ТОЧНО совпадающий с целевым, иначе
    ближайший нативный (апскейл только векторный — растр никогда не
    масштабируется);
  * cairosvg рендерит вектор сразу в целевой размер: у cairo точное
    площадное покрытие, поэтому суперсэмплинг не нужен — измерено, что
    4x→BOX даёт те же пиксели (±0.5 % «туши»), а 4x→LANCZOS вдвое
    увеличивает число полупрозрачных пикселей и даёт ореол. Флаг `--ss`
    оставлен для экспериментов, по умолчанию 1;
  * фаза: если сдвиг глифа на полпикселя делает штрих непрозрачным (больше
    «туши» в alpha>=200 при той же сумме альфы, ±5 %), сдвиг применяется.
    Геометрия не меняется — глиф просто встаёт на пиксельную сетку;
  * выход — регулярный чёрный (#000000) + альфа: RGB заменяет тинт в
    `src/icons.rs`.

Запуск (нужен sparse-checkout Fluent, см. `cache/pagh-icons`):

  python3 tools/gen_icons.py --svg-root ~/.cache/pagh-icons/fluent-sparse/assets
"""
import argparse
import glob
import io
import json
import os
import sys

import cairosvg
from PIL import Image

# Иконка -> папка глифа Fluent (регулярный стиль).
GLYPHS = {
    "grid": "Grid",
    "terminal": "Window Console",
    "folder": "Folder",
    "image": "Image",
    "network-wired": "Plug Connected",
    "window-minimize": "Subtract",
    "window-maximize": "Square",
    "window-close": "Dismiss",
}
SIZES = (16, 20, 24, 32, 48)
PHASES = ((0.0, 0.0), (0.5, 0.0), (0.0, 0.5), (0.5, 0.5))


def native_sizes(root, folder):
    paths = glob.glob(os.path.join(root, folder, "SVG", "*_regular.svg"))
    return {int(p.rsplit("_", 2)[-2]): p for p in paths}


def pick_master(avail, target):
    """Точный нативный размер, иначе ближайший (апскейл — только по вектору)."""
    if target in avail:
        return target
    return min(avail, key=lambda size: (abs(size - target), size))


def shift(text, master, target, dx, dy):
    """Сдвиг глифа на (dx, dy) пикселей цели (в единицах viewBox мастера)."""
    if not dx and not dy:
        return text
    sx, sy = dx * master / target, dy * master / target
    text = text.replace("<path", f'<g transform="translate({sx:.5f} {sy:.5f})"><path', 1)
    return text.replace("</svg>", "</g></svg>")


def rasterize(text, target, supersample):
    big = supersample * target
    png = cairosvg.svg2png(bytestring=text.encode("utf-8"), output_width=big, output_height=big)
    image = Image.open(io.BytesIO(png)).convert("RGBA")
    if supersample > 1:
        image = image.resize((target, target), Image.BOX)
    return image


def alpha_bytes(image):
    """Пиксели альфы; get_flattened_data есть в Pillow 12, getdata — в старых."""
    channel = image.getchannel("A")
    getter = getattr(channel, "get_flattened_data", channel.getdata)
    return [value for value in getter()]


def stats(image):
    alpha = alpha_bytes(image)
    ink = sum(alpha)
    solid = sum(value for value in alpha if value >= 200)
    peak = max(alpha)
    lit = sum(1 for value in alpha if value > 0)
    return {"ink": ink, "solid": solid / ink if ink else 0.0, "peak": peak, "lit": lit}


def best_phase(text, master, target, supersample, base):
    """Фаза из полпиксельной сетки, дающая непрозрачный штрих без потери «туши»."""
    best = (base, 0.0, 0.0)
    for dx, dy in PHASES[1:]:
        candidate = stats(rasterize(shift(text, master, target, dx, dy), target, supersample))
        if candidate["peak"] < 250:
            continue
        if abs(candidate["ink"] - base["ink"]) > 0.05 * base["ink"]:
            continue
        # Сдвиг не должен добавлять заметную невидимую кайму (alpha < 64).
        if candidate["solid"] > best[0]["solid"] + 0.02:
            best = (candidate, dx, dy)
    return best


def main():
    parser = argparse.ArgumentParser()
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    parser.add_argument("--svg-root", required=True,
                        help="каталог assets/ sparse-checkout fluentui-system-icons")
    parser.add_argument("--out", default=os.path.join(here, "assets", "icons"))
    parser.add_argument("--ss", type=int, default=1,
                        help="суперсэмплинг (1 = cairo прямо в целевой размер, измеренно лучший)")
    parser.add_argument("--no-phase", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    manifest = {
        "schema": "pagh-icons/1",
        "license": "MIT, Copyright (c) 2020 Microsoft Corporation",
        "source": "https://github.com/microsoft/fluentui-system-icons",
        "tooling": f"cairosvg {cairosvg.__version__}: вектор в точный размер + фазовая подгонка",
        "render": {"supersample": args.ss, "downsample": "none (cairo AA)" if args.ss == 1 else "BOX",
                   "style": "regular: #000000 + alpha, RGB заменяет тинт"},
        "sizes": list(SIZES),
        "icons": {},
    }
    problems = []
    for key, folder in GLYPHS.items():
        avail = native_sizes(args.svg_root, folder)
        if not avail:
            problems.append(f"{key}: нет SVG для «{folder}»")
            continue
        entry = {"fluent": folder, "files": {}, "master_svg_size": {}, "phase": {}}
        for target in SIZES:
            master = pick_master(avail, target)
            text = open(avail[master], encoding="utf-8").read()
            text = text.replace('fill="#212121"', 'fill="#000000"')
            image = rasterize(text, target, args.ss)
            base = stats(image)
            phase = (0.0, 0.0)
            best, dx, dy = best_phase(text, master, target, args.ss, base)
            if not args.no_phase and (dx or dy):
                image = rasterize(shift(text, master, target, dx, dy), target, args.ss)
                phase = (dx, dy)
            name = f"{key}-{target}.png"
            path = os.path.join(args.out, name)
            if not args.dry_run:
                image.save(path, optimize=True)
            entry["files"][str(target)] = name
            entry["master_svg_size"][str(target)] = master
            entry["phase"][str(target)] = [phase[0], phase[1]]
            final = stats(image)
            flag = " " if master == target else "↑"
            print(f"{flag} {name:24} master={master:2} phase={phase[0]:<3.1f},{phase[1]:<3.1f} "
                  f"peak={final['peak']:3} solid={final['solid']:.3f} lit={final['lit']:3}")
            if final["peak"] < 190:
                problems.append(f"{name}: max alpha {final['peak']} — глиф размыт")
        manifest["icons"][key] = entry

    if not args.dry_run:
        with open(os.path.join(args.out, "manifest.json"), "w", encoding="utf-8") as handle:
            json.dump(manifest, handle, indent=2, ensure_ascii=False)
            handle.write("\n")
    if problems:
        print("\nПРОБЛЕМЫ:", *problems, sep="\n  ")
        return 1
    print("\nOK: все ассеты имеют непрозрачное ядро")
    return 0


if __name__ == "__main__":
    sys.exit(main())
