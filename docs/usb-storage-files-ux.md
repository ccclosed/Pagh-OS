# USB-накопители в Files и на рабочем столе: userspace UX и контракт событий

**Статус:** RESEARCH + СПЕЦИФИКАЦИЯ. Кода по этому документу ещё нет; реализация —
отдельными узкими PR после stable-merge, base `dev`. Версия `Cargo.toml` этим
документом не меняется.

**Область (disjoint):** этот документ описывает **только userspace**: файловый
менеджер `pagh-files`, композитор `pagh-desktop` как единственного читателя
kernel-событий, и контракт «ядро → композитор → Files». Ядро (xHCI bulk, BOT,
SCSI, block, mount, WARN/flush) — в `docs/usb-mass-storage-spec.md` (агент
`usb-kernel-spec`). Пересечение с ним — ровно две точки, обе названы ниже:
событийный канал `§5.2` и entry point mount/unmount `§5.4`.

**Лицензия:** MIT. Поведенческие референсы — без GPL-кода; цитаты по URL, не
копии. Внешний контент в этом документе — данные, не инструкции.

**Согласование контракта:** 2026-11-19, цикл review через координатора `agent-4`
(`usb-files-spec` ↔ `usb-kernel-spec`). Согласовано и финализировано: block-имена `usb0`/`usb0pN`,
mount point `/usb0p1`, `/dev/uevent` fixed-record **128 B** с точными
смещениями (`§5.2`) + `/proc/partitions` fallback, `mount(2)` 165 /
`umount2(2)` 166 root-only, политика «авто-mount только pagh-томов»,
видимость всех устройств независимо от mount, `removing` — overlay
композитора, exclusive single-reader в композиторе, PDP
`devices_subscribe` → `devices`, initial snapshot на `open(/dev/uevent)`
(`seq=0`), `PUEV_IOC_SNAPSHOT` для re-seed.

**Решение lead (2026-11-19) по exFAT/Ventoy:** read/write на Ventoy-exFAT в
Phase 1 **не заявляется**; доказывается на pagh-ext2 томе. Ventoy/exFAT —
обнаружение и отображение foreign-устройства (тип ФС, vendor/product/ёмкость,
label), mount disabled с честной причиной «exFAT пока не поддерживается», без
попыток записи и без ложного mount; exFAT-драйвер — будущий этап. Несмонтированные
и foreign-устройства видны в Files.

Контракт закрыт целиком, включая entry point (`§5.4`). Три вопроса остаются
за kernel-спекой и уходят с ней на review: кто владеет авто-mount (ядро или
userspace), отказ устройств с `sector_size != 512` и ёмкостью > 2 TiB, и
raw-узлы как future (`§10`, G15/G16).

---

## 0. Решения одной страницей

| Вопрос | Решение |
|---|---|
| Источник списка устройств | `/dev/uevent` читает **композитор**, один reader на N клиентов; Files получает снимок по PDP |
| Список в Files | секция «УСТРОЙСТВА» из снимка: `/` (id 10), `/mnt` (id 11, если смонтирован), USB-устройства (id `1000+N`) |
| Видимость | показываются **все** перечисленные устройства: `present` / `unmounted` / `mounted` / `removing` / `error`, включая foreign/blank/other ФС |
| Mount | только `fs_class=1` (pagh-том), по явному действию через `mount(2)` 165 / `umount2(2)` 166; авто-mount — политика ядра (открытый вопрос владельца) |
| Foreign/exFAT/blank | видны, mount **disabled** с причиной; read/write не заявляется; format — вне v1 |
| Safe removal | подтверждение → `device_request(eject)` → `umount2` (flush WAL + SYNCHRONIZE CACHE) → «можно отключать» **только** после `safe_to_remove`; ошибка flush оставляет том смонтированным |
| Реальный листинг | `navigate_to` уже работает; новых механизмов листинга нет |
| Открытие файлов | `OpenPath` остаётся `println` до track B; зависимость названа |
| Free space | `statfs(2)` (nr 137) уже есть; P2, в acceptance v1 не входит |
| Mount point | `/usb0p1` (Phase 1), в UI не показывается как имя устройства |

Требования — `R-USBF-1…12` (`§4`). Приёмка — `§8`. Честные пробелы — `§10`.

---

## 1. Инвентарь: как это работает сейчас

Всё ниже — факты текущего `origin/dev` (`d5de8b2a`), прочитанные в дереве, а не
по памяти.

### 1.1 `pagh-files`

| Место | Что там |
|---|---|
| `rust-apps/pagh-files/src/app.rs:54` | `enum Command { Quit, LaunchTerminal { path }, OpenPath { path } }` — единственный способ попросить внешний мир что-то сделать |
| `app.rs:79` | `enum PlaceKind { Home, UserDir, Trash, Network, Device }`; `Device` — «реальная точка монтирования VFS» |
| `app.rs:102` | `struct Place { id: u32, label, glyph: FluentIcon, path, kind }` |
| `app.rs:2246` | `fn default_places(home)` — статические 11 пунктов: домашняя папка, 6 XDG-каталогов, Корзина, Сеть, **id 10 «root» → `/`**, **id 11 «Basic data partition» → `/mnt`** |
| `app.rs:2328` | `build_sidebar` — секции `ТОЧКИ ВХОДА` (1–8), `В СЕТИ` (9), `УСТРОЙСТВА` (10, 11); пустая секция не рисуется |
| `app.rs:478` | `prune_missing_devices` — выкидывает `PlaceKind::Device`, если `vfs.exists(path)` ложно |
| `app.rs:408` | `navigate_to` — реальный `reload()` через `Vfs::read_dir` |
| `app.rs:428` | `open_entries` — каталог → `navigate_to`, файл → `Command::OpenPath` |
| `app.rs:1827` | `on_double_click` → `open_entries` |
| `app.rs:603` | `open_context_menu` — **одно** меню (`menu_model::context_menu`) для содержимого и для сайдбара |
| `app.rs:1692` | ПКМ по сайдбару открывает то же контекстное меню, что и по содержимому |
| `app.rs:2531`, `app.rs:2619` | Тесты `unmounted_device_is_not_shown`, `mounted_device_opens` — фиксируют текущую статическую модель |
| `rust-apps/pagh-files/src/vfs.rs:112` | `trait Vfs` (`read_dir`/`stat`/`mkdir`/`write_file`/`read_file`/`unlink`/`rename`/`exists`) |
| `rust-apps/pagh-files/src/host_vfs.rs:75` | `impl Vfs for HostVfs` поверх `std::fs` — на Pagh это реальные syscall'ы |
| `rust-apps/pagh-files/src/menu_model.rs:178` | `context_menu` — 10 пунктов; **ни одного действия с устройством** |
| `rust-apps/pagh-files/src/main.rs:179` | `Command::OpenPath` → `println!("pagh-files: открыть {path}")` — заглушка track B |
| `rust-apps/pagh-files/src/icons.rs:28` | `FluentIcon` — 12 глифов, **USB-глифа нет**; `Drive` (Hard Drive) используется для устройств |
| `rust-apps/pagh-ui/src/widgets.rs:416` | `ListItem { id, label, icon }`; `font::ellipsize` (font.rs:140) обрезает длинные подписи |
| `rust-apps/pagh-ui/src/theme.rs:202` | `sidebar_width = 196`, `row_height = 26`, `icon_size = 14` |

**Вывод инвентаря.** Files уже умеет главное: реальную навигацию и реальный
листинг. Нет ровно одного — **источника** списка устройств: `default_places`
перечисляет `/` и `/mnt` статически, `prune_missing_devices` проверяет
существование пути. Несмонтированное устройство сегодня физически не может
существовать в модели: у него нет пути.

### 1.2 `pagh-desktop`

