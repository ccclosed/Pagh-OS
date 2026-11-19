# TinyGL 0.4.1 → Pagh-OS: спека порта (v2)

**Статус:** research + спецификация. Реализации в этом PR нет; кода нет, версия
`Cargo.toml` не меняется (`dev` несёт `2.24.0`; docs-only коммиты версию не бампают —
AGENTS.md, «Версионирование»).

**Что изменилось в v2 (решения lead после review v1):**

1. Выбран **вариант A** — upstream C 0.4.1 + тонкая Rust FFI. Переписывание C→Rust
   (вариант B) **не делается**; это отдельное будущее решение только если A провалится и
   будет новый review.
2. Первый шаг реализации — **крошечный build-probe (PR1)**: vendored C + `build.rs`
   **только этого крейта** + детекция musl C-компилятора + static-pie/`INTERP`-гейт.
   Систему сборки не трогаем.
3. Дерево: `rust-apps/tinygl/` (upstream C + FFI + notices) и `rust-apps/pagh-gl-demo/`
   (демо). Продуктовое имя «pagh-gl» не заводим.
4. CI: явный `cargo test` для затронутых userspace-крейтов; синхронная правка блока
   команд в AGENTS.md + `check_agents_md.py` — **отдельный PR** после спеки.
5. e2e GL — **не на каждый PR**; только при изменении TinyGL/демо/e2e-пути и перед
   релизом. 640×480, кадры по счёту, бюджет ≤ 5 минут на сценарий.
6. Depth/маски — **строго upstream 0.4.1** в v1, никакого форка/расширений.
7. Selection mode — **отложен** (демо он не нужен).
8. Версия: docs-only без bump; MINOR — в PR, который **выпускает** фичу, не в 2.24.0 и
   не в этой спеке.

**Не-цели этой волны:** GPU/DRM/NVIDIA, современный OpenGL (2.0+, GLSL, FBO/ARB),
EGL/GLX/Wayland, Mesa, аппаратный растеризатор, мультиконтекстная потокобезопасность,
mipmapping, MSAA. Обещать перечисленное нельзя (§3.2, §10).

---

## 0. TL;DR

1. **Лицензия чистая.** TinyGL **0.4.1** (5 марта 2022) переведён на **MIT**: официальный
   changelog — «version 0.4.1: - changed license to MIT»
   (<https://bellard.org/TinyGL/changelog.html>). В tarball'е `LICENSE` — дословный
   MIT-текст, `Copyright (c) 1997-2022 Fabrice Bellard`; tarball'ный `Changelog` повторяет
   ту же строку; `README` пишет «MIT license for easy integration». Старые версии не
   берём. Pagh остаётся MIT, notice — обязателен (§2.4).
2. **Форматы совпадают.** В 32-битном внутреннем режиме TinyGL пишет пиксель как
   `0x00RRGGBB` little-endian — ровно `XRGB8888` композитора; `ZB_open(..., frame_buffer)`
   / `ZB_resize(...)` рендерят во **внешний** буфер. Клиент рендерит **прямо в
   `mmap(MAP_SHARED)` своего memfd**, шлёт только `COMMIT`+`DAMAGE`.
   **Проверено локальным пробным рендером** (§5.2): треугольник в `unsigned int fb[64*64]`,
   центр `0x003c4c7a` (старший байт 0 → `0x00RRGGBB`), дайджест стабилен между прогонами.
3. **Подход — A (upstream C + FFI).** Меньше новых алгоритмических багов, проверенный
   растеризатор, upstream сам себе оракул. Цена — C-компилятор в сборке крейта и FFI-слой;
   и то, и другое закрывается build-probe'ом (PR1) до всякой интеграции с композитором.
4. **Точный минимальный патч upstream известен** (получен пробой): `zfeatures.h` →
   `TGL_FEATURE_RENDER_BITS 32`, **`#undef`/закомментировать `TGL_FEATURE_8_BITS`**
   (значение `0` не работает: код проверяет `defined()`, а `zdither.c` под этим флагом
   даёт `#error`), внешние 16/24 — off; файлы `glx.c`/`nglx.c`/`oscontext.c`/`BeOS` не
   собираются. Всё остальное — как в tarball'е.
5. **Честные ограничения остаются:** fixed-function subset, без shaders/GPU/современного
   GL, 16-битный Z, **stride обязан быть `w*4`**, **ширина окна кратна 4**, нет
   `glDrawArrays`/VBO, `gl_fatal_error` = `exit(1)`.

---

## 1. Инвентарь (проверено в дереве, `origin/dev` = `d5de8b2a`)

### 1.1 Что уже есть и на что опираемся

| Компонент | Факт (проверено) | Чем полезен |
|---|---|---|
| `rust-apps/pagh-desktop` | userspace-композитор: PDP/1 по AF_UNIX, backend'ы headless и fbdev | владеет экраном/окнами; GL-демо — обычный PDP/1-клиент |
| `rust-apps/pagh-desktop/src/protocol.rs` | `MAGIC "PDS1"`, `VERSION 1`, `HEADER_LEN 12`, типы 1–16 (`SHM_OFFER 14`, `SHM_READY 15`, `SHM_RELEASE 16`), `WELCOME_SHM_SURFACE = 1<<2` (`protocol.rs:20`, `:22`, `:24`, `:28`–`:47`, `:62`) | путь доставки пикселей без сокета |
| `rust-apps/pagh-desktop/src/shm.rs` | клиентский memfd (`memfd_create` c `MFD_CLOEXEC`, `shm.rs:80`, `:95`), серверный `map_readonly` (`MAP_SHARED\|PROT_READ`, `shm.rs:186`), `SCM_RIGHTS` (`shm.rs:217`, `:275`) | **сервер** не пишет в клиентскую память — рендерить в memfd может только клиент |
| `rust-apps/pagh-ui/src/shm.rs` | клиентская сторона: `MappedBuffer::map_shared` = `mmap(MAP_SHARED, PROT_READ\|PROT_WRITE)` (`pagh-ui/src/shm.rs:140`) | **это и есть** буфер, в который рендерит TinyGL |
| `rust-apps/pagh-desktop/src/surface.rs` | `MAX_SURFACE_DIM 4096`, `MAX_STRIDE_PADDING 4096`, `Xrgb8888` = «`u32` little-endian `0x00RRGGBB`; альфа игнорируется» (`surface.rs:7`, `:9`, `:13`–`:15`) | формат кадра = `PIXEL` TinyGL в 32bpp |
| `rust-apps/pagh-desktop/PROTOCOL.md` §8.1 | ShmSurface: `stride ∈ [width*bpp, width*bpp+4096]` (`PROTOCOL.md:32`), `data_len == stride*height`, монотонный `generation`, старый memfd не переиспользуется (`:316`–`:357`) | контракт поверхности окна |
| `rust-apps/pagh-desktop/src/fbdev.rs` | `/dev/fb0`: `FBIOGET_VSCREENINFO 0x4600` / `FBIOGET_FSCREENINFO 0x4602`, `FbLayout` (маски → `pack`/`unpack`), `blit_damage` (`fbdev.rs:23`, `:25`, `:148`, `:300`, `:390`) | композитор сам конвертирует XRGB→scanout; GL этого не касается |
| `rust-apps/pagh-desktop/tests/render_goldens.rs` | пиксельные goldens: FNV-1a 64 по словам пикселей (`render_goldens.rs:18`) | шаблон «pixel checks» для демо (§6.3) |
| `rust-apps/pagh-desktop/src/timing.rs` | `now_cycles()` (rdtsc), `cycles_to_ms` (`timing.rs:77`, `:137`) | замер frame time |
| `tools/build-rust-app.sh` | `TARGET=x86_64-unknown-linux-musl`, `--release --locked`, установка в `rust-apps/out/`, отказ при `INTERP` (`build-rust-app.sh:6`, `:26`, `:31`–`:34`) | статическая сборка демо |
| `tools/e2e.py` | `_desktop_bins()` собирает клиентов через тот же скрипт (`e2e.py:2969`–`:2996`); режим `desktop` (`:4029`) | место для live e2e GL-демо (не на каждый PR) |
| `vendor/cc-1.2.0` | `cc` уже vendored; `build.rs` в `rust-apps/` пока нет ни одного | PR1 добавляет `build.rs` **только** в `rust-apps/tinygl` |

### 1.2 Как окно получает пиксели

1. **ShmSurface (приоритет).** Клиент: `memfd_create` → `ftruncate(stride*height)` →
   `mmap(MAP_SHARED, PROT_READ|PROT_WRITE)` → `SHM_OFFER` (один fd через `SCM_RIGHTS`:
   формат/размеры/stride) → `SHM_READY`+`generation` → рисует **в свой mapping** →
   `COMMIT` (header flag bit 0) + `DAMAGE`-прямоугольники. Сервер отображает тот же memfd
   `MAP_SHARED|PROT_READ` и композитит.
2. **Frame upload (fallback).** Полный кадр `XRGB8888`/`RGB565` внутри сокета, если fd
   passing недоступен или сервер отказал.

```
pagh-gl-demo (ring 3)                          pagh-desktop (ring 3)
─────────────────────                          ─────────────────────
memfd_create + ftruncate(w*h*4)
mmap(MAP_SHARED, RW) ───────────────┐
SHM_OFFER(fd via SCM_RIGHTS) ───────┼────────► validate format/stride/size
                                    │          map_readonly(same memfd)
SHM_READY(generation) ◄─────────────┼─────────┘
ZB_open(w,h,ZB_MODE_RGBA, …, fb)   ← рендер прямо в mapping (FFI → C)
glClear / glBegin(GL_TRIANGLES) …
COMMIT(generation) + DAMAGE(rects) ─────────► compose + blit_damage → /dev/fb0
```

### 1.3 Чего в дереве нет

* **Никакого GL**: `grep -ri "opengl\|glbegin\|tinygl\|glx"` по `src/`, `rust-apps/`,
  `docs/`, `tools/` даёт единственное совпадение — `docs/desktop-render-spec.md`, где
  OpenGL перечислен в non-goals.
* **Никакой CPU-растеризации**, кроме 2D-холста `pagh-ui` (`Canvas`) и композитора.
* **CI не собирает `rust-apps/`** и не гоняет их тесты: в `.github/workflows/ci.yml` пять
  job'ов (kernel, host-tests, static-policy, docs-claims, формат), ни один не трогает
  userspace-крейты; корневой `cargo fmt --all` их тоже не видит (у каждого rust-app свой
  `[workspace]`, корневой `Cargo.toml` без `members`, только `exclude = ["host-tests"]`).
  Отсюда решение lead: явный `cargo test` + **отдельный** PR с правкой CI/AGENTS.md (§8.0).
* **`rust-apps/README.md`** ограничивает: «GUI libraries are not supported: there is no
  X11/Wayland server, only the kernel's own framebuffer», программы однопоточные. Спека не
  противоречит: TinyGL однопоточный (`gl_ctx` — глобальный указатель на текущий контекст,
  см. `init.c:37`), X11-слой выбрасывается.

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

Та же строка есть в tarball'ном `Changelog`:

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

README внутри tarball'а: «MIT license for easy integration in commercial designs (read the
LICENSE file)».

