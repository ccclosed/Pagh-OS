# pagh-ui — UI-тулкит рабочего стола Pagh

Клиентский тулкит для userspace-композитора Pagh desktop B (вариант «композитор
в user space, приложения — обычные статически собранные musl-бинарники»).
Библиотека **без внешних зависимостей**: свой растр, встроенный шрифт, PNG-энкодер
и клиент протокола **PDP/1** (сервер — `rust-apps/pagh-desktop`).

Композитор владеет заголовком окна (`v`/`^`/`x`), рамкой, панелью, размещением,
фокусом и drag/resize; клиент рисует **только клиентскую область** и получает
координаты ввода относительно неё.

Демо-сцены (рендер в PNG без экрана, см. `src/demo.rs`):

| Сцена | Что показывает |
|-------|----------------|
| `ui-gallery.png` | подписи, кнопки, поля ввода, список с секциями, сетка значков, набор иконок |
| `ui-menu.png` | окно Files с контекстным меню и подменю «Создать» |
| `ui-palette.png` | палитра темы Pagh dark |

Скриншоты лежат в [`screenshots/`](screenshots), их же проверяют
screenshot-тесты.

## Что внутри

| Модуль | Назначение |
|--------|------------|
| `geom` | `Point`, `Size`, `Rect`, `Insets`: целочисленная геометрия и деление прямоугольников |
| `color` | RGBA8888, смешивание straight-alpha, интерполяция |
| `canvas` | программный рендер: заливка, скругления, рамки, круги/треугольники, мягкие тени, градиенты, отсечение, blit |
| `font` | встроенный растровый шрифт DejaVu Sans 13 px (ASCII + русский алфавит), измерение/обрезка/перенос строк |
| `image` | PNG-энкодер (CRC32 + Adler32 + zlib stored-блоки) и PPM |
| `theme` | палитра и метрики, снятые с `docs/file-manager-concept.png` |
| `layout` | flex-полосы, сетка значков, центрирование |
| `event` | события мыши/клавиатуры, модификаторы, коды протокола |
| `widgets` | `Label`, `Button`, `TextField`, `List` (секции + выделение), `Grid` (центрированные подписи) |
| `menu` | `Menu`/`MenuEntry`, подменю, раскладка по содержимому, hit-тесты |
| `chrome` | заголовок окна `v`/`^`/`x`, строка состояния, панель рабочего стола |
| `icon` | векторные иконки, включая синюю папку с листом бумаги |
| `protocol` | клиентский кодек PDP/1: заголовок `PDS1`, сообщения 1..12, `launch(13)`, shm `14..16`, `MessageReader` с учётом header flags |
| `client` | `DesktopClient` поверх `AF_UNIX`: `hello`/`welcome`, `open_window`, `commit`, `enable_shm`/`commit_shm`/`resize_shm`, `poll`, `launch`, `damage_hint` |
| `shm` | `memfd_create` + `ftruncate` + `mmap`, `SCM_RIGHTS` send/recv, `MappedBuffer`/`ShmSurface` |
| `damage` | `Damage` (clip/merge/collapse) и `DamageTracker` (сравнение тайлов кадра, без аллокаций в кадре) |
| `image`/`testkit` | PNG/PPM, детерминированный хеш сцены, golden-тесты |
| `demo` | готовые сцены витрины/меню/палитры |

Клиентская сторона протокола описана в [`PROTOCOL.md`](PROTOCOL.md); нормативный
ABI и обязанности сервера — [`../pagh-desktop/PROTOCOL.md`](../pagh-desktop/PROTOCOL.md).

## Сборка и тесты

Крейт — отдельный workspace: корневой `.cargo/config.toml` репозитория настраивает
bare-metal ядро, поэтому локальный `.cargo/config.toml` переопределяет target на
хост и дополняет `build-std` до полного `std` (как в `host-tests/`).

