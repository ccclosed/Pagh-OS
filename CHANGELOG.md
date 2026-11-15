# Changelog

Новое сверху, один раздел на версию `Cargo.toml`.

## 2.24.0

Desktop-вариант B: kernel-примитивы под userspace-композитор (`pagh-desktop`).
ABI и ограничения — `docs/desktop-b.md`.

* **`memfd` + `MAP_SHARED`.** Новый тип VFS-узла `vfs::memfd::MemFd`: страницы —
  кадры PMM в карте `page_index -> phys`, файл держит pin-ссылку кэша, каждое
  отображение — свою (`SHARED_BIT` листа). Повторные `mmap` одного fd и
  fork-ребёнок видят одни байты, `read`/`write`/`pread64` согласованы с
  отображением, `munmap`/exit/`mprotect(PROT_NONE)` отпускают только ссылку, а
  кадр возвращается в PMM после последней. `ftruncate`/`truncate` (76/77)
  добавлены в ABI (пустой memfd → размер SHM-буфера); `MAP_SHARED` обычного
  файла — `EINVAL`, `PROT_NONE`/`mremap` shared — `EINVAL`.
* **`/dev/fb0`.** Символьное устройство над Limine-фреймбуфером:
  `FBIOGET_VSCREENINFO` (0x4600)/`FBIOGET_FSCREENINFO` (0x4602) отдают
  минимальные `fb_var_screeninfo` (160 байт) и `fb_fix_screeninfo` (80 байт) с
  реальными xres/yres/bpp/masks/stride/smem_start/smem_len/type/visual;
  `mmap(MAP_SHARED)` отображает физический scanout как MMIO. Первый
  `open`/`mmap` останавливает kernel-консольный flush (takeover; serial-логи
  продолжают), без desktop консоль работает как раньше. fb-листья несут
  MMIO-бит: `munmap`/exit/fork не возвращают экран в PMM. Нет фреймбуфера —
  `open` даёт `ENODEV`.
* **`/dev/input/event0`.** evdev-подобная очередь: `read` отдаёт 24-байтный
  `struct input_event` (timeval, type, code, value); `EV_KEY` из PS/2 и USB HID
  (set-1 → Linux keycodes, extended-таблица), `EV_REL` REL_X/REL_Y/REL_WHEEL из
  PS/2 и USB HID мыши, `EV_SYN`/`SYN_REPORT` на пакет. Blocking read,
  `O_NONBLOCK` → `EAGAIN`, `poll`/`select`/`epoll` → `POLLIN`; кольцо на 256
  событий с вытеснением старого. Ввод дублируется из того же PS/2-потока,
  поэтому shell-stdin и `/dev/stdin` не ломаются, если устройство не открыто.
