# PDP/1 — клиентская сторона (pagh-ui)

Клиентская половина протокола **PDP/1** (Pagh Desktop Protocol) для
userspace-композитора `pagh-desktop`. Нормативное описание провода, сообщений и
обязанностей сервера — [`rust-apps/pagh-desktop/PROTOCOL.md`](../pagh-desktop/PROTOCOL.md);
здесь зафиксировано, что именно делает клиент и как это проверяется.

## Транспорт

- `AF_UNIX`, `SOCK_STREAM`, одно управляющее соединение на клиента.
- Кадр: 12-байтный заголовок и payload:

  ```text
  off  size  поле
    0     4  magic  = b"PDS1"
    4     2  type   (u16 LE)
    6     2  flags  (u16 LE, должен быть 0)
    8     4  length (u32 LE, длина payload)
   12   ...  payload
  ```

- Все целые little-endian; строки — `u16` длина + UTF-8.
- Лимиты: payload ≤ `4096*4096*4 + 4096`, сторона поверхности ≤ 4096,
  stride ≤ `width*bpp + 4096`.
- Неизвестный тип, ненулевые `flags`, лишние/недостающие байты payload —
  ошибка кадра (соединение рвётся).

Типы сообщений: `hello(1)`, `welcome(2)`, `create(3)`, `configure(4)`,
`commit(5)`, `input(6)`, `focus(7)`, `close(8)`, `damage(9)`, `error(10)`,
`ping(11)`, `pong(12)` и расширение `launch(13)` (см. ниже).

## Что делает клиент

1. Подключается к сокету и первым шлёт `hello { version=1, pid, app }`.
   Ждёт `welcome { version, client_id, screen_w, screen_h, flags }` не дольше
   5 с; версия, отличная от 1, — ошибка.
2. Создаёт окно `create { window_id, width, height, format, title }`.
   `width/height` — **клиентская область** (без заголовка и рамки); размеры
   `64..=4096`. `window_id` выбирает клиент (не 0, уникален среди его окон).
3. Получает `focus` и `configure { window_id, x, y, width, height, flags }`.
   `configure` приходит после создания и после каждого изменения геометрии
   (drag/resize/maximize/restore/minimize); клиент отвечает кадром нового
   размера. Флаги: `FOCUSED=1`, `MAXIMIZED=2`, `MINIMIZED=4`, `RESIZABLE=8`.
4. Шлёт `commit { window_id, width, height, format, stride, data_len, pixels }`
   полной поверхностью. Форматы: `1 = XRGB8888` (`0x00RRGGBB`,
   little-endian), `2 = RGB565`. `damage` — только подсказка, в v1 кадр всё
   равно полный.
5. Читает `input` с координатами **внутри клиентской области**:
   `kind 1=motion, 2=button, 3=key, 4=axis`, `pressed`, `modifiers`
   (биты Shift/Ctrl/Alt/Super), `code` (кнопка: 1/2/3 или evdev-код клавиши),
   `x/y/dx/dy`, маска `buttons`.
6. Закрывает окно `close { window_id, reason }` (`reason=0` — клиент) и
   завершается на `close` от сервера (`reason=1` — кнопка `x`), `2` —
   обрыв, `3` — ошибка протокола, `4` — окно уничтожено.
7. На `ping` отвечает `pong` с тем же nonce.

Оформление окна (заголовок, `v`/`^`/`x`, рамка, панель, размещение, фокус,
drag/resize) принадлежит композитору: клиент рисует только клиентскую область.
Тулкит содержит модуль [`chrome`](../src/chrome.rs) для приложений, которые
рисуют свой заголовок, но Files его не использует.

### Текст и раскладка

PDP/1 передаёт только evdev-код клавиши без текста. `pagh_ui::client::map_key`
восстанавливает печатные символы по **US-раскладке** (Shift учитывается; Caps
Lock и другие раскладки — нет) и на нажатие печатной клавиши без Ctrl/Alt
отдаёт пару `Key::Char` + `Event::Text`. Приложение получает и горячие клавиши
(`Ctrl+C`, `Alt+Enter`), и ввод в поля. Для других раскладок нужен input method
или расширение протокола.

## Расширение `launch` (type 13)

Пункт меню «Открыть терминал в этой папке» не форкает процессы сам — он просит
композитор открыть известное приложение:

```text
launch (13), c→s
string app        // "terminal", "files", "paint", …
string cwd        // подсказка рабочего каталога; может быть пустой
```

- Разрешён после `hello`; окно для него не требуется.
- Состав приложений задаёт сервер (`ServerOptions.launch_targets`, CLI
  `--launch-target`); если явной цели нет, но включён `--spawn-demo` и `app` —
  известный `AppKind`, запускается демо-клиент.
- Успех отдельным сообщением не подтверждается; ошибка приходит как
  `error { code, message }` (`ERR_UNSUPPORTED` — приложение не настроено,
  `ERR_INTERNAL` — процесс не стартовал).
- `cwd` — только подсказка (демо-клиент её пока не принимает).

Расширение обратно совместимо: клиенты, которые его не шлют, работают как
раньше.

## Пример сессии

```text
client → hello(version=1, pid=4242, app="pagh-files")
server → welcome(version=1, client_id=1, 1280x720, flags=XRGB8888|RGB565)
client → create(window_id=1, 1000x600, format=1, "Домашняя папка — Files")
server → focus(1, true)
server → configure(1, x=96, y=64, 1000x600, flags=FOCUSED)
client → commit(1, 1000x600, format=1, stride=4000, pixels…)
server → input(1, button(x=120, y=80, button=3, pressed=true))
client → launch(app="terminal", cwd="/home/user")
client → close(1, reason=0)
server → close(1, reason=2)
```

## Проверки

- `tests/pdp_compat.rs` кодирует сообщения нашего клиента и разбирает их
  серверным кодеком `pagh-desktop` (и наоборот) — защита от дрейфа провода.
- `client.rs` проверяет рукопожатие, create/commit, ввод, `launch`, закрытие на
  тестовом PDP-сервере.
- `pagh-files/tests/pdp_e2e.rs` поднимает настоящий `pagh-desktop`, запускает
  настоящий бинарник `pagh-files`, шлёт ввод и `launch`, закрывает окно
  композитором и снимает кадр сервера.

## Ограничения v1

- Нет `SCM_RIGHTS`/`ShmSurface`: кадр идёт копией по сокету (4 МиБ на кадр при
  1000×600 XRGB8888).
- Нет частичного аплоада: `damage` — подсказка для композитора.
- Нет курсора/буфера обмена/drag-and-drop в протоколе: этим владеет композитор.
- Текст вводится US-раскладкой (см. выше); широкие символы (CJK, эмодзи) в
  заголовках клиента не поддерживаются встроенным шрифтом.