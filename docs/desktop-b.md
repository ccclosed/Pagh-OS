# Desktop B: kernel UAPI для userspace-композитора (2.24.0 + SCM_RIGHTS)

**Статус:** базовые примитивы реализованы в ядре Pagh-OS 2.24.0; `SCM_RIGHTS`
(fd passing, §7) добавлен отдельным изменением поверх 2.24.0 — `Cargo.toml`
в нём не менялся, `dev` по-прежнему несёт 2.24.0. Композитор
(`pagh-desktop`) — отдельный userspace-процесс; этот документ описывает
**только** примитивы, которые ему нужны от ядра. Самого композитора здесь нет.

Вариант B: userspace-сервер рисует кадр в разделяемую память и отдаёт его
`/dev/fb0`; ввод читает из `/dev/input/event0`. Kernel-консоль после handover
экран не трогает, serial-логи продолжают работать.

Ссылки на архитектуру, по которой проектировался слой:
Smithay/COSMIC (Rust compositor), wlroots (C reference), KWin/Plasma
(Wayland + compositing), Linux UAPI `fbdev`/`evdev`/`memfd`.
ABI ниже проверен host-тестами против `linux/fb.h` и `linux/input.h`.

---

## 1. Какие syscall'ы нужны композитору

| nr x86_64 | syscall | Что даёт |
|---|---|---|
| 319 | `memfd_create` | анонимный файл под SHM-буферы |
| 76 / 77 | `truncate` / `ftruncate` | задать размер SHM-буфера |
| 9 | `mmap` | `MAP_SHARED` — общие страницы; `MAP_FIXED` — загрузчик/резервы |
| 11 | `munmap` | вернуть VA и ссылку на страницы |
| 10 | `mprotect` | защита региона (в т.ч. read-only поверх) |
| 2 / 257 | `open` / `openat` | `/dev/fb0`, `/dev/input/event0`, файлы |
| 16 | `ioctl` | `FBIOGET_VSCREENINFO` / `FBIOGET_FSCREENINFO`; `PGVT_IOC_INFO` |
| 0 | `read` | события `/dev/input/event0` |
| 7 | `poll`, 271 `ppoll`, 270 `pselect6`, 23 `select` | ожидание событий |
| 3 | `close` | отпустить fd (страницы живут, пока живы отображения) |
| 56 | `clone` | fork-ребёнок (`SIGCHLD`, glibc `fork()`); `CLONE_VM|CLONE_THREAD` — потоки; `CLONE_VM|CLONE_VFORK` — `posix_spawn` |
| 59 | `execve` | запуск клиентов; маппинги старого AS снимаются, fd (без `CLOEXEC`) наследуются |
| 1 / 0 | `write` / `read` | AF_UNIX (`socketpair`/`connect`) уже есть; контроль ancillary data не несут |
| 46 / 47 | `sendmsg` / `recvmsg` | AF_UNIX `SCM_RIGHTS`: клиент передаёт композитору memfd (см. §7) |
| 48 | `shutdown` | половинное закрытие направления: `SHUT_WR` даёт пиру EOF, `SHUT_RD` — свои `recv` = 0 (см. §10) |

`fork` (57) и `vfork` (58) **не** поддерживаются как отдельные номера:
glibc `fork()` идёт через `clone(SIGCHLD)`, и этот путь работает. Форма
`clone(CLONE_VM|CLONE_VFORK)` (её зовут musl/glibc `posix_spawn`, а за ними
Rust-std `Command::spawn`) поддержана как COW-fork с явным `child_stack` и
остановкой родителя до `execve`/выхода ребёнка: совместный стек vfork не
нужен `posix_spawn` (статус идёт через pipe), а копия безопаснее. Поэтому
Кнопки панели Files/Terminal и Ctrl+Alt+T запускают дочерний процесс —
реальное приложение из `--launch-target`; демо-клиент `pagh-desktop-demo`
остаётся только тестовым фолбэком (`--spawn-demo`, `--legacy-demo` в e2e) и в
обычном рецепте образа не участвует. Так `posix_spawn`-путь композитора не
падает с `Function not implemented (os error 38)`. Рецепт автозапуска обычного
образа — `/mnt/etc/autorun` из `rust-apps/pagh-desktop/desktop-autorun`
(ставится в образ `tools/mkimage.py --autorun`, см. README крейта); он задаёт
`--launch-target files=`/`terminal=` и `--autostart files`, без demo-флагов.

---

## 2. `memfd_create` + `ftruncate` + `mmap(MAP_SHARED)`

### 2.1. Создание и размер

```c
int fd = memfd_create("surface-1", 0);          /* 319 */
ftruncate(fd, width * height * 4);              /* 77  */
void *p = mmap(NULL, size, PROT_READ | PROT_WRITE,
               MAP_SHARED, fd, 0);              /* 9   */
```

* `memfd_create` принимает `MFD_CLOEXEC` и `MFD_ALLOW_SEALING`. Sealing
  принимается как флаг, но **печати не реализованы**: `F_ADD_SEALS` в этом
  ядре отсутствует, поэтому `MFD_ALLOW_SEALING` ничего не разрешает.
* `ftruncate` расширяет файл без немедленной аллокации страниц (Linux-семантика):
  новые байты читаются нулями, страницы появляются при записи/`mmap`.
  Уменьшение отрезает целые страницы за новым EOF; байты в последней частичной
  странице сохраняются, но за `size` не видны.
* `read`/`write`/`pread64`/`pwrite64` и `MAP_SHARED` работают с **одними и теми
  же** физическими страницами. Запись через отображение видна `read(2)`, и
  наоборот.

### 2.2. Семантика `MAP_SHARED`

`mmap` требует ровно один из `MAP_PRIVATE`/`MAP_SHARED`; оба или ни одного —
`EINVAL`.

`MAP_SHARED`:

* поддерживается для **memfd** и для `MAP_SHARED | MAP_ANONYMOUS`;
* на обычном файле (`/tmp`, ext2) — `EINVAL`: page cache/writeback для
  произвольных ФС в этом ядре нет, и притворяться разделяемым отображением
  опаснее, чем отказать;
* `PROT_WRITE` требует fd, открытого на запись (иначе `EACCES`);
* `PROT_NONE` для shared отвергается (`EINVAL`): страницы отображены жадно, а
  ленивого дозаполнения из файла у слоя нет;
* `mremap` shared-региона — `EINVAL` (перемещение сломало бы общий кадр);
* `MAP_FIXED` разрешён: старые листья диапазона освобождаются, новый диапазон
  отображается поверх.
