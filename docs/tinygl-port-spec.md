# TinyGL 0.4.1 → Pagh-OS: спека порта (v1)

**Статус:** research + спецификация. Реализации нет, кода в этом PR нет.
**Скоуп:** `docs/`-документ + решение по лицензии и архитектуре. Версия `Cargo.toml`
не меняется (`dev` несёт `2.24.0`; docs-only коммиты версию не бампают — AGENTS.md,
«Версионирование»).

Что предлагается: небольшой **CPU-only fixed-function** GL-подмножества (OpenGL 1.1)
поверх уже работающего userspace-композитора `pagh-desktop`, чтобы клиенты вида
`pagh-3d`/demo рисовали треугольники/кубы/gears в своё окно **без GPU, без X11/Wayland,
без Mesa и без shaders**.

Не-цели этой волны: GPU/DRM/NVIDIA, современный OpenGL (2.0+, GLSL, FBO/VBO-расширения
ARB), EGL/GLX/Wayland, аппаратный растеризатор, мультиконтекстная потокобезопасность,
mipmapping, blending-расширения, MSAA. Всё перечисленное — вне спеки, и обещать его
нельзя (см. §3.2 и §10).

---

## 0. TL;DR

1. **Лицензия чистая.** TinyGL **0.4.1** (5 марта 2022) переведён на **MIT**:
   официальный changelog — «version 0.4.1: - changed license to MIT»
   (<https://bellard.org/TinyGL/changelog.html>). В самом tarball `LICENSE` — дословный
   MIT-текст, `Copyright (c) 1997-2022 Fabrice Bellard`; tarball'ный `Changelog` повторяет
   ту же строку. Более ранние версии (0.4 и ниже) — не берём (у них другая лицензия и
   другой код). Pagh остаётся MIT.
2. **Форматы совпадают идеально.** TinyGL в 32-битном внутреннем режиме пишет пиксель
   `PIXEL = u32` как `((r<<8)&0xff0000) | (g&0xff00) | (b>>8)`, то есть **`0x00RRGGBB`
   little-endian** — ровно формат PDP/1 `XRGB8888`, который композитор уже принимает.
   TinyGL умеет рендерить **в внешний буфер** (`ZB_open(..., frame_buffer)`,
   `ZB_resize(zb, frame_buffer, xsize, ysize)`), поэтому клиент рендерит **прямо в
   `mmap(MAP_SHARED)` своего memfd** и шлёт композитору только damage — ни копии кадра,
   ни конверсии формата, ни лишнего `commit`-трафика.
3. **Рекомендация:** портировать ядро TinyGL 0.4.1 (**MIT**), отбросив X11/GLX, NanoX и
   BeOS-слои, в отдельный Rust-крейт `rust-apps/tinygl` — переносом C→Rust (вариант B),
   с C-реализацией в роли **dev-оракула** для пиксельных дайджестов. Обоснование и
   альтернатива (собственная MIT-реализация) — §2.6.
4. **Цена вопроса:** ~5.7 тыс. строк C ядра против ~40 kB скомпилированного кода
   (заявка upstream), плюс новый клиент-демо. Сборка — существующий
   `tools/build-rust-app.sh` (`x86_64-unknown-linux-musl`, static, без `INTERP`).
5. **Риски честно:** CPU-растеризация в QEMU TCG будет медленной на больших окнах; объём
   механического переноса велик; без C-оракула легко получить «пиксели не те» незаметно.
   Все три закрываются дешёвыми гейтами (§6–§8).

---

## 1. Инвентарь (проверено в дереве, `origin/dev` = `d5de8b2a`)

### 1.1 Что уже есть и на что мы опираемся

| Компонент | Факт (проверено) | Чем полезен порту |
|---|---|---|
| `rust-apps/pagh-desktop` | userspace-композитор: PDP/1 по AF_UNIX, backend'ы headless и fbdev | владеет экраном и окнами; GL-клиент — обычный PDP/1-клиент |
| `rust-apps/pagh-desktop/src/protocol.rs` | `MAGIC "PDS1"`, `VERSION 1`, `HEADER_LEN 12`, типы 1–16 (`SHM_OFFER 14`, `SHM_READY 15`, `SHM_RELEASE 16`), `WELCOME_SHM_SURFACE = 1<<2` (`protocol.rs:20`, `:22`, `:24`, `:28`–`:47`, `:62`) | путь доставки пикселей без сокета |
| `rust-apps/pagh-desktop/src/shm.rs` | клиентский memfd (`memfd_create` c `MFD_CLOEXEC`, `shm.rs:80`, `:95`), серверный `map_readonly` (`MAP_SHARED\|PROT_READ`, `shm.rs:186`), `send_message_with_fd`/`recv_with_fds` через `SCM_RIGHTS` (`shm.rs:217`, `:275`) | **сервер** не пишет в клиентскую память — рендерить в memfd может только клиент |
| `rust-apps/pagh-ui/src/shm.rs` | клиентская сторона: `MappedBuffer::map_shared` = `mmap(MAP_SHARED, PROT_READ\|PROT_WRITE)` (`pagh-ui/src/shm.rs:140`) | **это и есть** буфер, в который рендерит GL |
| `rust-apps/pagh-desktop/src/surface.rs` | `MAX_SURFACE_DIM 4096`, `MAX_STRIDE_PADDING 4096`, `Xrgb8888` = «`u32` little-endian `0x00RRGGBB`; альфа игнорируется» (`surface.rs:7`, `:9`; вариант и комментарий формата — `surface.rs:13`–`:15`) | формат кадра = формат `PIXEL` в TinyGL 32bpp |
| `rust-apps/pagh-desktop/PROTOCOL.md` §8.1 | ShmSurface: `stride ∈ [width*bpp, width*bpp+4096]`, `data_len == stride*height`, сервер отдаёт монотонный `generation`, старый memfd не переиспользуется (`PROTOCOL.md:316`–`:357`) | контракт поверхности окна |
| `rust-apps/pagh-desktop/src/fbdev.rs` | `/dev/fb0`: `FBIOGET_VSCREENINFO 0x4600` / `FBIOGET_FSCREENINFO 0x4602`, `FbLayout` (маски → `pack`/`unpack`), `blit_damage` (`fbdev.rs:23`, `:25`, `:148`, `:300`, `:390`) | композитор сам конвертирует XRGB→scanout; GL этого не касается |
| `rust-apps/pagh-desktop/tests/render_goldens.rs` | пиксельные goldens: FNV-1a 64 по словам пикселей (`render_goldens.rs:18`) | готовый шаблон «pixel checks» для демо (§6.2) |
| `rust-apps/pagh-desktop/src/timing.rs` | `now_cycles()` (rdtsc), `cycles_to_ms` (`timing.rs:77`, `:137`) | замер frame time |
| `tools/build-rust-app.sh` | `TARGET=x86_64-unknown-linux-musl`, `--release --locked`, установка в `rust-apps/out/`, отказ при наличии `INTERP` (`build-rust-app.sh:6`, `:26`, `:31`–`:34`) | статическая сборка демо |
| `tools/e2e.py` | `_desktop_bins()` собирает `pagh-desktop`/`pagh-files`/`pagh-terminal` через этот же скрипт (`e2e.py:2969`–`:2996`); режим `desktop` (`e2e.py:4029`) | место для live e2e GL-демо |
| `vendor/cc-1.2.0` | `cc` уже vendored (пока не используется: `build.rs` в `rust-apps/` нет) | лазейка для варианта A (§5.1) |

### 1.2 Как окно получает пиксели (главное для интеграции)

Два пути, оба в продакшене:

1. **ShmSurface (приоритет).** Клиент делает `memfd_create`, `ftruncate` до
   `stride*height`, `mmap(MAP_SHARED, PROT_READ|PROT_WRITE)`, отправляет **один** fd через
   `SCM_RIGHTS` в `SHM_OFFER` (формат, размеры, stride), получает `SHM_READY` с
   `generation` и дальше **рисует прямо в свой mapping**, присылая только `COMMIT`
   (header flag bit 0) и `DAMAGE`-прямоугольники. Сервер отображает тот же memfd
   `MAP_SHARED|PROT_READ` и композитит. Пиксели по сокету не идут.
2. **Frame upload (fallback).** Полный кадр `XRGB8888`/`RGB565` внутри сокета, если fd
   passing недоступен или сервер отказал.

Следствие для TinyGL: `ZBuffer` надо открыть **поверх клиентского mapping**, а не поверх
собственного `malloc`-буфера — тогда «блит в окно» отсутствует как операция.

```
pagh-gl-demo (ring 3)                          pagh-desktop (ring 3)
─────────────────────                          ─────────────────────
memfd_create + ftruncate(w*h*4)
mmap(MAP_SHARED, RW) ───────────────┐
SHM_OFFER(fd via SCM_RIGHTS) ───────┼────────► validate format/stride/size
                                    │          map_readonly(same memfd)
SHM_READY(generation) ◄─────────────┼─────────┘
ZB_open/ZB_resize(zb, mapping, w, h)
glClear / glBegin(GL_TRIANGLES) ... (пишет прямо в mapping)
COMMIT(generation) + DAMAGE(rects) ─────────► compose + blit_damage → /dev/fb0
```

### 1.3 Чего в дереве нет

* **Никакого GL**: `grep -ri "opengl\|glbegin\|tinygl\|glx"` по `src/`, `rust-apps/`,
  `docs/`, `tools/` даёт единственное совпадение — `docs/desktop-render-spec.md`, где
  OpenGL перечислен в non-goals. То есть точку входа пишем с нуля.
* **Никакой CPU-растеризации**, кроме 2D-холста `pagh-ui` (`Canvas`) и композитора.
* **CI не собирает `rust-apps/`**: в `.github/workflows/ci.yml` пять job'ов (kernel,
  host-tests, static-policy, docs-claims, формат) и ни один не трогает userspace-крейты;
  `cargo fmt --all` в корне тоже их не видит (у каждого rust-app свой `[workspace]`,
  корневой `Cargo.toml` не имеет `members`, только `exclude = ["host-tests"]`).
  Значит тесты нового крейта надо запускать явной командой (§8.0), иначе они мертвы.