**Педантичная деталь (важна для «байт-в-байт»):** вторая строка upstream-файла — это
**одиночный пробел**, а не пустая строка. `LICENSE-TinyGL` надо копировать файлом из
tarball'а (или `git add` его целиком), а не набирать руками: иначе формулировка
«байт-в-байт» перестаёт быть правдой.

**Честная деталь, которую не замалчиваем:** HTML-страница проекта всё ещё описывает
лицензию как «Zlib-like licence ... (read the LICENCE file)» — устаревший абзац (заголовок
«News» обновлён до 0.4.1, а текст про лицензию — нет). Первоисточник — файлы tarball'а
(`LICENSE` + `README` + `Changelog`), и все три согласованно говорят MIT. Zlib-лицензия,
к слову, тоже permissive и MIT-совместима, так что вывод не меняется.

### 2.3 Что именно берём (артефакт и контрольная сумма)

| Параметр | Значение |
|---|---|
| Артефакт | `TinyGL-0.4.1.tar.gz` с <https://bellard.org/TinyGL/TinyGL-0.4.1.tar.gz> |
| Размер | 68 800 байт |
| SHA-256 | `b314f019a778d583a3778cb14bcc2a57626a8f866cbed0693705bb1bb785b16f` |
| Дата файлов в архиве | 2022-03-05 |
| `src/` | 35 файлов `.c`/`.h`, **6808** строк C (`ls src/*.c src/*.h \| wc -l`; `wc -l`) |
| Из них собираем | 23 файла (без `glx.c`, `nglx.c`, `oscontext.c`, `BeOS/*`) |
| Заявка upstream | «compiled code size of about 40 kB on x86» (README) |

Сумма посчитана локально из скачанного файла; при вендоринге перепроверить и записать в
`THIRD_PARTY_NOTICES.md`.

**Старые версии не берём.** У 0.1–0.4 другая лицензия и другой код. Любой сторонний форк
TinyGL, выросший из до-0.4.1 кода, для нас **не источник**.

### 2.4 Что обязано попасть в репозиторий

MIT требует сохранять copyright + permission notice в копиях «Software» и «substantial
portions», а **в исходниках 0.4.1 copyright-шапок нет** (проверено:
`grep -rl "Copyright" src/*.c src/*.h include/GL/*.h` → **0 файлов**). Поэтому notice
живёт только в файлах проекта, и без них распространение было бы без notice. Минимум:

1. `rust-apps/tinygl/LICENSE-TinyGL` — **байт-в-байт** текст §2.2 (без правок, без
   перевода, без переформатирования).
2. `rust-apps/tinygl/THIRD_PARTY_NOTICES.md` — кто/откуда/версия/sha256 tarball'а; список
   собираемых файлов; **точный список правок** upstream (`zfeatures.h`, §5.3);
   что `glx.c`/`nglx.c`/`oscontext.c`/`BeOS/*` не собираются и не распространяются.
3. `rust-apps/tinygl/UPSTREAM.md` — как обновлять (tarball → sha256 → patch set → probe).
4. Шапка в **наших** файлах (`src/lib.rs`, `src/ffi.rs`, `build.rs`):
   `// Derived from TinyGL 0.4.1 (c) 1997-2022 Fabrice Bellard, MIT — see LICENSE-TinyGL`.
   Файлы upstream **не перешапливаем** (кроме правок §5.3) — это упрощает сверку с tarball.