* `mmap` за EOF (в пределах `ftruncate`-размера и за ним) отдаёт нулевые
  страницы; запись через отображение за EOF не увеличивает размер файла
  (композитор мапит ровно `ftruncate`-размер). `mprotect(PROT_NONE)` на shared
  снимает листья; вернуть их обратно `mprotect(RW)` не может — это
  документированное сужение (страницы не переподпираются лениво).

Внутренняя модель (для понимания времени жизни):

* страница memfd — кадр PMM в `page_index -> phys` карте узла; файл держит одну
  pin-ссылку кэша (`pmm::cow_ref`), каждое отображение — ещё одну;
* лист PTE несёт программный бит `SHARED_BIT`; `fork` копирует PTE и добавляет
  ссылку, **не** делая COW-копию;
* `munmap`/exit/`mprotect(PROT_NONE)` снимают лист и отпускают ссылку
  отображения; кадр возвращается в PMM только когда ушла последняя ссылка;
* закрытие fd не вырывает страницы из-под живого отображения: `Drop` узла
  отпускает только cache-ссылку.

### 2.3. fork/exec

* `fork` (через `clone(SIGCHLD)`): ребёнок наследует fd (тот же memfd) и
  отображения; страницы общие. Запись родителя после `fork` видна ребёнку.
* `execve`: адресное пространство и его отображения сносятся штатным
  `drop_user_space` (shared-листья отпускают ссылки). Если fd не `CLOEXEC`,
  он остаётся открытым, и новая программа может снова `mmap(MAP_SHARED)` —
  содержимое memfd сохраняется, пока жив fd.
* `CLONE_VM|CLONE_THREAD`: адресное пространство общее, отображения видны
  потокам без дополнительных ссылок.

### 2.4. Ошибки

| errno | Когда |
|---|---|
| `EINVAL` | `len == 0`, оба/ни один из `MAP_PRIVATE`/`MAP_SHARED`, неизвестные биты флагов, невыровненный `offset`, `MAP_SHARED` не на memfd, shared `PROT_NONE`, `mremap` shared |
| `EACCES` | `PROT_WRITE` на fd, открытом только на чтение |
| `EBADF` | нет такого fd / fd не regular-файл |
| `ENOMEM` | регион не размещается ниже пользовательского потолка или PMM исчерпан |
| `EFAULT` | `munmap`/`mprotect`/`mremap` по непокрытому диапазону |

---

## 3. `/dev/fb0`

### 3.1. open и takeover

```c
int fb = open("/dev/fb0", O_RDWR);
struct fb_var_screeninfo var;
ioctl(fb, FBIOGET_VSCREENINFO, &var);
```

* Если Limine-фреймбуфера нет (serial-only boot), `open` возвращает `ENODEV`.
* Первый успешный `open`/`mmap` **забирает экран**: kernel-консольный flush
  прекращается (флаг takeover), чтобы shell не перерисовывал кадр под
  композитором. Serial-вывод (`kprintln!`) продолжает работать.
* Handover односторонний, как у DRM/fbcon: закрытие fd консоль не возвращает.
  Это осознанно; композитор — единственный владелец экрана до перезагрузки.
* in-QEMU selftest возвращает экран через внутренний `release_takeover()`; это
  не userspace-ABI.

### 3.2. ioctl

| Запрос | Значение | Размер |
|---|---|---|
| `FBIOGET_VSCREENINFO` | `0x4600` | `sizeof(fb_var_screeninfo)` = **160** байт |
| `FBIOGET_FSCREENINFO` | `0x4602` | `sizeof(fb_fix_screeninfo)` = **80** байт |

`FBIOPUT_VSCREENINFO`, `FBIOBLANK`, `FBIOGETCMAP`/`FBIOPUTCMAP` не реализованы:
`ENOTTY`. Режим не переключается — композитор берёт тот, что поднял Limine.

**`fb_var_screeninfo`** (little-endian, x86_64; заполняются только перечисленные
поля, остальные нули):

| offset | поле | значение |
|---|---|---|
| 0, 4 | `xres`, `yres` | ширина/высота видимой области |
| 8, 12 | `xres_virtual`, `yres_virtual` | равны `xres`/`yres` |
| 16, 20 | `xoffset`, `yoffset` | 0 |
| 24 | `bits_per_pixel` | 32 (типично) |
| 28 | `grayscale` | 0 |
| 32, 36 | `red.offset`, `red.length` | из Limine mask shift/size (обычно 16/8) |
| 44, 48 | `green.offset`, `green.length` | (обычно 8/8) |
| 56, 60 | `blue.offset`, `blue.length` | (обычно 0/8) |
| 68, 72 | `transp.offset`, `transp.length` | 0/0 (XRGB) |
| 80 | `nonstd` | 0 |
| 84 | `activate` | 0 |
| 88, 92 | `height`, `width` | 0 (мм неизвестны) |
| 96 | `accel_flags` | 0 |
| 100..136 | тайминги/margins/sync/vmode/rotate | 0 |
| 140 | `colorspace` | 0 |
| 144..160 | `reserved[4]` | 0 |

Формат пикселя: 32-битное little-endian слово в памяти — `B,G,R,X` при
`red.offset=16, green.offset=8, blue.offset=0` (типичный QEMU/Bochs scanout).
Композитор обязан читать masks из `var`, а не хардкодить.

**`fb_fix_screeninfo`:**

| offset | поле | значение |
|---|---|---|
| 0..16 | `id` | `"paghfb"` + NUL |
| 16 | `smem_start` | **физический** адрес scanout |
| 24 | `smem_len` | `line_length * height` |
| 28 | `type` | `FB_TYPE_PACKED_PIXELS` (0) |
| 32 | `type_aux` | 0 |
| 36 | `visual` | `FB_VISUAL_TRUECOLOR` (2) |
| 40, 42, 44 | `xpanstep`, `ypanstep`, `ywrapstep` | 0 |
| 48 | `line_length` | stride в байтах (`pitch` Limine) |
| 56 | `mmio_start` | 0 |
| 64, 68, 72 | `mmio_len`, `accel`, `capabilities` | 0 |

`smem_len` может быть не кратен 4096 у нестандартного pitch; `mmap` требует,
чтобы `len` покрывал целые страницы и `offset + len <= smem_len`.

### 3.3. mmap

```c
size_t len = (fix.smem_len + 4095) & ~4095UL;
uint32_t *fb = mmap(NULL, len, PROT_READ | PROT_WRITE,
                    MAP_SHARED, fb_fd, 0);
fb[y * (fix.line_length / 4) + x] = argb;
```

* Только `MAP_SHARED`: `MAP_PRIVATE` на `/dev/fb0` — `EINVAL` (приватная копия
  экрана не имеет смысла).
