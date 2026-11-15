# Desktop B: userspace-композитор `pagh-desktop`

Этот документ описывает вертикальный срез userspace-десктопа варианта B: отдельный
процесс `pagh-desktop` владеет экраном, клиенты рисуют окна и общаются с ним по
собственному протоколу **PDP/1**. Срез намеренно не зависит от kernel-ветки
`feat/desktop-kernel`: вывод идёт в headless-backend (PNG), ввод приходит командами
сервера/тестами, а `/dev/fb0` и `/dev/input/event0` подключаются позже как ещё один
backend.

## Зачем отдельный протокол

Wayland в Pagh-OS пока отсутствует (нет `wl_display`, xdg-shell, SHM/DMA-BUF). Для
первого рабочего десктопа нужен маленький протокол, который:

* передаёт кадры клиента без внешних библиотек (`XRGB8888`/`RGB565`, stride);
* умеет рукопожатие, окна, ввод, фокус и закрытие — минимум для Terminal/Files/Paint;
* оставляет место для `ShmSurface` (`SCM_RIGHTS`) и частичного commit без смены
  архитектуры;
* тестируется на хосте без QEMU и без графических драйверов.

Полное описание ABI — [`rust-apps/pagh-desktop/PROTOCOL.md`](../rust-apps/pagh-desktop/PROTOCOL.md).

Запуск приложений из клиентов — расширение **`launch` (type 13)**: клиент присылает
`app` (например `terminal`) и подсказку `cwd`, а композитор запускает настроенную
команду (`ServerOptions.launch_targets`, CLI `--launch-target`) или демо-клиент при
`--spawn-demo`. Оформление окна, кнопки `v/^/x`, панель и фокус принадлежат
композитору: клиенты (`pagh-files`, `pagh-terminal`) рисуют только клиентскую
область. Шрифт интерфейса композитора покрывает ASCII и кириллицу, поэтому
русские заголовки читаемы.

## Архитектура

```text
┌─────────────────────────────┐         ┌──────────────────────────────────┐
│ клиенты                     │         │ pagh-desktop (один процесс)      │
│  pagh-desktop-demo          │         │                                  │
│  Terminal / Files / Paint   │◄───────►│ Server (AF_UNIX SOCK_STREAM)     │
│  (позже pagh-terminal etc.) │  PDP/1  │   ├─ reader-поток на клиента     │
└─────────────────────────────┘         │   ├─ очередь Incoming            │
                                        │   └─ команды управления          │
                                        │            │                     │
                                        │            ▼                     │
                                        │      Compositor                  │
                                        │  окна/z-order/фокус/drag/resize  │
                                        │  панель 44 px, меню, flyout      │
                                        │  damage tracker + Canvas         │
                                        │            │                     │
                                        │     ┌──────┴──────┐              │
                                        │     ▼             ▼              │
                                        │ HeadlessBackend  fb0 backend     │
                                        │ (PNG/PPM)        (позже)         │
                                        └──────────────────────────────────┘
```

Композитор ничего не знает про сокеты и backend: методы ввода/окон возвращают
`Vec<HostEvent>` (`Send { client_id, message }` или `Launch(AppKind)`), а сервер
разносит сообщения по клиентам. Это позволяет гонять один и тот же сценарий как через
реальные сокеты, так и напрямую (скриншоты/тесты).

## Окна и управление

* Позиция окна хранится как внешняя рамка (`frame`), клиентская область —
  `content_rect()`, заголовок — 30 px, рамка — 1 px, радиус скругления — 10 px.
* Кнопки заголовка: `v` (minimize), `^` (maximize/restore), `x` (close). Клик по
  заголовку поднимает окно и фокусирует; drag заголовка двигает окно с живым damage;
  resize — правый нижний угол, минимальный контент 320×200.
* Maximize занимает экран над панелью; restore возвращает сохранённую рамку. После
  серверного resize/maximize клиент получает `configure` и присылает кадр нового
  размера; до этого старый кадр масштабируется nearest-neighbour.
* Minimize скрывает окно; чип с первой буквой заголовка на панели возвращает его.
* Alt+Tab циклически переключает фокус, Esc закрывает меню/flyout, F10 открывает
  контекстное меню, Ctrl+Alt+T запускает Terminal, Super — лаунчер.

## Панель и оболочка

Панель — 44 px внизу: лаунчер (3×3), Terminal, Files, Paint, чипы окон, Wi-Fi-иконка,
часы `23:38` / дата `15.11.2026` (по умолчанию фиксированы для детерминированных
скриншотов; `pagh-desktop --clock now` берёт UTC). Клик по Wi-Fi открывает flyout
(Pagh-OS-5G / Wired / VPN), клик по часам — календарь November 2026 с выделенным 15-м.
Правый клик по рабочему столу/заголовку открывает контекстное меню (Open Terminal/
Files/Paint, Change Wallpaper, Move/Minimize/Maximize/Close для окна). Меню и flyout
закрываются кликом вне или Esc.

Оформление — в духе KDE Breeze Dark: тёмные обои с бирюзовым свечением, скруглённые
окна, мягкие программные тени, акцентная линия фокуса, полупрозрачная подсветка hover.

## Damage/repaint

`DamageTracker` копит прямоугольники, объединяет пересекающиеся и ограничивает их
экраном. Перед отрисовкой сервер забирает damage, композитор рисует всю сцену **с
отсечением по damage-прямоугольникам**, затем backend получает кадр и список
повреждённых областей. Неповреждённые пиксели холста не меняются.

