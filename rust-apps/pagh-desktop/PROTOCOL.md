# Pagh Desktop Protocol (PDP/1)

Протокол между userspace-композитором `pagh-desktop` и клиентскими приложениями.
Транспорт — `AF_UNIX` / `SOCK_STREAM`; одно управляющее соединение на клиента.
Кадры в v1 передаются **внутри сокета** (frame upload), без `SCM_RIGHTS`; вариант
с разделяемой памятью (`ShmSurface`) зарезервирован на будущее (см. §10).

Все целые — little-endian. Координаты — логические пиксели, начало координат в левом
верхнем углу. Поверхности рисуются top-down.

## 1. Кадр (frame)

Каждое сообщение — 12-байтовый заголовок и payload:

```text
offset  size  field
     0     4  magic   = b"PDS1"
     4     2  type    (u16)
     6     2  flags   (u16, зарезервировано; должно быть 0)
     8     4  length  (u32, длина payload в байтах)
    12   ...  payload
```

Ограничения:

| Параметр | Значение |
|---|---|
| `MAX_PAYLOAD` | `4096*4096*4 + 4096` = 67 112 960 байт |
| максимальная сторона поверхности | 4096 px |
| максимальное окно | 4096×4096 (клиентская область) |
| минимальное окно | 64×48 |
| максимальный stride | `width * bpp + 4096` |
| screen (по умолчанию) | 1280×720 |

`length` проверяется до выделения памяти. Неверная сигнатура, ненулевые `flags`,
неизвестный `type`, лишние/недостающие байты payload считаются ошибкой кадра.

## 2. Типы сообщений

| type | имя | направление |
|---:|---|---|
| 1 | `hello` | c→s |
| 2 | `welcome` | s→c |
| 3 | `create` | c→s |
| 4 | `configure` | s→c |
| 5 | `commit` | c→s |
| 6 | `input` | s→c |
| 7 | `focus` | s→c |
| 8 | `close` | обе стороны |
| 9 | `damage` | c→s |
| 10 | `error` | s→c |
| 11 | `ping` | обе стороны |
| 12 | `pong` | обе стороны |
| 13 | `launch` | c→s (расширение, см. §8) |

Строки (`string`) кодируются как `u16 length` (байты UTF-8) + байты. Невалидный UTF-8 —
ошибка кадра.

## 3. Рукопожатие

```text
client                                  server
  |  ---- hello(version=1, pid, app) ---->  |
  |  <--- welcome(version=1, client_id,    |
  |        screen_w, screen_h, flags) ----- |
  |  ---- create(window_id, w, h, ...) ---> |
  |  <--- focus(window_id, true) ---------- |
  |  <--- configure(window_id, ...) ------- |
  |  ---- commit(window_id, frame) -------> |
  |  <--- input(window_id, event) --------- |
```

`hello` обязан быть первым сообщением; повторный `hello` — ошибка `ERR_BAD_MESSAGE`.
Версия протокола, отличная от 1, отклоняется `ERR_UNSUPPORTED`.

### hello (1)

```text
u32 version       // 1
u32 pid
string app        // имя приложения (сервер хранит первые 64 символа)
```

### welcome (2)

```text
u32 version
u64 client_id     // уникален в рамках сервера, используется как владелец окон
u32 width         // screen width
u32 height        // screen height
u32 flags
```

Флаги `welcome`:

| бит | смысл |
|---:|---|
| 0 | поддерживается `XRGB8888` |
| 1 | поддерживается `RGB565` |
| 2 | `ShmSurface`/`SCM_RIGHTS` (в v1 всегда 0) |

## 4. Окна

### create (3)

```text
u32 window_id     // выбирает клиент; не 0; уникален среди всех клиентов
u32 width         // клиентская область, 64..=4096
u32 height        // 48..=4096
u32 format        // 1 = XRGB8888, 2 = RGB565
string title
```

Сервер размещает окно каскадом, поднимает его, сфокусирует и отправляет
`focus(window_id, true)` и `configure`. Ошибки: `ERR_DUPLICATE_WINDOW`,
`ERR_INVALID_SURFACE`, `ERR_LIMIT` (не более 32 окон).

### configure (4)

```text
u32 window_id
i32 x             // позиция клиентской области на экране
i32 y
u32 width         // размер клиентской области
u32 height
u32 flags
```

Флаги `configure`:

| бит | смысл |
|---:|---|
| 0 | окно в фокусе |
| 1 | maximized |
| 2 | minimized |
| 3 | resizable (в v1 всегда 1) |