* `offset` выровнен на 4096; `offset + len <= smem_len`.
* Физические страницы Limine-фреймбуфера отображаются как MMIO; `munmap`/exit/
  `fork` **никогда** не возвращают их в PMM — лист помечен `MMIO_BIT`.
* Тип памяти: при поддержке PAT (`CPUID.1:EDX[16]`) boot программирует
  `IA32_PAT` entry 4/5 = WC, а лист fb0 получает `PAT` (bit 7) **без**
  `NO_CACHE|WRITE_THROUGH` — потоковая запись композитора сливается в
  write-combining-буферы (desktop-render §2 A). Без PAT — ровно прежняя пара
  `NO_CACHE|WRITE_THROUGH` и однократный serial-лог `fb0: PAT missing, uncached`.
* `fork` дублирует трансляцию на тот же scanout (ребёнок рисует в тот же экран,
  как и в Linux с наследованием fd), PTE копируется как есть — WC/MMIO
  сохраняются, COW для scanout не ставится.
* `mprotect` на fb-листе сохраняет MMIO-маркер **и** текущую политику кэша:
  WC не откатывается в uncached сменой прав `PROT_*`.

---

## 4. `/dev/input/event0`

### 4.1. Формат события

Ровно Linux `struct input_event`, 24 байта:

| offset | поле | тип |
|---|---|---|
| 0 | `time.tv_sec` | `int64` |
| 8 | `time.tv_usec` | `int64` |
| 16 | `type` | `uint16` |
| 18 | `code` | `uint16` |
| 20 | `value` | `int32` |

Таймштамп — монотонные тики ядра, переведённые в `timeval`; фиксируется в момент
события, не в момент `read`. `tv_usec < 1_000_000`.

### 4.2. Какие события приходят

| type | code | источник |
|---|---|---|
| `EV_SYN` (0) | `SYN_REPORT` (0) | конец каждого пакета |
| `EV_KEY` (1) | Linux keycodes | PS/2 IRQ1 и USB HID boot-клавиатура |
| `EV_KEY` (1) | `BTN_LEFT` 0x110, `BTN_RIGHT` 0x111, `BTN_MIDDLE` 0x112 | PS/2 и USB HID мышь |
| `EV_REL` (2) | `REL_X` 0, `REL_Y` 1 | PS/2 и USB HID мышь |
| `EV_REL` (2) | `REL_WHEEL` 8 | IntelliMouse/PS/2-колесо |

`EV_ABS`, touch, hotplug, `EV_MSC`, `EV_LED` и автоповтор **не** реализованы.
Автоповтор в Linux делает не evdev, а input core; здесь его нет — приложение
само повторяет действие по зажатому `EV_KEY value=1`, пока не придёт `value=0`.

Перевод PS/2 set-1 в Linux keycodes:

* обычный блок: set-1-код совпадает с Linux keycode (`0x1E` → `KEY_A` 30,
  `0x02` → `KEY_1` 2, `0x3B` → `KEY_F1` 59);
* extended (`E0`): `E0 1D` → `KEY_RIGHTCTRL` 97, `E0 38` → `KEY_RIGHTALT` 100,
  `E0 1C` → `KEY_KPENTER` 96, `E0 35` → `KEY_KPSLASH` 98,
  `E0 47..0x53` → `KEY_HOME` 102 / стрелки 103–109 / `KEY_INSERT` 110 /
  `KEY_DELETE` 111, `E0 5B/5C/5D` → Meta/Compose 125/126/127;
* fake shift PrintScreen (`E0 2A`, `E0 36`), ACK (`0xFA`), overrun (`0x00`) и
  Pause (`E1 1D 45 …`) не превращаются в нажатия.

Мышь: PS/2-дельты в screen-координатах (`dy` вниз), `REL_WHEEL` положителен
вверх. Кнопки сообщаются только при изменении состояния.

### 4.3. read / poll

* `read(fd, buf, count < 24)` → `EINVAL`; иначе возвращается **одно** событие
  (24 байта) за вызов.
* Пустая очередь: блокирующее ожидание; `O_NONBLOCK` (`open(O_NONBLOCK)` или
  `fcntl(F_SETFL, O_NONBLOCK)`) → `EAGAIN`; доставимый сигнал → `EINTR`.
* `poll`/`select`/`ppoll`/`pselect6`/`epoll`: `POLLIN` ровно когда в очереди
  есть событие.
* Очередь общая для всех открытых fd `event0`, кольцевая, 256 событий. При
  переполнении выбрасывается самое старое (свежий ввод важнее истории).
* События **дублируются** из PS/2/USB-потока: чтение `event0` не забирает
  скан-коды у `/dev/stdin`/shell. Если `event0` никто не открыл, shell работает
  как раньше. Когда клавиатурой владеет VT-клиент (`/dev/vtinput`, §5.4.1),
  `event0` продолжает получать физические события, но прямой путь в shell-кольцо
  подавлен — ввод приходит один раз через vtinput.
* Несколько читателей конкурируют за одну очередь (как несколько fd одного
  evdev-устройства); композитору нужен один.

### 4.4. Ошибки

| errno | Когда |
|---|---|
| `EBADF` | fd закрыт/не открыт |
| `EINVAL` | `count < 24`, ioctl не для evdev |
| `EAGAIN` | неблокирующее чтение пустой очереди |
| `EINTR` | ожидание прервано доставимым сигналом |

---

## 5. `/dev/vtconsole0` + `/dev/vtinput` — shared VT surface

Терминальное окно desktop B больше не зеркалит kernel-VT по умолчанию: оно
запускает **собственный shell-потомок** (`pagh-terminal --shell-child`) с pipe'ами
stdin/stdout, поэтому чистое окно не показывает boot-лог, а набор не уходит в
`lxrun`, который держит консоль в форграунде. Старое поведение — снимок видимой
текстовой матрицы + ввод байтами через `/dev/vtinput` — осталось диагностическим
режимом `--mirror-vt`. Полный ABI — в
`rust-apps/pagh-terminal/TERMINAL.md` (§«Kernel console shared surface ABI v1»);
ниже — контракт, который реализует ядро.

| путь | open | назначение |
|---|---|---|
| `/dev/vtconsole0` | `O_RDONLY`, режим 0600 | `mmap`/`read` shared-снимка видимой сетки |
| `/dev/vtinput` | `O_WRONLY`, режим 0600 | VT-байты (UTF-8/escape) → вход shell/VT |

`O_WRONLY` на `vtconsole0` и `O_RDONLY` на `vtinput` — `EACCES`; `write` в
`vtconsole0` и `read` из `vtinput` — `EBADF`. Без фреймбуфера поверхность живёт
сеткой 80×25, а `vtconsole0` при этом остаётся доступной (снимок пуст).

