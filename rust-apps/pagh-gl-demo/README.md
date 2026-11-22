# `pagh-gl-demo` — CPU TinyGL-демо для Pagh-OS

Демонстрационный клиент **PDP/1**, который рендерит CPU-геометрию через
vendored [TinyGL 0.4.1](../tinygl/) (MIT) **прямо в `mmap(MAP_SHARED)`**
memfd-поверхность окна. Композитору не уходит ни одного пикселя: только
`COMMIT` + `DAMAGE`, как у остальных ShmSurface-клиентов (`pagh-files`,
`pagh-terminal`).

Это PR5 спеки [`docs/tinygl-port-spec.md`](../../docs/tinygl-port-spec.md):
интеграция с композитором и три сцены. Версия `pagh`/`Cargo.toml` ядра здесь
**не** меняется (MINOR — только в финальном acceptance PR6).

## Сцены

| Сцена | Что доказывает | Анимация |
|---|---|---|
| `triangle` | perspective + clip + Gouraud | нет, фиксированный угол |
| `cube` | 6 граней, depth test, back-face culling | кадр `n` = `25° + n*15°` вокруг Y |
| `gears` | ~1.1k треугольников, освещение on/off | кадр `n` = `n*1.5°` × передаточное отношение |

Все сцены — **наш** код под `tinygl::Context` (immediate mode). `gears` не
скопирован из GPL-примера upstream: зубчатый профиль (12/14/10 зубьев,
внутреннее отверстие, крышки/стенки) сгенерирован здесь; тригонометрия профиля
считается собственным рядом Тейлора без libm, чтобы host/musl-дайджесты
совпали бит-в-бит.

## CLI

```text
pagh-gl-demo <triangle|cube|gears> [options]

  --connect <sock>     живой PDP/1 (pagh-desktop); без него — headless
  --frames <N>         кадров (0 = до закрытия окна; headless: >= 1)
  --size <WxH>         клиентская область (по умолчанию 640x480, W кратно 4)
  --out <dir>          писать кадры как PPM (P6) в каталог
  --hash               печатать дайджест каждого кадра (live: по умолчанию
                       только кадр 0 и каждый 60-й)
  --lighting <on|off>  освещение gears (по умолчанию on)
  -h, --help
```

Без `--connect` демо работает headless: рендер в `Vec<u32>`, на каждый кадр
печатается

```text
TINYGL <scene> frame=<n> digest=0x<16 hex> w=<w> h=<h>
```

и в конце — `ft=min/med/max ms (wall) cycles=...`. С `--connect` — живое окно
композитора; маркер `TINYGL` годен для grep из serial-лога, как `[BOOTID]` и
`LXSELFTEST`.

Кадры считаются **по номеру**, не по времени: `Instant`/`rdtsc` участвуют
только в замерах (спека §6.2). Поэтому дайджест кадра `n` воспроизводим в
headless-прогоне, в QEMU и на musl.

## PDP/1: ShmSurface

1. `memfd_create` → `ftruncate(w*h*4)` → `mmap(MAP_SHARED)` через
   `pagh_ui::shm::ShmSurface` (`SharedFrame` в `src/surface.rs`).
2. `hello`/`welcome`; требуется `welcome.flags` bit 2 (`ShmSurface`).
3. `create` окна, затем `SHM_OFFER` с fd через `SCM_RIGHTS` и ожидание
   `SHM_READY`.
4. `ZB_open(w, h, ZB_MODE_RGBA, 0, NULL, NULL, mapping_ptr)` + `glInit` —
   один контекст на размер; рендер пишет прямо в memfd.
5. На кадр — `COMMIT` (header flags SHM) с полным damage-прямоугольником.
6. На `configure` — новый memfd, новый `SHM_OFFER`, `SHM_RELEASE(old)`; на
   `SHM_RELEASE` от сервера — повторный offer того же размера.

**Fallback на full-frame upload отсутствует** намеренно (спека §4.5): если
композитор не объявил ShmSurface или нет memfd/mmap/`SCM_RIGHTS`, демо падает
с внятной ошибкой и кодом 1.

## Тесты и сборка

```bash
cd rust-apps/pagh-gl-demo
cargo test                                    # host: сцены/дайджесты/CLI
cargo run --bin pagh-gl-demo -- triangle --hash
cargo build --release --target x86_64-unknown-linux-musl
```

`cargo test` покрывает:

* pinned FNV-1a-64 дайджесты `triangle`/`cube`/`gears` (lighting on/off);
* точечную геометрию: центр Gouraud-треугольника, грань куба, «дырка»
  шестерёнки и точка на зубе;
* детерминизм (два рендера — один дайджест/пиксели);
* `keep_all`, независимость кадра 0 от числа кадров, отказ `w % 4 != 0`;
* разбор CLI (defaults, `--frames 0` только с `--connect`, ошибки).

Musl-сборке нужен C-компилятор для `x86_64-unknown-linux-musl`; его ищет
`tinygl/build.rs` (`CC_<target>` → `<target>-gcc` → `musl-gcc` → `clang
--target=...`) и печатает инструкцию, если не нашёл. Готовый бинарь
static-pie проверяется `INTERP`-гейтом `tools/build-rust-app.sh`.

## Честные ограничения

* **CPU only**: никакого GPU/DRM/NVIDIA, OpenGL 2+/GLSL/FBO/VBO/EGL/GLX/
  Wayland/Mesa. Upstream fixed-function subset GL 1.1.
* Z-буфер **16-битный**; для залитых треугольников Z-тест идёт всегда, даже
  без `GL_DEPTH_TEST` (upstream); `glDepthMask`/`glDepthFunc` нет.
* `stride == width*4`, `width % 4 == 0` — иначе `Surface::new`/`SharedFrame`
  возвращают ошибку (TinyGL молча срезал бы `xsize & ~3`).
* Один `Context` на процесс (`gl_ctx` глобальный); демо однопоточное.
* `glClear` заливает Z нулём, `glClearDepth` upstream игнорирует.
* Освещение — только fixed-function `GL_LIGHT0..7` (в демо `Light0`);
  `glColorMaterial` не используется, материал задаётся `glMaterialfv`.
* `gl_fatal_error` upstream — `exit(1)`; наш слой отвергает известные опасные
  входы, но внутренний fatal остаётся смертью процесса.
* Живой путь требует `pagh-desktop` с ShmSurface (PDP/1 версии 1); окно
  рисует композитор, клиент — только клиентскую область.
* Frame time в QEMU TCG — **верхняя граница**, не бенчмарк: TSC виртуален,
  абсолютные числа честны только в пределах одной конфигурации QEMU.

## Лицензия

Код демо — MIT. Vendored TinyGL 0.4.1 — MIT (Fabrice Bellard); см.
[`../tinygl/LICENSE-TinyGL`](../tinygl/LICENSE-TinyGL) и
[`../tinygl/THIRD_PARTY_NOTICES.md`](../tinygl/THIRD_PARTY_NOTICES.md).