Damage добавляется на: создание/удаление/фокус/подъём окна, перетаскивание и resize
(старая и новая рамка + тень), commit клиента (клиентская область), открытие/закрытие
меню/flyout/панели, подсветку кнопок/пунктов меню, движение курсора. Для z-order
повреждение расширяется до объединения пересекающихся рамок. Полный кадр
перерисовывается только при старте, смене обоев и по явному `render_full()` для
скриншота.

## Ввод

Сейчас ввод приходит либо командами `ServerControl` (тесты, скриншот-сценарий,
headless-демо), либо синтетическими событиями backend'а. Модель соответствует
wlroots/seat-подходу в упрощённом виде:

* absolute/relative motion → hit-test сверху вниз (меню → flyout → панель → окна);
* захват кнопки: press в клиентской области фиксирует окно, release уходит ему же;
* drag/resize — серверные grabs: motion двигает рамку, release отправляет `configure`;
* клавиатура — сфокусированному окну; серверные shortcuts обрабатываются до клиента;
* панель/меню/flyout забирают клик себе и не пробрасывают его в клиент.

Следующий шаг — reader `/dev/input/event0`: `struct input_event { time, type, code,
value }` (24 байта) → EV_KEY/EV_REL/EV_SYN → те же `Compositor::pointer_*`/`key_event`.
В kernel-ветке `desktop-kernel` этот ABI добавляется вместе с `/dev/fb0`.

## Граница с kernel-веткой

Этот PR **не трогает** `src/**` ядра и не зависит от него:

| Компонент | Здесь | В kernel-ветке (`feat/desktop-kernel`) |
|---|---|---|
| экран | headless PNG | `/dev/fb0` mmap + `FBIOGET_VSCREENINFO`/`FBIOGET_FSCREENINFO` |
| ввод | команды/тесты | `/dev/input/event0` (evdev) |
| общие буферы | frame upload по сокету | `MAP_SHARED` memfd + будущий `ShmSurface` |
| консоль | нет | takeover флага консоли/fb, serial остаётся |

Когда kernel-ABI появится, добавляются `FbdevBackend` (mmap + `present` по damage) и
`EvdevInput` (reader → события композитора); протокол и композитор не меняются.
Консоль/v1 terminal-window остаётся отдельным мостом (kernel VT → shared text surface),
как зафиксировано в `/tmp/desktop-refs/NOTES.md`.

## Проверено

* `cargo test` в `rust-apps/pagh-desktop`: **69 unit + 5 integration** (e2e по реальному
  AF_UNIX-сокету: hello/create/configure/commit/input/key/screenshot/close; два клиента
  и маршрутизация фокуса; повреждённый клиент получает `error` и не роняет сервер;
  `max_frames`/устаревший сокет; скриншот-сценарий с проверкой PNG и уникальности).
* `cargo fmt --all -- --check` — чисто.
* `./build-static.sh`: `x86_64-unknown-linux-musl`, оба бинарника `static-pie`,
  ~585 KiB сервер и ~497 KiB демо-клиент.
* Скриншоты: `rust-apps/pagh-desktop/screenshots/01-desktop.png` … `09-restored.png`,
  транскрипт `screenshots/scenario.txt`; сценарий проверяет, что фокус, drag, меню,
  запуск, flyout, календарь и maximize действительно меняют кадр.
* `screenshots/10-live-demo-client.png` — кадр от `--spawn-demo`: отдельный процесс
  `pagh-desktop-demo` подключился к статическому серверу по реальному сокету, прошёл
  hello/create/commit, и сервер отрисовал полученную по проводу поверхность (этот путь
  поймал regression с потерей непрозрачности XRGB при `from_wire`).

## Референсы

Перед реализацией сверялись (идеи, не код):

* `/tmp/desktop-refs/NOTES.md` — решение варианта B, ABI fbdev/evdev/memfd.
* Smithay: `desktop/space/mod.rs` (`element_under`, z-порядок, damage/render),
  `input/mod.rs` + `input/pointer` (focus/motion/button/grabs), `backend/headless`;
* cosmic-comp: `state.rs` (глобальное состояние), `shell/focus` (FocusTarget,
  render order), `shell/mod.rs` (activate/close/move/resize);
* wlroots: `types/scene/wlr_scene.c` (`wlr_scene_node_at` — hit-test в обратном
  z-порядке), `types/wlr_output_damage.c` (накопление damage), `types/seat/*`
  (pointer/keyboard focus), `types/wlr_compositor.c` (роли surface);
* KDE Plasma 6.7.4 / KWin 6.7.4 — UX панели, заголовков, контекстных меню, flyout.

## Остаток

1. `FbdevBackend`: mmap `/dev/fb0`, ioctl, damage-present; ждёт kernel-ABI.
2. `EvdevInput`: `/dev/input/event0` + EV_KEY/EV_REL/EV_SYN, absolute mouse, hotplug.
3. `ShmSurface` (`SCM_RIGHTS`, memfd) и частичный commit; frame upload остаётся fallback.
4. Реестр приложений и `.desktop`-подобные записи для `launch` (сейчас — явные
   `launch_targets`/`--spawn-demo`); управление дочерними процессами и перезапуск.
5. Клиентские popup/меню, курсоры, DnD, outputs/scale/rotation, анимации.
6. Kernel-console/VT bridge: общий текстовый surface, чтобы shell стал окном Terminal.
7. Ввод с реального железа (PS/2/USB HID через event0) и проверка в QEMU с
   `desktop-kernel`; headless-срез уже даёт воспроизводимый regression-набор.