### 5.1. Формат

Заголовок — **128 байт**, клетка — **16 байт**, little-endian; всё в одном
непрерывном буфере `128 + cols*rows*16`. Ключевые смещения заголовка:

| offset | size | поле |
|---|---|---|
| 0 | 4 | magic `0x54564750` (`"PGVT"`) |
| 4 | 2 | version = 1 |
| 6 | 2 | header_len = 128 |
| 8 | 4 | `seq` — seqlock (нечётный = идёт запись) |
| 12 | 4 | flags: bit0 Unicode, bit1 truecolor, bit2 cursor valid |
| 16, 18 | 2+2 | cols, rows (1…1000) |
| 20, 22 | 2+2 | cursor_x, cursor_y |
| 24, 25 | 1+1 | cursor_visible, cursor_style (0 block) |
| 26, 28 | 2+2 | scroll_top, scroll_bot |
| 30, 32, 34 | 2+2+2 | cell_width=8, cell_height=18, cell_size=16 |
| 40 | 4 | cells_offset = 128 |
| 44 | 4 | cells_bytes = cols*rows*16 |
| 56 | 4 | total_bytes |
| 60 | 4 | write_seq — счётчик завершённых публикаций |
| 64..128 | 64 | reserved, нули |

Клетка: `codepoint u32` (0 = пробел), `fg u32` `0x00RRGGBB`, `bg u32`,
`attrs u16` (bold 1, dim 2, italic 4, underline 8, blink 16, reverse 32,
hidden 64, strike 128, overline 256), `width u8` (1/2), `flags u8`
(bit0 = wide continuation). В v1 kernel публикует `attrs=0`, `width=1`:
kernel-VT хранит `u8`, а не Unicode-атрибуты; цвета уже RGB.

### 5.2. mmap и read

```c
int fd = open("/dev/vtconsole0", O_RDONLY);
struct pgvt_info info;
ioctl(fd, PGVT_IOC_INFO, &info);                 /* 0x8020_5601, 32 B */
const uint8_t *surf = mmap(NULL, info.total_bytes, PROT_READ,
                           MAP_SHARED, fd, 0);
```

* `mmap` принимает только `MAP_SHARED` + ровно `PROT_READ`, `offset == 0`,
  `len <= info.total_bytes`. PTE read-only и несёт `VT_RO_BIT`: `mprotect(
  PROT_WRITE)` возвращает `EACCES`, поэтому userspace не может подменить экран.
* Писатель — только ядро. Публикация идёт в живые кадры PMM; отображение
  когерентно, читатель обязан использовать seqlock: взять `seq`, если он
  нечётный — повторить, скопировать заголовок/клетки, проверить, что `seq` не
  изменился и чётен. `write_seq` увеличивается на каждой завершённой
  публикации — по нему удобно замечать «экран изменился».
* `read(fd, buf, total_bytes)` — fallback: ядро копирует целостный снимок под
  тем же локом, что и писатель (проверять seqlock вручную не нужно). Как и
  `read(2)` в Linux, копируется `min(count, total_bytes - смещение)`: клиент
  может прочитать заголовок (128 байт) отдельным вызовом, а клетки — следующим,
  и короткое чтение не обязано упираться в EOF. Страницы `buf` подпираются
  demand paging'ом по мере копирования, поэтому свежий анонимный `mmap`
  принимается; `EFAULT` остаётся только для реально недоступного диапазона (вне
  зарегистрированных регионов, `PROT_NONE`, read-only лист). `lseek` на
  устройствах — `ESPIPE`.
* `PGVT_IOC_INFO`: `{abi_version=1, total_bytes, cols, rows, cell_size=16,
  header_len=128, reserved[2]}`; прочие ioctl — `ENOTTY`.

### 5.3. Жизненный цикл и стоимость

* Первый `open` включает публикацию и сразу отдаёт снимок текущей консоли:
  терминал видит уже нарисованный shell-текст, а не пустую сетку.
* `close` последнего fd выключает публикацию. Пока поверхность не открыта,
  каждый `write_char` платит только один atomic load; boot/shell без desktop
  не деградируют.
* Это **не** takeover экрана: kernel-консоль продолжает рисовать во
  framebuffer/serial как раньше, shared surface — просто снимок. `/dev/fb0`
  takeover (§3) и `/dev/vtconsole0` независимы.
* `fork`/`exec`/`munmap` учитывают кадры поверхности через `VT_RO_BIT`: кадры
  запинены ядром и не освобождаются читателем.

### 5.4. `/dev/vtinput`

`write(2)` принимает VT-байты и переводит их в PS/2 set-1 скан-коды, которые
кладутся в то же кольцо, что и физическая клавиатура; shell обрабатывает их
своим line editor'ом/хоткеями:

* printable ASCII — make+break (Shift make/break вокруг заглавных и символов
  верхнего регистра, US-раскладка);
* `\r`/`\n` — Enter, `\x7f`/`\x08` — Backspace, `\t` — Tab, `Ctrl+A..Z`
  (0x01..0x1A) — Ctrl-аккорд;
* `ESC [ A/B/C/D`, `ESC O A..D`, `ESC [ H/F`, `ESC [ 1~..8~` — стрелки/
  Home/End/Insert/Delete/PageUp/PageDown;
* разорванная CSI-последовательность между `write`'ами сохраняется; одиночный
  `ESC` в конце write — Esc;
* UTF-8 > ASCII в v1 отбрасывается (kernel-VT хранит `u8`, shell-editor
  принимает ASCII) — это задокументированное сужение, а не молчаливая порча.

Если foreground-процесс — Linux-программа (`lxrun`), её stdin читается из
собственной очереди ядра (`STDIN_PENDING`): `/dev/vtinput` отдаёт байты туда
дословно (UTF-8/escape сохраняются, лимит 4 КиБ), а не в shell-редактор. Иначе
(shell prompt) работает set-1-мост выше. Синтетические скан-коды намеренно не
дублируются в `/dev/input/event0`: терминал получает ввод из event0 desktop'а, и
эхо `vtinput → event0` дало бы цикл.

#### 5.4.1. Один владелец клавиатуры — без двойного ввода

Физическая клавиша идёт двумя путями: IRQ1/USB → shell-кольцо (прямо) и
IRQ1/USB → `/dev/input/event0` → композитор → окно Terminal →
`write(/dev/vtinput)`. Пока оба пути активны, shell получает один и тот же байт
дважды (двойное эхо, два элемента истории, повтор команды). Поэтому запись
`/dev/vtinput` из userspace-процесса защёлкивает его pid как **владельца
клавиатуры** (`drivers::vtinput::client_active`): с этого момента `ps2_kbd` отдаёт
физические скан-коды только в `event0` — композитор не ломается, — а в
shell-кольцо они не попадают. Ввод доставляется ровно один раз, через vtinput
(для shell-редактора или `STDIN_PENDING` у `lxrun`). `^C`/`^Z` по-прежнему
отслеживаются драйвером на физическом пути, поэтому SIGINT/SIGTSTP
foreground-группе работают и при активном терминале.

