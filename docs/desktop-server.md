# Desktop B: userspace-композитор `pagh-desktop`

Этот документ описывает вертикальный срез userspace-десктопа варианта B: отдельный
процесс `pagh-desktop` владеет экраном, клиенты рисуют окна и общаются с ним по
собственному протоколу **PDP/1**. Два backend'а выбираются при старте: headless
(PNG для тестов/скриншотов) и **fbdev** (`/dev/fb0` + `/dev/input/event0` из
kernel-ABI 2.24.0). Headless-путь полностью автономен и не требует QEMU; реальный
backend проверяется отдельным QEMU-сценарием `tools/e2e.py desktop`.

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
                                        │ HeadlessBackend  FbdevBackend    │
                                        │ (PNG)            (mmap scanout)  │
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

## Реальный backend: fbdev + evdev

`OutputBackend` выбирается в `Server::bind`:

* `--backend auto` — пробует `/dev/fb0`; на хосте/без Limine-framebuffer молча
  откатывается в headless;
* `--backend fbdev` — обязателен `/dev/fb0` (иначе ошибка, а не тихий fallback);
* `--backend headless` — как раньше, PNG-вывод.

`FbdevBackend` (src/fbdev.rs) делает ровно ABI из `docs/desktop-b.md`:

1. `open("/dev/fb0", O_RDWR)` — первый open включает takeover (консоль перестаёт
   flush'ить кадр);
2. `FBIOGET_VSCREENINFO` (160 байт) и `FBIOGET_FSCREENINFO` (80 байт) — геометрия,
   bpp, маски red/green/blue, stride и smem_len;
3. `mmap(NULL, smem_len, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0)` — scanout;
4. `present()` пишет только damage-прямоугольники: пиксель упаковывается по маскам
   `var` (bpp 16/24/32), строка сдвигается на `fix.line_length`.

`EvdevInput` (src/evdev.rs) открывает `/dev/input/event0` неблокирующе, читает
24-байтные `struct input_event`, собирает `REL_X`/`REL_Y` до `SYN_REPORT`, превращает
`REL_WHEEL` в axis, `BTN_LEFT/MIDDLE/RIGHT` — в кнопки, отслеживает модификаторы
(Shift/Ctrl/Alt/Super) и отбрасывает события, накопившиеся до открытия (строка
запуска из shell). Сервер опрашивает fd в своём цикле (без futex-ожиданий), поэтому
ввод обрабатывается с задержкой ~2 мс.

Серверный цикл больше не блокируется на `mpsc::recv_timeout` (futex-таймаут в
compat-слое просыпался с задержкой в секунды): он неблокирующе дренирует канал,
опрашивает evdev и спит 2 мс. Это то, что делает `F10`/мышь отзывчивыми под QEMU.

**Takeover односторонний** (`docs/desktop-b.md` §3.1): после открытия `/dev/fb0`
kernel-консоль не перерисовывает экран, serial-логи/shell продолжают работать, но
закрытие fd экран не возвращает — в QEMU e2e это проверяется как «после выхода
desktop новый вывод shell не меняет кадр», а не как возврат консоли.

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
| экран | headless PNG; **fbdev уже реализован** | `/dev/fb0` mmap + `FBIOGET_VSCREENINFO`/`FBIOGET_FSCREENINFO` |
| ввод | команды/тесты; **evdev уже реализован** | `/dev/input/event0` (evdev) |
| общие буферы | frame upload по сокету | `MAP_SHARED` memfd + будущий `ShmSurface` |
| консоль | нет | takeover флага консоли/fb, serial остаётся |

Когда kernel-ABI появится, добавляются `FbdevBackend` (mmap + `present` по damage) и
`EvdevInput` (reader → события композитора); протокол и композитор не меняются.
Консоль/v1 terminal-window остаётся отдельным мостом (kernel VT → shared text surface),
как зафиксировано в `/tmp/desktop-refs/NOTES.md`.

## Проверено

* `cargo test` в `rust-apps/pagh-desktop`: **89 unit + 5 integration** (e2e по
  реальному AF_UNIX-сокету: hello/create/configure/commit/input/key/screenshot/close;
  два клиента и маршрутизация фокуса; повреждённый клиент получает `error` и не роняет
  сервер; `max_frames`/устаревший сокет; скриншот-сценарий с проверкой PNG и
  уникальности; host-тесты fbdev-упаковки/stride/damage и evdev-декодера).
* `cargo clippy --all-targets` — без предупреждений; `cargo fmt --check` — чисто.
* QEMU e2e `python3 tools/e2e.py desktop`: готовит pagh-ext2 диск (первая загрузка
  kernel-format + инъекция статических бинарников через debugfs), запускает
  `rust /mnt/pd --e2e`, проверяет serial/скриншоты: fbdev ioctl+mmap, takeover,
  selfcheck пикселей scanout, F10-меню, REL/BTN_LEFT через QMP, выход и отсутствие
  перерисовки консоли. Скриншоты: `screenshots/11-fbdev-qemu.png` (desktop на
  реальном scanout) и `screenshots/12-fbdev-context-menu.png` (меню).
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

1. `ShmSurface` (`SCM_RIGHTS`, memfd) и частичный commit; frame upload остаётся fallback.
2. Реестр приложений и `.desktop`-подобные записи для `launch` (сейчас — явные
   `launch_targets`/`--spawn-demo`); управление дочерними процессами и перезапуск.
3. Клиентские popup/меню, курсоры, DnD, outputs/scale/rotation, анимации.
4. Kernel-console/VT bridge: общий текстовый surface, чтобы shell стал окном Terminal.
5. Ввод с реального железа уже идёт через `/dev/input/event0` (PS/2/USB HID), но
   EV_ABS/touch, hotplug и автоповтор в kernel-ABI отсутствуют; проверено в QEMU,
   реальное железо — за оператором.
6. Возврат kernel-консоли после выхода desktop невозможен по текущему ABI
   (takeover односторонний); нужен отдельный kernel-примитив, если это требование
   появится.
7. Курсор в fbdev рисуется программно в кадре; аппаратного курсора нет.