5. Строка в `rust-apps/README.md` и в разделе лицензий корневого `README.md`:
   «`rust-apps/tinygl` содержит TinyGL 0.4.1 (MIT, Fabrice Bellard)».
6. Смены лицензии Pagh не требуется: `LICENSE` репозитория
   (`MIT License, Copyright (c) 2026 ccclosed`) остаётся; отдельный корневой `NOTICE`
   не нужен.

### 2.5 Порт 0.4.1: почему A, и почему B — только будущее решение

| Критерий | **A. Upstream C 0.4.1 + FFI (выбран)** | B. Переписывание C→Rust (~5.7 тыс. строк) |
|---|---|---|
| Новые алгоритмические баги | **нет** — алгоритмы не переписываются | риск в clip/perspective-correct/fixed-point |
| Скорость до проверенного растеризатора | **сразу** (PR1 probe + PR2 FFI) | 3–4 PR'а механического переноса |
| Оракул корректности | upstream сам оракул; пиксельные дайджесты фиксируют поведение | нужен отдельный C-оракул (сам по себе C) |
| Требования к toolchain | нужен C-компилятор для musl-таргета (PR1 это и проверяет) | только `cargo`/`rustup` |
| Соответствие «pure logic → host tests» | частично: тестируем FFI-обёртку и дайджесты, не внутренности | полное, но ценой переноса |
| Полнота | 100 % от 0.4.1 | 100 % только если перенести всё |
| Сопровождение | upstream заморожен с 2022 (0.4.1 — последняя) | свой бэклог багов |

**Решение lead: A.** Обоснование: проверенный растеризатор важнее «чистого Rust»;
вероятность незаметно сломать геометрию/текстурную интерполяцию при ручном переносе
существенно выше, чем стоимость C-тулчейна, а стоимость C-тулчейна измеряется один раз —
build-probe'ом.

**B не раздуваем.** Это отдельное будущее решение, допустимое только если A провалится
(например, toolchain не удаётся завести на нужных платформах) **и** будет новый review
спеки. В коде под B ничего не закладываем: FFI-обёртка остаётся тонкой, чтобы её можно было
заменить реализацией без изменения демо.

**Что не берём ни в каком варианте:** код ALMOS, Serenity, Redox, Mesa — только
поведенческие референсы (§12). Их лицензии в этом ревью построчно не проверялись, и
копировать оттуда нельзя ни строки.

---

## 3. API subset (OpenGL 1.1 fixed-function)

### 3.1 Что реально реализовано в 0.4.1

Опора — `LIMITATIONS`, `include/GL/gl.h` и **фактический список экспортируемых символов**
(проверено компиляцией ядра: 197 определённых `T`-символов, §5.2).

| Группа | API | Статус в 0.4.1 | Наш план |
|---|---|---|---|
| Контекст/состояние | `glInit(void *zb)`, `glClose`, `glEnable`, `glDisable`, `glGetIntegerv`, `glGetFloatv` | есть; `glGet*` — урезанный список pname (VIEWPORT, MAX_*_STACK_DEPTH, MAX_LIGHTS, MAX_TEXTURE_SIZE, MATRIX-запросы, LINE_WIDTH, POINT_SIZE); `gl_ctx` — **глобальный** указатель текущего контекста (`init.c:37`) | `glInit` принимает `ZBuffer*` → FFI: `ZB_open` + `glInit(zb)`; однопоточно |
| Матрицы | `glMatrixMode`, `glLoadIdentity`, `glLoadMatrixf`, `glMultMatrixf`, `glPushMatrix`, `glPopMatrix`, `glRotatef`, `glTranslatef`, `glScalef`, `glFrustum`, `glOrtho` | есть (double-прототипы — нет) | используем как есть, `f32` |
| Viewport/clear | `glViewport`, `glClear`, `glClearColor`, `glClearDepth` | есть; `glClear` чистит **весь** Z-буфер и цвет независимо от scissor/маски; начальный z не переопределяется | используем как есть; частичную очистку делаем damage-прямоугольниками окна, не GL |
| Примитивы | `glBegin`/`glEnd`, `glVertex2f/3f/3fv/4f`, `GL_POINTS/LINES/LINE_STRIP/LINE_LOOP/TRIANGLES/TRIANGLE_STRIP/TRIANGLE_FAN/QUADS/POLYGON` | есть (часть прототипов отсутствует) | полный набор для демо |
| Атрибуты | `glColor3f/3fv/4f/4fv`, `glNormal3f/3fv`, `glShadeModel` | есть | float-формы |
| Depth | `glEnable(GL_DEPTH_TEST)`, `glClearDepth` | **строго upstream в v1:** depth test реально влияет только на линии; для прочих примитивов Z-буфер используется всегда, `DepthMode` жёстко `GL_LESS`; **`glDepthMask` нет, `GL_DEPTH_TEST`-off не выключает Z** | **никакого форка/расширений** в v1; если понадобится — отдельная волна с новым review |
| Z-буфер | `ZB_Z_BITS 16`, `ZB_POINT_Z_FRAC_BITS 14` (`zbuffer.h:10`, `:12`) | 16-битный Z, фиксированная точка | оставляем (память/скорость); 24/32-битный Z — не цель |
| Цвета | 32bpp: `PIXEL = 0x00RRGGBB` (`zbuffer.h:60`–`:64`) | проверено пробой (§5.2) | наш путь — 32bpp |
| Текстуры | `glGenTextures`, `glBindTexture`, `glDeleteTextures`, `glTexImage2D`, `glTexEnvi`, `glTexParameteri`, `glTexCoord2f/2fv/4f` | `glTexImage2D` — только RGB+`GL_UNSIGNED_BYTE`, внутренний ресайз в **256×256**, mipmap нет, `glTexEnvi` только `GL_DECAL`, фильтры только `GL_NEAREST`, wrap только `GL_REPEAT`, `glTexCoord` не прогоняется через матрицу | используем как есть; ограничения — в README крейта |
| Массивы | `glEnableClientState`, `glDisableClientState`, `glVertexPointer`, `glNormalPointer`, `glColorPointer`, `glTexCoordPointer`, `glArrayElement` | `TGL_FEATURE_ARRAYS 1`; **`glDrawArrays`/`glDrawElements` отсутствуют** (`grep -rn "DrawArrays\|DrawElements" src/ include/ examples/` → пусто); индексация через `glArrayElement` из `glBegin/glEnd` | v1: демо на immediate mode; arrays включаем, но **проверяем на x86_64 отдельно** (предупреждение upstream README о 64-битных указателях; в коде указатели идут через `.p`, `arrays.c:133` — выглядит корректно, но это проверяем, а не предполагаем) |
| Display lists | `glNewList`/`glEndList`/`glCallList`/`glGenLists`/`glDeleteLists`/`glIsList` | есть (`TGL_FEATURE_DISPLAYLISTS`) | батчинг в демо — через них (VBO нет) |
| Selection mode | `glRenderMode(GL_SELECT)`, `glSelectBuffer`, `glInitNames`, `glPushName/glPopName`, `glLoadName` | полный handling | **ОТЛОЖЕНО** (решение lead): в v1 не входит, демо не используют |
| Прочее | `glCullFace`, `glFrontFace`, `glPolygonMode`, `glLineWidth`, `glPointSize`, `glPolygonOffset`, `glPolygonStipple`, `glBlendFunc`, `glFog*`, `glHint`, `glFlush`, `glPixelStorei`, `glRasterPos2f`, `glEdgeFlag` | часть — реально, часть — пустые/«only API»: `glPolygonOffset` не реализован, `glHint`/`glFlush` игнорируются, `glPixelStorei` только byte-aligned | экспортируем как есть (клиентский код линкуется), честно пишем «not implemented» |
| Освещение | `glLight*`, `glLightModel*`, `glMaterial*`, `glColorMaterial`, `glEnable(GL_LIGHTING/GL_LIGHTx)` | есть, «not very fast» (upstream) | освещение в демо — выключено по умолчанию, отдельный флаг для замера |

