# PaghTerm

Терминал для Pagh desktop B: VT100/ANSI-эмулятор, программный RGBA-рендерер,
headless PNG-скриншоты и клиент PDP/1 (Pagh Desktop Protocol).  Без внешних зависимостей,
собирается статически под `x86_64-unknown-linux-musl`.

![PaghTerm](screenshots/01-welcome.png)

E2E с реальным композитором: `pagh-desktop` из dev (PR #101) рисует окно
терминала, полученное по PDP/1 (headless backend, 1280×800).

![Live PDP/1](screenshots/05-live-pdp1.png)

## Что уже работает

* VT100/ANSI: CSI/OSC/ESC/DCS, курсор, scroll region, alt screen, tabs,
  16/256/truecolor SGR, атрибуты, UTF-8, wide/zero-width, DEC graphics,
  запросы DA/DSR/CPR/DECRQM.
* Рендерер 8×18 (Fira Code) + wide 16×18 (Noto CJK), box drawing/block
  elements, курсор, selection, scrollback/scrollbar, окно для скриншотов.
* PNG-выгрузка (RGBA8) без зависимостей.
* Клиент PDP/1: AF_UNIX, hello/welcome/create/commit/input/configure/focus/
  ping/pong/close, XRGB8888/RGB565, кодирование клавиш и SGR-мыши.
* `ShmSurface`: memfd/`SCM_RIGHTS` (`shm_offer`/`shm_ready`/`shm_release`),
  рендер прямо в mmap, по сокету — только damage-прямоугольники; мигание
  курсора и ввод дают damage порядка клетки, а не полный кадр. Legacy-сервер
  без bit 2 обслуживается прежним полным кадром.
* ABI v1 kernel console shared surface: валидатор, reader, конвертация в
  экран (`/dev/vtconsole0`, `/dev/vtinput`), см. [TERMINAL.md](TERMINAL.md).
* **Режим kernel-session (по умолчанию для `--socket`)**: окно подключается к
  свободной сессии ядра (`/dev/vtfb{N}`, ioctl `0x4700` + `mmap`), передаёт
  клавиатуру в `/dev/vtinput{N}`, указатель — в `/dev/vtmouse{N}` (колесо —
  scrollback, протяжка — выделение/копирование, вставка `Ctrl+Y`/`Ctrl+V`), и
  сообщает ядру клавиатурный фокус (`ioctl 0x4710`), чтобы ввод не дублировался
  в boot-VT. Команды, эхо, редактирование строки и scrollback делает ядро;
  клиент рендерит и передаёт ввод, ничего не имитируя. `exit` в сессии — честное
  «сессия завершена» с перезапуском по Enter в том же окне. Детали и границы —
  в [TERMINAL.md](TERMINAL.md).
* 180 host-тестов, включая эталонные escape-последовательности, скриншоты,
  mock-устройства kernel-session и ABI указателя (на хосте `/dev/vtfb1` нет).

## Сборка и проверки

```sh
cd rust-apps/pagh-terminal

cargo fmt --all -- --check
cargo test                    # host tests
cargo run -- --screenshots screenshots

# статический бинарник для pagh (нужен rustup target x86_64-unknown-linux-musl)
tools/build-static.sh         # результат: rust-apps/out/pagh-terminal
```

`cargo test` использует локальный `.cargo/config.toml`, чтобы нейтрализовать
`build-std` из корневого конфига ядра.  Статическая сборка выполняется через
`tools/build-static.sh` из нейтрального каталога — так cargo не наследует
bare-metal-конфиг ядра (см. комментарий в скрипте).

## Использование

```sh
# скриншот демо
cargo run -- --headless --demo 01-welcome --out /tmp/welcome.png --print

# произвольный вывод терминала
cargo run -- --headless --script session.bin --cols 100 --rows 30 \
  --out /tmp/session.png --print

# живое окно через compositor: сессия ядра (/dev/vtfb1 + /dev/vtinput1) —
# это режим по умолчанию, никаких специальных флагов не нужно
cargo run -- --socket /tmp/pagh-desktop.sock --size 900x600

# другой worktree/ядро: подменить устройства поверхности и ввода
cargo run -- --socket /tmp/pagh-desktop.sock \
  --vt-fb /dev/vtfb1 --vt-input /dev/vtinput1

# демо-шелл (скриншоты, A/B) и userspace-shell за pipe'ами (fallback/тест) —
# оба режима сохранены, но больше не по умолчанию
cargo run -- --socket /tmp/pagh-desktop.sock --demo-shell
cargo run -- --socket /tmp/pagh-desktop.sock --pipe-shell

# диагностика: зеркалить boot-VT (--vt + --socket) или один снимок (без --socket)
cargo run -- --vt /dev/vtconsole0 --socket /tmp/pagh-desktop.sock --mirror-vt
```

Headless-демо: `01-welcome`, `02-colors`, `03-scrollback`, `04-unicode`.
`--select R1,C1:R2,C2 --dump-selection` печатает выделенный текст;
`--print-full` включает scrollback.

## Ограничения

Live-режим подключается к `pagh-desktop` по PDP/1 и по умолчанию показывает
сессию ядра: клиент блитит `/dev/vtfb1` и пишет клавиши в `/dev/vtinput1`.
Полный набор команд (`df`, `ps`, `ppkg`, `lxrun`, `disk`, `nano`, history,
редактирование строки, scrollback) — это ядро; в клиенте их нет вообще.
Если устройств нет (старое ядро), окно говорит об этом в stderr и
откатывается на прежнюю pipe-сессию с userspace-shell'ом
(`help/ls/cd/pwd/echo/cat/uname/clear/exit`, флаг `--pipe-shell`, он же
`--vt` + `--socket`); `--mirror-vt` возвращает зеркало boot-VT для
диагностики, `--demo-shell` — демо-шелл.

Честные границы kernel-session: нет PTY/termios, нет `TIOCGWINSZ`/SIGWINCH
(размер знает только ядро), `Ctrl+C`/`Ctrl+Z` — просто байты в
`/dev/vtinput{N}` (сигналы доставляет ядро), пиксельная поверхность не несёт
DEC-режимов, поэтому стрелки всегда идут в normal-кадрах; alternate screen
(`\e[?1049h`) ядро не реализует; scrollback/selection/буфер обмена остаются в
ядре (клиент переносит протяжку/кнопки/клавиши, а колесо при 1:1-вьюпорте
прокручивает сам вьюпорт). По умолчанию кадр показывается 1:1: при меньшем
окне виден полноразмерный вьюпорт с прокруткой (колесо,
`Ctrl+Shift`+стрелки/PageUp/PageDown), вьюпорт следует за живым низом, так что
приглашение и новый вывод видны без прокрутки; прокрутка вверх останавливает
следование, `Ctrl+Shift+End` возвращает его.  Апскейла нет; необязательный
целый зум-обзор (`--scale 2`, `Ctrl+Shift+Z`) сжимает только в 2×/3×…, без
дробного шага.  Live-приёмка в QEMU (история/редактирование, колесо, выделение,
nano, `exit`→Enter, закрытие/переоткрытие ×20, изоляция двух окон) — evidence в
`/home/oleg/.cache/pagh-agent-notes/qterm4-live/`.  ShmSurface включается только если сервер объявил
`welcome.flags` bit 2 (иначе — полный кадр). Нет reflow при resize,
комбинирующие метки не компонуются, OSC 4/8/52 игнорируются.  Подробно —
[TERMINAL.md](TERMINAL.md).

## Лицензии

Код — MIT (см. корневой `LICENSE`).  Bitmap-шрифты в `assets/` — производные
Fira Code и Noto Sans CJK, распространяются по SIL OFL 1.1; тексты и
атрибуция — в `fonts/LICENSES.md` и `fonts/OFL-1.1.txt`.