* **`rust-apps/README.md`** прямо ограничивает: «GUI libraries are not supported: there is
  no X11/Wayland server, only the kernel's own framebuffer», программы должны оставаться
  однопоточными. Спека этому не противоречит: порт однопоточный, X11-слой выбрасывается.

---

## 2. Лицензия: решение и доказательство

### 2.1 Факт смены лицензии (официальный changelog)

`https://bellard.org/TinyGL/changelog.html` (зафетчено, HTTP 200), дословно:

```
# Changelog for TinyGL

## version 0.4.1:
 - changed license to MIT
 - fixed compilation errors
 - fixed lighting bug
 - fixed color conversions
```

Та же строка есть и в tarball'ном `Changelog`:

```
version 0.4.1:
 - changed license to MIT
```

Страница загрузки <https://bellard.org/TinyGL/> подтверждает: «(Mar 5 2022) TinyGL 0.4.1 is
out», ссылка — `TinyGL-0.4.1.tar.gz`.

### 2.2 Точный текст notice, который обязан сохраняться

Файл `LICENSE` из tarball'а 0.4.1 — дословный MIT:

```
TinyGL

Copyright (c) 1997-2022 Fabrice Bellard

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL
THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

README внутри tarball'а это подтверждает строкой «MIT license for easy integration in
commercial designs (read the LICENSE file)».

**Честная деталь, которую не надо замалчивать:** HTML-страница проекта всё ещё
описывает лицензию как «Zlib-like licence ... (read the LICENCE file)» — это устаревший
текст страницы (заголовок «News» там обновлён до 0.4.1, а абзац про лицензию — нет).
Первоисточник — файлы в tarball'е (`LICENSE` + `README` + `Changelog`), и все три
согласованно говорят MIT. Zlib-лицензия, к слову, тоже permissive и MIT-совместима, так
что расхождение сайта не меняет вывод.

### 2.3 Что именно берём (артефакт и его контрольная сумма)

| Параметр | Значение |
|---|---|
| Артефакт | `TinyGL-0.4.1.tar.gz` с <https://bellard.org/TinyGL/TinyGL-0.4.1.tar.gz> |
| Размер | 68 800 байт |
| SHA-256 | `b314f019a778d583a3778cb14bcc2a57626a8f866cbed0693705bb1bb785b16f` |
| Дата файлов в архиве | 2022-03-05 |
| `src/` | 35 файлов `.c`/`.h`, **6808** строк C (`ls src/*.c src/*.h \| wc -l`; `wc -l`) |
| Заявка upstream | «compiled code size of about 40 kB on x86» (README) |

Сумма посчитана локально из скачанного файла; при вендоринге её надо перепроверить и
зафиксировать в `THIRD_PARTY_NOTICES` (§2.5).

**Старые версии не берём.** У 0.1–0.4 другая лицензия и другой код (changelog: 0.4.1 —
«fixed compilation errors / fixed lighting bug / fixed color conversions»). Любой
репозиторий-форк TinyGL, который вырос из до-0.4.1 кода, для нас **не источник** — только
как поведенческий референс.

### 2.4 Что обязано попасть в репозиторий

MIT требует сохранять copyright + permission notice в копиях «Software» и «substantial
portions». Практический минимум:

1. `rust-apps/tinygl/LICENSE-TinyGL` — **байт-в-байт** текст из §2.2 (без правок,
   без перевода, без переформатирования).
2. `rust-apps/tinygl/THIRD_PARTY_NOTICES.md` — кто, откуда, версия, sha256 tarball'а,
   что именно перенесено (файл → модуль), и явное «upstream: Fabrice Bellard, MIT,
   0.4.1», плюс пометка, что `glx.c`/`nglx.c`/`oscontext.c`/`BeOS/*` не переносились.
3. Шапка в корне каждого перенесённого модуля (`src/lib.rs`, `raster.rs`, …):
   `// Derived from TinyGL 0.4.1 (c) 1997-2022 Fabrice Bellard, MIT — see LICENSE-TinyGL`.
4. Строка в корневом `README.md` (раздел лицензий) и/или в `rust-apps/README.md`:
   «`pagh-gl`/`tinygl` содержит код, производный от TinyGL 0.4.1 (MIT, Fabrice Bellard)».
5. **Не** нужен отдельный `NOTICE` в корне и **не** нужна смена лицензии Pagh: MIT + MIT
   совместимы, `LICENSE` репозитория (`MIT License, Copyright (c) 2026 ccclosed`) остаётся.

### 2.5 Порт 0.4.1 vs собственная MIT-реализация

| Критерий | A. Порт TinyGL 0.4.1 (C через `cc`) | B. Порт TinyGL 0.4.1 в Rust (рекомендую) | C. Собственная MIT-реализация |
|---|---|---|---|
| Лицензия | MIT, нужен notice | MIT, нужен notice | своя, notice не нужен |
| Объём работы | ~0 строк (сборка C) | ~5.7 тыс. строк переноса (механически) | ~3–5 тыс. строк **нового** кода + отладка |
| Toolchain | нужен C-компилятор для `x86_64-unknown-linux-musl` в `build-rust-app.sh`-пути | только `cargo`/`rustup` (как сейчас) | только `cargo`/`rustup` |
| Соответствие философии репо («чистая логика → host property tests», AGENTS.md) | низкое: внутренности C недоступны host-тестам | высокое: матрицы/растеризация — чистые Rust-функции | высокое |
| Полнота | 100 % от 0.4.1 (GL 1.1 subset, display lists, lighting, texture objects, selection) | 100 % (если переносить всё ядро) | по определению уже: MVP = треугольники+depth+текстуры, остальное «потом» |
| Риск «пиксели не те» | низкий (код проверен временем) | средний (перенос) → снимается C-оракулом | **высокий**: clipping, perspective-correct текстуры, fixed-point — классические источники тонких багов |
| Сопровождение | upstream заморожен с 2022 (0.4.1 — последняя), обновлений ждать неоткуда | то же + ручной трекинг | свой бэклог багов |
| Демо в QEMU | быстро (2 PR'а) | средне (5–6 PR'ов) | медленно и непредсказуемо |

**Рекомендация — вариант B** (порт в Rust), по трём причинам:

1. **Философия и гейты репозитория.** AGENTS.md требует «чистая логика → host property
   tests». У `rust-apps/` сегодня **нет ни одного `build.rs`** и ни одной C-зависимости;
   вариант A вводит в userspace-сборку C-компилятор, которого сейчас не требует ни один
   путь (`build-rust-app.sh` знает только `rustup target add` + `cargo build`). Это
   ломает воспроизводимость сборки на машинах без `musl-gcc`/clang.
2. **Переносимость.** Контейнер `PIXEL`/фиксированная арифметика TinyGL переводится в
   Rust почти один-к-одному (`u32`/`i32`, `<<`, `>>`); ничего из C-специфики (указатели,
   varargs, `assert`) не является алгоритмической сутью. Ядро — 5.7 тыс. строк, из них
   ~1.05 тыс. строк — X11/NanoX/BeOS-обвязка, которую мы **не** переносим (§5.2).
3. **Риск переноса управляем.** C-реализация остаётся в репозитории **как dev-оракул**
   (только для host-тестов, не в продукте): рендерим один и тот же кадр C-версией и
   Rust-портом, сравниваем FNV-1a дайджест (§6.2). Расхождение локализуется по стадии
   (матрицы → clip → raster → texture).

Fallback: если фаза 0 покажет, что перенос дороже ожидаемого, вариант A остаётся
**легальным** (MIT) — спека фиксирует его форму: вендоренный `TinyGL-0.4.1.tar.gz` +
`build.rs` с `cc` (уже в `vendor/cc-1.2.0`) + `LICENSE-TinyGL` + линковка `libm` из musl.

**Что не берём ни в каком варианте:** код ALMOS, Serenity, Redox, Mesa. Эти ОС —
**поведенческие** референсы (§12), их лицензии в этом ревью не проверялись построчно, и
копировать оттуда нельзя ни строки.

---

## 3. API subset (OpenGL 1.1 fixed-function)

### 3.1 Что реально реализовано в 0.4.1

Опора — `LIMITATIONS` и `include/GL/gl.h` из tarball'а (78 уникальных `gl*`-имён; включая
TinyGL-специфичные `glInit`, `glClose`, `glDebug`, которых в OpenGL нет).

| Группа | API | Статус в 0.4.1 | Наш план |
|---|---|---|---|
| Контекст/состояние | `glInit`, `glClose`, `glEnable`, `glDisable`, `glGetIntegerv`, `glGetFloatv` | есть; `glGet*` — урезанный список pname (VIEWPORT, MAX_*_STACK_DEPTH, MAX_LIGHTS, MAX_TEXTURE_SIZE; MATRIX-запросы, LINE_WIDTH, POINT_SIZE) | переносим как есть; перечисление pname — в доке и в host-тесте «неизвестный pname → ошибка» |
| Матрицы | `glMatrixMode`, `glLoadIdentity`, `glLoadMatrixf`, `glMultMatrixf`, `glPushMatrix`, `glPopMatrix`, `glRotatef`, `glTranslatef`, `glScalef`, `glFrustum`, `glOrtho` | есть (double-прототипы — нет) | переносим; стек modelview/projection/texture с лимитами; **`f32` только** |
| Viewport/clear | `glViewport`, `glClear`, `glClearColor`, `glClearDepth` | есть; важное ограничение: `glClear` чистит **весь** Z-буфер и цвет независимо от `glScissor`/маски; начальный z не переопределяется | переносим как есть (честно документируем; клипы делаем damage-прямоугольниками на уровне окна, не GL) |
| Примитивы | `glBegin`/`glEnd`, `glVertex*` (часть прототипов), `GL_POINTS/LINES/LINE_STRIP/LINE_LOOP/TRIANGLES/TRIANGLE_STRIP/TRIANGLE_FAN/QUADS/POLYGON` | есть; `GL_QUADS`/`GL_POLYGON` разворачиваются внутри | переносим полный набор — это ровно то, что нужно демо |
| Атрибуты | `glColor*`, `glNormal*`, `glShadeModel(GL_FLAT/GL_SMOOTH)` | есть (часть прототипов отсутствует) | переносим float-формы; целочисленные формы — только если дешево |
| Depth | `glEnable(GL_DEPTH_TEST)`, `glDepthFunc`, `glDepthMask`, `glClearDepth` | **честное ограничение:** depth test реально влияет только на линии; для прочих примитивов Z-буфер используется всегда, `DepthMode` жёстко `GL_LESS`; `glDepthMask`/`GL_DEPTH_TEST`-off появились в форках, **в 0.4.1 их нет** | переносим поведение 0.4.1; если хотим `glDepthMask` — это **отклонение от upstream**, отдельный PR и отдельная строка в доке |
| Z-буфер | `ZB_Z_BITS 16`, `ZB_POINT_Z_FRAC_BITS 14` (`zbuffer.h:10`, `:12`) | 16-битный Z, фиксированная точка | оставляем 16 бит (память и скорость); 24/32-битный Z — не цель |
| Цвета | `RGB_TO_PIXEL` в 32bpp = `0x00RRGGBB`; в 16bpp = `rrrrrggggggbbbbb` | есть | **наш путь — 32bpp** (§4.2) |
| Текстуры | `glGenTextures`, `glBindTexture`, `glDeleteTextures`, `glTexImage2D`, `glTexEnvi`, `glTexParameteri`, `glTexCoord*` | `glTexImage2D` принимает **только RGB + GL_UNSIGNED_BYTE**, внутри **ресайзит в 256×256**; mipmap нет; `glTexEnvi` — только `GL_DECAL`; фильтры — только `GL_NEAREST`; wrap — только `GL_REPEAT`; `glTexCoord` не прогоняется через матрицу | переносим как есть, ограничения — в доку крейта; «правильные» фильтры/mipmap — не обещаем |
| Массивы | `glEnableClientState`, `glDisableClientState`, `glVertexPointer`, `glNormalPointer`, `glColorPointer`, `glTexCoordPointer`, `glArrayElement` | поддержка OpenGL 1.1 arrays «limited» (`TGL_FEATURE_ARRAYS 1`, `zfeatures.h:7`); **`glDrawArrays`/`glDrawElements` в 0.4.1 отсутствуют вовсе** (проверено: `grep -rn "DrawArrays\|DrawElements" src/ include/ examples/` — пусто), индексация идёт через `glArrayElement` из `glBegin`/`glEnd`; `GL_INDEX_ARRAY_POINTER`/`GL_EDGE_FLAG_ARRAY_POINTER` — нет | переносим arrays как есть; **VBO нет** — ни настоящих (GL 1.5/ARB), ни `glDrawArrays` (см. §3.2). Если клиенту нужен батчинг — display lists, не VBO |
| Display lists | `glNewList`/`glEndList`/`glCallList`/`glGenLists`/`glDeleteLists`/`glIsList` | есть (`TGL_FEATURE_DISPLAYLISTS`) | переносим: это дешёвая экономия CPU и то, на чём стоят демо |
| Selection | `glRenderMode(GL_SELECT)`, `glSelectBuffer`, `glInitNames`, `glPushName`/`glPopName`, `glLoadName` | полный handling selection mode | переносим позже (фаза 4), демо на нём не стоит |
| Прочее | `glCullFace`, `glFrontFace`, `glPolygonMode`, `glLineWidth`, `glPointSize`, `glPolygonOffset`, `glPolygonStipple`, `glBlendFunc`, `glFog*`, `glHint`, `glFlush`, `glPixelStorei`, `glRasterPos2f`, `glEdgeFlag` | часть — реально, часть — пустые/«only API»: `glPolygonOffset` не реализован, `glHint`/`glFlush` игнорируются, `glPixelStorei` — только byte-aligned | переносим сигнатуры + честные no-op'ы, чтобы клиентский код компилировался |
| Освещение | `glLight*`, `glLightModel*`, `glMaterial*`, `glColorMaterial`, `glEnable(GL_LIGHTING/GL_LIGHTx)` | есть, но «not very fast» (upstream сам это пишет) | переносим; в бенчмарках — отдельная строка «lighting on/off» |

Удаляем **без переноса**: `glx.c` (413), `nglx.c` (128), `oscontext.c` (84), `BeOS/*`
(293 строки) — X11/NanoX/BeOS-обвязка, к растеризации не относится. Новый backend —
`PaghSurface` (§4).

### 3.2 Чего мы НЕ обещаем (фиксируем в доке крейта)

* **Никаких shaders** — нет GLSL, нет `glCreateShader`, нет programmable pipeline. Это
  fixed-function, и точка.
* **Никакого современного OpenGL** (2.0+), расширений `ARB_*`/`EXT_*`, FBO, VAO, настоящих
  VBO, `glDrawElements`-как-в-1.5, uniform-буферов, MSAA, стерео.
* **Никакого GPU**: CPU-растеризация единственная. GPU/NVIDIA/DRM/KMS — вне спеки.
* **Никакой ускоренной композиции в GPU**, никакого DMA-BUF: путь — memfd.
* **Нет 8-битного paletted режима** (dither выбросим вместе с `zdither.c`/`ZB_MODE_INDEX`,
  чтобы не тащить 225-цветную таблицу), нет color index mode — upstream сам пишет «no
  longer useful».
* **Нет цветового clamping выше 1.0 / HDR**, нет гамма-коррекции.

### 3.3 Контракт контекста (наш, не GL)

Публичный Rust-API крейта (`tinygl::Context`) — узкий и проверяемый:

```rust
pub struct Context { /* матрицы, стеки, lights, textures, state */ }
pub struct Surface<'a> { pixels: &'a mut [u32], width: u32, height: u32, stride_px: u32 }

