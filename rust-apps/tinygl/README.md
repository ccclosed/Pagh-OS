# `tinygl` — CPU-only fixed-function GL (TinyGL 0.4.1) для Pagh-OS

Тонкая Rust-обёртка над **vendored upstream TinyGL 0.4.1** (MIT, Fabrice
Bellard) для userspace Pagh. Рендер — **только CPU**, в буфер вызывающего
(`0x00RRGGBB` = `XRGB8888`, формат композитора).

> **Статус: PR2 — FFI core, headless.** PR1 доказал, что upstream C 0.4.1
> собирается под `x86_64-unknown-linux-musl`, линкуется static-pie и даёт
> эталонные пиксели. PR2 добавляет полный безопасный слой: контекст,
> viewport/clear, матрицы (stack, `load/mult`, `frustum`, перспектива),
> immediate-mode примитивы, depth/cull и `ZB_resize`.
>
> Это **не** полный OpenGL и **не** desktop-демо: fixed-function subset GL 1.1
> без shaders/GLSL/GL 2.0+/FBO/VBO/EGL/GLX/Wayland/Mesa и без GPU/DRM/KMS.
> Композитор (`pagh-desktop`) и демо `pagh-gl-demo` этим PR **не трогаются**;
> PDP/1-интеграция — PR4/PR5 по спеке `docs/tinygl-port-spec.md`.

## Сборка и тесты

```sh
cd rust-apps/tinygl

cargo test                       # host gnu; C-компилятор = $CC или cc/gcc
cargo run --bin tinygl-probe     # эталонный треугольник + ELF-класс
```

`cargo test` прогоняет 24 теста: unit-тесты `Surface` (валидация/resize),
`tests/criterion6.rs` (строгий эталон PR1 + детерминизм), `tests/ffi_core.rs`
(матрицы/viewport, depth, culling, `ZB_resize`, bad-input, матричные стеки) и
`tests/probe_binary.rs` (маркеры/cc/ELF собранного бинаря).

Эталон **criterion 6** спеки (`docs/tinygl-port-spec.md` §5.2) —
детерминированный треугольник в `fb[64*64]`:

```text
digest=0x3f7e5ece33386f9b center=0x003c4c7a corner=0x00000000
```

Любое расхождение валит тест: это сделано намеренно (никакого «запишем и
примем»). Зафиксированные дайджесты host-gcc и musl-gcc — в
[`UPSTREAM.md`](UPSTREAM.md). Повёрнутые кадры (`angle = n * 30°`) внешнего
эталона не имеют и проверяются на детерминизм, а не на выдуманное значение.

### static-pie / musl

```sh
cargo build --release --target x86_64-unknown-linux-musl
readelf -l target/x86_64-unknown-linux-musl/release/tinygl-probe | grep INTERP
# пусто; сам бинарь печатает interp=none elf=static-pie
```

Нужен **C-компилятор для musl**. `build.rs` ищет его в порядке:

1. `CC_x86_64_unknown_linux_musl` (или `CC_x86_64-unknown-linux-musl`);
2. `x86_64-unknown-linux-musl-gcc`;
3. `musl-gcc`;
4. `clang --target=x86_64-unknown-linux-musl` (+ `--sysroot`, если задан
   `TINYGL_MUSL_SYSROOT`).

Если ничего нет — сборка падает с инструкцией:

```text
Debian/Ubuntu: sudo apt install musl-tools
Fedora:        sudo dnf install musl-gcc
clang+sysroot: sudo apt install clang musl-dev
               export TINYGL_MUSL_SYSROOT=/usr/x86_64-linux-musl
```

Host-сборка (`cargo test`) musl-компилятора не требует. Системная сборка
(`tools/build-rust-app.sh`) не меняется: её `readelf`-проверка `INTERP`
остаётся внешним static-pie-гейтом; `tinygl-probe` дополнительно проверяет
собственный ELF (`--check-interp`).

## API