Не собираем: `glx.c` (413), `nglx.c` (128), `oscontext.c` (84), `BeOS/*` (293) — X11/NanoX/
BeOS-обвязка. Новый backend — наш `pagh-gl-demo` поверх PDP/1 (§4).

**Важная деталь для FFI:** `include/GL/gl.h` **не объявляет** immediate-mode прототипы
(`glVertex3f`, `glColor3f`, `glNormal3f`, `glTexCoord2f` и др.) — они есть в `src/api.c` и
**экспортируются** (подтверждено `nm`: `glVertex2f/3f/3fv/4f`, `glColor3f/4f`, …). Поэтому
`bindings.rs` объявляет прототипы сам, а сам заголовок upstream не правим.

### 3.2 Чего мы НЕ обещаем

* **Никаких shaders** (GLSL), программируемого пайплайна, GL 2.0+.
* **Никаких расширений** `ARB_*`/`EXT_*`, FBO, VAO, настоящих VBO, `glDrawArrays`.
* **Никакого GPU**: только CPU-растеризация. GPU/NVIDIA/DRM/KMS — вне спеки.
* **Никакого 8-битного paletted режима** и dithering (`TGL_FEATURE_8_BITS` выключен
  физически, §5.3), color index mode отсутствует.
* Ни MSAA, ни HDR/гаммы, ни стерео, ни user clip planes (в 0.4.1 нет).
* Никакого fallback «через Mesa», если демо медленное.

### 3.3 FFI-контракт (наш слой)

Публичный Rust-API крейта `tinygl` — тонкий и честный:

```rust
// src/lib.rs — безопасная обёртка над C
pub struct Context { raw: *mut GLContext }          // = glInit(zb), Drop → glClose
pub struct Surface<'a> { pixels: &'a mut [u32], width: u32, height: u32 }

impl Context {
    /// Открывает ZBuffer поверх готового буфера (stride обязан быть width*4).
    pub fn new(surface: Surface<'_>) -> Result<Self, FbError>;   // ZB_open + glInit
    pub fn finish(self);                                          // glClose + ZB_close
    pub fn set_viewport(&self, x: i32, y: i32, w: u32, h: u32);   // glViewport
    pub fn clear(&self, color: u32, z: u16);                      // glClear
    // … 1:1 с §3.1, имена как в GL (gl_glRotatef → rotate)
}
```

Правила слоя:

* `unsafe` — только в `src/ffi.rs`/`src/lib.rs`, у **каждого** блока `// SAFETY:`-комментарий
  (стиль репозитория; `tools/check_safety.py` этот крейт не сканирует, но дисциплину держим).
* `Surface` валидирует: `width % 4 == 0`, `stride == width*4`, `pixels.len() >= w*h`,
  `w,h ∈ (0, MAX_SURFACE_DIM]` → `FbError`, **не** молчаливое ужимание.
* Один `Context` за раз (TinyGL держит глобальный `gl_ctx`); `Context` не `Send`/`Sync`.
* `Drop` закрывает ZBuffer и контекст; двойного `finish` быть не может (by value).
* `gl_fatal_error` из `error.c` вызывает **`exit(1)`** — это поведение upstream, в v1 **не
  патчим**; документируем в README крейта: внутренний fatal = смерть процесса с сообщением
  в stderr (для демо приемлемо; для библиотеки — названное ограничение).

---

## 4. Framebuffer surface в композиторе

### 4.1 Поток (обязательный, менять нельзя)

1. `memfd_create("pagh-gl-fb", MFD_CLOEXEC)` → `ftruncate(w*h*4)`.
2. `mmap(NULL, w*h*4, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0)` — переиспользуем
   `pagh_ui::shm::MappedBuffer::map_shared` (`pagh-ui/src/shm.rs:140`).
3. `SHM_OFFER { width, height, format = XRGB8888, stride = w*4, data_len = w*h*4 }` + fd
   через `SCM_RIGHTS`.
4. `SHM_READY { generation }` — запомнить.
5. `ZB_open(w, h, ZB_MODE_RGBA, 0, NULL, NULL, mapping_ptr)` + `glInit(zb)` (FFI).
6. Рисовать кадр; `glViewport` = размер клиентской области.
7. `COMMIT { generation }` + `DAMAGE { rects }`.
8. По `SHM_RELEASE`/смене `generation` — **новый memfd** и `SHM_OFFER` заново
   (`PROTOCOL.md:357`).

### 4.2 Требования к буферу (из кода)

| Требование | Источник | Следствие |
|---|---|---|
| `stride == width*4`, **без паддинга** | `zbuffer.c:28`: `linesize = (xsize*PSZB + 3) & ~3`; при `PSZB=4` = ровно `xsize*4` | PDP/1 разрешает паддинг до +4096 (`PROTOCOL.md:32`), но TinyGL его не выражает → **плотный stride**, это легально |
| **`width` кратно 4** | `zbuffer.c:94`: `xsize = xsize & ~3` в `ZB_resize` | `width % 4 != 0` молча ужмётся → валидируем в `Surface::new` и в демо |
| Внешний буфер, а не `malloc` | `zbuffer.c:105`–`:113`: `frame_buffer_allocated = 0` | рендер **прямо в memfd**, `ZB_copyFrameBuffer` не нужен |
| 32bpp внутренний формат | `zfeatures.h:39` — активное `16` (по умолчанию!), `:41` — закомментированное `32` | **правка §5.3**: `RENDER_BITS=32` + `TGL_FEATURE_32_BITS`; иначе пиксель `u16` и формат не совпадёт |
| `PIXEL == 0x00RRGGBB` | `zbuffer.h:60`–`:64` + проба (§5.2): центр `0x003c4c7a` | совпадает с `Xrgb8888` — конверсии нет; альфа-байт 0, композитор игнорирует |
| Z-буфер отдельный, 16 бит | `zbuffer.c:103`, `zbuffer.h:79` | `w*h*2` байт в heap клиента, **не** в memfd |

### 4.3 Рендер-цикл

* **Полный кадр** (первый, resize, смена generation): `glClear` + сцена + damage на всю
  клиентскую область.
* **Анимация**: полный клиентский прямоугольник (проще, дайджест детерминирован);
  damage-оптимизация — не в v1.
* **Тайминг**: `timing::now_cycles()` вокруг кадра + `Instant` wall-clock (в QEMU TSC
  виртуален), см. §6.4.