impl Context {
    pub fn new() -> Self;                            // = glInit
    pub fn begin_frame(&mut self, surface: Surface<'_>);   // ZB_open/ZB_resize
    pub fn end_frame(&mut self);                     // resolve, вернуть damage
    pub fn viewport(&mut self, x: i32, y: i32, w: u32, h: u32);
    pub fn clear(&mut self, color: u32, z: u16);
    pub fn matrix_mode(&mut self, mode: MatrixMode);
    pub fn load_identity(&mut self);
    pub fn rotate(&mut self, angle_deg: f32, x: f32, y: f32, z: f32);
    // … 1:1 с §3.1
}
```

`Surface` — `&mut [u32]` поверх клиентского `mmap`, **stride в пикселях** (не в байтах),
чтобы один и тот же тип покрывал и плотный буфер (`stride_px == width`), и выровненный.

Ошибки — enum, не `GLenum`-числа (GL-совместимый слой, если понадобится, добавим
обёрткой; `glGetError`-семантику не эмулируем, `LIMITATIONS` 0.4.1 её и не обещает).

---

## 4. Framebuffer surface в композиторе

### 4.1 Поток (обязательный, менять нельзя)

1. `memfd_create("pagh-gl-fb", MFD_CLOEXEC)` → `ftruncate(w*h*4)`.
2. `mmap(NULL, w*h*4, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0)` — **клиентская** сторона
   (`pagh-ui/src/shm.rs:140` — переиспользовать, не писать заново).
3. `SHM_OFFER { width, height, format = XRGB8888, stride = w*4, data_len = w*h*4 }` +
   fd через `SCM_RIGHTS`.
4. `SHM_READY { generation }` — запомнить `generation`.
5. `Context::begin_frame(Surface { pixels: mapping_as_u32, width, height, stride_px: w })`.
6. Рисовать кадр (весь кадр или damage-область).
7. `COMMIT { generation }` + `DAMAGE { rects }`.
8. По `SHM_RELEASE`/смене `generation` — **новый memfd** и `SHM_OFFER` заново
   (`PROTOCOL.md:357`: старый memfd не переиспользуется).

### 4.2 Требования к буферу (из кода, не из головы)

| Требование | Источник | Следствие |
|---|---|---|
| `stride == width*4`, **без паддинга** | TinyGL `zbuffer.c:28`: `linesize = (xsize*PSZB + 3) & ~3`; при `PSZB=4` это ровно `xsize*4` | PDP/1 разрешает паддинг до +4096 (`PROTOCOL.md:32`), но TinyGL его не выражает → **берём плотный stride**, это легально |
| **`width` кратно 4** | `zbuffer.c:94`: `xsize = xsize & ~3` в `ZB_resize` | окно с `width % 4 != 0` молча ужмётся → **валидируем `width % 4 == 0` в демо и в клиентском хелпере**, иначе «правая полоса не рисуется» |
| Внешний буфер, а не `malloc` | `zbuffer.c:105`–`:113`: `frame_buffer_allocated = 0` при переданном указателе | рендер идёт **прямо в memfd**, `ZB_copyFrameBuffer` не нужен |
| 32bpp внутренний формат | `zfeatures.h:39` — активное `TGL_FEATURE_RENDER_BITS 16` (по умолчанию!), `:41` — закомментированное `32` | **обязательная** правка при порте: `RENDER_BITS=32` + `TGL_FEATURE_32_BITS`; иначе пиксель будет `u16` и формат не совпадёт |
| `PIXEL == 0x00RRGGBB` | `zbuffer.h:60`–`:64`: `RGB_TO_PIXEL(r,g,b) = ((r<<8)&0xff0000)\|((g)&0xff00)\|(b>>8)`, `PIXEL=unsigned int` | совпадает с `Xrgb8888` (§1.1) — конверсии нет; альфа-байт = 0 и композитор его игнорирует |
| Z-буфер отдельный, 16 бит | `zbuffer.c:103`, `zbuffer.h:79` | клиент платит `w*h*2` байт на Z — держать в heap клиента, не в memfd |

Итог: `ZBuffer`-эквивалент в крейте = `{ pixels: &mut [u32], z: Vec<u16>, w, h }`,
`begin_frame` только переустанавливает указатель при смене размера (никаких аллокаций
кадра в hot-path).

### 4.3 Рендер-цикл

* **Полный кадр** (первый кадр, resize, смена generation): `glClear` + сцена +
  `DAMAGE` на весь клиентский прямоугольник.
* **Анимация** (gears/spinning cube): либо полный клиентский прямоугольник (проще,
  детерминированный дайджест), либо bounding-box движущейся геометрии. Начинаем с
  полного — проще и честнее по пикселям; damage-оптимизация — фаза 5.
* **Тайминг**: `timing::now_cycles()` вокруг кадра, `cycles_to_ms` — по образцу
  `pagh-desktop`.

### 4.4 Resize

Композитор меняет размер окна (`CONFIGURE`) → клиент делает **новый** memfd нужного
размера и `SHM_OFFER` заново (старый `generation` инвалидируется). В крейте —
`begin_frame` с новой `Surface`; `ZB_resize`-эквивалент перевыделяет только Z-буфер.
Хук `gl_resize_viewport` из 0.4.1 (`zgl.h:274`, вызов — `misc.c:23`) в порте становится
обратным вызовом `Context::on_resize` — или просто удаляется, потому что размер приходит
извне и клиент сам решает (предпочтительно: меньше состояний).

### 4.5 Fallback

Если `welcome.flags` не содержит `WELCOME_SHM_SURFACE` (bit 2) или `SCM_RIGHTS` не
работает — демо **честно падает** с внятной ошибкой и кодом выхода, а не «рисует в никуда».
Frame-upload путь для GL не поддерживаем (копирование `w*h*4` по сокету каждый кадр — ровно
то, чего спека избегает).

---

## 5. Сборка: static musl, крейт, линковка

### 5.1 Вариант A (C через `cc`) — форма, если понадобится

* `rust-apps/tinygl-c/` (или `vendor/tinygl-0.4.1/`) с **неизменёнными** файлами upstream,
  кроме `zfeatures.h` (RENDER_BITS 32, выключить 8/24/15) и вырезанных `glx.c`/`nglx.c`.
* `build.rs` + `[build-dependencies] cc = "1"` — крейт `cc-1.2.0` **уже vendored**.
* Требует C-компилятора: для `x86_64-unknown-linux-musl` это `musl-gcc` либо clang с
  `--target=x86_64-unknown-linux-musl`; `build-rust-app.sh` сейчас этого не требует, и
  его пришлось бы расширять (плюс проверка `readelf` на `INTERP` остаётся).
* `LICENSE-TinyGL` обязателен; **в исходниках 0.4.1 copyright-шапок нет** (проверено:
  `grep -rl "Copyright" src/*.c src/*.h include/GL/*.h` → 0 файлов), copyright живёт
  только в `LICENSE`, поэтому `LICENSE-TinyGL` + `THIRD_PARTY_NOTICES.md` критичны —
  без них порт был бы распространением без notice.

### 5.2 Вариант B (Rust, рекомендуемый) — структура

```
rust-apps/tinygl/            # отдельный [workspace] (как у pagh-ui/pagh-desktop)
├── Cargo.toml               # name = "tinygl", license = "MIT", panic = "abort", opt-level = 3, lto = true
├── LICENSE-TinyGL           # дословный MIT из §2.2
├── THIRD_PARTY_NOTICES.md   # §2.4
└── src/
    ├── lib.rs               # Context, публичный API §3.3
    ├── math.rs              # ← zmath.c/.h     (275/53 строки C)
    ├── matrix.rs            # ← matrix.c       (241)
    ├── vertex.rs            # ← vertex.c       (364)
    ├── clip.rs              # ← clip.c         (445)
    ├── raster.rs            # ← ztriangle.c/.h (394/363)
    ├── line.rs              # ← zline.c/.h     (84/121)
    ├── fb.rs                # ← zbuffer.c/.h   (518/153), наша Surface вместо ZB_open
    ├── api.rs               # ← api.c          (668) — glBegin/glEnd/…
    ├── state.rs             # ← init.c (189), misc.c (147), get.c (72)
    ├── texture.rs           # ← texture.c (229), image_util.c (136), texobj-путь
    ├── light.rs             # ← light.c (304)
    ├── arrays.rs            # ← arrays.c (208)
    ├── display_list.rs      # ← list.c (255)
    ├── select.rs            # ← select.c (114)
    ├── clear.rs             # ← clear.c (30)
    └── error.rs             # ← error.c (16), msghandling.c (52)
```

Итого ~5.7 тыс. строк C → Rust (все числа выше — `wc -l` из tarball'а). Модули
`glx.c`/`nglx.c`/`oscontext.c`/`BeOS/*` (1 054 строки вместе с BeOS) **не переносятся**.

Свойства крейта:

* **`no_std` не требуется** (userspace, есть allocator), но и libc не нужен: `sin`/`cos`/
  `sqrt`/`fabs`, которыми пользуются `zmath.c:212`–`:248` и `light.c:115`–`:276`, в Rust —
  методы `f32` (на musl дают тот же IEEE-результат; при сравнении с C-оракулом
  допускаем ULP-расхождения только там, где upstream сам собирает `-ffast-math` —
  upstream Makefile не включает её, так что ожидаем бит-в-бит на x86-64).
* `panic = "abort"`, `opt-level = 3`, `lto = true` — как в `pagh-desktop/Cargo.toml`
  (горячий путь — попиксельные циклы).
* **`static-pie`**: цель `x86_64-unknown-linux-musl` в Rust по умолчанию собирает
  static-pie; `tools/build-rust-app.sh:31`–`:34` проверяет отсутствие `INTERP` —
  этого достаточно, отдельный флаг не нужен. PIE-перемещения kernel-ABI поддерживает
  (им же собраны `pagh-files`/`pagh-terminal`).
* Никаких новых `[patch.crates-io]`, никаких новых внешних зависимостей (кроме, в
  варианте A, `cc` для `build.rs`).

---

## 6. Демо и пиксельные проверки

### 6.1 Три демо (в одном бинаре `rust-apps/pagh-gl-demo`)

| Демо | Что рисует | Зачем |
|---|---|---|
| `triangle` | 1 треугольник с интерполированным цветом (Gouraud), фиксированный угол | минимальный proof: матрицы + clip + raster + цвет |
| `cube` | куб 6 граней, `GL_DEPTH_TEST`, вращение на **фиксированный шаг** (детерминированно), painter-free | proof Z-буфера и culling; классическая «кубик крутится» |
| `gears` | 3 шестерёнки (аналог `examples/gears` TinyGL, но **написанный заново** на нашем API) | proof нагрузки: много треугольников, lighting off/on, frame time |

Ключи: `<demo> --frames N --out <dir> --hash` — печатает FNV-1a дайджест каждого кадра и
время на кадр (`min/median/max ms`). Без `--connect` — headless (рендер в `Vec`, дайджест
без композитора), с `--connect` — живой PDP/1 к композитору.

### 6.2 Детерминизм и pixel checks

Эталон — `tests/render_goldens.rs` (§1.1): **FNV-1a 64 по словам пикселей** (построчно),
не PNG-дифф. Почему так:

* хеш падает на любое изменение цвета/геометрии/сглаживания, а не только на «картинка
  отличается размером»;
* не требует хранить бинарные golden-файлы в репозитории (PNG-гoldens тяжело ревьюить);
* одинаково работает в host-тесте и в QEMU-госте.

Правила:

1. Кадр рендерится с **фиксированными** параметрами (размер, угол, seed, `f32`, без
   времени в кадре).
2. Дайджест печатается как `TINYGL <demo> frame=<n> digest=0x................ w=<w> h=<h>`
   — маркер годен для grep из `tools/e2e.py` (там уже так устроены `[BOOTID]`,
   `LXSELFTEST`).
3. Эталонные значения — в host-тесте крейта; в QEMU сравниваются **те же** значения
   (одинаковый `f32`-путь, x86-64, без fast-math).
4. Дополнительно — точечные проверки конкретных пикселей (центр треугольника, угол
   усечённого куба, «в шестерёнке есть дырка» — как в tinygl-демо `NO_DRAW_COLOR`,
   упомянутом в обзоре форка jserv): они локализуют баг лучше, чем один хеш.

### 6.3 Frame time

* `timing::now_cycles()` (rdtsc) вокруг `begin_frame..end_frame` и вокруг полного
  кадра вместе с `COMMIT`.
* Печать: `TINYGL <demo> ft=min/med/max ms cycles_per_ns=…`.
* **Честность по QEMU**: TCG не даёт реального `cycles`-в-`ns` (TSC в QEMU виртуален).
  Поэтому в QEMU печатаем и **ms по wall-clock** (`std::time::Instant`) — она
  сопоставима только между прогонами в том же QEMU-конфиге. Формулировка для отчёта:
  «frame time измерен в QEMU TCG (медленнее реального железа); абсолютные числа — верхняя
  граница, не бенчмарк GPU».
* Отдельно: **frame budget на 1280×800**: gears в 60 fps на CPU недостижимы в TCG —
  принимаем «>= 1 fps в QEMU на 640×480», а на железе — измеряем и публикуем (без
  обещаний).

---

## 7. Acceptance (объективно проверяемое)

Демо считается принятым, если **все** пункты выполнены:

1. **Пиксели в окне композитора (QEMU fbdev).** `tools/e2e.py desktop` (или новый
   подрежим) поднимает QEMU, запускает композитор в fbdev-режиме и GL-демо как клиента;
   скриншот QEMU-framebuffer'а содержит **ненулевую** GL-геометрию в клиентском
   прямоугольнике: проверяем (а) присутствие характерных цветов (`#RRGGBB` из демо),
   (б) отсутствие полного «чёрного/фонового» кадра, (в) серийный маркер
   `TINYGL <demo> … digest=0x…` из гостя.
2. **Pixel checks PASS.** Дайджесты host-рендера == дайджесты, напечатанные в QEMU-госте,
   для `triangle`, `cube` (статические позы), `gears` (фиксированные углы).
3. **Frame time измерен** и напечатан (`min/med/max ms`) для каждого демо на
   `640×480` и `1280×800`, в том же прогоне, что (1).
4. **Нет регрессий:** `pagh-files` и `pagh-terminal` продолжают работать в том же
   QEMU-сеансе (окна, ввод, Alt+Shift+F4 launch) — GL-клиент не ломает протокол.
5. **Гейты 6/6 зелёные** (см. §8.0) + `cargo test` крейта зелёный.
6. **Лицензионная чистота:** `LICENSE-TinyGL` байт-в-байт равен MIT-тексту §2.2;
   `THIRD_PARTY_NOTICES.md` называет upstream, версию и sha256 tarball'а; в шапках
   производных модулей есть строка о происхождении.
7. **Честные ограничения** опубликованы: в README крейта и в корневом `README.md`
   (раздел «Ограничения») сказано: CPU-only, fixed-function subset, без shaders/GPU/
   современного GL, 16-битный Z, ширина окна кратна 4.

---

## 8. Тесты

### 8.0 Гейты (не менять состав)

Шесть команд AGENTS.md остаются арбитром и на каждый PR должны быть зелёными:

```sh
cargo fmt --all -- --check
python3 tools/build.py build
python3 tools/build.py build --release
python3 tools/host_tests.py
python3 tools/check_safety.py
python3 tools/check_agents_md.py
```

Важно: **ни одна из них не покрывает `rust-apps/`** (§1.3). Поэтому для нового крейта
вводится **явная документированная команда** в его README (и в PR-описании):

```sh
cd rust-apps/tinygl && cargo test          # host property/unit-тесты крейта
cd rust-apps/tinygl && cargo fmt --check   # локальная дисциплина (не гейт CI)
```

Расширять CI/`tools/host_tests.py`, чтобы он гонял rust-apps-тесты, **в этой спеке не
предлагается**: это меняет контракт `check_agents_md.py` (он сверяет шаги CI с блоком
команд в AGENTS.md в обе стороны) и требует отдельного решения lead — см. §11, Q4.

### 8.1 Host-тесты (чистые свойства, без QEMU и без окна)

Модули `math`/`raster`/`clip` держим чистыми (никакого I/O), чтобы покрыть свойствами —
по философии репозитория:

| Свойство | Что утверждает |
|---|---|
| `matrix_identity_is_neutral` | `glLoadIdentity` × любые `push/pop` не меняют вершину |
| `matrix_product_associative_f32` | композиция `M·N` в пределах ULP против эталонной формулы |
| `rotate_360_is_identity` | `rotate(360°)` ≈ identity с допуском 1e-5 |
| `frustum_planes_sign` | точка внутри frustum → все clip-условия ≥ 0; снаружи — хотя бы одно < 0 |
| `clip_produces_interior_vertices` | после клипа все вершины в [-w, w] по осям |
| `raster_covers_triangle_area` | число закрашенных пикселей ≈ |Shoelace| в пределах ±периметр (top-left rule) |
| `raster_no_pixel_outside_bbox` | ни один пиксель не вне bounding box (clip к scissor/окну) |
| `depth_less_wins` | при равном цвете ближний треугольник побеждает при любом порядке рисования |
| `clear_sets_uniform` | после `clear` все пиксели == заданный `0x00RRGGBB`, Z == `clear_z` |
| `texture_uv_bilinear_nearest` | при `GL_NEAREST` значение пикселя == тексель по `floor(u*256)` |
| `format_rrggbb_roundtrip` | `RGB_TO_PIXEL` порта == эталонная формула upstream на 65 536 значениях |
| `width_not_multiple_of_4_rejected` | клиентский хелпер возвращает ошибку, а не молча ужимает (§4.2) |

Плюс **оракульная проверка** (вариант B, dev-only): C-сборка TinyGL 0.4.1 (в
`/tmp`/вне продукта, из запиненного tarball'а) рендерит те же сцены; тест сравнивает
дайджесты. Пока оракул не в репозитории — эталонные дайджесты фиксируются один раз
руками и коммитятся как константы (как в `render_goldens.rs`).

### 8.2 Live e2e (QEMU)

* Предпочтительно — **расширить существующий режим**: `tools/e2e.py desktop` уже
  собирает бинари (§1.1) и проверяет fbdev/evdev/окна; добавляем фазу
  `_desktop_gl_phase()`, которая запускает GL-демо как третьего клиента и проверяет
  серийные маркеры + скриншот.
* Демо-бинарь кладём на тот же prepared disk (`/mnt/pagh-gl-demo`), как
  `pagh-files`/`pagh-terminal` (`e2e.py:2995`–`:2996`).
* Проверки: маркер `TINYGL` есть у каждого демо; дайджесты совпали; `frame time` напечатан;
  нет `ENOSYS`/`panic`/`[EXC #N]` в serial-логе; `pagh-files`/`pagh-terminal` живы.
* Дорогой прогон — **не** на каждый PR, а на PR'ы, трогающие `rust-apps/tinygl/**` или
  протокол композитора (ручное решение lead, Q5).

---

## 9. Поэтапный план (узкие PR'ы, каждый — самостоятельный шаг)

| # | PR | Содержимое | Гейт приёмки |
|---|---|---|---|
| 1 | `docs: tinygl 0.4.1 port spec` | **этот документ** | 6/6 (docs-only), версия не меняется |
| 2 | `tinygl: core math, matrix, vertex` | крейт `rust-apps/tinygl` (math/matrix/vertex/state), `Context`, `LICENSE-TinyGL`, `THIRD_PARTY_NOTICES.md`, host-свойства §8.1 (матрицы) | `cargo test` зелёный, 6/6, **никакого рендера** |
| 3 | `tinygl: clip + triangle rasterizer` | `clip.rs`, `raster.rs`, `fb.rs` (`Surface`, Z-буфер), свойства растеризации + `rgb_to_pixel` | `cargo test` + эталонные дайджесты `triangle` |
| 4 | `tinygl: textures, arrays, display lists` | `texture.rs` (только RGB/UNSIGNED_BYTE/256×256/NEAREST/REPEAT/DECAL — как upstream), `arrays.rs`, `display_list.rs`, `image_util`-конверсии | свойства текстур, `gears` с текстурой на host |
| 5 | `pagh-gl-demo: compositor surface (PDP/1 ShmSurface)` | бинарь `rust-apps/pagh-gl-demo`: memfd→`SHM_OFFER`→рендер в mapping→`COMMIT`/`DAMAGE`, resize через новый `SHM_OFFER` | live-прогон в QEMU: окно с треугольником, `TINYGL` маркеры, frame time |
| 6 | `demos: triangle, cube, gears + pixel checks + e2e` | три демо, дайджесты, `_desktop_gl_phase()`, доки ограничений (README крейта + корневой README) | **Acceptance §7 целиком**, 6/6 |

Правила для всех PR'ов: no version bump до PR, который «выпускает» фичу (AGENTS.md,
«Версионирование»; для userspace-демо это вопрос к lead — Q1); `dev` — база; ветки
удаляются после мержа.

### 9.1 Гейты 6/6 на каждом шаге

`cargo fmt --all -- --check` · `python3 tools/build.py build` ·
`python3 tools/build.py build --release` · `python3 tools/host_tests.py` ·
`python3 tools/check_safety.py` · `python3 tools/check_agents_md.py`.
Плюс для PR'ов 2–6: `cd rust-apps/tinygl && cargo test`. PR 5–6 дополнительно —
`tools/e2e.py desktop` (или новый подрежим) зелёный.

---

## 10. Риски (честно, с митигацией)

| Риск | Вероятность | Влияние | Митигация |
|---|---|---|---|
| **CPU-растеризация в QEMU TCG слишком медленная** (gears на 1280×800 — единицы секунд на кадр) | высокая | демо выглядит «не работает», хотя корректно | демо по умолчанию `640×480`; кадры по фиксированному счёту, не по времени; frame time печатается и сравнивается только между прогонами; на железо обещаний не даём |
| **Объём переноса C→Rust** (~5.7 тыс. строк, fixed-point, макросы `ztriangle.h` с `PUT_PIXEL`) | средняя | PR 3–4 растягиваются | переносить **по файлам upstream** (модуль ↔ файл), не «улучшать» алгоритмы; C-оракул для сверки; свойства на каждую стадию |
| **Тихие пиксельные расхождения** (clip/clip-плоскости, perspective-correct S/T по 1/Z, top-left rule) | средняя | демо «рисует», но картинка не та → ревью не примет | pixel checks на каждой фазе (§6.2, §8.1), дайджест в e2e, точечные пиксели |
| **Лицензионная чистота** (случайно притащить GPL-форк/старую версию, забыть notice) | низкая | юридическая претензия/блок мержа | источник — **только** tarball 0.4.1 с зафиксированным sha256; `LICENSE-TinyGL` байт-в-байт; `THIRD_PARTY_NOTICES.md`; ревью-чеклист в PR «откуда файл» |
| **Ширина окна не кратна 4** → «правая полоса не рисуется» | высокая (классический баг) | ложный «баг растеризатора» | валидация `width % 4 == 0` в клиентском хелпере с внятной ошибкой; отдельное свойство |
| **`RENDER_BITS` по умолчанию 16** — забыли переключить на 32 | средняя | формат не совпадёт, экран «мусор» | это compile-time выбор в крейте (не `#define`), тест `format_rrggbb_roundtrip` + сборка только 32bpp |
| **Blending/`glBlendFunc` фактически не работает как в GL** (upstream поддерживает ограниченно) | средняя | клиент ждёт прозрачности, получает непрозрачное | документируем как «не поддерживается», демо не используют blending |
| **`glClear` чистит всё** (игнорирует scissor/маску), `glDepthMask`/`GL_DEPTH_TEST`-off нет | высокая | клиентский код с частичной очисткой ведёт себя иначе | документируем; демо чистят полный кадр; «правильные» маски — отдельное отклонение от upstream, если вообще понадобится |
| **Нет CI-покрытия rust-apps** → тесты крейта могут «протухнуть» | средняя | регрессия проходит мерж | `cargo test` явно в PR-чеклисте + e2e-фаза; вопрос Q4 к lead про включение в CI |
| **Демо как новый крейт может потребовать MINOR bump** — политика версий не описывает userspace-примеры | низкая | спор о версии в PR | вынести решение lead'у (Q1), в этой волне версия не меняется |
| **Mesa/GPU-ожидания у пользователей** («а OpenGL будет?») | средняя | обещания, которых нет | формулировки §3.2 в README и в ответах; «fixed-function subset, CPU only» везде |

---

## 11. Открытые вопросы к lead

1. **Версия.** Новый крейт + демо — это MINOR bump для `pagh`, или остаётся PATCH
   (userspace-пример не меняет поведение ОС)? AGENTS.md говорит: «любая видимая фича →
   MINOR», а docs-only — нет. Спека исходит из «версию не бампать».
2. **Вариант A vs B окончательно.** Рекомендация — B (Rust-перенос, §2.6). Если lead
   выбирает A (быстрее к демо), нужен C-компилятор в `tools/build-rust-app.sh`-пути —
   согласовать, что это допустимо для машины, гоняющей e2e.
3. **Место в дереве.** `rust-apps/tinygl` (рядом с `pagh-ui`) или `rust-apps/pagh-gl`
   (имя «продукта»)? Спека предполагает `rust-apps/tinygl` + бинарь `pagh-gl-demo`.
4. **CI для userspace-крейтов.** Добавлять ли job/шаг, гоняющий `cargo test` по
   `rust-apps/*`? Это требует синхронного изменения AGENTS.md-блока команд (иначе
   `check_agents_md.py` упадёт) — отдельный PR после мержа спеки.
5. **e2e-политика.** Дорогой QEMU-прогон GL-демо — на каждый PR в `rust-apps/**` или
   только по запросу? Сколько минут QEMU бюджета приемлемо.
6. **Depth/маски.** Оставляем upstream-поведение 0.4.1 (`glDepthMask`/`GL_DEPTH_TEST`-off
   нет, depth test только для линий; `GL_LESS` жёстко) или расширяем (это уже не «порт
   0.4.1», а форк)? Рекомендация: оставить как upstream в v1, расширять отдельной волной.
7. **Selection mode** (нужен для picking в редакторах) — в v1 или отложить? Спека
   ставит его в PR 4 как «если дёшево», демо на нём не стоят.

---

## 12. Ссылки (все зафетчены; контент внешний, использован как данные)

**Первоисточник и лицензия:**

* TinyGL changelog — <https://bellard.org/TinyGL/changelog.html> («version 0.4.1:
  - changed license to MIT»).
* TinyGL home / download — <https://bellard.org/TinyGL/> (0.4.1, Mar 5 2022; там же
  устаревшая строка «Zlib-like licence», см. §2.2).
* Tarball — <https://bellard.org/TinyGL/TinyGL-0.4.1.tar.gz>, sha256
  `b314f019…16f` (§2.3); внутри `LICENSE`, `README`, `LIMITATIONS`, `Changelog`.

**Поведенческие референсы (код НЕ берём):**

* ALMOS (LIP6) — TinyGL входит в состав user-land библиотек ОС; подтверждение, что порт
  TinyGL в небольшую ОС — обкатанный путь:
  <https://largo.lip6.fr/trac/almos/wiki/WikiStart> (раздел «Available User-land
  Libraries» → «Graphics → … TinyGL»). **Честная пометка:** конкретный URL
  `.../browser/trunk/sys/TinyGL/README?rev=124…` из задания отдаёт **404** моему
  фетчеру (и `.../browser/trunk/sys?rev=292` тоже), поэтому цитирую wiki-страницу,
  которая реально доступна.
* SerenityOS LibGL — программный растеризатор с 2021 (плитки 16×16, барицентрические
  координаты; коммит «LibGL: Add software rasterizer» в логе
  `.../log/Userland/Libraries/LibGL/GLContext.cpp`): зеркало из задания
  <https://git.netizen.se/serenity/log/Userland/Libraries/LibGL/GLContext.cpp?h=master&ofs=50&showmsg=1#1>
  отдало **404**; структуру библиотеки проверил по GitHub API:
  <https://api.github.com/repos/SerenityOS/serenity/contents/Userland/Libraries/LibGL>
  (`GLContext.cpp` 31 771 B, `Texture.cpp` 44 139 B, `Matrix.cpp`, `Lighting.cpp`,
  `ClipPlane.cpp`, `Blending.cpp` — то же разделение ответственности, что в TinyGL).
  **Лицензия Serenity в этом ревью построчно не проверялась** — код не копируем ни в
  каком случае.
* Redox OS — <https://doc.redox-os.org/book/graphics-windowing.html>: «For software
  rendering we implemented a Redox backend in our Mesa3D fork/recipe to use
  LLVMPipe/Lavapipe»; Orbital «was preferred over Wayland and X11 … and had the fastest
  native software rendering performance»; «Apps (or 'clients') create a window and draw to
  it by using the orbclient client». Вывод для нас: тонкий собственный протокол клиент→
  композитор — нормальная архитектура для ОС без GPU; тащить Mesa мы не будем.

**Внутренние документы:**

* `docs/desktop-b.md` — kernel-UAPI композитора (memfd 319, `SCM_RIGHTS`, `/dev/fb0`).
* `docs/desktop-server.md` — PDP/1, backend'ы, e2e `desktop`.
* `docs/desktop-render-spec.md` — frame-time/дамаж-модель композитора (OpenGL там в
  non-goals).
* `rust-apps/pagh-desktop/PROTOCOL.md` §8.1 — контракт ShmSurface.