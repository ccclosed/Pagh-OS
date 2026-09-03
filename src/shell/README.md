# `src/shell/` — интерактивный shell, paint, nano, мини-toolchain

Framebuffer-shell с редактированием строки, историей, таб-комплишном, цветным выводом;
оконная рисовалка `paint`; полноэкранный редактор `nano+`; интерпретатор мини-Rust
«pagh-mini». Вход — `shell::shell_main()`, вызывается из `boot.rs` после поднятия ФС/сети.

## Файлы

| Файл | Роль |
|---|---|
| `mod.rs` | Единственный I/O-модуль: REPL-цикл (`shell_main`), диспетч через реестр, Tab, autorun, dual serial+framebuffer печать, status bar |
| `commands.rs` | Тела всех команд (`cmd_help`, `cmd_ls`, `cmd_apt`, …) + хелперы `shell_println`, `resolve_arg`, `rm_path` и др. |
| `registry.rs` | Статическая таблица `COMMANDS: &[CommandSpec]`; `lookup()`, `command_names()` |
| `editor.rs` | `LineEditor` — чистая буфер+курсор модель (курсор в чар-юнитах, cap 256) |
| `history.rs` | `History` — bounded ring (CAP=64), recall вверх/вниз, stash текущей строки, дедуп подряд идущих |
| `complete.rs` | Чистое таб-дополнение: `longest_common_prefix`, `Completion::{None, Single, Multiple}`, `complete_command`, `complete_path` |
| `keys.rs` | Декодер PS/2 Set 1: `KeyEvent`, `Decoder` (0xE0-префикс, Shift/Ctrl/CapsLock), таблица ASCII, `CTRL_C_LATCH` |
| `render.rs` | `Style`, цветовые константы, `with_style`, `prompt`, `error_line`, `success_line` (framebuffer — цвет, serial — plain) |
| `path.rs` | Глобальный CWD под `Spinlock<String>`; чистые `normalize` (фолдинг `.`/`..`, кламп в корень), `resolve`, `cwd`, `set_cwd` |
| `suggest.rs` | Bounded Levenshtein `edit_distance`, `nearest_command` для «did you mean» |
| `paint.rs` | Оконная рисовалка (~1470 строк): title bar, toolbar, taskbar, canvas `Vec<u32>` |
| `nano.rs` | `nano+` полноэкранный редактор: `Editor`, undo/redo, поиск/замена/goto, clipboard |
| `nano_config.rs` | Персист `NanoConfig` в `/mnt/.nanorc`; темы Dark/Light/Blue; CLI `nano --settings` |
| `toolchain.rs` | «pagh-mini» мини-Rust: интерпретатор (`execute`), `rustc` (компиляция в `.pbc` = магия `PAGH-MINI-RUST:1\n` + исходник), `cargo new/check/build/run`, `rustup` |

## Ключевые символы

- `shell::shell_main() -> !`
- `registry::{CommandSpec, ShellCtx, COMMANDS, lookup, command_names}`
- `editor::LineEditor::{insert, delete_back, delete_fwd, move_left/right/home/end, buffer, cursor}`
- `history::History::{push, recall_prev, recall_next, reset_nav, saved_line}`
- `complete::{longest_common_prefix, complete_command, complete_path}`
- `keys::{KeyEvent, Decoder::{new, feed}, CTRL_C_LATCH}`
- `path::{normalize, resolve, cwd, set_cwd}`
- `suggest::{edit_distance, nearest_command}`
- `paint::run()`, `nano::run(path)`, `toolchain::{execute, run_path, rustc, cargo, rustup}`

## Как работает REPL

1. Баннер на обе консоли (`kprintln!` = serial, `fb_println!` = framebuffer).
2. `run_autorun()`: исполняет непустые не-`#` строки `/mnt/etc/autorun` (cap 512 байт), `&&`-чейны.
3. Сессио-персистентные `keys::Decoder` и `History`; на промпт — свежий `LineEditor`.
4. Внешний цикл — промпт; внутренний — `cpu::halt()` до прерываний; на пробуждении:
   `ps2_mouse::poll()`, `cursor::hide()`, перерисовка status bar (OS, CWD, uptime,
   координаты мыши), дренаж сканкодов.
5. События: вставка, Backspace/Delete, стрелки/Home/End, Up/Down история, Tab, Enter →
   `execute_command`.
6. `execute_command`: split по `&&`, токенизация, `registry::lookup` → вызов хендлера;
   неизвестное → `error_line` + `nearest_command` «did you mean».