### 4.4 Resize

Композитор меняет размер (`CONFIGURE`) → клиент делает **новый** memfd, новый `SHM_OFFER`,
`ZB_close`/`ZB_open` (или `ZB_resize`, если размер кратен 4). Хук `gl_resize_viewport`
(`zgl.h:274`, вызов `misc.c:23`) **не подключаем**: размер приходит извне, лишнее состояние
не заводим.

### 4.5 Fallback

Если `welcome.flags` не содержит `WELCOME_SHM_SURFACE` (bit 2) или `SCM_RIGHTS` не
работает — демо **честно падает** с внятной ошибкой и кодом выхода. Frame-upload для GL не
поддерживаем (копия `w*h*4` по сокету каждый кадр — ровно то, чего спека избегает).

---

## 5. Сборка: upstream C + тонкая Rust FFI

### 5.1 Дерево и вендоринг

```
rust-apps/tinygl/
├── Cargo.toml              # name="tinygl", license="MIT", panic="abort", opt-level=3, lto
├── build.rs                # cc: компилирует 23 C-файла под $TARGET (musl)
├── LICENSE-TinyGL          # дословный MIT из §2.2
├── THIRD_PARTY_NOTICES.md  # §2.4
├── UPSTREAM.md             # как обновлять: tarball → sha256 → patch → probe
├── c_src/                  # распакованный upstream (только нужное)
│   ├── src/*.c *.h         # 23 файла; glx.c/nglx.c/oscontext.c НЕ копируем
│   ├── include/GL/gl.h
│   └── PATCHES.md          # точный diff-набор §5.3
└── src/
    ├── lib.rs              # безопасная обёртка (§3.3)
    ├── ffi.rs              # extern "C" прототипы + SAFETY
    └── surface.rs          # Surface + валидация (w%4, stride)

rust-apps/pagh-gl-demo/
├── Cargo.toml              # зависимость на tinygl (path) + pagh_ui/pagh_desktop (path)
└── src/
    ├── main.rs             # CLI: triangle | cube | gears, --connect, --frames, --hash
    ├── client.rs           # PDP/1 ShmSurface: memfd → SHM_OFFER → рендер → COMMIT
    └── scenes/*.rs         # три демо-сцены (наш код, MIT)
```

* Upstream-файлы копируются **как есть** (кроме §5.3); `PATCHES.md` фиксирует каждую
  правку строкой «файл → было → стало → зачем», чтобы обновление на новую версию (если
  когда-нибудь появится) было механическим.
* Ничего не кладём в `vendor/` корневого workspace: `rust-apps/tinygl` — отдельный
  `[workspace]` (как `pagh-ui`/`pagh-desktop`), его зависимости не влияют на ядро.
* `cc` — **build-dependency** этого крейта, `cc-1.2.0` уже в `vendor/`; при включении
  проверить, что вендоренный `cc` резолвится (иначе — `[patch.crates-io]` только для
  `rust-apps/tinygl`, корневой workspace не трогаем).

### 5.2 PR1 — build-probe (первый шаг реализации, после stable merge)

**Цель:** выяснить, можно ли собрать upstream C под `x86_64-unknown-linux-musl` в
static-pie и позвать его из Rust — **до** любой интеграции с композитором. Крошечный PR:
один крейт, один `build.rs`, один бинарь-проба, никаких изменений системной сборки.

Состав PR1:

1. `rust-apps/tinygl/` с `c_src/` (23 файла) + `LICENSE-TinyGL` + `THIRD_PARTY_NOTICES.md`
   + `UPSTREAM.md` + `PATCHES.md`.
2. `build.rs` только этого крейта: `cc::Build::new()` + список из 23 файлов +
   `-fno-strict-aliasing` + `-Wno-implicit-fallthrough` (иначе 3 предупреждения в `get.c`,
   проверено) + `-O2`. Цель берётся из `TARGET`/`HOST`.
3. **Детекция C-компилятора** (её и проверяем): `CC_x86_64_unknown_linux_musl` →
   `x86_64-unknown-linux-musl-gcc` → `musl-gcc` → `clang --target=x86_64-unknown-linux-musl`
   (с `--sysroot`, если задан) → **внятная ошибка** с инструкцией
   (`apt install musl-tools`, либо `clang` + sysroot) вместо `cc not found`.
   На машине, где снимались факты, **нет ни `musl-gcc`, ни `clang`** (проверено) — то есть
   детекция и инструкция — не формальность, а суть PR1.
4. Бинарь-проба `tinygl-probe`: `ZB_open(64,64,ZB_MODE_RGBA, …, fb)` → `glInit` →
   `glViewport` → `glClear` → `glFrustum`/`glTranslatef` → `glut`-подобный треугольник из
   §5.2 → FNV-1a дайджест → печать
   `TINYGL PROBE digest=0x… center=0x… corner=0x… cc=<id>`.
5. **static-pie/`INTERP` гейт**: `readelf -l` по бинарю (как в `build-rust-app.sh:31`) —
   гейт внутри `build-rust-app.sh`, не новый скрипт.

**PASS-критерии PR1** (все обязательны):

| # | Критерий | Как проверяется |
|---|---|---|
| 1 | C компилируется под musl | `cargo build --target x86_64-unknown-linux-musl` зелёный |
| 2 | Бинарь static-pie | нет `INTERP` (`readelf -l`) |
| 3 | FFI работает | проба печатает `digest`, `center`, `corner` |
| 4 | Формат = XRGB8888 | центр ≠ 0 и старший байт = `0x00` |
| 5 | Детерминизм | два прогона → один дайджест |
| 6 | Совпадение с host-gcc эталоном | ожидается `digest=0x3f7e5ece33386f9b`, `center=0x003c4c7a`, `corner=0x00000000`; расхождение — не FAIL, а известное отклонение: записать оба дайджеста в `UPSTREAM.md` и выбрать эталоном musl-значение |
| 7 | Ничего не сломано | 6/6 гейтов зелёные |

**Почему эталон есть заранее:** пробный рендер уже выполнен на этом дереве фактов —
gcc `-O2 -fno-strict-aliasing`, 23 файла, `-lm`; результат стабилен между прогонами:
`digest=0x3f7e5ece33386f9b center=0x003c4c7a corner=0x00000000`. Это и есть «известный
ответ» для PR1 (и заодно доказательство, что `0x00RRGGBB` — правда, а не предположение).

**Если PR1 FAIL** (toolchain не заводится): PR закрывается с записанным выводом; порядок
дальнейших действий — `clang --target=…-musl` с sysroot, затем `zig cc`, и только если всё
провалилось — вопрос lead'у про пересмотр решения (тогда и только тогда обсуждается B).

### 5.3 Точный минимальный патч upstream (проверен пробой)

`c_src/src/zfeatures.h` — единственный файл, который правим:

