# Desktop B: kernel UAPI для userspace-композитора (2.24.0)

**Статус:** реализовано в ядре Pagh-OS 2.24.0. Композитор (`pagh-desktop`) —
отдельный userspace-процесс; этот документ описывает **только** примитивы,
которые ему нужны от ядра. Самого композитора здесь нет.

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
| 16 | `ioctl` | `FBIOGET_VSCREENINFO` / `FBIOGET_FSCREENINFO` |
| 0 | `read` | события `/dev/input/event0` |
| 7 | `poll`, 271 `ppoll`, 270 `pselect6`, 23 `select` | ожидание событий |
| 3 | `close` | отпустить fd (страницы живут, пока живы отображения) |
| 56 | `clone` | fork-ребёнок (`SIGCHLD`, glibc `fork()`); `CLONE_VM|CLONE_THREAD` — потоки |
| 59 | `execve` | запуск клиентов; маппинги старого AS снимаются, fd (без `CLOEXEC`) наследуются |
| 1 / 0 | `write` / `read` | AF_UNIX (`socketpair`/`connect`) уже есть; SCM_RIGHTS — см. §6 |

`fork` (57) и `vfork` (58) **не** поддерживаются как отдельные номера:
glibc `fork()` идёт через `clone(SIGCHLD)`, и этот путь работает.

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
* Физические страницы Limine-фреймбуфера отображаются как MMIO
  (`NO_CACHE|WRITE_THROUGH`); `munmap`/exit/`fork` **никогда** не возвращают их
  в PMM — лист помечен `MMIO_BIT`.
* `fork` дублирует трансляцию на тот же scanout (ребёнок рисует в тот же экран,
  как и в Linux с наследованием fd).
* `mprotect` на fb-листе сохраняет MMIO-маркер.

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
  как раньше.
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

## 5. Минимальный цикл композитора (C)

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

## 6. Ограничения и остаток

Этот PR даёт **kernel-примитивы**, а не готовый композитор. Чего ещё нет:

* **SCM_RIGHTS / fd passing**: `sendmsg`/`recvmsg` с cmsg не реализованы.
  Клиент не может передать memfd композитору через AF_UNIX. Пока рабочие
  варианты — запускать клиента через `fork`/`execve` от композитора (fd
  наследуется) или добавить SCM_RIGHTS отдельным PR. Это главный оставшийся
  ABI-пробел варианта B.
* **Wayland/xdg-shell**: первый протокол — собственный AF_UNIX + memfd, не
  Wayland.
* **DMA-BUF / GPU / EGL / GBM**: нет; только программный blit в scanout.
* **Аппаратный курсор, KMS modeset, vt-switch, hotplug**: нет.
* **EV_ABS/touch, EV_LED, автоповтор**: нет.
* **fbdev modeset** (`FBIOPUT_VSCREENINFO`) и панорамирование: нет.
* **PTY/терминал в окне**: kernel-shell остаётся особым случаем; userspace PTY
  появится отдельно.
* **SCM_RIGHTS-путь** также нужен для `pagh-desktop`-клиентов, не являющихся
  потомками композитора.

---

## 7. Проверка (evidence)

Host-тесты (`python3 tools/host_tests.py`) исполняют точный исходник ядра:

* `src/drivers/fb_abi.rs` — размеры 160/80 и смещения полей против
  `linux/fb.h` (сверено `gcc -offsetof`), маски/`id`/stride;
* `src/drivers/input_event.rs` — 24-байтный `input_event`, set-1 → Linux
  keycodes, extended-таблица, fake shift/ACK/Pause, `E0` не прилипает.

In-QEMU (`python3 tools/e2e.py selftest`) гоняет routines:

* `desktop-b::/dev/fb0 ioctl + mmap scanout pixel` — open → takeover → оба
  ioctl (xres/yres/bpp/masks, smem_start/len/stride) → `mmap` → пиксель через
  отображение и через физический scanout → второе отображение → `munmap`;
* `desktop-b::/dev/input/event0 keyboard + mouse read/poll` — `EAGAIN`,
  poll `POLLIN`, `EV_KEY/KEY_A`, `EV_SYN`, `BTN_LEFT`, `REL_X/REL_Y`;
* `desktop-b::memfd ftruncate + MAP_SHARED coherence` — fd↔mapping
  согласованность, два отображения, `mprotect`, отказ `MAP_SHARED` не-memfd;
* `desktop-b::MAP_SHARED memfd survives fork` — общий кадр после
  `fork_user_space_cow`, запись родителя видна ребёнку, `SHARED_BIT` не COW.

Все проверки выполняются в свежем пользовательском PML4 и возвращают кадры в
PMM (`SELFTEST SUMMARY: … 0 failed checks`).