* **Shared VT surface (`/dev/vtconsole0` + `/dev/vtinput`).** Терминальное окно
  desktop B получает read-only снимок видимой текстовой матрицы kernel-VT/shell:
  128-байтный заголовок (`PGVT`, `seq`-seqlock, cursor/margins/cols/rows,
  `write_seq`) + 16-байтные клетки, `mmap(MAP_SHARED, PROT_READ)` через
  кадры PMM (`VT_RO_BIT`: `mprotect(PROT_WRITE)` = `EACCES`) и `read()`
  fallback; `PGVT_IOC_INFO` (0x80205601). Первый open публикует снимок уже
  нарисованного shell-текста, close выключает публикацию; без клиента консоль
  работает как раньше. `/dev/vtinput` принимает VT-байты и переводит их в PS/2
  set-1 (`printable`/Shift/Ctrl/стрелки/CSI, split между write'ами), чтобы ввод
  терминала дошёл до shell его же line editor'ом; UTF-8 > ASCII в v1
  отбрасывается. ABI — `docs/desktop-b.md` §5 и `TERMINAL.md`.
* **Проверка.** Host-тесты `fb_abi`/`input_event`/`vt_abi`/`vt_keys` сверяют
  размеры/смещения/seqlock/таблицу ввода с ABI; in-QEMU `selftest` получил
  пять routine `desktop-b::*`: fb ioctl/mmap/пиксель (через mapping и
  физический scanout), event0 read/poll (клавиатура + мышь), memfd coherence,
  `MAP_SHARED` после `fork` (общий кадр, не COW) и vtconsole surface + vtinput
  (маркер shell в снимке, read-only mmap, команда через vtinput выполняется
  shell после selftest). Все routine выполняются в свежем пользовательском
  PML4 и возвращают кадры в PMM. `tools/e2e.py selftest` зелёный.
* **SCM_RIGHTS.** `sendmsg`/`recvmsg` (46/47) с `SOL_SOCKET`/`SCM_RIGHTS`
  добавлены поверх 2.24.0 (версия в этом изменении не менялась): клиент
  передаёт композитору memfd, получатель получает dup в свою таблицу, ABI —
  `docs/desktop-b.md` §6.
* **Остаток.** DMA-BUF/GPU/EGL, Wayland/xdg-shell, EV_ABS/touch, fb modeset,
  аппаратный курсор и vt-switch — отдельные будущие PR.

**Как читать этот файл.** Заголовок — это bump версии, не обязательно выпущенная сборка. Git-теги / GitHub-релизы существуют для `2.17.0` (2026-11-10), `2.18.6` (2026-11-10) и бандла `2.18.12-demo` (2026-11-11, демо-медиа снято с 2.18.12); `2.17.1`, `2.17.2` и серия исправлений по реальному железу `2.18.0`–`2.18.12` — это bump'ы версии, чьи заметки записаны здесь и в `AGENTS.md` и входят в следующий тегированный релиз (`2.18.13`, влит как PR #56). Полная техническая история каждого релиза — в release-note блоке `AGENTS.md`; разделы здесь называют видимое пользователю изменение и указывают туда, а не дублируют. Единственная статусная страница по находкам безопасности — `SECURITY.md`.

## 2.23.0

USB-тетеринг: сеть через телефон в режиме RNDIS, без e1000 и без кабеля.

* **Дескрипторы.** `usb_desc.rs` находит пару RNDIS — communication-интерфейс
  (`EF/04/01`, `E0/01/03` или `02/02/FF`) с interrupt-IN и CDC Data (`0A/00/00`) с
  bulk-IN/OUT — и возвращает эндпоинты вместе с `bConfigurationValue`. Разбор
  тотальный: усечённый/лживый дескриптор даёт отказ, а не панику; host-свойства
  гоняют QEMU- и телефонную раскладки, альтернативные настройки и произвольные байты.
* **xHCI.** Control-OUT с data-стадией (SEND_ENCAPSULATED_COMMAND), bulk-кольца и
  контексты (EP type 6/2) в одной Configure Endpoint с HID, bulk-IN поллинг с
  разбором батча конвертов и bulk-OUT отправка. Ошибки восстанавливаются Reset
  Endpoint, как у HID. HID-путь не изменён.
* **RNDIS.** `usb_rndis.rs` выполняет INITIALIZE 1.0, QUERY physical medium и MAC,
  SET packet filter, keepalive каждые ~5 с; RX/TX-очереди ограничены 64 кадрами и
  не делят лок с сетевым стеком.
* **Сеть.** `net::link` выбирает e1000 или USB-RNDIS; `arp`/`ip` шлют через
  абстракцию, а не напрямую в e1000. RNDIS, перечислившийся после загрузки,
  подхватывается сетевым потоком; DHCP работает поверх RNDIS, fallback —
  `192.168.42.21/24`, gw `192.168.42.129`.
* **Проверка.** `tools/e2e.py rndis`: QEMU без e1000, только
  `usb-net` на xHCI; проверяются MAC, DHCP `10.0.2.15`, ping `10.0.2.2` и HTTP GET.
  Реальное железо (Xiaomi 2717:ff88) проверяет пользователь. e1000/HID-пути
  регрессионно не тронуты.

## 2.22.9

Hotfix раунда 4 (main, PR-only): пять x86-находок, каждая воспроизведена на 2.22.6.

* **Read-only user-out → остановка машины навсегда (high).** `check_user_ptr` не требовал
  `WRITABLE`, а `rt_sigprocmask`/`sigaltstack` писали `oldset`/`old_ss` в user-память
  **внутри** `with_current_compat`. Store в present read-only страницу давал ring-0 #PF,
  обработчик отказывал (`compat_lock_held`), а kill-путь делал повторный
  `COMPAT_STATES.lock()` с уже выключенным IF → вечный спин. Добавлен
  `check_user_ptr_writable` (эффективные флаги + листовой `WRITABLE`/fork-COW), все
  `copy_out`/`copy_out_pub`/явные out-write проверяются на запись, значение вычисляется под
  локом, а копируется после выхода. Kill-пути (#PF, #GP, синхронные исключения) отвечают
  `compat_exists_for_exit` без лока и `abandon_held_registry` аварийно отпускают
  удерживаемый реестр перед завершением задачи. PoC `lockpf`/`ro_sigmask`/`ro_altstack`:
  `EFAULT`/SIGSEGV, shell жив.
* **FPU/SSE не изолированы (high).** Инвертированное условие `(rsp as i64) < 0` делало
  `frame_uses_fp` всегда false: FXSAVE/FXRSTOR не исполнялись, XMM0-15/x87 и FCW/MXCSR
  перетекали между процессами. Условие исправлено (канонический верхний half + ring-3 CS
  либо compat-фрейм), сохранение/восстановление реально идут на switch, новый pid
  получает образ сброса (FINIT/`MXCSR=0x1f80`), а `fpu::free` затирает область мёртвого
  pid. PoC `fpu_leak`: канарейки не текут, control mxcsr/cw сброшены.
* **Межпакетный symlink-split установщика (high).** `own_links` строились заново на пакет,
  а резолвер разворачивал pre-existing относительную in-root ссылку: пакет A ставил
  `x/evil -> ../etc`, пакет B писал `x/evil/autorun` → `/mnt/etc/autorun` → root при
  загрузке. Теперь разворачивается только точная документированная провижиненная пара
  `lib64 -> usr/lib64`; любая другая pre-existing ссылка даёт `UnsafePath` до записи, а
  preflight всего пакета (`refuse_preexisting_redirectors`) сохраняет атомарность. PoC
  `craft_r4_deb.py` (r4-plant+r4-write): второй пакет отвергнут, `/mnt/etc/autorun` нет,
  payload не исполняется; normal `.deb` и конструкции r2/r3 по-прежнему отвергаются.
* **`SFMASK` не маскировал AC/DF/NT/IOPL (medium, spec).** Теперь `SFMask` = TF|DF|IF|IOPL|
  AC|NT, как Linux; битсет закреплён в host-тестируемом `abi::SYSCALL_SFMASK` (QEMU #AC не
  моделирует).
* **fd-«cap» обходился `pipe()`/`dup2` (low).** `FD_TABLE_LIMIT` проверялся только в
  `try_alloc` (fcntl): `pipe()` выдавал fd 1024+, а `set` рос до индекса вызывающего.
  `alloc`/`alloc_with_flags`/`pipe`/`socketpair` теперь fallible (`EMFILE`), `dup2`/`dup3`
  на `newfd >= 1024` отвечают `EBADF`, `FdSlots::try_set` ограничен. Host-тесты обновлены.

Гейты: шесть команд, host-tests 561/0, in-QEMU selftest, PoC раунда 4 и регрессия
r1-r3.

## 2.22.8

Перевод комментариев и документации на русский по всему дереву (docs/comments-only,
без изменений поведения). Переведена проза `//!`/`///`/`//` в `src/vfs`, `src/fs`,
`src/arch`, `src/task`, `src/memory`, `src/shell`, `src/net`, `src/pkg`, `src/drivers`
(USB был переведён в 2.22.7), `src/boot.rs`, `src/provision.rs`, `src/test.rs`,
`src/selftest_lx.rs`; корневые README/AGENTS/CHANGELOG/SECURITY/BRANCHING/RELEASING и
`docs/**` — тоже. Удалён task/round/field/photo-нарратив, audit-ID сведены к
`SECURITY.md`, длинные эссе сжаты; `R…`/`P…`/`§`/`REQ`/тест-ссылки, код,
идентификаторы, строки, пути и `SAFETY`-маркеры не тронуты. Добавлен
`COMMENT_STYLE.md`. Host-tests 556/0, USB e2e/selftest и CI зелёные.

## 2.22.7

Follow-up раунда 3 на `dev`: hotfix 2.22.6 закрыл четыре high-находки, эти три x86-находки меньшей серьёзности выходят в следующем релизе; `main` остаётся на 2.22.6.

* **`mprotect` на fork-shared и частично покрытых страницах (low):** `mprotect`-защиты были совещательными на fork-shared и частично покрытых страницах. Новый программный бит листа `RO_BIT` фиксирует, что маппинг запрещает запись, `cow_copy_page` копирует только при записываемом маппинге, а `update_region_flags` делит частично покрытый регион на before/covered/after, поэтому `PROT_NONE` — настоящий fault. Остаток: `PROT_NONE` на `brk`-странице всё ещё переподкрепляется brk-fast-path — задокументированный follow-up.
* **crafted ext2 superblock с невозможными счётчиками inode (low-medium):** crafted ext2 superblock с `s_inodes_count = s_inodes_per_group = 0xFFFFFFFF` заставлял mount сканировать 2^32-битный span и портил `s_free_inodes_count`. `ext2::bounds::validate_group_counts` отвергает невозможный superblock до чтения bitmap; span клампится до одного блока.
* **`execve` и sibling, припаркованный в `STOPPED_TASKS` (high, static):** `execve` мог освободить адресное пространство sibling-задачи, припаркованной SIGSTOP/SIGTSTP. Политика teardown теперь чистая и host-тестируемая (`src/task/as_refs.rs`) и учитывает ready queue, stopped set и pending-reap set.
* **USB HID:** boot protocol, буферы по `wMaxPacketSize`, Reset Endpoint при ошибке передачи, композитные интерфейсы (`Vec` HID-эндпоинтов вместо одного `Option`), report-протокол best effort. Host-свойства `usb_hid_report`/`usb_desc`, in-QEMU selftest, `tools/e2e.py usb` и PS/2-путь зелёные; реального железа не было.
* **Подъём корневого порта xHCI:** neutral-записи PORTSC (`port_state_to_neutral()`), последовательность сброса по `hub.c`, без PP power-cycle; диагностика ограничена warn'ами и командой `usb`. Доказательство — QEMU `tools/e2e.py usb`, `selftest`, host-свойства `xhci_hub`.
* **Замыкание HID transfer-кольца:** Link TRB теперь несёт текущий цикл и toggle-бит; `tools/e2e.py usb` печатает через замыкание и требует роста счётчика клавиатуры.

## 2.22.6

Hotfix раунда 3: четыре high-находки (три x86 machine-kill/lifetime и один обход confinement установщика).

* **Исключения ring 3 вместо `halt()`:** ring-3 `#DE`/`#DB`/`#UD` (и `#OF`/`#BR`/`#NM`/`#AC`) уходили в `halt()`. Теперь они делят политику `trap_frame::exception_action`: fault ring 3 убивает только процесс, fault ring 0 на адресе ядра фатален; `SYSCALL` маскирует TF.
* **Неограниченный `fcntl(F_DUPFD)`:** `fcntl(F_DUPFD)` без cap: `arg >= NOFILE_MAX` теперь `EINVAL` до аллокации, `FdSlots::try_alloc` независимо отказывает с `EMFILE`.
* **Самодеадлок `epoll_wait` поверх signalfd:** `epoll_wait` по signalfd самодеадлочил `COMPAT_STATES`; ветка вычисляет готовность из уже принадлежащего замыканию `CompatState`, `signalfd_readable` debug-assert'ит неудержание реестра.
* **Spelling-split в установщике пакетов:** installer spelling desync: `own_links` хранил архивное написание symlink, резолвер сравнивал пройденное; `attack-final.deb` писал `/mnt/etc/autorun`. Планировщик отвергает пакет целиком (`UnsafePath`) до первого inode; регистрируются оба написания и inode созданной ссылки.
* Гейты: шесть команд, новые host-свойства (`exception_trap`, bounded fd allocation, resolved-spelling detection), in-QEMU `install_spelling_split_confinement`, регрессия PoC раундов 1/2/3.

## 2.22.5

* **Hardlink-алиас symlink в `.deb` (high):** hardlink-алиас symlink обходил confinement `.deb`. Планировщик вычисляет замыкание эквивалентности hardlink/symlink и отвергает алиас с каждым членом под ним (`UnsafeKind::HardlinkAliasesSymlink` → `InstallError::UnsafePath`); эффектный слой не создаёт второе имя архивной symlink и не идёт через symlink с `nlink > 1`. PoC `attack-alias.deb` отвергнут целиком, `/mnt/etc/autorun` отсутствует.
* Попутно исправлена сборка `lx_selftest`: шесть match'ей `RunError` в `src/selftest_lx.rs` не имели ветки для добавленного в 2.22.2 `RunError::AccessDenied`.

## 2.22.4

* **Владение fork-COW кадрами (critical/high):** fork-COW кадры не unref'ились на `munmap`/`mprotect(PROT_NONE)`/`mprotect(RW)`/`MAP_FIXED`/`mremap`; эксплойт давал произвольное чтение/запись физической памяти и uid 0, а exit — 1017 `DOUBLE FREE`. Единая точка `vmm::release_user_leaf` очищает PTE/TLB, потребляет один `cow_unref` и освобождает только на последней ссылке; `mprotect` сохраняет `COW_BIT`, `vmm::map` не перезаписывает живой COW-лист, PMM отказывает кадру с живыми COW-ссылками. Учёт — `memory/cow.rs`, свойство `cow_ownership`; PoC `cow_root`/`cow_min` на исправленном: uid 1000, нет reuse и `DOUBLE FREE`, shell жив.

## 2.22.3

* **TOCTOU пользовательских буферов в блокирующих сисколлах:** TOCTOU на пользовательских буферах в блокирующих сисколлах. Циклы ожидания `poll`/`select`/`epoll_wait` работают на приватных копиях и перепроверяют диапазон после каждого `yield`; тот же паттерн закрыт в read-семействе, iovec `readv`/`writev`, `wait4`, `sendfile`; записи `clone` в `parent_tid`/`child_tid` — внутри окна с выключенными прерываниями; `ioctl(TCSETS*)` уважает валидацию. Page fault режима ядра на пользовательском адресе убивает только процесс. PoC: `tools/security/poc/poc_poll_toctou.c`.
* **Сырой `u64` count в `sendto`/`recvfrom`:** `sendto`/`recvfrom` валидировали урезанную длину, но аллоцировали сырой `u64` count. `abi::socket_payload_len` даёт `EMSGSIZE` до аллокации, `recvfrom` урезает ёмкость до 1 MiB. PoC: `tools/security/poc/poc_sock_large_count.c`.

## 2.22.2

Файловая половина пакета аудита 2026-11-13: crafted ext2-образы и отсутствующие проверки прав обхода пути.

* **Crafted ext2 symlink `i_size`:** crafted ext2 symlink-inode с огромным `i_size` заставлял `read_symlink()` аллоцировать до 4 GiB → panic. Форма цели классифицируется по `i_blocks`/`i_size` до аллокации (fast ≤ 60 байт inline, slow ≤ 4096 байт один блок, иначе `FsError::Corrupt`).
* **Отсутствие search-права при обходе пути:** обход пути не требовал search (`x`) на промежуточных каталогах — uid 1000 читал `0644`-файл в root-owned `0700`-каталоге. Чистый обходчик спрашивает `may_search`, отказ → `EACCES`; `chdir` требует search и на цели.
* **Неограниченное копирование в `rename`:** копирование в `rename` ограничено 64 MiB (`ENOMEM` выше) и резервируется fallible.
* **Fast/slow symlink на `unlink`:** `unlink` решает fast/slow по одному `i_blocks`; `i_blocks == 0, i_size > 60` больше не освобождает текст цели как номер блока.
* **Crafted размер каталога:** размеры каталогов валидируются до lookup/readdir/unlink (закрыт stall на ~1M блоков).
* **Неограниченная аллокация образа в `execve`:** читатель `execve`/`PT_INTERP` ограничен 256 MiB с failible-резервом и проверяет права на exec-пути.

Host-свойства закрепляют чистые правила, ring-3 PoC воспроизводит каждую находку на стоковом 2.21.0 и получает `ENOENT`/`EACCES`/`ENOMEM`/`ENOEXEC` на 2.22.2.

## 2.22.1

* **Symlink-член `.deb` вне корня установки (high):** symlink-член `.deb` с целью вне корня установки (`./y -> /etc`) позволял записать `/mnt/etc/autorun`, исполняемый как root при следующей загрузке. Планировщик отвергает члена или hard-link-цель, чей предок — symlink того же архива; `install_data_tar` отвергает пакет целиком (`UnsafePath`) до создания inode; резолвер родителей стартует от узла корня установки и не идёт через созданные пакетом или абсолютные ссылки. Доказательство: host-свойства `tar_links`, in-QEMU `install_symlink_confinement`, `tests/pkg/e2e_pkg_escape.py`.

## 2.22.0

Изоляция ring 3 по аудиту 2026-11-13: закрыты три пути ring3-to-ring0 и убийство машины одним сисколлом.

* **`mprotect` принимал адреса ядра (critical):** `mprotect` принимал адреса ядра; `mprotect`/`munmap`/`mremap` теперь делят пользовательский гейт диапазона, `PROT_NONE` освобождает только пользовательские кадры, `vmm::map` отвергает `USER_ACCESSIBLE`-маппинг ядра.
* **Поддельный `rt_sigreturn` и `IOPL=3` (critical):** поддельный `rt_sigreturn` ставил `IOPL=3`. Декодер отвергает `ip`/`sp` вне пользовательской половины и маскирует RFLAGS в `USER_RFLAGS_MASK`.
* **`recvfrom` писал через непроверенные указатели:** `recvfrom` писал `sockaddr`/`socklen_t` через непроверенные указатели; оба выхода валидируются `check_user_ptr`.
* **`connect` читал `sa_family` через непроверенный указатель:** `connect` читал `sa_family` через непроверенный указатель и валил машину ring-0 #GP; теперь проверка до чтения.
* Диагностика EINVAL-сисколлов ограничена одной строкой в секунду.

## 2.21.0

Console scrollback: framebuffer-консоль держит ограниченное кольцо ушедших вверх строк (символы и цвет ячеек) и показывает их при прокрутке — колесо PS/2-мыши (IntelliMouse, четырёхбайтные пакеты), PageUp/PageDown по три строки. Новый вывод удерживает те же абсолютные строки, статус-строка печатает `scroll N/M`, обычная клавиша или клик возвращают к живому низу, `clear` забывает историю. USB HID boot-мышь не имеет оси wheel, поэтому прокручивает PageUp/PageDown (`README.md`). Попутно `wallpaper` теперь действительно вызывает `theme::apply`. Доказательство: host-тесты `drivers::scrollback` и `tools/e2e.py scroll`.

## 2.20.0

`/proc/<pid>/maps` перестал печатать `00:00 0` для файлов: сегменты `PT_LOAD` образа и интерпретатора и file-backed `mmap` печатают `st_dev`/`st_ino` и pathname из `stat(2)`/`fdinfo` (`08:00` — это реальный `major:minor` этого ядра), плюс реальное смещение файла у `mmap`. `[heap]`, `[stack]` и анонимные маппинги сохраняют честные `00:00 0` без pathname: у них нет backing-файла. VMA-списка ядро по-прежнему не держит, поэтому карта показывает ровно то, что знает; DEVIATION-7 сужен до этих случаев. Контракт: `docs/procfs.md` §14. Доказательство: in-QEMU `procfs::maps file identity`, `properties/procfs_maps.rs` и фикстура `lx_selftest` `maps_metadata`.

## 2.19.0

Девять новых shell-команд плюс `ls -l` и человеческие единицы `-h` для `free`/`df`. `free` печатает total/used/free физической памяти (счётчики PMM) и фиксированную кучу ядра в busybox-раскладке; `date` — CMOS RTC в UTC; `ls -l` — mode, links, owner/group, размер и цели symlink (без колонки timestamp: у VFS нет mtime); `chmod`/`chown` пишут метаданные ext2, а файловые системы без on-disk метаданных отвечают «operation not supported» по пути; `tree` — рекурсивный ASCII-листинг со счётчиками; `head`/`tail`/`wc`/`grep` — потоковый обход с лимитом 4 MiB. `shared`, `buffers` и `Swap` читаются нулём, потому что в ядре нет учёта shared-памяти, block cache и swap.

Per-thread procfs: `/proc/<pid>/task/` и `/proc/self/task/` листят по одному числовому каталогу на живую задачу thread group; каталог потока намеренно **тот же VFS-объект, что `/proc/<tid>`** (Linux identity: одна задача — один inode). Мёртвый pid, tid чужой thread group и нечисловой компонент — `ENOENT`. Контракт — `docs/procfs.md` §13; доказательство — in-QEMU `procfs::per-thread task directories`, `lx_selftest`-проверки реального `clone` и `properties/procfs_task.rs`.

## 2.18.13

xHCI diagnostics: слово Supported Protocol PORT INFO теперь разбирается по спецификации (PSIC в битах 31:28, count в 15:8, 1-based offset в 7:0). Старый парсер читал count из 31:16, offset из 15:8 + 1, поэтому каждый реальный USB2-порт печатал `proto=usb0`. Только диагностика; политика reset/adopt не менялась.

## 2.18.12

Релиз остановленного контроллера. Полевые отчёты 2.18.11 показывали `usbcmd=0`, `usbsts=0x15`: xHCI **остановлен** ко времени shell, все USB2-порты в Polling с `try=19`. Три реальных бага: DCBAA и кадр scratchpad-array были локальными в `start_controller`, и `DmaFrame::drop` возвращал их в PMM, пока контроллер ими владел; xHCI был единственным DMA-драйвером, не вызывавшим `pci::enable_bus_master`; все USB-ожидания были спин-счётчиками, то есть разной длительностью на разных машинах. Драйвер теперь владеет кадрами, включает bus mastering до BAR, калибрует TSC по PIT channel-2 one-shot и использует реальные дедлайны; `poll()` замечает остановленный контроллер и перезапускает его. Плюс: дождаться порта в mid-detect вместо reset, resume для enabled-but-suspended (PLS=U3), WPR только на SuperSpeed, печать протокольных масок и raw capability.

## 2.18.11

Boot-меню, переставшее считать: `xhci::poll()` обрабатывал все порты за один вызов, а одна попытка reset на железе стоит сотни миллисекунд MMIO-ожиданий, поэтому первый poll блокировал меню на секунды и 20-секундный отсчёт не тикал. Теперь `poll()` инициализирует ровно один порт за вызов round-robin, бюджеты ожидания урезаны до долей секунды, диагностика печатает диапазоны USB2/USB3-портов.

## 2.18.10

Extended capabilities не достигались: обход обрывался на 4 KiB, тогда как полевая машина сообщает HCCPARAMS1 xECP = 32 KiB, поэтому BIOS/USB-legacy handoff никогда не запускался, а Supported Protocol не читались. Лимит обхода — замапленные 64 KiB. Уже включённый контроллером порт усыновляется вместо reset в Polling, PP power-cycle выполняется каждый четвёртый раз, а не каждый (именно он мигал светодиодом мыши).

## 2.18.9

Retry, переживающий settle: неудачная энумерация оставляла xHCI-слот включённым, а poll пропускает порт со слотом, поэтому «дальше всех ушедшие» порты не могли повториться. Драйвер шлёт Disable Slot, очищает DCBAA и освобождает кольца слота через `Drop`, а порты ретраятся вечно с экспоненциальным backoff: PCH USB2 PHY после `HCRST` может успокаиваться секундами.

## 2.18.8

Короткое включение и цена reset: контроллер включает устройство и снова теряет его (`ped_sc` — здоровое high-speed enable, `post=0x6e1` — снова Polling). Исправлены две слепые зоны: Supported Protocol capabilities не читались (`proto=usb0` везде), а обход extended capability получил fallback на область 0x20 при xECP=0. Диагностика: HCIVERSION/HCSPARAMS1/HCCPARAMS1, списки attached-before-reset и now; порты без устройства ретраятся вечно с медленной частотой.

## 2.18.7

Reset, который не держится: одиночное появление PED принималось за успех, затем порт падал обратно в Polling. Теперь требуется стабильное enabled-состояние (PED с link state U0) до адресации; провал стабильности ведёт к warm reset, затем к PP power-cycle с реальными задержками; диагностика дополнена поколением USB-протокола и снимком PORTSC в первое появление PED.

## 2.18.6 — released 2026-11-10

Порт-reset не даёт эффекта, а QEMU не отличает «контроллер игнорирует записи» от «драйвер читает не тот регистр». Релиз выводит неоднозначность на экран: скан контроллеров (оба PCI-контроллера, число портов, attached-маски, какой используется), raw-дамп четырёх регистров выбранного порта, USBCMD/USBSTS/CONFIG и CAPLENGTH, а также write-read-back тест безвредного регистра, показывающий, доходит ли вообще хоть одна запись драйвера.

## 2.18.5

Сигнал port reset: диагностика 2.18.4 вернула `stage=address code=0x4` и устойчивое `ccs=1 ped=0 pls=7` — устройства есть, порты никогда не ENABLED, Address Device ушёл устройству, не покидавшему Polling. Драйвер считал port-reset-change (PRC) признаком «reset завершён», но на реальном железе незавершённый reset всё равно поднимает PRC при PED=0. Теперь bring-up ждёт PED и только затем читает скорость и адресует устройство; hot reset без enable ретраится warm reset'ом, затем PP power-cycle; строка отказа несёт снимки PORTSC до/после.

## 2.18.4

Полевой тест ответил на счётчик 2.18.3 своими числами («2 контроллера, 26 портов, устройства на шести») и **0 HID**: контроллер видел клавиатуру, а энумерация останавливалась там, где старая сборка не могла это назвать. Загрузка печатает по строке на порт с достигнутой стадией энумерации, баннер shell несёт последний отказ, команда `usb` — то же плюс PORTSC. Три исправления: max packet size EP0 обновляется из device descriptor (full-speed стартует с 8, иначе 18-байтное чтение падает); USB3-порты с suspended link получают warm reset; неудачная энумерация ретраится с растущим интервалом.

## 2.18.3

Первый полевой отчёт после disk-меню: на реальной машине часы шли медленно, USB-клавиатура и мышь мертвы. У часов одна причина — вызов калибровки LAPIC остался отключён при отладке BIOS-стека. Для ввода драйвер находит клавиатуру на любом xHCI-контроллере (пробует все, считает attached-порты, стартует лучший), забирает владение у firmware USB legacy и ретраит порты, ещё не давшие устройства, вместо отказа после первого взгляда: USB2-connect требует ~100 мс debounce, которые мгновенный ответ QEMU скрывал. Устройство за хабом детектируется и называется вслух (хабы всё ещё не поддерживаются); boot-меню и баннер shell печатают строку — контроллеры, порты, HID, report'ы.

## 2.18.2

USB-ввод, работающий в QEMU: путь энумерации никогда не исполнялся из-за четырёх дефектов — регистры портов xHCI адресовались с шагом 4 байта вместо 16 («порт 5» был PORTSC порта 2), Address Device получал HDM-виртуальный адрес input context вместо физического, SET_CONFIGURATION не отправлялся (HID-эндпоинт оставался stalled), а контекст эндпоинта клал CErr/MaxPacket не в тот dword с сырым `bInterval`, из-за чего interrupt-IN конфигурировался как ISO OUT с нулевым пакетом. После исправлений эмулированная USB-клавиатура энумерируется, отчитывается и печатает в shell и boot-меню, включая стрелки/Enter/Esc.

## 2.18.1

Disk-меню и правило владения: загрузка показывает меню видимых разделов (рисуется прямо в framebuffer до появления shell) с содержимым — собственный том pagh, пустая область, `FOREIGN`-файловая система или неизвестные данные. `Enter` монтирует, `F` форматирует только после ввода имени устройства обратно, `Esc` пропускает, 20-секундный таймаут выбирает неразрушающий вариант. Том считается своим, только если несёт WAL-superblock pagh (`Ext2Fs::is_pagh_volume`), поэтому ext3/ext4 отвергаются по feature-словам, чужой ext2 не трогается, а автоматический путь может форматировать только RAM-диск с загрузочного носителя. Попутно: магия superblock ext-семейства проверялась по 1024 вместо 1024+0x38; загрузочный путь получил собственный стек 128 KiB (стека Limine на BIOS-загрузке мало); меню — ASCII (шрифт ядра не содержит кириллицы).

## 2.18.0

Первые исправления по реальной машине, дошедшей до shell: LAPIC-таймер **измеряется** против PIT при загрузке вместо предположения о шине QEMU 1 ГГц (на железе это растягивало тик ~в 60 раз, и часы shell шли секундой в минуту); `drivers::partition` читает GPT (и MBR) и публикует каждый раздел отдельным блочным устройством, чтобы pagh мог владеть одним разделом, а не всем диском; `logfile` зеркалит каждую строку лога ядра в `/mnt/pagh-log.txt` — буферизуется в фиксированном кольце с первого сообщения загрузки и выливается потоком `logd` после монтирования `/mnt`, поэтому машина без serial оставляет читаемый лог на диске.

## 2.17.2

Реальная причина полевого отказа 2.17.1: `#GP` был в `vmm::map`, а адреса LAPIC от firmware корректны — обходчик таблиц страниц принимал **huge page** за таблицу страниц. Limine мапит HHDM страницами 2 MiB (иногда 1 GiB), поэтому обход в окно LAPIC находил present-запись с HUGE_PAGE и «адресом кадра», указывающим на начало крупной области RAM, а не на 512 записей; индексация давала неканонический указатель — general-protection fault, а не page fault. `ensure_next` теперь учитывает уровень записи и расщепляет huge-запись в реальную таблицу (2 MiB → 512 × 4 KiB, 1 GiB → 512 × 2 MiB, флаги сохраняются), а `map_mmio` отвергает неправдоподобные физические базы.

## 2.17.1

Первый полевой отчёт с реального железа. Аварийная консоль назвала шаг (`init_apic`) и поймала `#GP` внутри обхода таблиц страниц LAPIC: general-protection fault, а не page fault, значит виртуальный адрес из базы MADT был неканоническим — firmware сообщил мусорный адрес LAPIC. `vmm::map_mmio` теперь отвергает неправдоподобную физическую базу с сообщением вместо fault внутри обходчика; `apic::init` печатает полученные адреса и откатывается на архитектурный `0xFEE00000` вместо panic; handover с x2APIC детектируется и переключается обратно на xAPIC; загрузка объявляет каждый шаг *до* его выполнения, чтобы зависший шаг называл себя.

## 2.17.0 — released 2026-11-10

Пакет, который заставил pagh работать на реальном железе, плюс три давних дефекта отображения/сети: пир, отвечающий и закрывающийся одним burst'ом, теперь завершает `connect(2)` — тест завершения `ever_established` (рукопожатие состоялось), а не `state == Established`, окно, которое reply+FIN проскакивает (из-за этого clipboard bridge работал один раз и падал); VT-эмулятор и текстовая консоль делят одну систему координат и передают курсор в обе стороны внутри `without_interrupts` спавна, поэтому `lxrun` больше не заливает экран чёрным и не уводит prompt вниз; `ps` листит потоки ядра (`[shell]`, `[net]`, `[usb]`, ...), а `df` сообщает байты, которые держит ramfs. Загрузка показывает disk-меню (`src/boot_menu.rs`), USB-ввод идёт через `drivers::xhci` + `drivers::usb_hid` (подъём контроллера и трансляция отчётов проверены; энумерация устройств ещё нет), `drivers::earlyfb` даёт машине без serial видимый boot-trace, а `tools/mkimage.py` собирает оба реально загружаемых артефакта — GPT+ESP USB-образ и BIOS+UEFI ISO, каждый с `disk.img`.

## 2.16.1 — 2026-11-09

Исправления, найденные прогоном реальных apt-бинарников (fastfetch) и persistent-диска:

- `mprotect` больше не возвращает ENOMEM для диапазона, чьи страницы резидентны, но не отслежены как mmap/brk-регионы (загрузчик ELF мапит `PT_LOAD` без записи в `VmRegionSet`); ld.so glibc сообщал это как «cannot apply additional memory protection after relocation», fastfetch теперь стартует.
- COW-write-fault обслуживается для каждой страницы с `COW_BIT`, а не только внутри отслеживаемого writable VMA или brk-кучи: стек пользователя, стеки потоков и fork-shared страницы кучи — настоящие writable-маппинги, и промах учёта убивал и родителя, и ребёнка сразу после `fork`.
- Boot fs demo усекает `bootdemo.txt` перед перезаписью, поэтому более короткая демо-строка на persistent-диске не оставляет хвост прошлого прогона и ложный MISMATCH.

Live: `lxrun /mnt/usr/bin/fastfetch` печатает Kernel/Uptime/Terminal/CPU/Memory/Disk и чисто выходит; fs demo снова PASS; 414 host-тестов и e2e selftest 82/83 зелёные.

## 2.16.0 — 2026-11-09

Пакетный релиз: Linux compat, срез 5 procfs, job control и переписанный NVMe.

### Linux compat
- `signalfd`/`signalfd4`: читаемый fd над ожидающими сигналами, `POLLIN` в poll/select/epoll.
- Настоящие process groups: `pgid` в `CompatState`, `setpgid`/`getpgid`/`getpgrp`/`setsid`, групповой `kill`.
- `memfd_create`, `timerfd_create/settime/gettime` (интеграция с read/poll/epoll).
- Широкие батчи сисколлов: семейство `*at`, `sendfile`, `copy_file_range`, legacy/advisory-вызовы, `setrlimit`/`getrusage`/`times`.

### Job control
- Ctrl+Z останавливает foreground process group (SIGTSTP); таблица jobs в shell с `jobs`/`fg`/`bg` и возобновлением по SIGCONT.
- Управляющий терминал ядра ведёт foreground pgid, `TIOCGPGRP`/`TIOCSPGRP`, SIGTTIN/SIGTTOU на фоновом tty I/O; `lxrun` передаёт терминал job'у и восстанавливает.

### procfs, срез 5
- `/proc/<pid>/fdinfo/<n>` (реальные `pos`/`flags`/`mnt_id`/`ino`) и `/proc/<pid>/limits` (16 строк Linux rlimit), плюс `self`-варианты.

### NVMe
- Многостраничные PRP-списки, до 64 KiB на команду, четыре команды в полёте, сбор завершений по CID и bring-up/протокольные исправления, сделавшие QEMU NVMe пригодным (смещение Identify NN, поля Create-IOCQ/IOSQ, opcode Write/Flush).

Гейты: 414 host-тестов, e2e selftest 83 рутины / 0 отказов, все AGENTS-гейты зелёные.

## 2.15.0 — 2026-11-08

Пакетный релиз: 15 коммитов, сфокусирован на слое совместимости с Linux и двух GUI-исправлениях.

### Linux compat
- `faccessat`/`fchmodat`, `mkdirat`/`unlinkat`/`fchownat` — современное семейство `*at` (dirfd-модель с AT_FDCWD; `AT_EACCESS`/`AT_SYMLINK_NOFOLLOW`).
- `sendfile` (regular-file → файл/консоль/pipe/socket) и `copy_file_range` между regular-файлами.
- Legacy/auxiliary-номера: `eventfd`, `epoll_create`, `getcpu`, `sched_getaffinity`/`sched_setaffinity`, `fadvise64`.
- `setrlimit`, `getrusage` (честные обнулённые поля), `times`.
- Широкий advisory/runtime-батч: `faccessat2`, `readahead`, `sync`, `syncfs`, `getpriority`, `setpriority`, `personality`, `sched_getparam`, `sched_setscheduler`, `sched_getscheduler`, `sched_get_priority_{max,min}`, `sched_rr_get_interval`, `rt_sigpending`, `sync_file_range`, `fallocate` (mode 0).
- `memfd_create`: анонимный RAM-backed fd (`MFD_CLOEXEC`, `MFD_ALLOW_SEALING` принимаются; seal-сисколлов нет).
- `timerfd_create`/`settime`/`gettime`, интегрированные с read/poll/select/epoll.
- SIGCHLD ставится родителю в очередь на exit/stop/continue ребёнка (process groups job control и `^Z` tty-стопы остаются открытыми).

### procfs / shell
- `/proc/self/mountinfo` и `/proc/<pid>/mountinfo` с порядком полей Linux.
- Shell `df` (реальная ёмкость ext2, `-` для синтетических файловых систем) и `ps` (PID/PPID/STAT/COMMAND из `/proc/<pid>`).
- `/proc/self/cgroup` + `/proc/<pid>/cgroup` (`0::/`, без контроллеров).

### GUI
- Выделение мышью перерисовывается инкрементально: большой drag больше не перерисовывает весь диапазон на каждое событие мыши (исправлено падение FPS при выделении текста).

### Tooling
- Удалён неиспользуемый хелпер `pagh_mcp.py`.
- Каденция релизов — 15 коммитов на тег; версия = изменение с наибольшим влиянием.