| Было | Стало | Почему |
|---|---|---|
| `#define TGL_FEATURE_RENDER_BITS    16` | `#define TGL_FEATURE_RENDER_BITS    32` | нужен `PIXEL = u32 0x00RRGGBB` = `XRGB8888` |
| `#define TGL_FEATURE_8_BITS     1` | **закомментировать** (не `0`!) | `zdither.c` проверяет `#if defined(TGL_FEATURE_8_BITS)` **и** требует `RENDER_BITS == 16`, иначе `#error`; и `ZB_open`/`ZB_close` тогда ссылаются на `ZB_initDither`/`ZB_closeDither`. Значение `0` **не помогает** — `defined()` истинно |
| `#define TGL_FEATURE_16_BITS    1` | `0` (опционально) | внешние 16-бит конверсии не нужны |
| `#define TGL_FEATURE_24_BITS    1` | `0` (опционально) | внешние 24-бит конверсии не нужны |

Сборка: 23 `.c` (все, кроме `glx.c`, `nglx.c`, `oscontext.c`; `BeOS/` не копируется),
`-I c_src/include -I c_src/src`, `-O2 -fno-strict-aliasing -Wno-implicit-fallthrough`,
линковка `-lm`. Проверено: сборка на x86_64 gcc чистая, зависимостей кроме libc/libm нет
(`nm -u`: `malloc/calloc/free/memcpy/memset/pow/cos/sincos/exit/floorf/fprintf/vfprintf/fwrite/fputc/stderr/__assert_fail` — всё есть в musl).

### 5.4 static-pie, musl, размер

* Цель `x86_64-unknown-linux-musl` в Rust собирает static-pie; проверка отсутствия
  `INTERP` уже есть в `tools/build-rust-app.sh:31`–`:34` и переиспользуется.
* `panic = "abort"`, `opt-level = 3`, `lto = true` — как в `pagh-desktop/Cargo.toml`
  (горячий путь — попиксельные циклы C-кода; LTO через границу C/Rust не работает, поэтому
  главное — `-O2` в `build.rs`).
* Ожидаемый размер: ~40 kB C (upstream) + FFI-обёртка; для сравнения — собранный пробный
  бинарь на хосте занял **81 464 байта** с glibc; на musl static-pie будет другого размера —
  измерить в PR1 и записать.
* PIE-перемещения kernel-ABI поддерживает (им собраны `pagh-files`/`pagh-terminal`).

### 5.5 FFI safety policy

* `bindings` пишем руками (никакого `bindgen`: не тянем build-dep и работу в офлайне).
* Все `extern "C"` — в `ffi.rs`; `unsafe`-блоки только в двух файлах; у каждого —
  `// SAFETY:`.
* Инварианты, которые обёртка обязана держать: указатель буфера живёт ≥ `Context`;
  `pixels.len() >= w*h`; `stride == w*4`; `w%4 == 0`; одновременно один `Context`
  (глобальный `gl_ctx`); `Drop` не даёт утечки/двойного `ZB_close`.
* В тестах — сценарии «плохих» входов (нулевой размер, `w%4 != 0`, слишком маленький
  буфер), проверяющие, что падает **наш** `Err`, а не C.

### 5.6 Вариант B (переписывание C→Rust) — только как будущее решение

Зафиксировано, чтобы не потерять контекст, но **в работе не участвует**: ~5.7 тыс. строк
переноса (35 файлов `src/` минус ~1.05 тыс. строк X11/NanoX/BeOS-обвязки), C-реализация
остаётся оракулом для дайджестов, риск — clip/perspective-correct/fixed-point. Условие
возврата к B: провал A (§5.2) **и** новый review спеки.

---

## 6. Демо и пиксельные проверки

### 6.1 Три демо в `rust-apps/pagh-gl-demo`

| Демо | Что рисует | Зачем |
|---|---|---|
| `triangle` | 1 треугольник с Gouraud-интерполяцией, фиксированный угол | минимальный proof: матрицы + clip + raster + цвет |
| `cube` | куб 6 граней, depth test, вращение **фиксированным шагом** | proof Z-буфера и culling |
| `gears` | 3 шестерёнки (аналог `examples/gears`, **написанный заново** под наш API) | нагрузка: много треугольников, lighting off/on, frame time |

CLI: `pagh-gl-demo <triangle|cube|gears> [--connect /tmp/pagh-desktop.sock]
[--frames N] [--size 640x480] [--out DIR] [--hash]`. Без `--connect` — headless (рендер в
`Vec<u32>`, дайджест без композитора). С `--connect` — живой PDP/1.

### 6.2 Кадры по счёту, не по wall-clock

Анимация — детерминированная: кадр `n` соответствует углу `n * step`. Никаких `Instant` в
логике кадра (только в замерах). Это делает дайджесты воспроизводимыми и в QEMU, и на
хосте.

### 6.3 Pixel checks (дайджесты)

Эталон — `tests/render_goldens.rs` (§1.1): **FNV-1a 64 по словам пикселей**, не PNG-дифф:
падает на любое изменение цвета/геометрии, не требует бинарных golden-файлов, одинаково
работает в host-тесте и в QEMU-госте.

Правила:

1. Фиксированные параметры кадра (размер, угол, seed, `f32`, без времени в кадре).
2. Печать `TINYGL <demo> frame=<n> digest=0x................ w=<w> h=<h>` — маркер годен
   для grep из `tools/e2e.py` (как `[BOOTID]`, `LXSELFTEST`).
3. Эталонные дайджесты — в host-тесте крейта; в QEMU сравниваются **те же** значения.
4. Дополнительно — точечные проверки пикселей (центр треугольника, угол куба, «в
   шестерёнке есть дырка»): они локализуют баг лучше одного хеша.

### 6.4 Frame time

* `timing::now_cycles()` (rdtsc) вокруг `ZB_open`-кадра и вокруг `COMMIT`.
* Плюс wall-clock `Instant` (в QEMU TSC виртуален): абсолютные числа честны только в
  пределах одного QEMU-конфига.
* Печать: `TINYGL <demo> ft=min/med/max ms (wall) cycles=min/med/max`.
* Формулировка для отчёта: «замерено в QEMU TCG — верхняя граница, не бенчмарк GPU».

---

## 7. Acceptance

Демо принято, если **все** пункты выполнены:

1. **Пиксели в окне композитора (QEMU fbdev).** `tools/e2e.py desktop` (или подрежим)
   поднимает QEMU, композитор в fbdev-режиме и GL-демо как клиента; в QEMU-framebuffer'е
   есть ненулевая GL-геометрия в клиентском прямоугольнике: характерные цвета демо, кадр не
   пустой/фоновый, в serial-логе есть `TINYGL … digest=0x…`.
2. **Pixel checks PASS.** Дайджесты host-рендера == дайджесты из QEMU-гостя для `triangle`,
   `cube` (статические позы), `gears` (фиксированные углы).
3. **Frame time измерен** и напечатан (`min/med/max ms`) на **640×480** (и, если успеваем
   по бюджету, на 1280×800) в том же прогоне.
4. **Нет регрессий:** `pagh-files` и `pagh-terminal` в том же сеансе работают (окна, ввод,
   Alt+Shift+F4 launch).
5. **Гейты 6/6** (§8.0) + `cargo test` затронутых userspace-крейтов.
6. **Лицензионная чистота:** `LICENSE-TinyGL` байт-в-байт = §2.2; `THIRD_PARTY_NOTICES.md`
   называет upstream/версию/sha256/список файлов; `PATCHES.md` перечисляет правки
   `zfeatures.h`; в наших файлах есть строка происхождения.