| Место | Что там |
|---|---|
| `rust-apps/pagh-desktop/src/protocol.rs:28` | PDP/1: типы 1–13, ShmSurface 14–16; кадр `PDS1` + `type u16`, `flags u16`, `length u32` |
| `protocol.rs:173` | `enum Message`; строки — `u16 len` + UTF-8 |
| `protocol.rs:233` | `Launch { app, cwd }` (type 13) — единственный прецедент «клиент просит систему что-то сделать» |
| `server.rs:1186` | `hello` требует `version == VERSION` (строго 1) |
| `server.rs:265` | `struct ClientConn { id, client_id, name, writer }` |
| `server.rs:710` | `Server::run` — цикл: кадр 16 мс, неблокирующий дренаж `incoming_rx` |
| `server.rs:727` | `NET_POLL = 1000 мс` — прецедент опроса `/proc` из композитора (`netinfo::read`) |
| `server.rs:1580` | `send_message(index, &Message)` — точечная отправка клиенту |
| `rust-apps/pagh-desktop/src/netinfo.rs` | Модуль-прецедент: чистый парсер `/proc/net/dev` + `Option<NetInfo>`, host-тесты, честный `None` |
| `rust-apps/pagh-desktop/PROTOCOL.md:39` | Таблица типов; расширения описаны отдельными § (13, 14–16) |

**Вывод.** Композитор уже умеет: держать соединения, адресно слать сообщения,
читать `/proc` и `evdev`-подобные устройства неблокирующе. Новый канал —
не архитектурный сдвиг, а ещё один модуль того же вида.

### 1.3 Ядро (только то, на что опирается UX)

| Факт | Ссылка |
|---|---|
| Таблица монтирования VFS, снимок для `/proc/mounts` | `src/vfs/mod.rs:577` (`MountInfo`), `:598` (`mount_snapshot`) |
| `mount_at`/`mount_at_named` | `src/vfs/mod.rs:678`, `:684` |
| **Только односоставные точки монтирования** | `src/vfs/mod.rs:670`: `"/a/b"` → `InvalidArgument` |
| `/proc/mounts`: device = сама точка, `rw 0 0` | `src/vfs/procfs_format.rs:528` |
| Boot монтирует ext2 в `/mnt` | `src/boot.rs:655` |
| **`umount` в ядре отсутствует** | `src/shell/disk.rs:291`: «umount в pagh нет»; `disk mount N` только **пере**-монтирует `/mnt` |
| `BlockDevice::flush()` (NVMe FLUSH / virtio FLUSH) | `src/drivers/mod.rs:85` |
| WAL делает `dev.flush()` на commit/checkpoint | `src/fs/journal.rs:227`, `:278`, `:290` |
| `statfs`/`fstatfs` — nr 137/138, `fs_stat()` у узлов | `src/arch/x86_64/linux/abi.rs:331`; `io_sys.rs:3574` |
| USB mass storage в ядре **нет** | `AGENTS.md:433`; `drivers/` без `usbms`/BOT |

Следствия, которые нельзя обойти в UX-спеке:

1. **В текущем `dev` нет mount/umount-сисколлов** (это факт `d5de8b2a`).
   Контракт S1 их добавляет (`mount` 165 / `umount2` 166, `§5.4`); до S1
   userspace не может ни смонтировать, ни отмонтировать USB-том — поэтому
   mount/unmount UX включается только вместе с S1.
2. **Нет вложенных mount points.** `/media/usb0` сегодня невозможен; `/usb0p1`
   возможен (односоставный) — поэтому Phase 1 берёт его (`§5.3`).
3. **`/proc/mounts` не несёт device/vendor/capacity.** Для списка устройств он
   непригоден как единственный источник; нужен событийный канал (`§5.2`).

---

## 2. Референсы (поведение, не код)