```rust
use tinygl::{Capability, Context, CullFace, MatrixMode, Primitive, Surface};

let mut fb = vec![0u32; 640 * 480];
let surface = Surface::new(&mut fb, 640, 480)?;   // w % 4 == 0, len == w*h
let mut ctx = Context::new(surface)?;             // ZB_open + glInit
ctx.set_viewport(0, 0, 640, 480)?;                // 0/огромный размер → наша Err

ctx.matrix_mode(MatrixMode::Projection);
ctx.load_identity();
ctx.perspective(60.0, 640.0 / 480.0, 0.1, 100.0)?; // наш gluPerspective-эквивалент
ctx.matrix_mode(MatrixMode::ModelView);
ctx.load_identity();
ctx.translate(0.0, 0.0, -3.0)?;
ctx.rotate(30.0, 0.0, 0.0, 1.0)?;
ctx.enable(Capability::DepthTest);                 // для линий; заливка Z-тестится всегда
ctx.set_cull_face(CullFace::Back);                 // + enable(Capability::CullFace)

ctx.begin(Primitive::Triangles)?;
ctx.color3f(1.0, 0.0, 0.0); ctx.vertex3f(-1.0, -1.0, 0.0);
ctx.color3f(0.0, 1.0, 0.0); ctx.vertex3f( 1.0, -1.0, 0.0);
ctx.color3f(0.0, 0.0, 1.0); ctx.vertex3f( 0.0,  1.0, 0.0);
ctx.end()?;

// ctx.pixels() — те же пиксели, что писал C; ctx.get_matrix(...),
// ctx.viewport(), ctx.max_lights() — чтение состояния из C.
```

`src/ffi.rs` приватный: сырые `extern "C"`-вызовы снаружи крейта недоступны, а
безопасный слой держит инварианты. Прототипы immediate-mode
(`glVertex3f`, `glColor3f`, `glNormal3f`, `glTexCoord*` и т. д.) объявлены в
`ffi.rs` **сами**: upstream `include/GL/gl.h` их не объявляет, хотя `api.c` их
экспортирует. Заголовок upstream не правится.

### Что покрывает FFI

`ZB_open` / `ZB_close` / `ZB_resize`; `glInit` / `glClose`;
`glViewport`; `glClear` / `glClearColor` / `glClearDepth`;
`glMatrixMode` / `glLoadIdentity` / `glLoadMatrixf` / `glMultMatrixf` /
`glPushMatrix` / `glPopMatrix` / `glTranslatef` / `glRotatef` / `glScalef` /
`glFrustum`; `glGetIntegerv` / `glGetFloatv`;
`glEnable` / `glDisable` / `glCullFace` / `glFrontFace`;
`glBegin` / `glEnd` / `glVertex{2,3,4}f` / `glVertex3fv` / `glColor{3,4}f` /
`glColor{3,4}fv` / `glNormal3f` / `glNormal3fv` / `glTexCoord{2,4}f` /
`glTexCoord2fv`; `glShadeModel`.

### Наши проверки до C

* `Surface`: `width % 4 == 0`, `pixels.len() == width*height` (плотный
  `stride == width*4`), `1..=4096` — иначе `FbError` **до** `ZB_open`;
* `set_viewport`: нулевая/слишком большая сторона → `BadViewport` (upstream
  `glViewport` иначе вызывает `gl_fatal_error` → `exit(1)`);
* матрицы: `push`/`pop` сверяются с пределом, прочитанным из C
  (`glGetIntegerv(GL_MAX_*_STACK_DEPTH)`) → `MatrixStackOverflow`/`Underflow`
  вместо C-`assert`; `load/mult/translate/rotate/scale/frustum/perspective`
  отвергают NaN/∞ и вырожденные проекции;
* `begin`/`end`: вложенный `glBegin` и `glEnd` без `glBegin` → `FbError`
  (upstream — C-`assert`);
* `resize(width, height)`: одна и та же память, площадь не больше исходной
  ёмкости (`ResizeTooSmall`), `w%4==0`; viewport обновляется на весь новый
  размер. Для роста нужен новый `Surface`/`Context` (новый memfd — путь PR4/5).

## Честные ограничения

