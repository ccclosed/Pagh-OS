# pagh-ui — UI-тулкит рабочего стола Pagh

Клиентский тулкит для userspace-композитора Pagh desktop B (вариант «композитор
в user space, приложения — обычные статически собранные musl-бинарники»).
Библиотека **без внешних зависимостей**: свой растр, встроенный шрифт, PNG-энкодер
и клиент протокола.

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
| `protocol` | клиентские типы PGUI v1 и кадрирование |
| `client` | `DesktopClient` поверх `AF_UNIX`: `hello`/`welcome`, `create_window`, `commit`, `poll` |
| `image`/`testkit` | PNG/PPM, детерминированный хеш сцены, golden-тесты |
| `demo` | готовые сцены витрины/меню/палитры |

Протокол описан в [`PROTOCOL.md`](PROTOCOL.md).

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

## Пример: окно и содержимое

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
use pagh_ui::client::DesktopClient;
use pagh_ui::canvas::Canvas;
use pagh_ui::geom::Size;

let mut client = DesktopClient::connect("/run/pagh-desktop.sock", "pagh-files")?;
let window = client.create_window(Size::new(1060, 640), "Домашняя папка — Files")?;
let frame = Canvas::filled(1060, 640, pagh_ui::Color::hex(0x081216));
client.commit(window, &frame)?;
for incoming in client.poll()? {
    // Обработать события ввода/configure/closed.
    let _ = incoming;
}
```

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
- Протокол v1 передаёт полную поверхность без damage — ок.
  для локального сокета, но избыточно для больших окон; см. `PROTOCOL.md`.
- Курсор мыши, drag-and-drop и буфер обмена — зона композитора, не тулкита.