Защёлка самовосстанавливается: shell раз в ~250 мс проверяет
`compat_exists(pid)` и после смерти терминала возвращает прямой PS/2-путь.
Shell-only загрузка (никто не писал в vtinput) не меняется. Названные остатки:
самый первый keystroke сразу после старта терминала может уйти в shell напрямую
(защёлка ещё не выставлена — со второго символа ввод одиночный), а при живом, но
нефокусированном терминале физические клавиши в shell не идут — у desktop-варианта
B shell и так виден только через окно Terminal.

### 5.5. Ограничения VT-поверхности

Одна консоль (`vtconsole0`); scrollback/selection остаются в kernel и не
экспортируются; нет частичных damage-обновлений (клиент сравнивает `seq` и
перерисовывает весь видимый экран); `attrs`/широкие символы в v1 всегда 0/1;
UTF-8-сетка — v2; палитра-override и hyperlinks не публикуются.

## 6. Минимальный цикл композитора (C)

```c
/* Ввод */
int ev = open("/dev/input/event0", O_RDONLY | O_NONBLOCK);
struct pollfd pfd = { .fd = ev, .events = POLLIN };

/* Экран */
int fb = open("/dev/fb0", O_RDWR);
struct fb_var_screeninfo var;
struct fb_fix_screeninfo fix;
ioctl(fb, FBIOGET_VSCREENINFO, &var);
ioctl(fb, FBIOGET_FSCREENINFO, &fix);
size_t fb_len = (fix.smem_len + 4095) & ~4095UL;
uint8_t *screen = mmap(NULL, fb_len, PROT_READ | PROT_WRITE,
                       MAP_SHARED, fb, 0);

/* Клиентский буфер */
int shm = memfd_create("client-7", 0);
ftruncate(shm, w * h * 4);
uint32_t *pixels = mmap(NULL, w * h * 4, PROT_READ | PROT_WRITE,
                        MAP_SHARED, shm, 0);

for (;;) {
    if (poll(&pfd, 1, 16) > 0) {
        struct input_event e;
        while (read(ev, &e, sizeof e) == sizeof e) {
            if (e.type == EV_KEY) { /* клавиатура/кнопки */ }
            if (e.type == EV_REL) { /* указатель */ }
        }
    }
    /* composite pixels -> screen по fix.line_length/4 */
}
```

---

## 7. `SCM_RIGHTS`: передача memfd между клиентом и композитором

`sendmsg`/`recvmsg` для AF_UNIX `SOCK_STREAM` (`socketpair`, `connect`/
`accept`) поддерживают ancillary data `SOL_SOCKET`/`SCM_RIGHTS` с массивом
`int fd`. Это Linux-совместимое подмножество: раскладка структур и наблюдаемое
поведение сверены с Linux x86_64 (см. §7.7), а неподдержанные части названы
ниже явно.

### 6.1. Syscall'ы и флаги

| nr | syscall | args | Флаги, которые принимаются |
|---|---|---|---|
| 46 | `sendmsg` | `fd`, `struct msghdr *`, `flags` | `MSG_DONTWAIT` 0x40, `MSG_EOR` 0x80, `MSG_NOSIGNAL` 0x4000 |
| 47 | `recvmsg` | `fd`, `struct msghdr *`, `flags` | `MSG_DONTWAIT` 0x40, `MSG_CMSG_CLOEXEC` 0x40000000 |

Прочие биты `flags` — `EINVAL` (в частности `MSG_OOB` у `sendmsg` и
`MSG_PEEK`/`MSG_WAITALL` у `recvmsg`: молча вернуть не то было бы хуже отказа).
`fd` — только AF_UNIX socket; AF_INET TCP/UDP поддержаны в data-only виде
(`SCM_RIGHTS` через них — `EINVAL`). Не-сокет — `ENOTSOCK`, непривязанный/
неподключённый AF_UNIX — `ENOTCONN`.

### 6.2. Раскладка (x86_64 Linux)

`struct msghdr` — 56 байт:

| offset | size | поле |
|---|---|---|
| 0 | 8 | `msg_name` (игнорируется: у подключённого stream адреса нет) |
| 8 | 4 | `msg_namelen` (на выходе 0) |
| 16 | 8 | `msg_iov` — `struct iovec[]` |
| 24 | 8 | `msg_iovlen` (≤ `IOV_MAX` 1024) |
| 32 | 8 | `msg_control` |
| 40 | 8 | `msg_controllen` |
| 48 | 4 | `msg_flags` (на выходе — входные флаги \| `MSG_CTRUNC`) |

`struct iovec` — 16 байт (`void *iov_base`, `size_t iov_len`);
`struct cmsghdr` — 16 байт: `size_t cmsg_len`, `int cmsg_level`,
`int cmsg_type`, дальше полезная нагрузка. Следующий cmsg начинается с
`CMSG_ALIGN(cmsg_len) = (cmsg_len + 7) & ~7`; `CMSG_LEN(n) = 16 + n`,
`CMSG_SPACE(n) = CMSG_ALIGN(16 + n)`. Для `SCM_RIGHTS`
(`cmsg_level = SOL_SOCKET = 1`, `cmsg_type = SCM_RIGHTS = 1`) нагрузка —
массив `int fd` (`n = 4 * число fd`), до `SCM_MAX_FD = 253` дескрипторов в
одном cmsg. Несколько `SCM_RIGHTS` cmsg в одном `sendmsg` накапливаются в
порядке появления (как Linux); cmsg чужих уровней (например `IPPROTO_TCP`)
игнорируются, неизвестный тип `SOL_SOCKET` — `EINVAL`.

Пределы этого ядра: данные одного `sendmsg`/`recvmsg` — ≤ 1 МиБ
(`EMSGSIZE`), control-буфер отправки — ≤ 64 КиБ и ≤ `INT_MAX` (`ENOBUFS`).
Первый предел — тот же `MAX_SOCKET_IO`, что ограничивает одну копию у
`sendto`/`recvfrom`; второй ограничивает копию control-буфера в ядро
(законный `SCM_RIGHTS` ≤ `CMSG_SPACE(1012) = 1028` байт). На **потоке**
(`SOCK_STREAM`) `sendto` сверх предела — не ошибка, а короткая запись: Linux
не отвечает `EMSGSIZE` на длинную запись в поток, и `write_all` вызывающего
доносит остаток следующей записью. Протокольные сообщения композитора
(`SHM_OFFER` — десятки байт) много меньше любого из пределов.