* **CPU-only**, fixed-function subset GL 1.1; **нет** shaders/GLSL/GL 2.0+,
  ARB/EXT-расширений, FBO/VAO/настоящих VBO, `glDrawArrays`/`glDrawElements`;
* **клиентские массивы не в v1** (`gl*Pointer`/`glArrayElement`) — отдельный
  PR после v1 с проверкой 64-битных указателей под UBSan/ASan; в v1 только
  immediate mode;
* **16-битный Z** (`ZB_Z_BITS 16`), фиксированная точка;
* `stride` обязан быть `width*4` (TinyGL не выражает паддинг), `width` кратна
  4 (`ZB_resize` делает `xsize &= ~3`); `Surface::new` отвергает и то, и другое
  **до** C, а не «ужимает»;
* **один `Context` на процесс**: `gl_ctx` глобальный; `Context` не `Send`/`Sync`,
  второй живой контекст → `FbError::ContextAlreadyLive`;
* `glClear` чистит цвет и Z целиком (scissor/маски не учитываются), Z всегда
  заливается нулём: `glClearDepth`/`z` upstream игнорирует (`clear.c`, TODO);
* depth test реально влияет только на линии; для залитых треугольников Z-тест
  идёт **всегда**, `glDepthMask`/`glDepthFunc` в 0.4.1 нет — строго upstream,
  без форка. `GL_FRONT_AND_BACK` отбрасывает все треугольники (так в upstream);
* **`glGetError` в TinyGL 0.4.1 нет.** `Context::get_error()` — совместимость-
  заглушка, всегда `GlError::NoError`; ошибки в upstream либо невозможны, либо
  фатальны;
* **`glOrtho` в upstream — пустой `inline`** в `gl.h` (внешнего символа нет),
  поэтому не биндится; `perspective()` — наш f64-эквивалент `gluPerspective`
  поверх `glFrustum`;
* **`gl_fatal_error` = `exit(1)`** с сообщением в stderr — upstream-поведение,
  в v1 не патчим; наш слой заранее отвергает входы, которые привели бы к
  fatal/`assert`, но вершины/цвета на горячем пути валидируются только
  `debug_assert!` (в release NaN — ошибка вызывающего: C-каст float→int UB);
* `resize` не растит буфер (он принадлежит вызывающему); для большего окна —
  новый memfd/`Surface`/`Context`;
* текстуры (когда дойдёт PR3) — upstream: только RGB/`UNSIGNED_BYTE`, ресайз в
  256×256, `GL_NEAREST`, `GL_REPEAT`, `GL_DECAL`.

## Лицензия и происхождение

* `c_src/` — TinyGL 0.4.1, **MIT**, `Copyright (c) 1997-2022 Fabrice Bellard`;
  полный текст — [`LICENSE-TinyGL`](LICENSE-TinyGL) (байт-в-байт upstream).
* Изменён ровно один upstream-файл — `c_src/src/zfeatures.h`
  ([`c_src/PATCHES.md`](c_src/PATCHES.md)).
* Полные notices/список файлов/что не вендорится —
  [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
* Rust/build/probe-код этого крейта — MIT, Pagh-OS; производные файлы несут
  шапку `Derived from TinyGL 0.4.1 …`.

## Файлы

```text
Cargo.toml  build.rs  LICENSE-TinyGL  THIRD_PARTY_NOTICES.md  UPSTREAM.md
c_src/      23 C-файла ядра + 9 заголовков + include/GL/gl.h + PATCHES.md
src/lib.rs  безопасная обёртка (Context/Surface/матрицы/immediate mode/depth/cull)
src/ffi.rs  приватные extern "C" прототипы (+ отсутствующие в gl.h immediate-mode)
src/surface.rs  Surface + FbError (валидация, resize в пределах ёмкости)
src/bin/tinygl-probe.rs  PR1-проба + ELF self-check (INTERP/static-pie)
tests/criterion6.rs      строгий эталон дайджеста/центра/угла + детерминизм
tests/ffi_core.rs        матрицы/viewport/depth/culling/resize/bad-input
tests/probe_binary.rs    проверка собранного бинаря (маркеры/cc/interp)
```