`configure` приходит после создания окна и после каждого серверного изменения
геометрии/состояния: перетаскивания заголовка, resize за правый нижний угол,
maximize/restore, minimize. Клиент должен ответить `commit` с кадром нового размера;
до этого композитор масштабирует последний полученный кадр (nearest neighbour).

### commit (5) — загрузка кадра по сокету

```text
u32 window_id
u32 width
u32 height
u32 format
u32 stride        // байт на строку
u32 data_len      // ровно stride * height
bytes pixels[data_len]
```

Форматы:

* `1` — `XRGB8888`: пиксель `u32` little-endian, `0xAARRGGBB`; альфа-канал
  игнорируется, кадр непрозрачный.
* `2` — `RGB565`: пиксель `u16` little-endian, `rrrrrggggggbbbbb`.

Требования: `stride >= width*bpp`, `stride <= width*bpp + 4096`,
`data_len == stride*height`. Кадр может быть любого допустимого размера; если он не
совпадает с текущим `configure`, композитор масштабирует его. Ошибки:
`ERR_UNKNOWN_WINDOW`, `ERR_NOT_OWNER`, `ERR_INVALID_SURFACE`.

### damage (9) — подсказка для будущего частичного аплоада

```text
u32 window_id
u16 count
repeat count:
    i32 x, i32 y, u16 w, u16 h     // координаты внутри клиентской поверхности
```

В v1 `damage` не уменьшает объём `commit` (кадр всё равно полный), но композитор
использует подсказку для своей damage-области. Сообщение опционально.

### close (8)

```text
u32 window_id
u32 reason
```

Причины (`reason`):

| значение | смысл |
|---:|---|
| 0 | клиент закрывает окно |
| 1 | сервер просит закрыть (кнопка `x`) |
| 2 | соединение разорвано |
| 3 | ошибка протокола |
| 4 | окно уничтожено |

Клиент может закрыть только своё окно; при нарушении — `ERR_NOT_OWNER`.
При `reason=1` сервер уже удалил окно, клиенту остаётся завершиться (или удалить
связанные ресурсы). При обрыве соединения сервер уничтожает все окна клиента.

## 5. Ввод

### input (6)

```text
u32 window_id
u8  kind          // 1=motion, 2=button, 3=key, 4=axis
u8  pressed       // для button/key
u16 modifiers     // для key
u32 code          // button (1=left,2=middle,3=right) или evdev-код клавиши
i32 x
i32 y             // координаты внутри клиентской поверхности
i32 dx
i32 dy            // motion: delta; axis: dx=horizontal, dy=vertical
u32 buttons       // маска нажатых кнопок
```

Маска `buttons`: бит 0 — left, бит 1 — middle, бит 2 — right.

Модификаторы: бит 0 — Shift, 1 — Ctrl, 2 — Alt, 3 — Super.

Коды клавиш — подмножество Linux evdev (`KEY_ESC=1`, `KEY_ENTER=28`, `KEY_TAB=15`,
`KEY_BACKSPACE=14`, стрелки 103..108, `KEY_DELETE=111`, `KEY_F10=68`, `KEY_SUPER=125`,
`KEY_A=30`, `KEY_T=20`, `KEY_X=45`). Полный набор не требуется: клиент получает те
коды, которые присылает backend ввода.

Маршрутизация:

* motion — окну под указателем (или окну, захватившему кнопку);
* button press/release — окну под указателем; нажатие внутри содержимого также
  поднимает и фокусирует окно;
* key — сфокусированному окну;
* axis — окну под указателем.

Сервер сам обрабатывает `Esc` (закрыть меню/flyout), `F10` (контекстное меню),
`Alt+Tab` (переключение фокуса), `Ctrl+Alt+T` (Terminal), `Super` (лаунчер) и не
пересылает эти комбинации клиенту. Заголовок окна, кнопки `v/^/x`, панель и меню
принадлежат композитору, клиент их не получает.

### focus (7)

```text
u32 window_id
u8  focused       // 1/0
bytes pad[3]      // нули
```

Приходит при смене фокуса. Создание окна фокусирует его; клик по окну или `Alt+Tab`
меняет фокус. Флаг `CONFIGURE_FOCUSED` в `configure` дублирует состояние.

## 6. Ошибки

### error (10)

```text
u32 code
string message
```