| Источник | Что берём | Что не берём |
|---|---|---|
| [SerenityOS USB tour, part 1](https://quaker762.github.io/Serenity-USB/) | Устройство = дескрипторы (VID/PID, manufacturer/product strings) + один узел на устройство; descriptor кэшируется для userspace-перечисления. Отсюда: UI-имя устройства берётся из дескрипторов, а не из имени блочного узла | Kernel-структуры `Device`/`Pipe`/`Transfer` — они в kernel-спеке |
| [Serenity USBMS PR #24919](https://github.com/SerenityOS/serenity/pull/24919) | Поведенческая последовательность: INQUIRY, ожидание готовности носителя перед использованием — то, что делает `fs_class=unknown → blank/…` переход наблюдаемым. UX обязан уметь «ещё не готово» | Код, BOT/SCSI-детали |
| [Redox: GUI](https://doc.redox-os.org/book/gui.html) | File Browser показывает значки, имена, размеры и детали; файлы открываются через launcher. Отсюда: у строки устройства в сайдбаре есть имя и ёмкость, а открытие файла — отдельное действие | Ничего из Orbital API |
| [Redox: windowing](https://doc.redox-os.org/book/graphics-windowing.html) | Разделение «композитор/клиент» и то, что файловый менеджер — обычный клиент. Наш PDP — тот же принцип | Wayland/DRM |
| [Linux ABI: sysfs uevent](https://www.kernel.org/doc/Documentation/ABI/testing/sysfs-uevent) | `ACTION` ∈ add/remove/change, KEY=VALUE, edge-события. Мы сознательно берём **fixed-record**, а не текст (см. `§5.2`) | Текстовый формат, `SYNTH_UUID` |
| [UDisks2 `Filesystem.Mount`](https://storaged.org/doc/udisks2-api/latest/gdbus-org.freedesktop.UDisks2.Filesystem.html) | `mount_path` — подкаталог `/run/media/$USER`; mount возвращает путь; mount/unmount — отдельные операции с результатом. Наш `/usb0p1` — тот же контракт «операция возвращает точку» | D-Bus/поликит |
| [UDisks2 через udisks-обёртку](https://pub.dev/documentation/udisks/latest/udisks/UDisksClient-class.html) | Модель «список + поток добавлений/удалений + отдельные стримы на каждое устройство» — соответствует `devices` snapshot + `device_result` | Реализация |
| [USB in a Nutshell](https://www.beyondlogic.org/usbnutshell/usb1.shtml) | VID/PID выбирает драйвер; manufacturer/product — из дескрипторов; hotplug штатен | Электрофизика |
| [Ventoy: disk layout](https://www.ventoy.net/en/doc_disk_layout.html) | Первый раздел по умолчанию exFAT, с 1.0.11 его можно переформатировать в exFAT/FAT32/NTFS/UDF/XFS/**Ext2/3/4**; второй — VTOYEFI (FAT); с 1.0.14 можно зарезервировать место под Part3/Part4. Отсюда реальная проверка на железе (решение lead): exFAT-раздел Pagh не монтирует, ext2-раздел — «чужой» (`fs_class=2`); оба видны, mount disabled, read/write на них не заявляется; Pagh-том для read/write — отдельная флешка или Part3/Part4 из reserved space (`§8.3`) | Ничего из кода Ventoy |

---

## 3. Границы

**Входит:** динамический список устройств в Files; метаданные; состояния и
UI-реакции; mount/unmount/safe-removal UX; PDP-расширение; модуль композитора,
читающий `/dev/uevent`; crate- и live-тесты; поэтапный план.

**Не входит:** BOT/SCSI/block/xHCI, формат носителя, `fs_class`-детект, WAL,
flush на kernel-стороне, mount-политика (это `docs/usb-mass-storage-spec.md`);
форматирование разделов; exFAT/NTFS/FAT-драйверы; multi-LUN; хабы; per-user
сессии и capabilities; панель рабочего стола (P2, `§6.4`).

---

## 4. Требования

| ID | Требование | Приёмка |
|---|---|---|
| **R-USBF-1** | Секция «УСТРОЙСТВА» строится из снимка композитора: статические `/` и `/mnt` (если существует) + USB-устройства; порядок стабилен; пустая секция не рисуется | T10, T18 |
| **R-USBF-2** | Строка устройства показывает: UI-имя из `vendor`/`product`, ёмкость; полные метаданные — в «Свойствах устройства» | T10, T17, T18; `§8.2` |
| **R-USBF-3** | Устройство видно **в любом** состоянии, включая несмонтированное и foreign; исчезает только по `REMOVE` от ядра | T2, T10, T12, T13 |
| **R-USBF-4** | Клик по смонтированному устройству открывает его точку через существующий `navigate_to`; клик по несмонтированному — честное пояснение, не переход в пустоту | T11, T12 |
| **R-USBF-5** | Mount доступен только `fs_class=1 && !mounted && !in_use`; foreign/other/blank — disabled с причиной; format отсутствует | T14, T16 |
| **R-USBF-6** | Unmount и safe removal — через контекстное меню устройства и подтверждение; «можно отключать» показывается только после `safe_to_remove`, ошибка flush оставляет том смонтированным | T15, T16; `§8.2` |
| **R-USBF-7** | `removing` — overlay композитора на время операции; ядро не сообщает transient-состояний | T16 |
| **R-USBF-8** | Листинг после mount — реальный `read_dir` того же `Vfs`; никаких синтетических записей, «пусто» и «ошибка» различаются | T11; `§8.2` шаг 3 |
| **R-USBF-9** | Событийный канал не теряет состояние: композитор держит полный снимок, `seq`-разрыв вызывает resync; поздно подключившийся клиент получает полный список | T3, T5, T8 |
| **R-USBF-10** | Files не хардкодит mount path: берёт `mount` из события; путь валидируется (начинается `/usb`) до навигации | T10, T17 |
| **R-USBF-11** | Ошибки показываются текстом с причиной (io/mount_failed/flush_failed/unsupported_fs/foreign/blank/no_media/gone; `in_use` — флаг, текст приходит из `device_result`); молчаливых отказов нет | T9, T12, T13 |
| **R-USBF-12** | Открытие файла остаётся `OpenPath` (track B); устройство-специфичного открытия в v1 нет | `§7` |

---

## 5. Контракт: ядро → композитор → Files

### 5.1 Почему так

Рассматривались три варианта доставки списка устройств в Files:

1. **Files сам опрашивает `/proc`.** Отклонено: N клиентов × M опросов,
   у каждого своя задержка и своё представление о моменте; поздно запущенный
   клиент либо делает свой снимок, либо живёт с дырой; mount-операции
   пришлось бы сериализовать между процессами.
2. **Композитор опрашивает `/proc` (прецедент `netinfo`).** Работает, но
   `/proc/mounts` не несёт vendor/product/capacity, а нового `/proc`-файла с
   этими полями kernel-спека не предложила; опрос даёт задержку и лишний парсинг.
3. **Fixed-record устройство `/dev/uevent` + один reader в композиторе +
   PDP-снимок клиентам** — принято. Edge-события без polling-лага, один
   парсер на систему, снимок идемпотентен, late-join корректен.

`/proc/partitions` остаётся **fallback** (деградированный режим): только имена
и размеры блоков, без vendor/product/fs_class/mount.

### 5.2 Ядро → композитор: `/dev/uevent`

**Контракт финализирован kernel-агентом (2026-11-19).** Char-устройство,
fixed-record **ровно 128 байт**, `read()` возвращает одну запись, `poll` как у
`/dev/input/event0`, кольцо 256 записей, монотонный `seq`.

**Открытие эксклюзивное.** `open(/dev/uevent)` — единственный reader; второй
`open` получает `EBUSY`. Это правильно для нашей архитектуры (композитор —
единственный reader), но означает: второй экземпляр композитора не получит
событий, а деградированный режим включается и по `EBUSY`, не только по
`ENOENT`/`ENODEV`.

**Initial snapshot.** На `open` ядро кладёт в кольцо `ADD` для каждого уже
перечисленного устройства с текущими `mount`/`flags`/`fs_class`/`error`;
`seq = 0` помечает записи снимка. Live-записи продолжают `seq++` (u32, wrap).
Разрыв монотонности или `PUEV_IOC_SNAPSHOT`-ioctl → re-seed: композитор
перечитывает снимок и перестраивает таблицу с нуля (закрывать/открывать
устройство не нужно и нельзя — оно занято им же).

**Чтение.** `read(count)`: `count >= 128` — ровно одна запись (128 байт);
`count < 128` — `EINVAL`; пусто при `O_NONBLOCK` — `EAGAIN`; `poll` — `POLLIN`.
Переполнение кольца (256) инкрементит внутренний `dropped`; для UX это не
потеря состояния, потому что пропуск `seq` всё равно вызывает re-seed.

**Запись, точные смещения** (фиксирует `docs/usb-mass-storage-spec.md`;
host-property там же; композитор держит эти смещения в одном модуле `usbdev`
и отвергает запись с чужим `magic`/`version`):

| Смещение | Поле | Тип | Смысл |
|---:|---|---|---|
| 0 | `magic` | u32 | `PUEV` = `0x56455550` |
| 4 | `version` | u16 | 1 |
| 6 | `kind` | u8 | 1 `ADD`, 2 `REMOVE`, 3 `CHANGE`, 4 `ERROR`, 5 `MOUNTED`, 6 `UNMOUNTED` |
| 7 | `flags` | u8 | bit0 `removable`, bit1 `read_only`, bit2 `mounted`, bit3 `in_use`, bit4 `safe_to_remove`, bit5 `surprise` |
| 8 | `fs_class` | u8 | 0 unknown, 1 pagh-ext2, 2 foreign-ext2, 3 blank, 4 other-fs, 5 no-media |
| 9 | `error` | u8 | 0 none, 1 io, 2 mount_failed, 3 flush_failed, 4 unsupported_fs, 5 no_media |
| 10 | `reserved` | u16 | 0 |
| 12 | `seq` | u32 | 0 = снимок; далее монотонно |
| 16 | `sectors` | u64 | ёмкость узла в секторах |
| 24 | `sector_size` | u32 | Phase 1 = 512 |
| 28 | `dev` | 16 | `usb0`, `usb0p1`, … ASCII, NUL-padded |
| 44 | `mount` | 16 | `/usb0p1`, пусто = не смонтирован; **≤ 15 символов** |
| 60 | `vendor` | 16 | ASCII, NUL-padded |
| 76 | `product` | 32 | ASCII, NUL-padded |
| 108 | `serial` | 16 | VPD 0x80; Phase 1 может быть пустым |
| 124 | `reserved` | 4 | 0 |

Ёмкость = `sectors × sector_size` (Phase 1 `sector_size = 512`). Строки
ASCII-only, не-ASCII → `?`; UI-fallback «USB-накопитель» — честный пробел, а не
недосмотр. `mount[16]` — жёсткий лимит 15 символов: Phase 1 top-level
`/usb0p1` влезает, будущие вложенные `/<dir>/usbNpM` тоже влезают при
`N, M ≤ 99` (например `/media/usb99p99` = 15 символов, ровно лимит); если
понадобится длиннее — это новый ABI, а не «допишем в строку».

**Состояние устройства = вывод из полей, а не отдельное поле ядра:**

| Условие | `state` | `reason` |
|---|---|---|
| `flags.mounted && mount != ""` | `mounted` | `none`, либо `error`-код, если пришёл `ERROR` |
| `fs_class == 1 && !mounted` | `unmounted` | `none` |
| `error ∈ {io, mount_failed, flush_failed}` | `error` | `io` / `mount_failed` / `flush_failed` |
| `error == 4` (`unsupported_fs`) | `present` | `unsupported_fs` — классификация, не сбой |
| `error == 5` (`no_media`) | `present` | `no_media` — классификация, не сбой |
| `fs_class == 2` | `present` | `foreign_volume` |
| `fs_class == 3` | `present` | `blank` |
| `fs_class == 4` | `present` | `unsupported_fs` |
| `fs_class == 5` | `present` | `no_media` |
| `fs_class == 0` | `present` | `enumeration_failed` |

`MOUNTED`/`UNMOUNTED` применяются как переход (`mounted=1/0`) и затем
пересчитываются по этой же таблице — kind не заменяет состояние, а двигает
флаг. `ERROR` с `error=flush_failed` **не** снимает `mounted`: том остаётся
смонтированным, UI не имеет права говорить «можно отключать».
`safe_to_remove` — флаг успешного `umount2`; `surprise` — физическое
выдёргивание без извлечения.

**Агрегация разделов — правило композитора.** Ядро сообщает **блочные узлы**
(`usb0`, `usb0p1`, `usb0p2`, …). Files показывает **одну строку на физическое
устройство** `usb0`: база = `dev` без хвоста `p<digits>`. Представитель строки:
сначала смонтированный узел, иначе узел с лучшим `fs_class` в порядке
`1 > 2 > 3 > 4 > 5 > 0`, иначе первый. Ёмкость — `sectors × sector_size`
представителя; `mount`, `fs_class`, `read_only`, `safe_to_remove` — представителя;
`in_use` и `surprise` — OR по узлам; `removable` — OR. Это даёт одну строку на
флешку, а не строку `VTOYEFI`, и сохраняет видимость multi-partition носителя.
Разделы целиком видны в «Свойствах устройства» (`§6.2`).

**Деградированный режим.** Нет `/dev/uevent` (`ENOENT`/`ENODEV`) или он занят
(`EBUSY`) → композитор раз в 1000 мс читает `/proc/partitions` + `/proc/mounts`,
строит строки без vendor/product/`fs_class` (устройство показывается как
«USB-накопитель» с ёмкостью из `#blocks`), печатает один warning. Это не
приёмка, а честное поведение на ядре без канала.

### 5.3 Композитор → Files: PDP/1, opt-in расширение

**Почему opt-in, а не версия 2.** `server.rs:1191` требует `version == VERSION`;
бамп сломал бы все клиенты, а расширение обязано быть обратно совместимым.
Поэтому клиент явно подписывается (`devices_subscribe`), и только подписанным
сервер шлёт новые типы. Terminal/Paint/старые клиенты не видят ни одного
нового сообщения и работают как раньше. Номера 17–20 свободны (1–16 заняты).

| type | имя | направление | payload |
|---:|---|---|---|
| 17 | `devices_subscribe` | c→s | пусто |
| 18 | `devices` | s→c | снимок (ниже) |
| 19 | `device_request` | c→s | `u32 request_id`, `string id`, `u8 action` |
| 20 | `device_result` | s→c | `u32 request_id`, `u8 status`, `string message` |

`devices` (18):

```text
u32 generation                  // растёт на каждом изменении; 0 зарезервирован
u16 count                       // <= 32 (MAX_DEVICES)
повтор count раз:
  string id                     // "usb0"
  u8 state                      // 1 present, 2 unmounted, 3 mounted, 4 removing, 5 error
  u8 fs_class                   // 0..5, как в uevent
  u8 flags                      // биты как в uevent: bit0 removable, bit1 read_only,
                                // bit2 mounted, bit3 in_use, bit4 safe_to_remove, bit5 surprise
  u8 reason                     // 0 none, 1 unsupported_fs, 2 foreign_volume, 3 blank,
                                // 4 io, 5 mount_failed, 6 flush_failed,
                                // 7 enumeration_failed, 8 device_gone, 9 no_media
                                // (in_use — это flags.bit3, не reason)
  u64 capacity                  // байты; 0 = неизвестно
  string vendor
  string product
  string mount                  // "" если не смонтирован
  string serial                 // "" если ядро не отдало
```

Ограничения: `count ≤ 32`; каждая строка ≤ 255 байт; при переполнении
композитор обрезает список и печатает warning (тихая потеря устройства
запрещена). Строки `id`/`mount` валидируются при приёме: `id` — `[A-Za-z0-9]+`,
`mount` — начинается с `/usb`, без `..`.

`device_request` (19): `action` — 1 `mount`, 2 `unmount`, 3 `eject`.
`device_result` (20): `status` — 0 `ok`, 1 `error`, 2 `safe_to_remove`;
`message` — готовый русский текст для UI (композитор — единственное место, где
живёт формулировка kernel-ошибки; Files его только показывает).

**Коды ошибок.** Добавляются `ERR_DEVICE = 10` (неизвестный id, недопустимое
действие, запрос без подписки) и `ERR_DEVICE_BUSY = 11` (операция уже идёт).
Ошибка **валидации** запроса приходит как `error` (type 10); ошибка
**операции** — как `device_result{status=1}`. Это разные вещи: первая означает
«запрос не принят», вторая — «принят и провалился».

**Гарантии порядка.** После успешной операции сервер (1) дренирует
`/dev/uevent` до `EAGAIN`, (2) перестраивает снимок, (3) рассылает `devices`
с новой `generation`, (4) отправляет `device_result`. Клиент не имеет права
считать том смонтированным по одному `ok`: авторитет — снимок.

**`removing` — overlay.** Получив валидный `device_request`, сервер **сразу**
рассылает снимок, в котором у этого `id` `state = removing` (остальные поля
сохранены), затем запускает операцию. `removing` никогда не приходит из ядра:
ядро не врёт про transient. По завершении — реальный снимок и `device_result`.
Таймаут операции (10 с — flush на реальном носителе не мгновенный) снимает
overlay и отдаёт `error`/`reason=io`.

### 5.4 Выполнение mount/unmount/eject (entry point подтверждён)

**Entry point финализирован:** `mount(2)` nr **165** и `umount2(2)` nr **166**,
root-only (`euid == 0`), Linux-сигнатуры.

* `mount(source, target, fstype, flags, data)`: `source` — **имя блочного
  устройства**, не путь (`"usb0p1"`); `target` — top-level `"/usb0p1"`
  (совпадает с source); `/mnt`, `/proc`, `/dev`, `/tmp`, `/media`
  зарезервированы → `EINVAL`; `fstype` — только `"ext2"` в Phase 1; `flags = 0`;
  `data = 0`.
* `umount2(target, flags)`: `flags = 0`; `MNT_FORCE`/`MNT_DETACH` → `EINVAL`.
* **Eject = `umount2(target, 0)`** (отдельного сисколла нет). `EBUSY`, если под
  точкой есть открытые fd; иначе WAL-flush + SCSI `SYNCHRONIZE CACHE (10)`
  (0x35); ошибка flush → `EIO`, том **остаётся смонтированным**, приходит
  `ERROR error=flush_failed`; успех → detach + `CHANGE mounted=0,
  safe_to_remove=1`. Порт Phase 1 не обесточивается: «безопасное извлечение» =
  flush+detach, физически выдёргивает пользователь.
* **Surprise removal:** dead-флаг, I/O → `EIO`, `REMOVE surprise=1`; WAL
  replay при следующем mount.

**Unmount vs «Безопасно извлечь».** Обе кнопки вызывают один и тот же
`umount2`, разница только в UX: «Отмонтировать» — без подтверждения,
`device_result status=ok`; «Безопасно извлечь…» — подтверждение и
`status=safe_to_remove` с сообщением «теперь можно отключить». Второго
kernel-пути нет и выдумывать его нельзя.

Композитор не вызывает сисколлы из своего цикла: операция может длиться
секунды (flush носителя). Схема:

```text
server loop ──device_request──► worker thread ──mount(2)/umount2(2)──► ядро
     ▲                               │
     └──── DeviceOutcome ◄───────────┘   (через тот же incoming-канал)
```

* Одна операция в полёте; второй запрос → `ERR_DEVICE_BUSY`.
* Worker вызывает трейт `DeviceOps` (`mount(id, target)`,
  `umount(target) -> Result<safe_to_remove, E>`); тесты подставляют фейковый
  `DeviceOps` — реальные сисколлы в crate-тестах не нужны.
* Overlay `removing` ставится до запуска worker'а, снимается по `Outcome`.
* Права: `mount`/`umount2` root-only; в single-user Pagh композитор и Files
  работают от root. Модель per-user сессий не проработана (`§10`, G8).

**Авто-mount: два допустимых владельца (открытый вопрос O7/G16).** Если авто-mount
остаётся за ядром, при `ADD fs_class=1` приходит `MOUNTED` и UX только
отображает точку. Если за userspace, композитор при `ADD fs_class=1 &&
!mounted && !in_use` сам вызывает `mount(2)` без диалога (это не «явное
действие пользователя», а политика), а `device_request(mount)` остаётся для
повторного монтирования после `umount2`. PDP-контракт и UI в обоих случаях
одинаковы — меняется только место вызова.

Валидация на стороне композитора (до вызова): `id` есть в снимке; `state` и
`fs_class` допускают действие; `target` из события, а не собранный из строки
пользователя; повторный mount того же `id` отвергается.

### 5.5 Что меняется в коде (карта)

| Файл | Изменение |
|---|---|
| `rust-apps/pagh-desktop/src/usbdev.rs` (новый) | `const` записи, `parse_record`, `DeviceTable` (агрегация, `seq`, resync), `snapshot()`, host-тесты |
| `rust-apps/pagh-desktop/src/protocol.rs` | `Message::{DevicesSubscribe, Devices, DeviceRequest, DeviceResult}`, типы 17–20, `ERR_DEVICE`, `ERR_DEVICE_BUSY`, roundtrip-тесты |
| `rust-apps/pagh-ui/src/protocol.rs` | те же типы (вторая реализация протокола — обязана совпадать; тесты в обоих крейтах) |
| `rust-apps/pagh-ui/src/client.rs` | `DesktopClient::{subscribe_devices, request_device}`, `Incoming::{Devices, DeviceResult}` |
| `rust-apps/pagh-desktop/src/server.rs` | подписка per-client, broadcast снимка, `device_request` → worker, `device_result`, serial-маркеры |
| `rust-apps/pagh-desktop/src/usbdev_ops.rs` (новый) | `DeviceOps` + реализация через `mount(2)`/`umount2(2)` (extern-FFI, без libc-крейта); фейк для тестов |
| `rust-apps/pagh-files/src/device.rs` (новый) | `DeviceInfo`, `DeviceState`, `DeviceAction`, `reason_text`, `label`/`detail` |
| `rust-apps/pagh-files/src/app.rs` | `devices: Vec<DeviceInfo>`, `apply_devices`, `device_result`, `build_device_places`, place-aware контекстное меню, диалоги, `Command::DeviceRequest` |
| `rust-apps/pagh-files/src/menu_model.rs` | `place_menu(...)` + id действий 400–403 |
| `rust-apps/pagh-files/src/icons.rs` + `assets/icons/usb-*.png` | `FluentIcon::UsbPlug` (Fluent «Usb Plug», MIT; атрибуция в README) |
| `rust-apps/pagh-files/src/main.rs` | подписка при connect, `Incoming::Devices/DeviceResult` → app, `Command::DeviceRequest` → `device_request` |
| `rust-apps/pagh-ui/src/widgets.rs` | `ListItem.detail: Option<String>` — приглушённая ёмкость справа; ellipsize с учётом detail |
| `rust-apps/pagh-ui/src/theme.rs` | `Palette.danger` (единственный новый токен) — цвет строки `error`; два литерала палитры + тест |
| `rust-apps/pagh-desktop/PROTOCOL.md` | §8.2 «Устройства (types 17–20)» |
| `docs/desktop-server.md` | абзац о единственном reader'е `/dev/uevent` |
| `README.md` / `rust-apps/*/README.md` | снять «USB mass storage нет» после реализации; до неё — не трогать |

---

## 6. Состояния и UI

### 6.1 Модель состояний

| Состояние | Источник (по `§5.2`) | Как выглядит | Действия |
|---|---|---|---|
| `present` | `fs_class ∈ {0,2,3,4,5}`, не смонтирован | глиф USB приглушён; detail — ёмкость; «Смонтировать» disabled с причиной | Свойства, Безопасно извлечь (no-op) |
| `unmounted` | `fs_class=1`, `mount=""`, без error | глиф USB обычный, но не акцент; detail — ёмкость | Смонтировать, Свойства, Безопасно извлечь (no-op) |
| `mounted` | `flags.mounted && mount != ""` | глиф USB акцентный; клик открывает точку | Отмонтировать, Безопасно извлечь, Свойства |
| `removing` | overlay композитора на время `device_request` | глиф приглушён, detail «извлечение…»; действия disabled | — |
| `error` | `ERROR error ∈ {io,mount_failed,flush_failed}` | глиф красным (`palette.danger`), detail — причина; при `flush_failed` том **всё ещё смонтирован** | Свойства; «Отмонтировать» best effort, если `mounted`; повторная попытка, если устройство ещё здесь |

Дополнительные флаги, видимые в UI: `safe_to_remove` — после успешного
`umount2` показывается сообщением/диалогом «теперь можно безопасно отключить»
(detail остаётся ёмкостью; строку в «можно отключать» не переписываем);
`surprise` — устройство выдернули без извлечения; `REMOVE` всегда убирает
строку (ghost-строки мёртвого носителя не держим), а Files показывает
предупреждение, только если `cwd` был внутри этой точки (иначе — тихо, чтобы
не спамить). Detail на время `removing` временно показывает «извлечение…»
вместо ёмкости — это единственный transient, который его подменяет.

`reason` → текст (единственная таблица, `device.rs::reason_text`):

| reason | Текст |
|---|---|
| `unsupported_fs` | «Файловая система не поддерживается (exFAT/FAT/NTFS — будущий этап; Pagh монтирует ext2)» |
| `foreign_volume` | «Том не помечен как pagh — автоматическое монтирование отключено» |
| `blank` | «Пустой носитель» |
| `io` | «Ошибка ввода-вывода» |
| `mount_failed` | «Не удалось смонтировать» |
| `flush_failed` | «Не удалось сбросить данные на носитель — не отключайте» |
| `enumeration_failed` | «Устройство не удалось опознать» |
| `device_gone` | «Устройство отключено» |
| `no_media` | «Нет носителя (картридер без карты)» |

### 6.2 Сайдбар

* Порядок: `/` (id 10), `/mnt` (id 11, если `vfs.exists`), затем USB в порядке
  снимка (стабильная нумерация ядра). Секция целиком скрывается, если в ней
  нет ни одного пункта (существующее поведение `build_sidebar`).
* **id пункта:** `1000 + N`, где `usb<N>` — id ядра. Ядро не переиспользует
  номер в пределах загрузки, поэтому id стабилен и не зависит от позиции
  (появление второй флешки не сдвигает выделение). `N ≥ 1000` → fallback
  `2000 + индекс` + warning.
* **Подпись:** `«USB: <vendor> <product>»` (согласовано с kernel-спекой),
  пустые части схлопываются: только vendor → `«USB: <vendor>»`, только
  product → `«USB: <product>»`, обе пусты → `«USB-накопитель»`. Ни при каких
  условиях не показывается `usb0p1` (ellipsize — обычный, по ширине сайдбара).
* **Иконка:** статические `/` и `/mnt` остаются на Fluent `Drive` (Hard Drive);
  USB-устройства получают новый `FluentIcon::UsbPlug` (Fluent «Usb Plug», MIT,
  16/20/24/32 — как остальные ассеты). Тинт: `mounted` — обычный цвет
  сайдбара, `present`/`unmounted` — приглушённый, `error` — `palette.danger`.
  Векторный `Icon` из `FluentIcon` нужен только для резервирования места, как
  и у существующих пунктов.
* **Detail:** `format::format_size(capacity)` (та же функция, что в статусной
  строке). Ёмкость 0/неизвестна → detail пуст.
* **Навигация:** `path` = `mount` для `mounted`; для остальных — пустая строка.
  Клик по пустому пути не вызывает `navigate_to`, а ставит `notice` с
  `reason_text` (R-USBF-4).
* **Выделение:** `sidebar_selected` — по id; при исчезновении устройства и
  `cwd` внутри его точки — переход в `$HOME` + `notice` «Устройство
  отключено» (без падения и без «Не удалось открыть папку»). При `surprise`
  notice жёстче: «Устройство отключено без извлечения — данные могли не
  сохраниться».
* **`safe_to_remove`** не делает строку неактивной: она остаётся `unmounted` с
  ёмкостью в detail, пока пользователь физически не выдернет носитель;
  «можно отключать» живёт в результате операции/notice, а не в подписи строки.
* **`prune_missing_devices`** применяется **только** к статическим устройствам
  (`/`, `/mnt`): у USB-пункта путь может быть пуст, и это не «мёртвый пункт».

Свойства устройства (диалог, `Dialog::DeviceProperties`): UI-имя, vendor,
product, serial (если есть), ёмкость (`format_size` + точное число байт),
блочные узлы и разделы с `fs_class`, состояние, mount, `removing`/причина.
VID:PID **не показывается** — его нет в согласованной записи `uevent`
(пробел `§10`, не выдумываем).

### 6.3 Контекстное меню устройства

ПКМ по пункту сайдбара сейчас открывает меню содержимого (`app.rs:1692`).
Вводится `menu_model::place_menu(place)` — **только для устройств**; для
остальных пунктов остаётся текущее поведение, чтобы не ломать привычные
действия.

| id | Пункт | Enabled |
|---:|---|---|
| 400 | «Смонтировать» | `fs_class=1 && !mounted && !in_use` |
| 401 | «Отмонтировать» | `mounted` (включая `state=error` — best effort очистка) |
| 402 | «Безопасно извлечь…» | `!mounted` (no-op: нечего сбрасывать) или `mounted` без `error`/`flush_failed`; при `in_use` — enabled, ошибка показывается после попытки |
| 403 | «Свойства устройства» | всегда |
| — | «Форматировать» | **отсутствует** (вне v1, destructive) |

Disabled-пункт несёт причину в подписи или в `notice` при клике — «серое без
объяснения» запрещено (R-USBF-11).

В v1 действия живут **только** в контекстном меню пункта: отдельной кнопки
«Извлечь» в панели инструментов нет — там она была бы без явного объекта
(какое устройство?) и потребовала бы второго состояния выбора. Это осознанное
«кнопки/context menu»-решение в пользу меню; панельный индикатор — P2 (`§6.4`).

**Диалог подтверждения safe removal** (`Dialog::ConfirmRemoval { id, label }`):

* заголовок — «Безопасно извлечь?»;
* текст — «Устройство «<label>» будет отмонтировано, а данные сброшены на
  носитель. Не отключайте его до подтверждения.»
* кнопки — «Извлечь» (primary) и «Отмена» (Esc). Только primary порождает
  `Command::DeviceRequest { action: Eject }`; отмена не делает ничего.

«Отмонтировать» подтверждения не требует. Пока операция в полёте
(`state=removing`), Esc/повторный клик операцию не отменяют — kernel-вызов уже
идёт; UI просто не даёт отправить второй запрос. Результат:

* `status=safe_to_remove` → сообщение «Теперь можно безопасно отключить
  «<label>»» (закрывается кликом/Esc);
* `status=1` → диалог с `message` композитора и кнопкой «Понятно»; строка
  остаётся в состоянии, которое прислал снимок (например `mounted` при
  `flush_failed`), а не в оптимистичном «извлечено».

### 6.4 Рабочий стол (P2, вне acceptance)

Композитор уже владеет снимком. P2: иконка USB в панели, когда есть хоть одно
устройство; клик — открыть Files на точке (через `launch_targets`, `cwd`) или
показать меню «Извлечь». В v1 не реализуется и в приёмку не входит: панель —
отдельная поверхность с собственными golden-тестами (`panel.rs`,
`tests/panel_network_icon.rs`), и расширять её до готовности списка — лишний
риск.

---

## 7. Листинг и открытие файлов

**Листинг.** `FilesApp::navigate_to` (`app.rs:408`) и `reload()` уже читают
реальный каталог через `Vfs`. Спека ничего не добавляет: после mount путь
`/usb0p1` появляется в VFS, клик по устройству вызывает `navigate_to`, листинг
реальный. Запрещено: подмешивать записи из снимка устройств в листинг, показывать
«пустую папку» вместо ошибки (`missing_directory_reports_error_instead_of_fake_listing`
— существующий тест), считать устройство смонтированным до снимка.

**Read/write.** Файловые операции идут через `HostVfs` → syscall'ы →
ext2/WAL → block → USB. Отдельного кода в Files не нужно; приёмка пишет и
читает файл через `FilesApp` (`§8.2`, шаг 3).

**Открытие файла — track B.** `Command::OpenPath` сегодня только печатает
(`main.rs:179`). Для USB это значит: двойной клик по файлу на флешке не
откроет его, пока не появится контракт «открыть файл приложением». Варианты
(не решены, вне этого PR): (a) PDP `launch` с `app = MIME/расширение` и `cwd =
путь к файлу`; (b) новый PDP `open_path` с путём и ожидаемым типом; (c) сам
Files запускает приложение через `launch_targets`-реестр. Зависимость названа
в `§10`; acceptance v1 проверяет **чтение** файла (листинг/размер/открытие
каталога), а не запуск приложения.

---

## 8. Приёмка

Тяжёлые прогоны (QEMU/e2e) выполняются **после** stable-валидации и только на
этапе реализации; эта спека их не запускает.

### 8.1 Crate-тесты (host, без QEMU)

`pagh-desktop`:
* T1 `parse_record_*` — 128 байт, `magic`/`version`/`kind` (1–6)/смещения по
  таблице `§5.2`; чужой `magic`/`version`/короткая запись отвергаются;
  ASCII-санитайзинг (`0x7F`, `0x00` внутри строки) и NUL-padding; `mount`
  длиннее 15 символов не читается как путь.
* T2 `table_aggregates_partitions` — `usb0` + `usb0p1` + `usb0p2` → одна
  строка `usb0`; представитель: смонтированный > `fs_class`; ёмкость
  представителя (`sectors × sector_size`); `in_use`/`surprise` — OR.
* T3 `snapshot_seq_zero_then_gap_reseeds` — `seq=0` у серии `ADD` — это
  снимок, не gap; затем `seq=1,2,4` → `PUEV_IOC_SNAPSHOT` (через фейковый
  reader), таблица перестроена, `usb resync` в логе; без re-seed строка не
  остаётся в старом состоянии.
* T4 `snapshot_order_and_generation` — порядок стабилен, `generation` растёт
  только при изменении снимка.
* T5 `initial_adds_build_table` — `ADD`-серия снимка (с `MOUNTED`-записями) →
  полный снимок без ожидания live-событий.
* T6 `exclusive_open_and_degraded` — `EBUSY`/`ENOENT`/`ENODEV` → degraded
  `/proc/partitions`-режим, один warning, без паники и без второго reader'а.
* T7 протокол: roundtrip всех четырёх сообщений в `pagh-desktop/src/protocol.rs`
  **и** в `pagh-ui/src/protocol.rs`; неизвестный тип по-прежнему ошибка кадра;
  `flags`-биты PDP совпадают с uevent (общий тест-вектор).
* T8 `socket_e2e`: подписка → немедленный `devices`; `device_request` →
  `device_result`; клиент без подписки не получает ни одного нового типа;
  запрос без подписки → `error(ERR_DEVICE)`; второй запрос → `ERR_DEVICE_BUSY`.
* T9 `failing_ops_surface_message` — фейковый `DeviceOps` с `EBUSY`/`EIO`/
  `EINVAL` (зарезервированная точка, `MNT_FORCE`) → `device_result{status=1}`
  с русским текстом; overlay `removing` снят.
* T19 `state_derivation` — таблица `§5.2` целиком: `fs_class` 0..5,
  `mounted`, `error` 1–3, `safe_to_remove`, `surprise` → ожидаемые
  `state`/`reason`; `ERROR flush_failed` не превращает `mounted` в
  `unmounted`.
* T20 `read_contract` — `read(128)` = одна запись, `read(127)` = `EINVAL`,
  `O_NONBLOCK` пусто = `EAGAIN`, `poll` = `POLLIN`; 257-я запись → `seq`-gap,
  а не тихая потеря.

`pagh-files`:
* T10 `snapshot_builds_device_places` — `/`, `/mnt` сохранены; USB-строки с
  id `1000+N`, подписью, detail-ёмкостью; `present` не навигируется.
* T11 `mounted_device_opens` (расширение существующего) — снимок `usb0`
  `mounted /usb0p1` → клик → `cwd == /usb0p1`, `error().is_none()`.
* T12 `unmounted_device_click_is_explained` — клик по `present` → `cwd` не
  меняется, `notice` содержит причину.
* T13 `device_disappears_while_inside` — `cwd=/usb0p1`, снимок без `usb0` →
  `cwd == home`, notice «Устройство отключено», диалог свойств закрыт.
* T14 `place_menu_by_state` — таблица enabled/disabled для
  `present/unmounted/mounted/removing/error` × `in_use` × `fs_class`.
* T15 `safe_removal_requires_confirmation` — eject открывает диалог; только
  подтверждение возвращает `Command::DeviceRequest`; отмена — ничего.
* T16 `removing_overlay_disables_actions` — снимок `removing` → пункты меню
  disabled, повторный клик не порождает второй запрос.
* T17 `label_fallbacks` — `«USB: vendor product»`, только vendor, только
  product, обе пусты → четыре подписи; `usb0p1` не попадает в UI;
  `format_size` для 0/неизвестной ёмкости.
* T18 `screenshots`: сцена `files-usb-devices` (снимок инжектируется явно как
  синтетический кадр концепта) + golden hash; существующие сцены не меняются.

`pagh-ui`: `ListItem.detail` рисуется приглушённо, ellipsize не съедает
основную подпись, golden-тест виджетов.

### 8.2 Live e2e (QEMU, после stable)

Новый режим `python3 tools/e2e.py usbfiles` (имя — предложение; существующие
`usb`/`rndis`/`desktop` не трогаются). Подготовка образа — как у
`_desktop_prepare_disk` (`tools/e2e.py:3045`): «первая загрузка форматирует»
нужна потому, что ядро монтирует только том со своим WAL-суперблоком, а
`mkfs.ext2` с хоста даёт `fs_class=2`. Поэтому:

1. `qemu-img create` 64 МиБ, GPT + раздел (`tools/mkimage.py:build_disk_image`,
   `--make-disk`-путь), первая загрузка **как data disk** — ядро форматирует
   ext2+WAL и provision'ит → это pagh-том (`fs_class=1`).
2. Вторая загрузка: desktop-диск (`/mnt/pd`, `/mnt/pagh-files`) + `qemu-xhci`
   + `-drive if=none,id=usb0,file=usb.img -device usb-storage,drive=usb0,bus=xhci.0`;
   автозапуск Files.
3. `pagh-files --connect … --device-selftest usb0 [--keep]` — отдельный
   процесс-драйвер: подписывается на `devices`, ждёт снимок, проверяет
   `state=mounted`, `mount=/usb0p1`, навигирует `FilesApp`, пишет
   `/usb0p1/.pagh-selftest`, читает обратно и сверяет байты, удаляет (кроме
   `--keep` — файл нужен шагу 7), запрашивает `unmount`, ждёт снимок
   `unmounted`, запрашивает `eject`, ждёт `safe_to_remove`. Печатает
   `USBFILES <step> PASS/FAIL`. Это **не заглушка**: тот же `FilesApp`, тот же
   `HostVfs`, тот же PDP; клики заменены вызовами тех же методов (иначе
   приёмка зависела бы от указателя — см. `§10`, press-routing).
4. Ассерты serial (детерминированные маркеры, без времени). Точные строки
   ядра задаёт kernel-спека; ниже — контрактные примеры, grep-паттерны
   фиксируются при реализации: `usb: uevent open ok`;
   `usb: add usb0 vendor=… product=… sectors=… class=1`;
   `usb: mount usb0 /usb0p1`; `pagh-files: devices 1 (usb0 mounted /usb0p1)`;
   `USBFILES write PASS`, `USBFILES read PASS`, `USBFILES unmount PASS`,
   `USBFILES eject PASS`.
5. Hotplug: загрузка **без** `usb-storage`, после старта QMP
   `device_add usb-storage,drive=usb0,bus=xhci.0` → `usb: add` ≤ 3 с и Files
   `devices 1`; `device_del` при смонтированном → маркер ядра о surprise
   removal (точная строка — в kernel-спеке; UX ждёт `REMOVE surprise=1`, а
   читатели получают `EIO`), Files убирает строку, не падает и показывает
   `surprise`-notice, если `cwd` был внутри точки.
6. Негатив: `usb.img` как чужой ext2 (`mkfs.ext2` с хоста) → `class=2`, mount
   пуст, строка видна, mount disabled; `blank`-образ → `class=3`.
7. Persistence: после eject — shutdown, повторная загрузка, файл, оставленный
   `--keep`, читается (второй прогон `--verify-only`).
8. Скриншот окна Files с секцией «УСТРОЙСТВА» (визуальное доказательство,
   как в `desktop`-режиме) — `e2e_usbfiles_sidebar.png`.

### 8.3 Реальное железо: Ventoy

Физическая проверка выполняется человеком (как прочие USB-проверки проекта).

**Решение lead 2026-11-19 (обязательно для приёмки):** на Ventoy-exFAT Phase 1
проверяются только обнаружение и отображение. Read/write на Ventoy **не
заявляется** и не проверяется; попыток записи и ложного mount нет.

**Что проверяется на Ventoy-флешке как есть:** hotplug; строка появляется;
vendor/product/ёмкость соответствуют дескрипторам и `READ CAPACITY`; состояние
честное — `fs_class=4` (exFAT по умолчанию; [Ventoy поддерживает и ext2/3/4 с
1.0.11](https://www.ventoy.net/en/doc_disk_layout.html), такой раздел был бы
`fs_class=2` — тоже чужой); mount **disabled** с причиной «exFAT пока не
поддерживается» (или «том не помечен как pagh» для чужого ext2); «Свойства
устройства» показывает разделы, включая VTOYEFI; безопасное извлечение
подтверждает «можно отключать» (для несмонтированного тома это no-op), а
строку убирает физическое отключение. Несмонтированное foreign-устройство
**остаётся видимым** в Files.

**Что проверяется на томе Pagh:** вторая флешка или Part3/Part4 из reserved
space Ventoy (с 1.0.14; создаётся пользователем в свободном месте),
отформатированная самой Pagh при первой загрузке → `fs_class=1`, авто-mount в
`/usb0p1`, реальный листинг, запись и чтение файла, unmount, safe removal,
исчезновение из списка. Это единственное место, где v1 заявляет read/write.

**Почему так:** data-раздел Ventoy — чужой том (exFAT, а после переформатирования
в ext2 — `fs_class=2` без WAL-суперблока). Политика, согласованная с
kernel-спекой, запрещает авто-mount и mount чужих томов; exFAT-драйвера нет и в
этом этапе не будет. Поэтому буквальная формулировка «Ventoy монтируется» в v1
не выполняется осознанно; честный foreign-статус — это и есть корректное
поведение, а не недоделка. Read-only mount чужого ext2 — future этап (был
вопросом O1, закрыт решением lead: не в Phase 1).

---

## 9. План (узкие PR) и гейты

| Этап | Содержание | Зависит от | Гейты |
|---|---|---|---|
| **S0** | Эта спека + согласование контракта | — | review lead |
| **S1** | kernel: `/dev/uevent` (128 B, exclusive, `PUEV_IOC_SNAPSHOT`), `mount(2)` 165 / `umount2(2)` 166, flush/SYNCHRONIZE CACHE, surprise-remove; `docs/usb-mass-storage-spec.md` | kernel-агент | kernel-гейты (их PR) |
| **S2** | `pagh-desktop/src/usbdev.rs` + PDP 17–20 + подписка/broadcast + serial-маркеры; **без UI** | S1 (или фейковый reader для тестов) | `cargo test -p pagh-desktop`, `-p pagh-ui` |
| **S3** | Files: модель устройств, сайдбар, detail, свойства, контекстное меню, состояния; mount/unmount — disabled до S4 (нет worker'а и подтверждений) | S2 | `cargo test -p pagh-files -p pagh-ui`; screenshot |
| **S4** | mount/unmount/eject: worker + `DeviceOps` + `mount`/`umount2` + overlay `removing` + диалоги | S1 (uevent + сисколлы), S3 | crate + `e2e.py usbfiles` |
| **S5** | Реальное железо (Ventoy = обнаружение/foreign; Pagh-том = mount/read/write), README/`docs`, снятие «USB mass storage нет» | S4 | ручная проверка + e2e |

Правила: каждый этап — отдельный PR в `dev` **после** stable-merge; PR base —
`dev`, не `main`; ветка удаляется после merge; версия бампается только PR'ом,
который выпускает фичу (MINOR — видимая пользователю возможность), docs-only
коммиты версию не трогают. S1 и S2 можно вести параллельно: S2 тестируется на
фейковых записях, S1 подключается последним коммитом.

Гейты каждого PR — шесть команд из `AGENTS.md`
(`cargo fmt --all -- --check`, `python3 tools/build.py build`,
`python3 tools/build.py build --release`, `python3 tools/host_tests.py`,
`python3 tools/check_safety.py`, `python3 tools/check_agents_md.py`) плюс
crate-тесты затронутых rust-apps; для S4 — `python3 tools/e2e.py usbfiles`.

---

## 10. Честные пробелы, зависимости, риски

| # | Пробел/риск | Влияние | Что делаем |
|---|---|---|---|
| G1 | **В `dev` нет mount/umount-сисколлов** | mount/unmount UX невозможен до S1 | контракт закрыт (`mount` 165 / `umount2` 166); S4 ждёт S1; до него кнопки disabled с причиной, listing работает |
| G2 | **Ventoy-флешка не монтируется** (чужой том; exFAT не поддерживается) | буквальная приёмка «Ventoy монтируется» не выполнима в v1 | решение lead: Ventoy = обнаружение/отображение/честный foreign-статус; read/write — на pagh-ext2 томе (`§8.3`); read-only mount чужого ext2 — future |
| G3 | **VID:PID отсутствуют в финальной записи** `uevent` | «Свойства» без VID:PID | не показываем, не выдумываем; поле — future ABI |
| G4 | **Serial опционален**, ASCII, 16 байт | идентификация в свойствах неполная | показываем, если есть; иначе строка скрыта |
| G5 | **ASCII-only строки**, vendor 16 / product 32 (согласовано) | не-ASCII и длинные имена теряются | fallback-подпись «USB-накопитель»; UTF-8 в v1 не обещан |
| G6 | **`/dev/uevent` ещё не в ядре** (initial snapshot/exclusive/ioctl — контракт S1) | до S1 cold-plug устройства не видны композитору | S2 на фейковых записях; S4/S5 требуют S1 |
| G7 | **Surprise removal смонтированного тома** (dead-флаг, `EIO`, `REMOVE surprise=1`, WAL replay при следующем mount) | возможна потеря данных при выдёргивании | UX: safe removal + честный `error`/`surprise`-notice; kernel-спека владеет teardown и replay |
| G8 | **Нет per-user сессий/capabilities** | mount — root-only; в single-user Pagh это ок, но модель не проработана | записать в `docs/security-model.md` при S4 |
| G9 | **Free space P2** (`statfs` nr 137 есть, UI нет) | нет индикатора заполнения | S4+; в acceptance не входит |
| G10 | **`OpenPath` — заглушка** (track B) | файл с флешки не открывается | отдельный контракт открытия; не блокирует v1 |
| G11 | **Две реализации протокола** (`pagh-desktop/src/protocol.rs` и `pagh-ui/src/protocol.rs`) | расхождение типов ломает клиентов | roundtrip-тесты в обоих; правка в одном PR |
| G12 | **Press-routing по окну** не в `dev` (ветка `fix/window-press-routing` не смержена) | клики по сайдбару в QEMU могут уходить в фон | приёмка S4 через `--device-selftest`; клики — ручная проверка/после мержа |
| G13 | **Хабы не поддерживаются**, только корневые порты | флешка в хабе не появится | kernel-спека; UX ничего не обещает |
| G14 | **Деградированный `/proc/partitions`** без vendor/fs_class | «USB-накопитель» без деталей | warning в serial; не приёмка |
| G15 | **Устройства с `sector_size != 512` и ёмкостью > 2 TiB** kernel-контракт отвергает (открытый вопрос review) | такие носители не появятся/покажут error | UI показывает `error`/`enumeration_failed`, если ядро вообще отдаст устройство; иначе — «не отдано ядром» |
| G16 | **Владелец авто-mount не решён** (ядро или userspace) | если userspace — композитор сам зовёт `mount(2)` при `ADD fs_class=1`; если ядро — UX только отображает | вопрос уходит с kernel-спекой на review; контракт PDP от этого не меняется |
| G17 | **`mount[16]` = 15 символов** | будущие длинные точки не влезут | Phase 1 ok; при вложенных путях держать `usbNpM` ≤ 15 символов; иначе — новый ABI |

**Зависимости:** `docs/usb-mass-storage-spec.md` (S1, включая `mount`/`umount2`),
OpenPath track B (v2 UX), стабильный `dev` (PR base).

---

## 11. Вопросы и решения для lead

1. **O1 — закрыт решением lead (2026-11-19).** Read-only mount чужого ext2 в
   Phase 1 не делаем; Ventoy проверяется на обнаружение/отображение/честный
   foreign-статус, read/write — на pagh-ext2 томе (`§8.3`). Остаётся future-
   этапом вместе с exFAT-драйвером.
2. **O2 — закрыт контрактом.** `mount(2)` 165 / `umount2(2)` 166, root-only,
   `source="usb0p1"` (имя, не путь), `target="/usb0p1"`, `fstype="ext2"`,
   `flags=0`, `data=0`; `/mnt`,`/proc`,`/dev`,`/tmp`,`/media` → `EINVAL`;
   `umount2(flags=0)`, `MNT_FORCE`/`MNT_DETACH` → `EINVAL`. S4 реализует
   `DeviceOps` поверх этих сисколлов.
3. **O3 — закрыт контрактом.** Top-level `usbNpM`; `/mnt`, `/proc`, `/dev`,
   `/tmp`, `/media` зарезервированы (`EINVAL`), коллизия с файлом невозможна
   (rootfs не writable). `/media` остаётся за будущими вложенными точками.
4. **O4 — «Безопасно извлечь» без mount.** Для `present`/`unmounted` eject —
   no-op с сообщением «можно отключать». Подтвердить, что это ожидаемое
   поведение, а не «кнопка должна отсутствовать».
5. **O5 — `--device-selftest` как продуктовая поверхность.** Оставляем скрытый
   флаг в `pagh-files` (как `--shot`) для e2e или считаем его тестовым и
   убираем после S4? Рекомендация: оставить — он проверяет те же пути кода на
   реальном железе без указателя.
6. **O6 — VID:PID и serial в `uevent`.** В финальной записи есть `serial[16]`
   (может быть пустым), VID:PID **нет**. Для v1 свойств достаточно
   vendor/product/serial; VID:PID — future-поле (новый ABI), не выдумываем.
7. **O7 — три вопроса kernel-спеки идут с ней на review, не блокируют UX:**
   владелец авто-mount (ядро vs userspace, G16); отказ `sector_size != 512`
   и > 2 TiB (G15); raw-узлы как future. Контракт PDP/`devices` от их решения
   не меняется.

---

## Приложение A. Пример снимка и сообщений

`/dev/uevent` (семантика, не байты; `kind` — числом 1–6, `seq=0` — снимок):

```text
ADD      seq=0 usb0   fs_class=0 flags=removable                    sectors=131072 ss=512 vendor="QEMU" product="QEMU HARDDISK"   # 64 MiB
ADD      seq=0 usb0p1 fs_class=1 flags=removable|mounted            mount="/usb0p1" sectors=129024                              # 63 MiB
CHANGE   seq=1 usb0p1 fs_class=1 flags=removable                    mount="" sectors=129024   # после umount2
CHANGE   seq=2 usb0p1 fs_class=1 flags=removable|safe_to_remove    mount=""                # detach прошёл, можно отключать
REMOVE   seq=3 usb0   flags=surprise                                                       # выдернули без извлечения
```

Снимок `devices` (18) после агрегации:

```text
generation=7 count=1
id="usb0" state=mounted fs_class=1 flags=mounted|removable reason=none
capacity=66060288 vendor="QEMU" product="QEMU HARDDISK" mount="/usb0p1" serial=""
```

`device_request` (19): `request_id=3 id="usb0" action=2 (unmount)`.
`device_result` (20): `request_id=3 status=0 message=""` → затем `devices`
`generation=8`, `state=unmounted`, `mount=""`.

Ошибка занятого тома: `device_result{status=1, message="Устройство используется
другой программой"}`; снимок остаётся `state=mounted`, `reason=none`, а
`flags.in_use` — это флаг, не `reason` (результат операции — текст, а не
состояние; UI показывает текст и оставляет строку смонтированной).

Ошибка flush при eject: `ERROR seq=4 usb0p1 error=3 (flush_failed)`
`flags=removable|mounted`, mount остаётся `/usb0p1`; `device_result{status=1,
message="Не удалось сбросить данные на носитель — не отключайте"}`; строка
остаётся `mounted` с `state=error`.

---

## Приложение B. Что проверено в дереве для этой спеки

`origin/dev@d5de8b2a`: `app.rs` (Command/PlaceKind/Place/default_places/
build_sidebar/prune_missing_devices/navigate_to/open_entries/on_double_click/
context menu/тесты), `vfs.rs`/`host_vfs.rs` (trait и std-реализация),
`menu_model.rs` (10 пунктов, ни одного про устройства), `main.rs`
(`OpenPath`→`println`), `icons.rs` + `assets/icons/README.md` (12 глифов, MIT
Fluent), `pagh-ui` (List/ListItem/font/theme), `pagh-desktop` (protocol 1–16,
server hello/run/launch, netinfo-прецедент, PROTOCOL.md), ядро (`vfs::mount_at`
и одноуровневое ограничение, `/proc/mounts`-формат, `boot.rs` `/mnt`, отсутствие
`umount`, `BlockDevice::flush`, WAL-flush, `statfs` 137), `tools/e2e.py`
(`boot`, `_desktop_prepare_disk`, режимы), `tools/mkimage.py` (GPT+раздел,
формат ядром при первой загрузке), `AGENTS.md` (пробел «USB mass storage нет»).