### 6.3. Семантика доставки

* **fd дублируются, отправитель не теряет свой.** Получатель получает новый
  номер (наименьший свободный ≥ 3) в **своей** таблице; режим доступа
  (`O_RDONLY`/`O_RDWR`) наследуется. Два fd в одном cmsg приходят двумя
  разными номерами.
* **`FD_CLOEXEC`** ставится только при `MSG_CMSG_CLOEXEC`; `execve` вычищает
  такие дескрипторы (как обычно, по `FdTable::close_cloexec`).
* **Границы потока.** Ancillary data привязаны к первому байту своей записи
  `sendmsg`: `recvmsg` может вернуть данные, отправленные раньше, и данные
  этой записи, но не идёт дальше её конца; следующая запись читается следующим
  вызовом. Два `recvmsg` подряд не могут получить cmsg из чужой записи.
* **Данные + control в одном вызове** — штатный путь: `sendmsg` пишет payload
  и привязывает fd к его первому байту; `recvmsg` возвращает payload и cmsg.
* **`sendmsg` с нулевой длиной данных** возвращает 0 и fd не передаёт (Linux
  для SOCK_STREAM тоже не создаёт запись в потоке).
* **`read`/`readv`** не принимают ancillary data: проход мимо позиции cmsg
  отбрасывает (закрывает) её fd. Если control-буфера нет или он мал,
  `recvmsg` закрывает не поместившиеся fd и выставляет `MSG_CTRUNC`; в
  `msg_controllen` возвращается использованная ёмкость (Linux-правило:
  при частичном размещении — вся ёмкость, при полном — `CMSG_SPACE(cmsg_len)`,
  но не больше ёмкости).
* **Без cmsg поведение не изменилось**: data-only `sendmsg`/`recvmsg` идут по
  тем же очередям, что `write`/`read`; nvim/libuv (msgpack-rpc через
  socketpair) не затронуты.
* **Дубликат — это `dup`**: общий VFS-узел/страницы memfd, но собственное
  смещение чтения/записи (в ядре нет разделяемого open-file-description; для
  memfd это не наблюдаемо — данные ходят через `mmap(MAP_SHARED)`/`pread`).
* **Позиция cmsg считается по одной шкале с обычной записью.** `write(2)` и
  `sendmsg` двигают один и тот же абсолютный счётчик, поэтому fd,
  отправленный `sendmsg` ПОСЛЕ любого `write(2)` в тот же поток, доезжает
  штатно. Обратное (запись мимо счётчика) означало бы `pos = 0` — позади уже
  прочитанного, — и `recvmsg` молча отдал бы данные без cmsg: ровно так
  `SHM_OFFER` терял memfd у реальных клиентов (`hello`/`create` уходят
  `write(2)`), композитор закрывал соединение, а окно оставалось с
  плейсхолдером. Регрессия — in-QEMU routine
  `desktop-b::SCM_RIGHTS after plain write (SHM_OFFER shape)`.
* **Принятый fd существует только в fd-таблице того треда, который сделал
  `recvmsg`.** `clone_current_compat` копирует `FdTable` (отклонение 22 в
  `docs/procfs.md` §13.7: `CLONE_FILES` пока не разделяет таблицу), поэтому
  номер, вернувшийся из `recvmsg` в читающем треде, в главном треде — чужой
  или закрытый. Практическое правило для композитора: отображать memfd
  (`mmap`) и `close` его в ТОМ ЖЕ треде, который принял cmsg, а дальше
  передавать уже отображение (`pagh-desktop` так и делает: `ShmMapping`
  уезжает в главный поток по каналу, а голый номер fd — никогда).

### 6.4. Ошибки

| errno | Когда |
|---|---|
| `EBADF` | нет самого fd; передан неоткрытый/отрицательный fd в массиве; читающий fd закрыт из соседнего потока, пока вызов спал (Linux оставил бы вызов блокированным — здесь ожидание обязано проснуться, иначе watchdog печатает `stuck in syscall ? (nr=45 … closed)`) |
| `ENOTSOCK` | `sendmsg`/`recvmsg` на не-сокете (pipe, файл, консоль) |
| `ENOTCONN` | AF_UNIX ещё не подключён (unbound/listener) |
| `EINVAL` | битый cmsg (`cmsg_len < 16` или за буфером), неизвестный `SOL_SOCKET`-тип, > 253 fd, неподдержанные флаги |
| `ENOBUFS` | `msg_controllen > INT_MAX`; у `sendmsg` ещё и control > 64 КиБ |
| `EMSGSIZE` | сумма iovec > 1 МиБ; `msg_iovlen > 1024` |
| `EAGAIN` | неблокирующий вызов (`O_NONBLOCK`/`MSG_DONTWAIT`) без данных/места |
| `EPIPE` | приёмный конец закрыт до отправки |
| `EMFILE` | в fd-таблице получателя нет места (частичной доставки нет) |
| `EFAULT` | недоступный пользовательский указатель (проверяется и перед записью) |

### 6.5. Ограничения

* `SCM_CREDENTIALS`/`SO_PASSCRED` не поддержаны: такой cmsg — `EINVAL`.
* `MSG_PEEK`/`MSG_WAITALL`/`MSG_TRUNC` не поддержаны (`EINVAL`).
* `sendmsg` с `SCM_RIGHTS` по AF_INET TCP/UDP — `EINVAL` (Linux тоже не
  передаёт fd, но здесь отказ явный, а не тихая потеря).
* `sendmsg` с данными больше `MAX_SOCKET_IO` на stream-сокете отвечает
  `EMSGSIZE`, а не короткой записью (Linux отдал бы частичную запись и
  вернул счётчик). Клиенты композитора отправляют кадры через `sendto`
  (`write_all`), поэтому этот путь не задевают; `EMSGSIZE` здесь — честно
  названный пробел, а не молча потерянные байты.
* Ядро не имеет `unix_gc`: если процесс передаёт конец socketpair самому себе
  и закрывает все обычные ссылки, очередь с cmsg образует цикл Arc и остаётся
  в куче ядра. Это утечка kernel-heap при враждебном сценарии, не PMM-кадров
  и не данных других процессов; для протокола композитора не встречается.

### 6.6. Пример для композитора (C)

Клиент передаёт memfd вместе с заголовком запроса:

```c
struct create_surface { uint32_t w, h, stride, format; };

int shm = memfd_create("surface-7", 0);            /* 319 */
ftruncate(shm, w * h * 4);                          /* 77  */
uint32_t *px = mmap(NULL, w * h * 4, PROT_READ | PROT_WRITE,
                    MAP_SHARED, shm, 0);            /* 9   */

struct create_surface req = { w, h, w * 4, FORMAT_ARGB8888 };
struct msghdr msg = {0};
struct iovec iov = { .iov_base = &req, .iov_len = sizeof req };
char cbuf[CMSG_SPACE(sizeof(int))];
msg.msg_iov = &iov;         msg.msg_iovlen = 1;
msg.msg_control = cbuf;     msg.msg_controllen = sizeof cbuf;
struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
c->cmsg_level = SOL_SOCKET;
c->cmsg_type  = SCM_RIGHTS;
c->cmsg_len   = CMSG_LEN(sizeof(int));
memcpy(CMSG_DATA(c), &shm, sizeof shm);
if (sendmsg(ctl, &msg, 0) != (ssize_t)sizeof req)
        fatal("sendmsg: surface fd not passed");
```

Композитор принимает fd в свою таблицу (он живёт и после закрытия сокета
клиентом):

```c
struct create_surface req;
struct msghdr msg = {0};
struct iovec iov = { .iov_base = &req, .iov_len = sizeof req };
char cbuf[CMSG_SPACE(sizeof(int) * 4)];
msg.msg_iov = &iov;         msg.msg_iovlen = 1;
msg.msg_control = cbuf;     msg.msg_controllen = sizeof cbuf;
ssize_t n = recvmsg(ctl, &msg, MSG_CMSG_CLOEXEC);   /* 47 */
if (n != (ssize_t)sizeof req) fatal("recvmsg");
if (msg.msg_flags & MSG_CTRUNC) fatal("control truncated");

int shm = -1;
for (struct cmsghdr *c = CMSG_FIRSTHDR(&msg); c; c = CMSG_NXTHDR(&msg, c)) {
        if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) {
                memcpy(&shm, CMSG_DATA(c), sizeof shm);
        }
}
if (shm < 0) fatal("no SCM_RIGHTS");
void *px = mmap(NULL, req.stride * req.h, PROT_READ, MAP_SHARED, shm, 0);
/* ... composite ... */
close(shm);
```

`MSG_CMSG_CLOEXEC` здесь обязателен, если композитор потом запускает клиентов:
иначе принятый fd унаследуется `execve`-ребёнком.

### 6.7. Проверка

Linux-поведение снято одноразовым пробником на хосте (socketpair + memfd):
`sizeof(struct msghdr) = 56`, `CMSG_SPACE(sizeof(int)) = 24`, `recvmsg`
останавливается на записи с cmsg, `msg_controllen` возвращается как
использованная ёмкость, при ёмкости `< 20` байт cmsg не пишется и
выставляется `MSG_CTRUNC`, частичная ёмкость доставляет столько fd, сколько
влезло, остальные закрываются, `read(2)` отбрасывает fd, `sendmsg` нулевой
длины fd не передаёт. Эти числа закреплены host-тестами
`src/arch/x86_64/linux/cmsg.rs` (+ свойства `scm_cmsg`) и in-QEMU routine
`desktop-b::SCM_RIGHTS …`; полный список — §8.

---

## 8. Ограничения и остаток

Этот слой даёт **kernel-примитивы**, а не готовый композитор. Чего ещё нет:

* **Wayland/xdg-shell**: первый протокол — собственный AF_UNIX + memfd, не
  Wayland.
* **DMA-BUF / GPU / EGL / GBM**: нет; только программный blit в scanout.
* **Аппаратный курсор, KMS modeset, vt-switch, hotplug**: нет.
* **EV_ABS/touch, EV_LED, автоповтор**: нет.
* **fbdev modeset** (`FBIOPUT_VSCREENINFO`) и панорамирование: нет.
* **PTY/терминал в окне**: `/dev/vtfb1` + `/dev/vtinput1` (§11) дают окну **kernel-shell
  сессию**, то есть настоящий shell, но не userspace PTY: управляющий терминал по-прежнему
  один (§11, ограничения), per-window job control нет, а окно, подключённое к VT-поверхности
  сессии 0 (`/dev/vtconsole0` + `/dev/vtinput`), — read-only зеркало boot-VT. Привязка окна к
  kernel-сессии по умолчанию — PR #147, в `dev` её ещё нет: обычный путь сегодня запускает
  собственный shell-потомок за pipe'ами.

---

## 9. Проверка (evidence)

Host-тесты (`python3 tools/host_tests.py`) исполняют точный исходник ядра:

* `src/drivers/fb_abi.rs` — размеры 160/80 и смещения полей против
  `linux/fb.h` (сверено `gcc -offsetof`), маски/`id`/stride;
* `src/drivers/input_event.rs` — 24-байтный `input_event`, set-1 → Linux
  keycodes, extended-таблица, fake shift/ACK/Pause, `E0` не прилипает;
* `src/drivers/vt_abi.rs` — заголовок 128 байт (смещения magic/version/seq/
  cursor/margins/cells/total), клетка 16 байт, `PGVT_IOC_INFO`, валидатор
  границ и seqlock-паритет;
* `src/drivers/vt_keys.rs` — VT-байты → set-1: printable/Shift/Ctrl, стрелки и
  CSI-split между write'ами, backspace/tab, отброс UTF-8;
* `src/arch/x86_64/linux/cmsg.rs` — 56/16/24 байта `msghdr`/`cmsghdr`/
  `CMSG_SPACE(int)`, разбор control-буфера (`__scm_send`-семантика: битые
  `cmsg_len`, чужие уровни, `SCM_MAX_FD`, слияние нескольких cmsg) и таблица
  доставки/усечения `recvmsg`; host-свойства `properties/scm_cmsg.rs` гоняют
  encode/decode round-trip и инварианты плана доставки на произвольных входах.

In-QEMU (`python3 tools/e2e.py selftest`) гоняет routines:

* `desktop-b::/dev/fb0 ioctl + mmap scanout pixel` — open → takeover → оба
  ioctl (xres/yres/bpp/masks, smem_start/len/stride) → `mmap` → пиксель через
  отображение и через физический scanout → второе отображение → `munmap`;
* `desktop-b::/dev/input/event0 keyboard + mouse read/poll` — `EAGAIN`,
  poll `POLLIN`, `EV_KEY/KEY_A`, `EV_SYN`, `BTN_LEFT`, `REL_X/REL_Y`;
* `desktop-b::memfd ftruncate + MAP_SHARED coherence` — fd↔mapping
  согласованность, два отображения, `mprotect`, отказ `MAP_SHARED` не-memfd;