7. **Честные ограничения опубликованы** в README крейта и корневом `README.md`: CPU-only,
   fixed-function subset, без shaders/GPU/современного GL, 16-битный Z, `stride == w*4`,
   `width % 4 == 0`, `gl_fatal_error` = `exit(1)`.

**Бюджет e2e:** ≤ 5 минут на сценарий (решение lead), 640×480, кадры по счёту. Прогон — не
на каждый PR: только при изменении `rust-apps/tinygl/**`, `rust-apps/pagh-gl-demo/**` или
e2e-пути, и перед соответствующим релизом.

---

## 8. Тесты

### 8.0 Гейты

Шесть команд AGENTS.md — арбитр, зелёные на каждом PR:

```sh
cargo fmt --all -- --check
python3 tools/build.py build
python3 tools/build.py build --release
python3 tools/host_tests.py
python3 tools/check_safety.py
python3 tools/check_agents_md.py
```

Ни одна не покрывает `rust-apps/` (§1.3). Поэтому:

* для PR1–PR6 обязательна **явная** команда в README крейта и в описании PR:
  `cd rust-apps/tinygl && cargo test` (и `cargo build --target x86_64-unknown-linux-musl`);
* **включение userspace-тестов в CI — отдельный PR** (решение lead): job/шаг,
  гоняющий `cargo test` по затронутым крейтам, **синхронно** с правкой блока команд в
  AGENTS.md, иначе `check_agents_md.py` сравнивает CI и AGENTS.md в обе стороны и упадёт.
  В этой спеке состав CI не меняется.

### 8.1 Host-тесты (без QEMU, без окна)

Так как C-ядро не переписывается, тестируем **стык и поведение**, а не внутренности:

| Свойство | Что утверждает |
|---|---|
| `surface_rejects_width_not_multiple_of_4` | `Surface::new(65,64)` → `Err`, не молчаливое ужимание |
| `surface_rejects_padded_stride` | `stride != w*4` → `Err` (TinyGL не выражает паддинг) |
| `surface_rejects_small_buffer` | `pixels.len() < w*h` → `Err` до вызова C |
| `context_is_single` | второй `Context` при живом первом — ошибка/запрет типа |
| `clear_is_uniform` | после `clear(0x00RRGGBB, z)` все пиксели и Z равны заданным |
| `matrix_roundtrip_matches_c` | `glGetFloatv(GL_MODELVIEW_MATRIX)` после `rotate/translate` == ожидаемая матрица (сверка с эталонными числами из C) |
| `triangle_digest_matches_probe` | дайджест пробного треугольника == `0x3f7e5ece33386f9b`, центр `0x003c4c7a` (§5.2) |
| `triangle_deterministic` | два прогона подряд → один дайджест |
| `depth_less_wins_upstream` | ближний треугольник побеждает при любом порядке рисования (upstream `GL_LESS`) |
| `draw_outside_viewport_clipped` | геометрия вне viewport не пишет за границы буфера |
| `drop_closes_cleanly` | `Drop` → нет утечек (проверка через счётчики `gl_malloc`-хуков не в v1: просто отсутствие падения/двойного free) |
| `ffi_rejects_null` | `Context::new` с пустым/огромным размером — `Err`, не C-краш |

Дополнительно (по желанию, дешево): прогон PR1-пробы под `-fsanitize=undefined` вручную
(в CI не заводим) — это и есть проверка предупреждения upstream README о 64-битных
указателях в float-массивах.

### 8.2 Live e2e (QEMU) — по политике lead

* Расширяем существующий `desktop`-режим фазой `_desktop_gl_phase()`: демо запускается
  третьим клиентом с prepared disk (`/mnt/pagh-gl-demo`, как `pagh-files`/`pagh-terminal`,
  `e2e.py:2995`–`:2996`).
* Проверки: маркеры `TINYGL` у каждого демо; дайджесты совпали; frame time напечатан; нет
  `ENOSYS`/`panic`/`[EXC #N]`; `pagh-files`/`pagh-terminal` живы.
* **Запуск не на каждый PR** (§7), бюджет ≤ 5 минут на сценарий.

---

## 9. Поэтапный план (узкие PR'ы)

| # | PR | Содержимое | Гейт приёмки |
|---|---|---|---|
| 1 | `tinygl: build-probe for upstream C 0.4.1 (musl + FFI)` | `rust-apps/tinygl/`: `c_src/` (23 файла), `build.rs` только этого крейта, детекция musl C-компилятора, `ffi.rs`-минимум, `tinygl-probe`, `LICENSE-TinyGL`, `THIRD_PARTY_NOTICES.md`, `UPSTREAM.md`, `PATCHES.md`, static-pie/`INTERP`-гейт | **PASS-критерии §5.2**: сборка под musl, нет `INTERP`, дайджест `0x3f7e5ece33386f9b`, детерминизм, 6/6 |
| 2 | `tinygl: FFI core (context, matrices, viewport, clear, triangles, depth)` | безопасная обёртка §3.3, `Surface`-валидация, host-тесты §8.1 (матрицы/clear/треугольник/depth) | `cargo test` крейта зелёный, 6/6, никакого композитора |
| 3 | `tinygl: textures, display lists, arrays` | биндинги + host-тесты текстур (RGB/UNSIGNED_BYTE/256×256/NEAREST/REPEAT/DECAL), display lists как батчинг, arrays с проверкой на x86_64 | свойства + дайджесты сцен с текстурой |
| 4 | `pagh-gl-demo: PDP/1 ShmSurface (memfd → SHM_OFFER → рендер → COMMIT)` | крейт демо: клиент, memfd/mmap/offer/commit/damage, resize через новый `SHM_OFFER`, честная ошибка без ShmSurface | live-прогон в QEMU: окно с треугольником, `TINYGL` маркеры |
| 5 | `pagh-gl-demo: triangle, cube, gears + pixel checks + frame time` | три сцены, дайджесты, замеры, `--hash` | дайджесты стабильны, frame time напечатан |
| 6 | `docs+e2e: GL phase, acceptance, ограничения` | `_desktop_gl_phase()`, README крейта + корневой README (ограничения), прогон acceptance §7 | **Acceptance §7 целиком**, 6/6 |
| — | **отдельно, после спеки:** `ci: run userspace crate tests` | job/шаг `cargo test` по затронутым `rust-apps/*` + синхронная правка AGENTS.md | 6/6 и `check_agents_md.py` зелёный |

Правила: base = `dev`; ветки удаляются после мержа; версия — **MINOR в PR, который
выпускает фичу** (§0.8), не в этой спеке и не в 2.24.0.

---

## 10. Риски (честно, с митигацией)