| код | константа | смысл |
|---:|---|---|
| 1 | `ERR_BAD_MESSAGE` | некорректный кадр/последовательность |
| 2 | `ERR_NOT_HELLO` | запрос до `hello` |
| 3 | `ERR_DUPLICATE_WINDOW` | id окна уже занят |
| 4 | `ERR_UNKNOWN_WINDOW` | окно не найдено |
| 5 | `ERR_NOT_OWNER` | окно принадлежит другому клиенту |
| 6 | `ERR_INVALID_SURFACE` | размер/формат/stride/размер окна |
| 7 | `ERR_LIMIT` | превышен лимит окон |
| 8 | `ERR_UNSUPPORTED` | неизвестный тип/версия/запрос |
| 9 | `ERR_INTERNAL` | внутренняя ошибка сервера |

Для ошибок разбора кадра (`magic`, `length`, зависшие байты) сервер отправляет
`error(ERR_BAD_MESSAGE)` и закрывает соединение. Для семантических ошибок соединение
сохраняется.

## 7. Ping/Pong

### ping (11) / pong (12)

```text
u32 nonce
```

`ping` может отправить любая сторона; ответ — `pong` с тем же `nonce`. В v1 сервер
отвечает на ping клиента; клиентский ответ на серверный ping не обязателен.

## 8. Запуск приложений (launch, type 13)

Расширение PDP/1: клиент просит композитор открыть известное приложение —
например, файловый менеджер по пункту «Открыть терминал в этой папке».
Обратно совместимо: клиенты, которые его не шлют, работают как раньше.

### launch (13)

```text
string app        // идентификатор приложения: "terminal", "files", "paint", …
string cwd        // подсказка рабочего каталога; может быть пустой
```

* Состав приложений и команды запуска задаёт сервер
  (`ServerOptions.launch_targets`, CLI `--launch-target`); если явной цели нет,
  но включён `--spawn-demo` и `app` — известный `AppKind`, запускается
  демо-клиент.
* Успешный запуск отдельным сообщением не подтверждается.
* Ошибка (приложение не настроено, процесс не стартовал) приходит как `error`:
  `ERR_UNSUPPORTED` или `ERR_INTERNAL`.
* `launch` разрешён после `hello`; создавать окно для него не требуется.
* `cwd` — только подсказка: приложения без понятия рабочего каталога её
  игнорируют (демо-клиент `pagh-desktop-demo` пока не принимает).

## 9. Пример сессии (два окна)

```text
C: hello(version=1, pid=4242, app="pagh-files")
S: welcome(1, client_id=1, 1280, 720, flags=3)
C: create(window_id=101, 560, 340, XRGB8888, "Files - /mnt")
S: focus(101, true)
S: configure(101, x=680, y=332, w=560, h=340, flags=9)
C: commit(101, 560x340, stride=2240, ...)
C: create(window_id=102, 640, 400, XRGB8888, "Terminal - pagh")
S: focus(101, false)
S: focus(102, true)
S: configure(102, x=97, y=95, w=640, h=400, flags=9)
C: commit(102, ...)
S: input(102, motion{...})
S: input(102, button{left, pressed})
C: close(102, reason=0)
S: focus(101, true)          // если 101 было следующим видимым окном
S: close(101, reason=1)      // при нажатии «x» сервер сам инициирует закрытие
```

## 10. Ограничения v1

* Нет `SCM_RIGHTS`/shared memory: каждый кадр копируется через сокет.
* Нет частичного аплоада: `commit` всегда передаёт полный кадр; `damage` — только
  подсказка.
* Нет pointer enter/leave, grab-ов, DnD, курсоров клиента, popup-ов.
* Один глобальный экран; нет outputs/scale/rotation.
* Окна клиента не имеют собственных sub-surface/декораций; рамка и заголовок —
  серверные.
* `input` не содержит timestamp/серийных номеров.

## 11. Будущее (не в v1)

* `ShmSurface`: `welcome.flags[2]=1`; `create` передаёт memfd через `SCM_RIGHTS` +
  offset/stride/format, `commit` — только dirty-регионы и id буфера. Frame upload по
  сокету остаётся fallback-путём.
* Частичный `commit` (rect list вместо полного кадра) и явный fence/ack кадра.
* fb0/event0 backend ядра: `/dev/fb0` (mmap + `FBIOGET_VSCREENINFO`/`FBIOGET_FSCREENINFO`)
  и `/dev/input/event0` (`struct input_event`), см. `docs/desktop-b.md` kernel-ветки.
* Клиентские popup/меню и мульти-экран.

Любое несовместимое изменение повышает версию протокола; сервер обязан отклонять
неизвестную версию в `hello`, а новые поля добавляются только вместе с `welcome.flags`.