**Модель рендера (v1, задокументирована)**: консоль деструктивная; `erase_visible(n)` =
`"\x08 \x08"` на обеих консолях; `redraw_line` стирает `shown` символов и перепечатывает буфер.
Видимая каретка всегда в конце строки; логический курсор может быть в середине.

### Tab-комплишн
Без whitespace → `complete_command`; иначе `readdir()` родителя токена через VFS →
чистая `complete_path`. `Single` заменяет токен; `Multiple` расширяет до LCP и листит кандидатов.

## Команды (полный список)

`help`, `clear`, `echo`, `uptime`, `ls`, `cat` (cap 64 KiB), `mkdir`, `touch`, `write`,
`rm` (`-r`/`-rf`, post-order, depth cap 24), `sync`, `fscrash` (демо journal replay:
запись, ремоунт ext2, `journal.recover()`, верификация), `pci`, `exec` (встроенный
тестовый ring-3 процесс), `ifconfig`, `ping`, `nc`, `selftest` (`test::run_all()`),
`cd`, `pwd`, `cp` (cap 16 MiB), `mv` (только файлы), `stat`, `sleep`, `nano`, `lua`,
`python`/`python3`, `pythonc`, `cargo`, `rustc`, `rustup`, `rust`, `paint`,
`pkg <host> <path> [port]` (HTTP GET .deb → ar → decompress → tar → install в /mnt),
`apt <update|install|show|list|setmirror>`, `lxrun <path> [args]` (Linux ELF в ring 3,
fixed env `TERM=xterm`, `PATH=/mnt/usr/bin`; foreground-ожидание с polling ^C),
`warn <on|off>` (тумблер fb-зеркала warn-логов).

## paint — архитектура

- Одиночное модальное приложение, общего window manager нет: `PaintApp` держит свой chrome —
  `WinMode::{Windowed, Maximized}`, кнопки Minimize/Maximize/Close, драг титлбара (кламп
  в десктоп), фон десктопа, taskbar с кнопкой «Paint».
- Общие layout-хелперы `tool_buttons()`/`brush_ui()` — рендер и hit-test физически не могут разойтись.
- Canvas: `Vec<u32>` (1 пиксель = u32, белый) + one-level undo (второй undo = redo).
  Windowed-режим = 5/6 экрана по центру; maximize/restore сохраняет перекрывающиеся пиксели.
- Инструменты (клавиши p/e/l/r/f/c/d/b/i): Pencil, Eraser, Line, Rect, FilledRect, Circle,
  Disc, Fill (scanline flood fill), Picker. Brush 1..=64. ЛКМ = рисовать, ПКМ = белый.
- Shape-preview: rubber-band прямо в framebuffer, откат через `blit_canvas_rect`.
- Геометрия: Bresenham, brush-thick midpoint circle, локальный `isqrt` (Ньютон).
- Сохранение: магия `PAGHIMG1` + LE u32 размеры + LE u32/пиксель в `/mnt/paint.img`.
- Цикл: halt на idle; Esc — выход; события мыши по изменению `MouseState.seq`;
  taskbar репейнтится не чаще раза в 6 тиков.

## nano

`MAX_FILE=64KiB`, `MAX_LINE=4096`, `UNDO_LIMIT=32`; 3 темы через `NanoConfig::palette()`.
Клавиши: `^S` save (опц. `.bak`), `^Q`/Esc — двухнажатный quit guard, `^F` find, `^R` replace,
`^G` goto, `^Z`/`^Y` undo/redo, `^C`/`^K`/`^U` copy/cut/paste строки. Полный репейнт на клавишу;
не-ASCII → `?` (дисплей лосси, ввод ASCII); курсор — синее подчёркивание 2px.

## Зависимости

- **От:** `drivers` (framebuffer, cursor, ps2_mouse, ps2_kbd), `vfs`, `net`, `pkg`,
  `task::{process, compat, scheduler}`, `arch::cpu`, `apic::TICK_HZ`, `log`, `sync::spinlock`.
- **На неё:** `boot.rs` (вызов `shell_main`), `test.rs` (свойства P21–P27 гоняют чистые
  модули `complete/editor/history/keys/path/render/suggest/registry` напрямую).

## Грабли

- Всё в `shell` — `pub(crate)`; наружу крейта ничего не экспортируется.
- `execute_command` игнорирует коды ошибок отдельных команд `&&`-чейна (стоп только на
  неизвестной команде).
- `cmd_mv` работает только с файлами (каталоги отклоняются).
- Дедуп истории — только подряд идущих.
- Контракт cursor/framebuffer: `hide()` до любого ренда под курсором, потом `move_to`;
  замыкания `framebuffer::with` не логируют (дедлок на нереентерабельном спинлоке).