| Риск | Вер-ть | Влияние | Митигация |
|---|---|---|---|
| **Нет C-компилятора для musl** на dev-машинах/CI (на машине фактов нет ни `musl-gcc`, ни `clang`) | высокая | PR1 не собирается | PR1 и существует, чтобы это выяснить: детекция + инструкция (`musl-tools`), затем `clang --target`+sysroot, затем `zig cc`; при полном провале — вопрос lead'у (не самовольный переход к B) |
| **64-битные указатели в float-массивах** (upstream README сам предупреждает; upstream тестировал 32-бит x86 и sparc) | средняя | тихая порча геометрии при client arrays | v1 демо — immediate mode; arrays включаются отдельным PR с проверкой на x86_64 (+ по желанию UBSan); указатели в коде идут через `.p` (`arrays.c:133`) — выглядит корректно, но проверяем |
| **Пиксельные расхождения host↔musl/clang** (f32/FP-contract) | средняя | «дайджест не тот» | эталон — musl-значение; host-эталон фиксируем рядом; расхождение документируем, а не прячем; `-ffast-math` не включаем |
| **CPU-растеризация в QEMU TCG медленная** | высокая | демо «выглядит сломанным» | 640×480, кадры по счёту (не по времени), бюджет e2e ≤ 5 мин, честная формулировка в отчётах |
| **Тулчейн/линковка тянет системную зависимость** (libm/libc musl) | низкая | порча static-pie | `INTERP`-гейт в `build-rust-app.sh`; `nm -u` уже проверен — только libc/libm |
| **`zbuffer.c` требует `w*4` и `w%4==0`** | высокая (классика) | «правая полоса не рисуется» | валидация в `Surface::new` с внятной ошибкой + host-свойства |
| **`RENDER_BITS`/`TGL_FEATURE_8_BITS` через `defined()`** | высокая | сборка падает `#error`, или пиксель `u16` | точный патч §5.3 зафиксирован; PR1 проверяет фактическим рендером |
| **`gl_fatal_error` = `exit(1)`** (`error.c`) | средняя | внутренний fatal убивает процесс демо | документируем; в v1 upstream не патчим (решение lead: строго upstream) |
| **Depth/маски строго upstream** (`glDepthMask` нет, Z пишется всегда) | средняя | клиент ждёт GL-семантику | названо в README; расширения — отдельная волна с review |
| **Лицензионная чистота** (притащить форк/старую версию; забыть notice) | низкая | блок мержа | источник — только tarball 0.4.1 с sha256; `LICENSE-TinyGL` байт-в-байт; `THIRD_PARTY_NOTICES.md`; в PR — «откуда файл» |
| **CI не видит userspace-тесты**, пока не сделан отдельный CI-PR | средняя | регрессия проходит мерж | явный `cargo test` в PR-чеклисте + e2e по политике; CI-PR сразу после спеки |
| **Ожидания «настоящий OpenGL»** | средняя | обещания, которых нет | формулировки §3.2 в README и в ответах |

---

## 11. Решения lead (зафиксированы в v2)

| # | Вопрос | Решение |
|---|---|---|
| 1 | Версия | docs-only **без bump**; MINOR — в PR, который выпускает фичу (не 2.24.0, не спека) |
| 2 | A vs B | **A**: upstream C 0.4.1 + тонкая Rust FFI; B — только будущее решение после провала A и нового review |
| 3 | Дерево | `rust-apps/tinygl/` (C + FFI + notices) и `rust-apps/pagh-gl-demo/`; имени «pagh-gl» не заводить |
| 4 | CI | явный `cargo test` для затронутых userspace-крейтов; правка AGENTS.md + `check_agents_md.py` — **отдельный PR** |
| 5 | e2e GL | не на каждый PR; только при изменении TinyGL/демо/e2e-пути и перед релизом; 640×480; кадры по счёту; ≤ 5 мин/сценарий |
| 6 | Depth/маски | строго upstream 0.4.1 в v1, без форка/расширений |
| 7 | Selection mode | **отложен** |

**Оставшиеся follow-up'ы (не блокируют спеку):**

1. CI-PR с `cargo test` по userspace-крейтам + синхронная правка AGENTS.md.
2. Решение по client arrays, если PR3 покажет проблему на x86_64 (включить/отложить).
3. Порог MINOR-bump: какой именно PR считается «выпускающим» фичу (PR5 или PR6).
4. Нужен ли `--size 1280x800` в acceptance сверх 640×480 (сейчас — «если успеваем по
   бюджету»).

---

## 12. Ссылки (зафетчены; контент внешний, использован как данные)

**Первоисточник и лицензия:**

* TinyGL changelog — <https://bellard.org/TinyGL/changelog.html> («version 0.4.1:
  - changed license to MIT»).
* TinyGL home/download — <https://bellard.org/TinyGL/> (0.4.1, Mar 5 2022; там же
  устаревшая строка «Zlib-like licence», §2.2).
* Tarball — <https://bellard.org/TinyGL/TinyGL-0.4.1.tar.gz>, sha256 `b314f019…16f`
  (§2.3); внутри `LICENSE`, `README`, `LIMITATIONS`, `Changelog`.
* `LIMITATIONS` (в tarball'е) — источник таблицы §3.1 (depth только для линий, `GL_LESS`,
  текстуры 256×256/NEAREST/REPEAT/DECAL, `glClear` чистит всё, `glPolygonOffset` — API only).

**Поведенческие референсы (код НЕ берём):**

* ALMOS (LIP6): TinyGL входит в user-land библиотеки ОС —
  <https://largo.lip6.fr/trac/almos/wiki/WikiStart> («Available User-land Libraries» →
  «Graphics → … TinyGL»). Конкретные URL `.../browser/trunk/sys/TinyGL/README?rev=124` и
  `.../browser/trunk/sys?rev=292` из задания отдают **404** моему фетчеру — цитирую
  доступную wiki-страницу.
* SerenityOS LibGL — программный растеризатор (плитки 16×16, барицентрика),
  <https://api.github.com/repos/SerenityOS/serenity/contents/Userland/Libraries/LibGL>
  (`GLContext.cpp` 31 771 B, `Texture.cpp` 44 139 B, `Matrix.cpp`, `Lighting.cpp`,
  `ClipPlane.cpp`, `Blending.cpp`). Зеркало лога из задания
  (`git.netizen.se/serenity/log/...GLContext.cpp...`) отдало **404**. Лицензия Serenity
  построчно не проверялась — код не копируем.
* Redox OS — <https://doc.redox-os.org/book/graphics-windowing.html>: «For software
  rendering we implemented a Redox backend in our Mesa3D fork/recipe to use
  LLVMPipe/Lavapipe»; Orbital «was preferred over Wayland and X11 … fastest native
  software rendering performance»; «Apps (or 'clients') create a window and draw to it by
  using the orbclient client». Вывод: тонкий собственный протокол клиент→композитор —
  нормальная архитектура для ОС без GPU; Mesa не тащим.

**Внутренние документы:** `docs/desktop-b.md` (kernel-UAPI: memfd, `SCM_RIGHTS`,
`/dev/fb0`), `docs/desktop-server.md` (PDP/1, backend'ы, e2e `desktop`),
`docs/desktop-render-spec.md` (frame time/damage; OpenGL в non-goals),
`rust-apps/pagh-desktop/PROTOCOL.md` §8.1 (ShmSurface).

**Локальная проверка фактов v2 (не артефакт репозитория):** распакованный tarball в
`/tmp/tg041/TinyGL`, пробная сборка 23 файлов (`gcc -O2 -fno-strict-aliasing`,
`-lm`), пробный рендер с дайджестом `0x3f7e5ece33386f9b`, `nm` по объектам
(197 `T`-символов), `gcc -Wall -Wextra` (3 предупреждения `implicit-fallthrough` в
`get.c:42,44,67`), `nm -u` (только libc/libm).