```bash
cd rust-apps/pagh-ui

cargo test                 # unit- и screenshot-тесты, 100+ проверок
cargo run --bin pagh-ui-gallery -- --out screenshots   # перерисовать демо-PNG
cargo build --release --target x86_64-unknown-linux-musl   # статический бинарник
```

Статическая сборка даёт `static-pie` ELF без `INTERP` — такой бинарник
запускается на Pagh из ring 3:

```text
file target/x86_64-unknown-linux-musl/release/pagh-ui-gallery
```

## Пример: окно и содержимое (CSD — приложение рисует заголовок само)

```rust
use pagh_ui::canvas::Canvas;
use pagh_ui::chrome::{self, WindowStyle};
use pagh_ui::geom::Rect;
use pagh_ui::theme::Theme;
use pagh_ui::widgets::{Button, ButtonState};

let theme = Theme::pagh_dark();
let mut canvas = Canvas::filled(320, 200, theme.palette.desktop);
let layout = chrome::draw_window(
    &mut canvas,
    &theme,
    Rect::new(10, 10, 300, 180),
    WindowStyle::new("Домашняя папка — Files"),
);
let button = Button::primary("Открыть");
button.draw(
    &mut canvas,
    &theme,
    Rect::new(layout.content.x + 12, layout.content.y + 12, 110, 32),
    ButtonState::default(),
);
```

## Пример: подключение к композитору

```rust
use pagh_ui::client::{DesktopClient, Incoming};
use pagh_ui::canvas::Canvas;
use pagh_ui::geom::Size;
use std::time::Duration;

let mut client = DesktopClient::connect("/run/pagh-desktop.sock", "pagh-files")?;
let window = client.open_window(Size::new(1000, 600), "Домашняя папка — Files")?;
let mut frame = Canvas::filled(1000, 600, pagh_ui::Color::hex(0x081216));
client.commit(window, &frame)?;
loop {
    for message in client.poll(Duration::from_millis(16))? {
        match message {
            Incoming::Configure { size, .. } => {
                frame = Canvas::filled(size.w, size.h, pagh_ui::Color::hex(0x081216));
                client.commit(window, &frame)?;
            }
            Incoming::Event { event, .. } => { /* передать в приложение */ let _ = event; }
            Incoming::Close { .. } => return Ok(()),
            _ => {}
        }
    }
}
# Ok::<(), std::io::Error>(())
```

`launch("terminal", cwd)` отправляет композитору PDP/1 `launch (type 13)` —
запуск известного приложения (см. [`PROTOCOL.md`](PROTOCOL.md)).

## Детерминизм и screenshot-тесты

Рендер чисто целочисленный (без плавающей точки в раскладке), шрифт встроен,
поэтому один и тот же кадр даёт одинаковый хеш FNV-1a на любой платформе.
`testkit::assert_golden` сравнивает хеш сцены с эталоном и при расхождении
сохраняет PNG в `PAGH_SCREENSHOT_DIR` (по умолчанию `screenshots/`), чтобы было
видно, что изменилось.

## Ограничения

- Один размер шрифта (13 px, 4-битное сглаживание); крупные заголовки пока не
  поддерживаются — для них нужен отдельный шрифт/масштабирование.
- Нет прокрутки как виджета: `Grid`/`List` отдают раскладку, полосу прокрутки и
  смещение рисует приложение (Files делает это само).
- Если сервер не объявил `ShmSurface` (`welcome.flags` bit 2), клиент шлёт
  полную поверхность: `damage` остаётся подсказкой. На shm-сервере рендер идёт
  прямо в memfd, а по сокету уходят только прямоугольники (`commit` с
  `flags=SHM`); fallback обязан оставаться рабочим и покрыт тестами.
- Текст вводится по US-раскладке (PDP/1 передаёт только evdev-код); другие
  раскладки требуют input method/расширения протокола.
- Курсор мыши, drag-and-drop, буфер обмена, заголовок и панель — зона
  композитора, не тулкита. Модуль `chrome` оставлен для приложений с
  собственным заголовком (CSD), Files его не использует.