* `desktop-b::MAP_SHARED memfd survives fork` — общий кадр после
  `fork_user_space_cow`, запись родителя видна ребёнку, `SHARED_BIT` не COW;
* `desktop-b::/dev/vtconsole0 shared VT surface + vtinput` — open → ioctl
  `PGVT_IOC_INFO` → read-only `mmap` → в снимке найден напечатанный маркер shell
  → `mprotect(PROT_WRITE)` = `EACCES`; запись команды в `/dev/vtinput`; после
  `selftest` shell выполняет её (`vtinput-ok` в serial — это проверяет
  `tools/e2e.py selftest`).
* `desktop-b::SCM_RIGHTS memfd fd passing + shared pages` — data+fd одним
  `sendmsg` → `recvmsg` с cmsg → `mmap(MAP_SHARED)` принятого fd видит байты
  отправителя и наоборот, отправитель сохраняет fd, дубликат ссылается на тот
  же узел и наследует режим доступа, два fd в одном cmsg,
  `MSG_CMSG_CLOEXEC` + exec-вычистка (`close_cloexec`), закрытие отправного fd
  не вырывает страницы;
* `desktop-b::SCM_RIGHTS truncation, errors, plain-stream regression` —
  `MSG_CTRUNC` при ёмкости control на один fd (второй закрывается, а не
  устанавливается), приём без control-буфера, `read(2)` отбрасывает fd,
  `EINVAL`/`EBADF`/`ENOBUFS`/`ENOTSOCK`, data-only `sendmsg`/`recvmsg` и
  обычные `write`/`read` на том же socketpair не изменились.

Все проверки выполняются в свежем пользовательском PML4 и возвращают кадры в
PMM. Итог печатает сам прогон — `SELFTEST SUMMARY: N routines, 0 failed checks`; `N`
растёт с каждым PR (routine добавляются вместе с фичами), поэтому здесь нет константы:
сверяйтесь с выводом своей сборки, а не с числом из документа.

---

## 10. `shutdown(2)` (nr 48): половинное закрытие

`close` закрывает обе половины, поэтому «я закончил писать, но ответ ещё
читаю» им не выразить. `shutdown(fd, how)` закрывает одно направление, оставляя
дескриптор и `dup`-дубликаты рабочими для другой половины.

| `how` | Своя сторона | Пир |
|---|---|---|
| `SHUT_WR` (1) | запись → `EPIPE` | читатель видит EOF (`recv` → `0`) |
| `SHUT_RD` (0) | `recv` → `0` | запись в AF_UNIX-очередь → `EPIPE` |
| `SHUT_RDWR` (2) | обе половины | обе половины |

AF_UNIX реализован флагами на очередях направлений (`PipeState::read_shut`/
`write_shut`), а не на номере fd: `dup` и форкнутый ребёнок видят то же
состояние, как в Linux, где `shutdown` действует на сокет. Блокирующий читатель
пира просыпается с `Ok(0)`, блокирующийся писатель — с `EPIPE`; закрытие самого
дескриптора во время ожидания по-прежнему даёт `EBADF` (как у `recvmsg`).

AF_INET TCP `SHUT_WR` заказывает FIN в собственном стеке и **оставляет сокет в
таблице**, поэтому соединение продолжает принимать данные, а `close` доигрывает
close-рукопожатие как раньше. Ограничение названо честно: байты, пришедшие
после `SHUT_RD`, отбрасываются при следующем `recv`, а не асинхронно.

### 10.1. Ошибки и порядок проверок

Порядок — как в Linux (`sockfd_lookup_light` → `inet_shutdown`/`unix_shutdown`);
он закреплён host-свойством `p59` и пробой на живом ядре:

| errno | Когда |
|---|---|
| `EBADF` | дескриптора нет в таблице |
| `ENOTSOCK` | дескриптор не сокет (файл, pipe, консоль, eventfd) — **до** проверки `how` |
| `EINVAL` | `how` не 0/1/2; проверяется **после** дескриптора и вида, но **до** состояния соединения |
| `ENOTCONN` | TCP до `connect`; UDP всегда (`udp_shutdown` не умеет полузакрытия) |

AF_UNIX без соединения (несвязанный, слушающий) — успешный no-op: закрывать
нечего, Linux отвечает `0`. Повторный `shutdown` той же половины идемпотентен.

### 10.2. Проверка

* host-свойство `p59` — таблица отказов и независимость половин;
* in-QEMU `desktop-b::shutdown(2) half-close EOF/EPIPE + errno order` —
  `socketpair`, EOF пира после `SHUT_WR`, `EPIPE` на закрытом направлении,
  вторая половина жива, `dup`, `EINVAL`/`ENOTSOCK`/`EBADF`/`ENOTCONN`;
* пробуждение уже заблокированного вызова проверяется host-политикой: в
  boot-selftest нет второго потока, которому можно сделать `shutdown` во время
  ожидания.

---

## 11. Терминальное окно: `/dev/vtfb1` + `/dev/vtinput1` (kernel-shell сессия)

Второй shell desktop'а реализован на стороне ядра как **настоящая консольная
сессия**, а не имитация: та же команда `shell_main` (полный реестр команд,
`nano+`, `paint`, история, Tab, scrollback), но со своим экземпляром консоли,
своим кольцом ввода и своим `cwd`. Композитору она видна двумя устройствами,
которые дополняют §5 (текстовая сетка `vtconsole0` и ввод boot-консоли):

| Устройство | Режим | Назначение |
|---|---|---|
| `/dev/vtfb1` | `O_RDONLY`, `root:tty 0660` | пиксельная поверхность сессии: `ioctl(VTFBIOGET_INFO = 0x4700)` (24 байта LE `width,height,stride,bpp,generation`), `mmap(PROT_READ, MAP_SHARED, 0)`, `read(2)` = заголовок + пиксели |
| `/dev/vtinput1` | `O_WRONLY`, `root:input 0660` | VT-байты окна → `vt_keys::KeyParser` сессии → её кольцо ввода (физическую клавиатуру не забирает) |

`generation` инкрементируется на каждый flush сессии, поэтому композитор блитит
кадр только при изменении. Первый `open` любого из двух устройств спавнит
задачу `shell-term`, последний `close` — запрос выхода (задача выходит на границе
цикла, без асинхронного kill), повторный `open` поднимает её заново. Пиксели —
кадры PMM, запиненные как у `vtconsole0`; кадры не возвращаются, пока их держит
отображение клиента.

Полный дизайн, ABI, маршрутизация «задача → консоль/cwd/ввод», тесты и честные
границы (нет PTY/termios и per-window job control, два окна делят одну сессию,
глобальная идентичность `su`/`login`, экранные координаты мыши) —
**[`docs/terminal-session.md`](terminal-session.md)**.
