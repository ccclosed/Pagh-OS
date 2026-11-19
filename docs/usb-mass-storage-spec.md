# USB mass storage (Bulk-Only Transport): спецификация драйвера

**Статус:** RESEARCH + СПЕЦИФИКАЦИЯ. Код по этому документу ещё не написан; реализация —
узкими PR после stable merge, с review lead. Ветка спеки базируется на `origin/dev`
(`d5de8b2a`), версия `Cargo.toml` намеренно **не** меняется (docs-only).

**Задача:** флешка (USB mass storage, BOT/SCSI) должна появиться в Files: устройство видно,
pagh-том монтируется, читается и пишется, извлекается безопасно; QEMU-приёмка и реальная
Ventoy-флешка — по шагам пользователя после QEMU, без заглушек.

**Что этот документ НЕ делает:** не обещает поддержку exFAT/FAT/NTFS, UAS, хабов,
EHCI/UHCI/OHCI, изохронных передач и multi-LUN в Phase 1. Каждое из этих ограничений
названо явно в §1.2 и §16, а не подразумевается.

---

## 0. Решения одной таблицей

| Вопрос | Решение Phase 1 | Почему |
|---|---|---|
| Контроллер | только xHCI (`class 0x0C/subclass 0x03`) | уже есть рабочий драйвер; EHCI/UHCI — другой scheduling и ноль переиспользования (§4) |
| Транспорт | Bulk-Only Transport (BOT), bulk-пара | требование задачи; UAS — future |
| SCSI | SCSI transparent, PDT 0x00 (direct access) | флешки; CD-ROM/tape не поддерживаются |
| Блочные имена | `usb0`, `usb0p1`…`usb0pN`; счётчик `usbN` монотонный, не переиспользуется | совпадает с `nvme0pN`-схемой; старые fd не алиасятся на новое устройство; **`usbN` не стабилен между reboot** — персистентность по UUID/label (§10.1) |
| Точки монтирования | `/usb0p1` (top-level) | `vfs::mount_at` поддерживает только односоставные точки (§11.1); `/media/...` — отдельный этап вложенных mount points |
| Монтирование | `mount(2)` nr 165 / `umount2(2)` nr 166, root-only | Linux-стандарт, не нужен новый блочный узел в `/dev`, укладывается в трёхправный контракт `abi.rs` |
| Авто-mount | **userspace-driven**: ядро только enumerate→`ADD` с `fs_class`; root-композитор монтирует pagh-том через `mount(2)`; ядро на hotplug **не** монтирует | policy ownership остаётся в userspace; boot-mount flow не меняется; ядро не пишет ФС по своей инициативе (§11.3) |
| События | `/dev/uevent` **v2: 144 B**, `mount[16]` + `uuid[16]`, push + initial snapshot, `seq`/`ioctl` re-seed | event0-подобный контракт; polling `/proc/mounts` теряет транзиенты |
| Сырые блочные узлы | `/dev/block/<name>` — **future**, не Phase 1 | Files хватает mount-точки; raw-write в смонтированный том — footgun (§10.3) |
| Flush | `BlockDevice::flush` → SCSI `SYNCHRONIZE CACHE(10)` 0x35 | WAL-durability (issue #15) без этого ложна (§12.3) |
| Безопасное извлечение | clean: flush→detach→`safe_to_remove=1`; surprise: EIO+detach, `unclean=1`, запись запрещена, `safe_to_remove=0` до clean unmount после WAL-replay или явного `MNT_FORCE`; порт не обесточивается | физическое выдёргивание — пользователь; loss без replay не выдаётся за safe (§13) |
| Размер логического блока | Phase 1 только 512 B; иное — явный `error=unsupported_block`, без усечения | partition/ext2 слой адресует 512 B секторы; 4K — future |
| Ёмкость > 2 TiB | Phase 1 явный `error=capacity_overflow`, без усечения; READ(16)/WRITE(16) — следующий этап | 32-битный LBA READ(10) не покрывает; CDB-билдеры готовы заранее |

---

## 1. Область и не-цели

### 1.1. Входит в Phase 1

1. xHCI root-порты, одно устройство mass storage на порт (несколько устройств на разных
   портах — да; несколько LUN одного устройства — нет).
2. USB BOT: `CBW` 31 B, data stage, `CSW` 13 B, `GET_MAX_LUN`, Bulk-Only Reset,
   `CLEAR_FEATURE(ENDPOINT_HALT)`, Reset Endpoint, STALL/phase-error recovery.
3. SCSI: `INQUIRY`, `TEST UNIT READY`, `READ CAPACITY(10)`, `READ(10)`, `WRITE(10)`,
   `REQUEST SENSE`, `SYNCHRONIZE CACHE(10)`; `READ CAPACITY(16)`/`READ(16)`/`WRITE(16)` —
   билдеры и разбор готовы, включение — после проверки на >2 TiB.
4. Разделы: reuse `drivers::partition` (GPT primary + MBR primary), плюс fallback
   «superfloppy» (ext2 без таблицы разделов) как отдельное целое устройство.
5. BlockDevice `usbN`/`usbNpM`, `sector_count`, `flush`, проверка границ как у
   `PartitionSlice`. RMW не нужен: все вызывающие (ext2, partition) передают длину,
   кратную 512 B; иное — `EINVAL`, а не тихая запись соседних секторов.
6. Монтирование pagh-ext2+WAL **по `mount(2)` от root-композитора**; `umount2(2)`, busy-проверка, flush на eject. Ядро не делает auto-mount.
7. `/dev/uevent` + `/proc/partitions`, initial snapshot, hotplug ADD/REMOVE/CHANGE/ERROR.
8. Hot-unplug: teardown слота, dead-флаг, EIO, WAL replay при следующем mount.
9. QEMU-приёмка `usb-storage`; реальная Ventoy-флешка (pagh-раздел) — проверяет пользователь по шагам, до этого «работает на Ventoy» не утверждается.

### 1.2. Не входит (и не будет обещаться в этом PR)

| Что | Почему / когда |
|---|---|
| exFAT/FAT/NTFS/ext4 mount | нет драйверов ФС; Ventoy data-раздел обычно exFAT — устройство **видно**, но `fs_class=other_fs`, mount запрещён с причиной. exFAT — отдельный будущий этап. |
| UAS (USB Attached SCSI) | BOT достаточен для флешек; UAS — очередь команд и другой протокол. |
| Хабы | в ядре нет hub-драйвера; устройство за хабом невидимо (§6.4). |
| EHCI/UHCI/OHCI | другой контроллер и scheduling; xHCI покрывает QEMU и современное железо. |
| Multi-LUN | `GET_MAX_LUN` читается; LUN>0 логируется и пропускается (§7.1). |
| 4K logical block | partition/ext2 слой — 512 B; отказ с явным логом. |
| CD-ROM/tape/PDT != 0x00 | не mass-storage диск; честный `unsupported`. |
| Eject с power-off порта | safe removal = flush+detach; питание порта не трогаем. |
| `/dev/block/*` raw-узлы | future; mount(2) по имени достаточно (§10.3). |
| Запись в foreign ext2 | mount чужого ext2 переписывает group descriptors; запрещено policy. |
| Изохронные передачи / suspend-resume / LPM | вне задачи. |

---

## 2. Референсы и лицензионная граница

Код Pagh-OS — MIT. **Никакой GPL-код не копируется.** Linux-драйверы
(`usb-storage`, `xhci-hcd`, `sd`) — только поведенческий референс: порядок команд,
тайминги, обработка ошибок. Публичные спецификации и MIT/BSD-примеры — источники фактов.

| Источник | Что взято (поведение, не код) |
|---|---|
| [USB Mass Storage Class — Bulk-Only Transport 1.0](https://www.usb.org/document-library/mass-storage-bulk-only-10) | CBW/CSW, сигнатуры, class-запросы, Reset Recovery, 30-секундное ожидание CSW |
| [USB 2.0 specification](https://www.usb.org/documents) | chapter 9 (дескрипторы, SET_CONFIGURATION, CLEAR_FEATURE), bulk-семантика |
| [xHCI 1.2 specification](https://www.intel.com/content/www/us/en/architecture-and-technology/universal-serial-bus/extensible-host-controler-interface-usb-xhci.html) | TRB, transfer event, completion codes, Reset Endpoint, PORTSC |
| [TinyUSB host classes](https://docs.tinyusb.org/en/latest/reference/class/host.html) | host-модель: mount callback, асинхронные transfer, lifetime буферов |
| [TinyUSB CDC/MSC/HID example](https://docs.tinyusb.org/en/stable/examples/host/cdc_msc_hid.html) | MSC на mount делает INQUIRY и печатает vendor/product/revision + capacity |
| [SerenityOS USB tour](https://quaker762.github.io/Serenity-USB/) | слои Host Controller / USB core / device driver, enumeration 8→18 байт, pipe/transfer |
| [Redox `usbscsid`](https://github.com/redox-os/drivers) (MIT) | поведенческий пример: GET_MAX_LUN (0xFE), Bulk-Only Reset (0xFF), CSW signature/tag, Reset Recovery, READ16/WRITE16, MODE SENSE fallback |
| [USB mass storage device class (Wikipedia)](https://en.wikipedia.org/wiki/USB_mass_storage_device_class) | обзор MSC, subclasses, «no particular FS required» |
| [SCSI command set (Wikipedia)](https://en.wikipedia.org/wiki/SCSI_command) | CDB, status CHECK CONDITION → REQUEST SENSE, LUN, READ/WRITE варианты |

SCSI SBC/SPC (T10) цитируются по именам: SBC-3 (`READ(10)`/`WRITE(10)`/`READ CAPACITY`),
SPC-4 (`INQUIRY`, `REQUEST SENSE`, sense key/ASC/ASCQ). Номера опкодов и раскладки CDB ниже
даны по этим стандартам.

Лицензии цитируемых проектов: TinyUSB — MIT, Redox `drivers` — MIT, SerenityOS —
BSD-2-Clause. Ни один из них не копируется: берутся только наблюдаемое поведение,
порядок команд и тайминги; реализации в Pagh-OS пишутся заново под MIT.

---

## 3. Инвентарь текущего ядра: что есть и чего нет

Проверено чтением кода на `origin/dev` (`d5de8b2a`). Это не пересказ README: у каждой
строки ниже есть файл и функция.

### 3.1. xHCI — есть, но только HID и RNDIS

`src/drivers/xhci.rs` (3514 строк):

* `init()` (`:1010`) ищет PCI `class=0x0C/subclass=0x03`, выбирает контроллер с наибольшим
  числом подключённых портов, делает BIOS/USB-legacy handoff (`bios_handoff`, `:1126`),
  сбрасывает контроллер, поднимает command/event rings, DCBAA, scratchpad.
* PCI: `pci::enable_bus_master()` включается до MMIO (`start_controller`, `:1189`);
  BAR0 — 64-битный memory BAR (`map_bar0`, `:1079`); `pci` — legacy config space
  0xCF8/0xCFC, class/subclass из offset 0x08 (`src/drivers/pci/mod.rs`).
* `control_in` (`:1932`), `control_no_data` (`:2000`), `control_out` (`:2055`) — один
  DMA-буфер `Slot.ctrl_buf` на страницу; `len > PAGE_SIZE` отвергается. Это транспорт
  EP0 для дескрипторов и class-запросов.
* `configure_endpoints` (`:2367`) умеет **только** HID interrupt-IN и RNDIS bulk-пару;
  `enumerate_slot` (`:2630`) ищет в configuration descriptor HID/RNDIS и при отсутствии
  обоих завершает перечисление `no-supported-interface`. **MSC-флешка сейчас
  отвергается именно здесь.**
* `poll()` (`:2902`) — один проход: политика одного порта, вооружение HID interrupt-IN,
  RNDIS bulk-IN/OUT, сбор transfer events. `usb_thread` (`:3405`) — `poll();
  sleep_ticks(1)`.
* `reset_endpoint` (`:3329`) — xHCI Reset Endpoint, используется для HID/RNDIS recovery;
  тот же примитив нужен MSC.
* `Slot` (`:528`) содержит `ep0`, `input_ctx`, `device_ctx`, `ctrl_buf`, `hid: Vec<...>`,
  `rndis: Option<...>`; **поля `msc` нет**.
* `Ring` (`:261`) — одна страница, 64 TRB, один `outstanding`; для bulk-передачи
  пригоден, но общего API «отправить N байт и дождаться» нет: RNDIS делает это вручную
  в `poll()`.

**Чего нет:** generic bulk transfer API; MSC-дескрипторов; `GET_MAX_LUN`; BOT; teardown
слота при отключении; поддержки хабов (device class 9 отвергается, `enumerate_slot`:
`hub-unsupported`); EHCI/UHCI/OHCI; IRQ (всё поллится); multi-controller (выбирается один).

### 3.2. Разбор дескрипторов

`src/drivers/usb_desc.rs` (332 строки):

* `parse_hid_interfaces(conf)` — все HID-интерфейсы (class 3) с interrupt-IN,
  `bAlternateSetting == 0`.
* `parse_rndis_interfaces(conf)` — communication + CDC Data по тройкам
  `EF/04/01`, `E0/01/03`, `02/02/FF`.
* **MSC (class 8, subclass 6, protocol 0x50) не разбирается.** Нужен
  `parse_msc_interfaces` с bulk-IN/bulk-OUT и `wMaxPacketSize`.

### 3.3. Блочный слой

`src/drivers/mod.rs`:

* `trait BlockDevice` (`:60`): `name`, `read_block`, `write_block`, `sector_count`
  (512-байтовые секторы), `flush` (default no-op).
* `DeviceManager` (`:92`) — `BTreeMap` имён; `register_block`, `list_blocks`, `get_block`.
* Блочные устройства **не являются VFS-узлами**: в `/dev` есть только
  `null/zero/full/serial/fb0/input/event0/vtconsole0/vtinput` (`vfs::init`, `:607`).
  Shell `disk` видит их через `drivers::list_blocks()`, а не через `/dev`.

`src/drivers/nvme.rs` (1130 строк) — образец polled block-драйвера: PRP, до 4 команд в
полёте, `flush` = NVMe FLUSH (opcode 0x00), RMW для невыровненных записей, один namespace.
NVMe **синхронный**: syscall-поток крутит phase-bit, IRQ нет.

### 3.4. Разделы

`src/drivers/partition.rs` (401 строка):

* GPT: сигнатура `EFI PART` в LBA 1, массив записей; **primary header только**, backup
  не читается (комментарий модуля обещает больше, чем код — это отдельная находка, не
  предмет данной спеки).
* MBR: 4 primary, extended (0x05/0x0F/0x85) не разбираются.
* `register_partitions(dev)` (`:142`) создаёт `PartitionSlice` с именем
  `format!("{}p{}", dev.name(), index+1)` и регистрирует каждый раздел.
* `candidates()` (`:368`) берёт только имена с `p<digits>`; целое устройство без таблицы
  разделов (superfloppy) в кандидаты не попадает.
* `classify()` (`:347`) — `PaghVolume` / `ForeignExt2` / `Blank` / `Other`.

### 3.5. VFS и mount

`src/vfs/mod.rs`:

* `VfsNode` (`:84`) — `read`/`write` по байтовому смещению, `lookup`, `readdir`, `sync`,
  device-предикаты (`is_fbdev`, `is_evdev`, …).
* `mount_at` (`:678`) / `mount_at_named` (`:684`) — **только одна компонента пути**:
  `"/mnt"` да, `"/media/x"` нет (`name.contains('/')` → `EINVAL`). Повторное
  монтирование того же имени заменяет запись; **`unmount_at` отсутствует**.
* `MOUNTS` + `mount_snapshot()` (`:599`) питают `/proc/mounts`.
* `resolve_path` (`src/arch/x86_64/linux/io_sys.rs:1202`) переписывает гостевые
  абсолютные пути под `/mnt`, кроме первых компонент из
  `ROOT_KEEPING_COMPONENTS = ["mnt","dev","proc","sys","tmp"]`
  (`src/arch/x86_64/linux/io.rs:31`). **`/usb0p1` без правки этого списка станет
  `/mnt/usb0p1`** — обязательная интеграционная правка (§11.2).

### 3.6. ext2 + WAL

`src/fs/ext2/mod.rs`:

* `Ext2Fs::mount_fs` (`:1083`): читает суперблок, `Journal::open` + `recover`, затем
  `reconcile_free_counts` — **монтирование пишет** group descriptors и резервирует WAL.
  Поэтому «чужой ext2» нельзя монтировать без явного разрешения: это чужая ФС.
* `Ext2Fs::mount` (`:1107`) отдаёт корневой `VfsNode`.
* `Tx::commit` (`:618`) держит `tx_lock: Spinlock<()>`; data-блоки пишутся напрямую
  (ordered), метаданные — через WAL. `Journal::commit` (`src/fs/journal.rs`) делает
  `dev.flush()` после commit-записи и после checkpoint.
* `Ext2Fs` **не имеет `Drop`/unmount/ro-режима**; после detach узлы живут, пока их держат
  fd/Arc.

### 3.7. boot и потоки

`src/boot.rs`:

* `init_usb()` (`:335`) — `xhci::init(&pci::enumerate())`.
* `init_fs()` (`:413`) — монтирует `/mnt` один раз, до `kernel_main`; NVMe/virtio и
  boot-menu. **Динамического mount пути нет.**
* `kernel_main` (`:793`) спавнит `"usb"`-поток (`:817`) безусловно; `usb_thread` —
  `poll(); sleep_ticks(1)`.

### 3.8. Desktop / Files

* `pagh-desktop` — userspace-композитор: `/dev/fb0`, `/dev/input/event0`, PDP.
  Устройств/дисков в его протоколе сейчас нет.
* `pagh-files` — отдельное приложение с `HostVfs` (`std::fs`); боковая панель
  «Устройства» — статический `/mnt` с label `"Basic data partition"`
  (`app.rs:2320`); `prune_missing_devices()` скрывает точку, если путь не резолвится.
* `/dev/uevent` отсутствует; push-событий «устройство добавлено/удалено» нет.
* `statfs(2)` nr 137 **есть**: `abi.rs` `nr::STATFS`, `io_sys.rs:3568` `sys_statfs`,
  `:3580` `sys_fstatfs`; ext2 отдаёт реальные `FsStat` (`ext2/mod.rs:1826`, `:1929`,
  `:2026`). Free space в uevent не кладём — Files считает сам после mount.

### 3.9. Пробелы, уже названные в AGENTS.md

`USB mass storage не реализован (boot-диск как RAM-disk), хабы не поддержаны,
hot-unplug нет, interrupt-IN уведомления RNDIS нет`. Этот документ закрывает первый
пункт и добавляет hot-unplug для MSC; хабы и EHCI остаются названными пробелами.

### 3.10. Память и куча

`src/memory/heap.rs`:

* глобальный аллокатор — `good_memory_allocator` (galloc) с IRQ-safe обёрткой
  (выключает прерывания на время allocate/free, чтобы таймер не вытеснил владельца
  лока кучи);
* куча **фиксированного** размера: `HEAP_INITIAL_PAGES = 131072` (512 MiB) —
  желаемый размер, `cap_to_available()` урезает его по доступной RAM; galloc не
  растёт по требованию, неудачная аллокация — abort;
* DMA-кадры берутся из PMM (`alloc_frame`, `alloc_frames_contiguous(count)`);
  contiguous-аллокация 16 страниц (64 KiB) может не удаться после фрагментации —
  поэтому у MSC есть fallback 4 KiB (§7.1).

Требование к драйверу: **не аллоцировать на каждую BOT-команду**. Все буферы
(`cbw_buf`, `csw_buf`, `data_buf`, кольца) создаются один раз на `MscLink`; на
команду — только копирование/заполнение. `Arc<MscDevice>`/`Vec<MscDevice>` — один раз
на устройство. Иначе куча (не growable) и PMM будут фрагментироваться на каждом
чтении/записи, а abort кучи — машина.

---

## 4. Scope контроллера: только xHCI

Phase 1 — **xHCI only**. Причины:

1. Драйвер xHCI уже есть и проверен в QEMU (HID, RNDIS), а HID-путь дополнительно
   правился по реальным устройствам (2.22.7): enumeration, PORTSC policy,
   control/bulk/interrupt, Reset Endpoint — переиспользуются.
2. EHCI (class `0x0C/0x20`), UHCI (`0x0C/0x00`), OHCI (`0x0C/0x10`) имеют другой
   scheduling (periodic frames, QH/qTD, ED/TD) и не дают ни одной строки переиспользования.
3. QEMU (`qemu-xhci`) и машины после ~2012 имеют xHCI; отсутствие EHCI-пути — честно
   названное ограничение, а не скрытый fallback.

Multi-controller: `xhci::init` выбирает один контроллер (с наибольшим числом подключённых
портов). Второй контроллер в Phase 1 не обслуживается; если флешка воткнута в него —
устройства не будет. Это ограничение фиксируется в README при реализации.

---

## 5. PCI init (как есть, что требуется)

Существующий путь достаточен, менять его не нужно:

1. `drivers::pci::enumerate()` обходит bus/device/function через 0xCF8/0xCFC, читает
   vendor/device/class/subclass/header type.
2. `xhci::init` фильтрует `class == 0x0C && subclass == 0x03`.
3. `enable_bus_master` ставит memory-space + bus-master в command register (offset 0x04).
4. `map_bar0` проверяет 64-битный memory BAR (биты 0x4) и мапит 64 KiB MMIO через
   `vmm::map_mmio`; BAR size-probing не делается.
5. BIOS/USB legacy handoff: OS-owned семафор, ожидание снятия BIOS-owned, ограниченный
   таймаут.
6. Reset контроллера: stop → HCRST → wait CNR=0 → DCBAAP/CRCR/ERST/ERDP → run.

Требование к MSC: ничего нового в PCI не добавляется. Если контроллер не найден —
`xhci::poll` no-op, USB-флешка не появится; это штатное поведение, а не ошибка загрузки.

---

## 6. Enumeration, хабы, hotplug

### 6.1. Текущий путь (факты)

`poll_port` (`:1644`) сначала проверяет `ctrl.slots.iter().any(|s| s.port == port)` и
**возвращается, если слот уже есть**. Поэтому:

* отключение устройства не замечается: слот остаётся, порт больше не перечисляется;
* `brought_up` — только диагностика, не teardown;
* повторное подключение в тот же порт без перезагрузки не работает.

`enumerate_port` (`:1754`) после reset подтверждает change-биты, проверяет PED+U0,
вызывает `enumerate_with_speed`; при неудаче `release_slot` (`Disable Slot` + очистка
DCBAA) и backoff. Это единственный существующий teardown.

### 6.2. Что добавляет MSC

1. `usb_desc::parse_msc_interfaces(conf)` — интерфейс `class=8, subclass=6,
   protocol=0x50`, первый bulk-IN и bulk-OUT, `wMaxPacketSize`, `bInterfaceNumber`,
   `bConfigurationValue`. Только `bAlternateSetting == 0`.
2. `enumerate_slot` получает ветку MSC: если HID/RNDIS нет, но MSC есть — SET_CONFIGURATION
   по `bConfigurationValue`, `GET_MAX_LUN`, `Configure Endpoint` с bulk-парой.
3. `Slot` получает `msc: Option<Arc<MscLink>>`; `configure_endpoints` расширяется
   аргументом MSC (или отдельной веткой), чтобы одна команда Configure Endpoint
   покрывала все DCI.
4. Смешанное устройство (HID + MSC, например клавиатура с fdd) — обе ветки
   конфигурируются одной командой; HID продолжает работать.
5. `GET_MAX_LUN` (bmRequestType `0xA1`, bRequest `0xFE`, wIndex = interface, 1 байт):
   если устройство STALL-ит — LUN 0 (спека BOT разрешает). `max_lun > 0` → лог
   `usb-msc: max_lun=N; LUN>0 unsupported in phase 1`, обслуживается только LUN 0.

### 6.3. Адресация и слоты

* Enable Slot → Address Device (EP0) → GET_DESCRIPTOR 8/18 → SET_CONFIGURATION →
  GET_MAX_LUN → Configure Endpoint. Тот же порядок, что HID/RNDIS.
* Слот живёт, пока устройство подключено. При ошибке конфигурации — `release_slot`,
  как сейчас.
* `MscLink` владеет кольцами и DMA-буферами (см. §7.1), а не `Slot`: это позволяет
  `Arc<MscDevice>` пережить выключение слота и не дать UAF открытым fd.

### 6.4. Хабы

Сейчас `enumerate_slot` видит `bDeviceClass == 9` и пишет `hub-unsupported`, устройство
за хабом невидимо. Phase 1 это не меняет: поддержка хаба требует hub-драйвера
(дескриптор hub, port status/change, SET_PORT_FEATURE, address assignment за портом,
TT/split для LS/FS за HS-хабом). Многие корпуса маршрутизируют передние порты через
внутренний хаб — это честный риск (§16); на QEMU `qemu-xhci` порты корневые.

### 6.5. Hotplug и teardown (план)

Добавляется в `poll_port` до early-return:

1. Для каждого слота проверить `PORTSC.CCS` порта слота (один порт за poll — уже так).
2. `CCS=0` (или `CSC` без последующего подключения) → `disconnect_slot`:
   * `msc.dead = true` (AtomicBool) **до** любых освобождений;
   * `DeviceManager::unregister_block("usbNpM"/"usbN")` — новые `get_block` больше
     не видят устройство, старые `Arc` (fd/Ext2Fs) живут;
   * `Command(Disable Slot)`; очистить DCBAA[slot_id]; убрать `Slot`;
   * `usb_uevent::push(REMOVE, dev, flags.surprise)`; если том был смонтирован —
     `CHANGE/ERROR` с `error=io`, mount остаётся «stale» (I/O→EIO) до `umount2`;
   * `brought_up` очищается для порта; следующий poll снова поднимает порт.
3. Новое устройство получает следующий `usbN` (монотонный счётчик), не переиспользует
   имя; `usb0p1` старого устройства больше не существует.
4. `poll_port` больше не делает early-return по `slot.port`; вместо этого — явная
   проверка disconnect.

Latency: один порт за `poll` (1 мс sleep) + debounce; исчезновение замечается за
несколько мс–десятки мс. Это не IRQ, но достаточно для UI и безопасного teardown.

---

## 7. USB Bulk-Only Transport (BOT)

### 7.1. Конечные точки и буферы

MSC-интерфейс даёт ровно две bulk-точки: bulk-IN (данные/CSW к хосту) и bulk-OUT
(CBW/данные к устройству). Interrupt-точки не опрашиваются.

`MscLink` (создаётся в `configure_endpoints`, живёт в `Arc`):

| Поле | Размер | Назначение |
|---|---|---|
| `cbw_buf` | 32 B (31 значащий) | CBW, DMA |
| `csw_buf` | 16 B (13 значащих) | CSW, DMA |
| `data_buf` | 64 KiB contiguous (`pmm::alloc_frames_contiguous(16)`) | data stage |
| `in_ring` / `out_ring` | `Ring` (64 TRB, страница) | bulk-IN / bulk-OUT |
| `in_dci` / `out_dci` | u8 | DCI из адресов точек |
| `max_in` / `max_out` | u16 | `wMaxPacketSize & 0x7FF` |
| `tag` | u32 | монотонный CBW tag |
| `dead` | `AtomicBool` | устройство отключено/сбой |
| `recoveries` | u8 | счётчик Reset Recovery |

Почему 64 KiB: `xHCI` TRB transfer length — 17 бит (до 128 KiB); `alloc_frames_contiguous`
уже есть в `pmm` (`:350`). Если contiguous-аллокация не удалась — fallback на 4 KiB
(`alloc_frame`, 8 секторов за команду) с предупреждением; это медленнее, но корректно.
Один Normal TRB на data stage с IOC; CBW/CSW — отдельные Normal TRB.

### 7.2. CBW — 31 байт

| Offset | Размер | Поле | Значение |
|---|---|---|---|
| 0 | 4 | `dCBWSignature` | `0x43425355` (`"USBC"`, LE) |
| 4 | 4 | `dCBWTag` | `tag`, эхо в CSW |
| 8 | 4 | `dCBWDataTransferLength` | длина data stage, 0 если нет |
| 12 | 1 | `bmCBWFlags` | bit7: 1 = IN (device→host), 0 = OUT |
| 13 | 1 | `bCBWLUN` | 0 (Phase 1) |
| 14 | 1 | `bCBWCBLength` | 1..16 |
| 15 | 16 | `CBWCB` | CDB, остаток нули |

Передача: bulk-OUT ровно 31 B; short write/ошибка → Reset Recovery (§7.6).

### 7.3. Data stage

* `dCBWDataTransferLength > 0` и IN: один bulk-IN TRB на `min(len, 64 KiB)`; short packet
  допустим (device может вернуть меньше), остаток учитывается по `residue`.
* OUT: один bulk-OUT TRB на `len`; short write — ошибка (данные не доехали).
* `len = 0` — стадии нет (например, TUR, SYNCHRONIZE CACHE с length 0).
* Команды > 64 KiB режутся на несколько BOT-команд по 64 KiB на уровне BlockDevice
  (`read_block`/`write_block`), а не одним CBW: так проще residue/timeout.

### 7.4. CSW — 13 байт

| Offset | Размер | Поле | Значение |
|---|---|---|---|
| 0 | 4 | `dCSWSignature` | `0x53425355` (`"USBS"`, LE) |
| 4 | 4 | `dCSWTag` | должен совпасть с CBW tag |
| 8 | 4 | `dCSWDataResidue` | сколько байт data stage не передано |
| 12 | 1 | `bCSWStatus` | 0 Passed, 1 Failed, 2 Phase Error |

Правила:

* ровно 13 B; short read CSW → Reset Recovery, одна повторная попытка;
* signature/tag mismatch → Reset Recovery; если после recovery снова mismatch —
  `EIO` и `dead` (устройство врёт);
* `status=1 Failed` → `REQUEST SENSE` (кроме самой REQUEST SENSE), разбор §8.5;
* `status=2 Phase Error` → Reset Recovery + `EIO`;
* `residue`: для IN допустим (short data), для OUT обязан быть 0; иначе `EIO`.

### 7.5. Последовательность команды

```
host: bulk-OUT CBW(31)
host: [bulk-IN data | bulk-OUT data]        (если length > 0)
host: bulk-IN CSW(13)
```

Всё — под `MscLink`-транзакционным `Spinlock`, один CBW в полёте на устройство. BOT не
поддерживает очередь команд; параллельные BlockDevice-вызовы сериализуются.

### 7.6. STALL и Reset Recovery

Точный порядок (BOT 1.0 §5.3.4; поведение Redox/Linux — референс):

1. Если bulk-IN или bulk-OUT застрял (xHCI completion code 6 = STALL):
   * `CLEAR_FEATURE(ENDPOINT_HALT)` на адрес точки (bmRequestType `0x02`, bRequest 1,
     wValue 0, wIndex = ep address);
   * xHCI `Reset Endpoint` (TRB type 14) для DCI;
   * переармировать ring (снять `outstanding`).
2. Если после этого transfer снова STALL или CSW невалиден:
   * Bulk-Only Mass Storage Reset: bmRequestType `0x21`, bRequest `0xFF`, wValue 0,
     wIndex = interface, no data;
   * `CLEAR_FEATURE(HALT)` для bulk-IN и bulk-OUT (обе точки, даже если не halted —
     ошибку игнорировать);
   * `Reset Endpoint` для обеих DCI;
   * сбросить tag/outstanding, повторить команду **один** раз.
3. Три неудачных recovery подряд → `MscLink.dead = true`, `EIO`, uevent `ERROR`
   (`error=io`); том, если смонтирован, остаётся, но новые I/O дают `EIO`.

xHCI-специфика: после transfer error эндпоинт Halted; простая переармировка TRB даёт ту
же ошибку. `reset_endpoint` уже реализован (`xhci.rs:3329`) и используется HID/RNDIS —
для MSC он переиспользуется.

### 7.7. Таймауты и повторы

| Фаза | Таймаут | Действие по истечении |
|---|---|---|
| Control-запрос EP0 (дескрипторы, GET_MAX_LUN, CLEAR_FEATURE, BOT Reset) | 5 с | 2 повтора, затем fail enumerate / recovery |
| CBW (bulk-OUT 31 B) | 5 с | Reset Recovery, 1 повтор |
| Data stage | 10 с | Reset Recovery, 1 повтор |
| CSW | 30 с (BOT 1.0) | Reset Recovery, 1 повтор |
| TUR при media-not-ready | 10 × 100 мс | затем `EIO`/`not ready` |
| Reset Recovery | 3 попытки | `dead` |

Все ожидания — по TSC (`xhci::now_us`, калибровка PIT), а не по числу спин-итераций:
на реальном PCH USB2 reset/transfer занимает миллисекунды, а QEMU — микросекунды.

### 7.8. FLUSH

`BlockDevice::flush` для MSC = `SYNCHRONIZE CACHE(10)` (opcode 0x35, LBA=0, length=0 —
весь кэш). Если команда отвергнута `ILLEGAL REQUEST` (некоторые дешёвые мосты):
* **не** возвращать `Ok` молча; вернуть `Err(())`;
* WAL-commit тогда завершается `FsError::IoError` — вызывающий видит ошибку, а не
  «данные durable», которых нет;
* в лог: `usb-msc: SYNCHRONIZE CACHE unsupported; write-through durability is not
  guaranteed` — один раз на устройство.

Это осознанно строже, чем `BlockDevice::flush` default no-op: WAL-аргумент
(`src/fs/journal.rs`) требует реального flush.

### 7.9. Модель конкурентности: почему синхронно и где предел

`BlockDevice` — синхронный трейт, а `Ext2Fs::Tx::commit` держит `tx_lock: Spinlock<()>`
и вызывает `write_block`/`flush` внутри. `Spinlock::lock()` **выключает прерывания**
(`src/sync/spinlock.rs:43`). Значит:

* нельзя делать MSC-очередь с «разбудить usb-поток и поспать»: поток, держащий
  `tx_lock`, не даст тику/другому потоку работать, а `sleep_ticks` с выключенным IF
  не проснётся;
* MSC-команда должна выполняться **тем же потоком**, который вызвал `read_block`:
  взять `MscLink`-лок, взять `CONTROLLER`, отправить CBW/data/CSW, дождаться событий,
  отпустить. Это тот же паттерн, что `control_in` в boot-меню и NVMe phase-bit.

Следствие (главный риск, §16): на время одной BOT-команды прерывания выключены;
HID/сеть/планировщик стоят. Бюджет: CBW+data+CSW на QEMU — единицы мс; на реальной
флешке — до десятков мс, `SYNCHRONIZE CACHE` — до сотен мс. Phase-гейт: измерить
worst-case и не мержить, если одна команда > 250 мс (иначе UI замирает). Долгосрочное
решение — sleepable-лок для ext2-транзакций или асинхронный MSC-очередь; это отдельный
архитектурный PR, не входит в Phase 1.

Где выполняется mount: **не внутри `poll()`** (там уже held `CONTROLLER`; повторный
`lock()` — дедлок, спинлок не реентерабелен). После `poll()` `usb_thread` толкает
`Arc<MscDevice>` в очередь `usb_storage`; отдельный поток `usb-storage` делает
partition scan, classify и uevent; mount выполняет root-композитор через `mount(2)`.
Каждый `read_block` берёт/отпускает
`CONTROLLER`, поэтому HID-поток может вклиниться между командами.

---

## 8. SCSI

### 8.1. Команды Phase 1

| Команда | Opcode | CDB | Ответ | Зачем |
|---|---|---|---|---|
| TEST UNIT READY | `0x00` | 6 | нет | media ready; TUR-цикл |
| REQUEST SENSE | `0x03` | 6 | 18 B | после Failed |
| INQUIRY | `0x12` | 6 | ≥36 B | vendor/product/revision, PDT |
| READ CAPACITY(10) | `0x25` | 10 | 8 B | last LBA + block size |
| READ(10) | `0x28` | 10 | data | чтение |
| WRITE(10) | `0x2A` | 10 | нет | запись |
| SYNCHRONIZE CACHE(10) | `0x35` | 10 | нет | flush |
| READ CAPACITY(16) | `0x9E`/`0x10` | 16 | 32 B | >2 TiB (future) |
| READ(16) | `0x88` | 16 | data | >2 TiB (future) |
| WRITE(16) | `0x8A` | 16 | data | >2 TiB (future) |

`MODE SENSE(6/10)` не обязателен: READ CAPACITY даёт block size. Если устройство
возвращает block size != 512 — Phase 1 отказ `unsupported block size` (не пытаться
монтировать: partition/ext2 адресуют 512 B).

### 8.2. INQUIRY

CDB: `12 00 00 00 24 00` (EVPD=0, alloc=36). Разбор:

* byte 0: PDT (`& 0x1F`); Phase 1 требует `0x00` (direct access). Иное — `unsupported`.
* byte 1: RMB (removable) → `flags.removable`.
* bytes 8..16: vendor (8 ASCII), 16..32: product (16 ASCII), 32..36: revision.
* `additional_length` (byte 4) + 5 — реальная длина; буфер не менее 36.

Строки обрезаются до полей uevent (vendor 16, product 32); не-ASCII → `'?'`.

### 8.3. READ CAPACITY

`READ CAPACITY(10)` CDB: `25 00 00 00 00 00 00 00 00 00`; ответ 8 B:
`last_lba` BE u32 @0, `block_length` BE u32 @4. `sector_count = last_lba + 1`.

Phase 1 принимает **только** `block_length == 512`. Иное — явный отказ:
`error=unsupported_block` + лог `usb-msc: unsupported block size N (only 512)`;
устройство остаётся видимым (`ADD`/`ERROR`), но не регистрируется как BlockDevice и
не монтируется; никакого усечения/округления нет.

Если `last_lba == 0xFFFF_FFFF` — устройство может быть >2 TiB. Phase 1 отвечает
`error=capacity_overflow` + лог `usb-msc: capacity overflow; READ CAPACITY(16) required`
и **не** пытается угадать ёмкость; `READ CAPACITY(16)`
(CDB `9E 10 ... 00 20 00 00`, ответ `last_lba` BE u64 @0, `block_length` BE u32 @8)
реализуется билдером/разбором, но для принятия тома в Phase 1 не используется —
READ(10) всё равно ограничен 32-битным LBA. Включение RC16/READ16/WRITE16 —
следующий этап.

### 8.4. READ/WRITE

`READ(10)`: `28 <flags> <lba:4 BE> <group> <len:2 BE> <control>`; `WRITE(10)`: `2A ...`.
`flags`: DPO/FUA=0, RDPROTECT=0. `len` — число блоков (не байт), 1..65535.
`READ(16)`/`WRITE(16)`: opcode `88`/`8A`, LBA 8 B @2, length 4 B @10.

BlockDevice режет запрос на куски ≤ 64 KiB (128 секторов) и на ≤ 65535 блоков
(READ(10)); для Phase 1 64 KiB — меньший лимит.

### 8.5. REQUEST SENSE и политика ошибок

`REQUEST SENSE(6)`: `03 00 00 00 12 00` (alloc 18). Ответ:

* byte 0: response code (0x70 current / 0x71 deferred);
* byte 2: sense key (`& 0x0F`);
* byte 7: additional sense length;
* byte 12: ASC, byte 13: ASCQ.

| Key | ASC/ASCQ | Смысл | Действие |
|---|---|---|---|
| 0x0 | 00/00 | NO SENSE | успех |
| 0x2 | 3A/00 | medium not present | `EIO`, uevent `ERROR` (`no_media`) |
| 0x2 | 04/01 | becoming ready | TUR-цикл (10×100 мс) |
| 0x2 | 04/02 | initialize command required | TUR, затем повторить команду 1 раз |
| 0x3 | 11/00 | unrecovered read error | 1 повтор, затем `EIO` |
| 0x3 | 0C/00 | write error | 1 повтор, затем `EIO` |
| 0x5 | 20/00 | invalid command opcode | команда не поддерживается → `EIO`/`unsupported` |
| 0x5 | 24/00 | invalid field in CDB | баг драйвера; `EIO`, лог CDB |
| 0x6 | 28/00 | not ready to ready change | UNIT ATTENTION: TUR, повторить 1 раз |
| 0x6 | 29/00 | power on reset | UNIT ATTENTION: TUR, повторить 1 раз |
| 0x7 | 27/00 | write protected | `EROFS`, `flags.read_only` |
| 0xB | 00/00 | aborted command | Reset Recovery, 1 повтор |
| — | — | Phase Error (CSW=2) | Reset Recovery, `EIO` |

UNIT ATTENTION — не ошибка пользователя: после подключения/смены носителя устройство
может ответить им на первую команду; драйвер обязан «съесть» его TUR-циклом.

### 8.6. Retry-матрица (сводно)

| Событие | Повторы | После |
|---|---|---|
| STALL bulk | ClearFeature + Reset Endpoint | 1 повтор команды |
| CSW mismatch/phase error | Reset Recovery | 1 повтор, затем `dead` |
| UNIT ATTENTION | TUR | 1 повтор |
| READ/WRITE medium error | 1 | `EIO` |
| timeout CBW/data/CSW | Reset Recovery | 1 повтор, затем `dead` |
| `dead` | 0 | все I/O `EIO` до переподключения |

---

## 9. Разделы

После того как `usbN` отвечает на INQUIRY/READ CAPACITY и читает LBA 0:

1. `drivers::partition::register_partitions(&usb)` — reuse с одним дополнением:
   GPT (LBA 1, primary entries) → `usbNp1..`; иначе MBR (4 primary) → `usbNp1..`;
   extended не разбирается. Для GPT сохраняется unique partition GUID (байты 16..32
   записи) — он идёт в `uuid` uevent; у MBR UUID нет (нули), персистентность — по
   label/capacity (§10.1).
2. Если таблицы нет (`register_partitions` вернул 0) — **superfloppy fallback**:
   классифицировать целое `usbN` (`partition::classify`), и если это pagh/ext2 или
   blank-под-формат — работать с `usbN` как с томом. Это нужно для флешек, где ФС
   лежит с сектора 0 без MBR.
3. `usbNpM` — `PartitionSlice` поверх `usbN`; `sector_count` — из таблицы; reads/writes
   транслируются с проверкой границ (существующий код).
4. Hostile-таблица: `register_partitions` уже проверяет границы и лимит GPT-записей;
   никаких новых исключений.
5. Регистрация — в `DeviceManager` (`usbN`, `usbNpM`), затем `/proc/partitions` и
   uevent.

Имя `usbN` выдаётся при подключении; `N` монотонный, не переиспользуется. Раздел
получает `usbNpM`; после отключения имена исчезают навсегда (до перезагрузки).
Это защищает открытые fd/старые mount-точки от алиасинга на новое устройство.

---

## 10. Блочные устройства, имена и VFS

### 10.1. Канонические имена

| Имя | Что | `sector_count` |
|---|---|---|
| `usb0` | whole LUN 0 первого устройства | из READ CAPACITY |
| `usb0p1`…`usb0pN` | разделы (GPT/MBR) | из таблицы |
| `usb1` | второе устройство | … |
| `usb0l1` | LUN 1 (future) | … |

Phase 1: `GET_MAX_LUN` читается, но `max_lun > 0` → лог и обслуживается только LUN 0.
Имена `usbN` не совпадают с `nvme0`, `virtio-blk0`, `ram0`: `disk`-команда и
`partition::is_partition` продолжают работать без спецкейсов.

**`usbN` не стабилен между reboot.** Это счётчик enumerate, а не идентичность тома.
Персистентность (какой том открыть/куда вернуться) — по `uuid[16]` из uevent:
* pagh-ext2 том: `s_uuid` суперблока — идентичность **ФС** (при переформатировании
  меняется; это ожидаемо);
* иначе GPT-раздел: unique partition GUID (байты 16..32 записи GPT) — идентичность
  раздела;
* MBR/без таблицы: UUID нет (нули) — fallback `label` + `capacity` + vendor/product,
  честно менее надёжный. UI не должен хранить `usbN` между загрузками.

### 10.2. `/proc/partitions`

Диагностический текстовый файл (Linux-подобный, но major/minor синтетические):

```
major minor  #blocks  name
  240     0   1048576  usb0
  240     1   1046528  usb0p1
```

* major `240` зарезервирован за USB в этом ядре; minor = `16*N + (0 для whole,
  M для pM)`; `#blocks` — в 1024-байтовых единицах (как в Linux `/proc/partitions`).
  Это **не** Linux-совместимые major/minor, а стабильная диагностика.
* Файл рендерится из `DeviceManager` на `read(offset==0)` (как procfs), не snapshot.
* `/proc/partitions` — fallback для UI/диагностики, если `/dev/uevent` недоступен
  (старое ядро); Files в норме использует uevent.

### 10.3. `/dev/block/<name>` — будущее, не Phase 1

Причина: Files нужна **точка монтирования**, а не raw-блок. Raw-узел с записью в
смонтированный том позволяет обойти WAL и испортить ФС; read-only raw-узел полезен
только диагностике (`dd`/`hexdump`). Phase 1 не добавляет блочных VFS-узлов:
`mount(2)` принимает имя устройства, `disk`/`/proc/partitions` дают диагностику.
Если понадобится: `VfsNode` с байтовым offset → `read_block(offset/512)`, O_RDONLY,
и запись только при `O_EXCL` и отсутствии mount — отдельный PR с security-review.

### 10.4. Точки монтирования

* Phase 1: `/usb0p1`, `/usb1p2`, … — top-level, ровно одна компонента, работает с
  текущим `mount_at`.
* `mount[16]` в uevent — обязательное поле: `"/usb0p1"` или `""`; максимум 15
  символов + NUL. `/usb0p12` (8) и `/media/usb0p12` (14) влезают; более длинные имена —
  ABI v3.
* `/media/usb0p1` требует вложенных mount points (сейчас `mount_at` их отвергает) —
  отдельный этап; строка `mount` уже несёт точку, поэтому смена размещения не ломает
  потребителя, если имя влезает.

---

## 11. Монтирование: mount(2)/umount2(2)

### 11.1. Почему syscall, а не ioctl

`mount(2)` nr 165 / `umount2(2)` nr 166 — Linux-стандарт; userspace (композитор/Files)
не нужен новый блочный узел и второй ABI. Альтернатива «ioctl на control-device»
отвергнута: пришлось бы заводить `/dev/block` ради одной операции, а `mount` всё равно
остался бы нереализованным для будущих ФС.

### 11.2. `mount(2)` — контракт

```c
long mount(const char *source, const char *target,
           const char *filesystemtype, unsigned long mountflags,
           const void *data);          /* nr 165 */
```

* `source` — имя зарегистрированного блочного устройства, Phase 1: `usbN` или
  `usbNpM` (не путь). Иное → `ENODEV`.
* `target` — абсолютный путь; Phase 1 обязан быть **ровно** `"/" + source`
  (`/usb0p1` для `usb0p1`) и не длиннее 15 символов (граница `mount[16]`).
  `/mnt`, `/media`, `/proc`, `/dev`, `/tmp` и вложенные пути → `EINVAL`;
  длиннее 15 → `ENAMETOOLONG` + лог `bad_mount` (не молчаливое усечение).
* `filesystemtype` — `"ext2"` (единственный в Phase 1); иное → `EINVAL`.
* `mountflags` — 0; `MS_RDONLY`/`MS_REMOUNT`/прочее → `EINVAL` (ro-режим — future).
* `data` — 0/игнорируется.
* root-only: `euid != 0` → `EPERM` (как у привилегированных syscall; проверка current
  creds, не VFS). Files может быть non-root: mount/eject заказывает композитор
  (root) через PDP, а не сам Files.
* Проверки: устройства нет → `ENODEV`; `!is_pagh_volume` → `EINVAL` (чужой ext2 не
  монтируем, потому что mount пишет); уже смонтировано → `EBUSY`; `mount_at` занят
  другим устройством → `EBUSY`.
* Успех: `Ext2Fs::mount_fs` → `vfs::mount_at_named(target, root, "ext2")`; запись
  `MountOrigin::User`; uevent `MOUNTED` (`flags.mounted=1`, `error=0`, `mount=target`);
  вернуть 0.
* Ошибка mount_fs → `EIO` (или `EINVAL` для «не pagh»); uevent `ERROR`
  (`error=mount_failed`); mount не создаётся.

### 11.3. Auto-mount: userspace-driven (решение lead)

**Ядро не монтирует USB-том на hotplug.** Последовательность:

1. `usb-storage` enumerate → `usbN`/`usbNpM` зарегистрированы;
2. `ADD` в `/dev/uevent` с `fs_class` (`pagh_ext2`/`foreign_ext2`/`blank`/`other_fs`),
   capacity, vendor/product/label, `uuid`; `mount` пуст, пока том не смонтирован;
3. root-композитор получает `ADD`, проверяет `fs_class == pagh_ext2` и
   `flags.read_only == 0`, и только тогда вызывает
   `mount("usbNpM", "/usbNpM", "ext2", 0, 0)`; чужой/read-only том не монтируется;
4. ядро выполняет `mount(2)` (§11.2) и отдаёт `MOUNTED` с `mount=target` (§14.2).

Почему так:

* boot-mount flow (`boot.rs::init_fs`, `/mnt`) **не меняется**; USB — отдельный путь;
* ядро не принимает решение «этот том наш» по содержимому на hotplug: ownership —
  policy, а policy живёт в userspace-композиторе, который видит `fs_class`/UUID;
* ядро не пишет group descriptors по своей инициативе; только явный `mount(2)`;
* чужой ext2/exFAT остаётся видимым, но не монтируется.

`Files` (non-root) не вызывает `mount(2)` сам: он просит композитор по PDP; композитор
— root и вызывает syscall. Если композитор не запущен (headless boot), устройство
видно через `/proc/partitions`/uevent, но не смонтировано — это ожидаемое состояние,
а не ошибка. QEMU-приёмка использует `mountprobe` (§15.1), потому что в e2e нет
композитора.

Write-protect (sense `07/27`) → `flags.read_only=1`; композитор такой том не монтирует
(`Ext2Fs::mount_fs` пишет group descriptors); ro-mount — future.

### 11.4. `umount2(2)` — контракт, eject и surprise

```c
long umount2(const char *target, int flags);   /* nr 166 */
```

* root-only; `flags` — только `0` или `MNT_FORCE` (1). `MNT_DETACH` и прочие → `EINVAL`.
* `target` обязан быть USB-mount, созданным `mount(2)`; `/mnt`, `/proc`, `/dev` →
  `EINVAL`/`EPERM`.
* **busy (живое устройство)**: открытые fd под mount (`mount_id` на ext2-узлах + скан
  fd-таблицы) → `EBUSY`; том остаётся смонтированным. `MNT_FORCE` на живом устройстве
  → `EINVAL`: принудительный отрыв живой ФС не разрешён.
* **clean unmount (`flags=0`, устройство живо)**: `dev.flush()` →
  `SYNCHRONIZE CACHE(10)`. Ошибка → `EIO`, том **остаётся** смонтированным, uevent
  `ERROR` (`error=flush_failed`), UI не показывает «можно отключать». Успех →
  `vfs::unmount_at(target)`, `MOUNTS` обновляется, uevent `UNMOUNTED`:
  `flags.mounted=0`, `flags.detached=1`, `flags.unclean=0`, `flags.safe_to_remove=1`.
* **surprise/dead (`msc.dead`, устройство уже исчезло)**:
  * `flags=0`: detach разрешён, но это **не** clean unmount. Возврат `EIO`, uevent
    `UNMOUNTED`: `mounted=0`, `detached=1`, `unclean=1`, `safe_to_remove=0`.
    Новые записи запрещены (dead-флаг); открытые fd получают `EIO`.
  * `flags=MNT_FORCE`: **явное подтверждение пользователем возможной потери**.
    Detach, возврат 0, `unclean=1`, `detached=1`, `safe_to_remove=1`. Флаг допустим
    только для dead-устройства; на живом — `EINVAL`.
* **WAL replay**: при следующем появлении тома `mount(2)` выполняет
  `Journal::recover`. Успех снимает `unclean` (том можно монтировать), но
  `safe_to_remove` остаётся 0, пока том смонтирован; 1 — после clean `umount2`.
  `MNT_FORCE` на dead-устройстве — второй, явный путь к `safe_to_remove=1`
  (с `unclean=1`). Replay fail → `mount(2)` fail-closed (`EIO`), uevent `ERROR`
  (`error=replay_failed`), `safe_to_remove=0`; том не монтируется.
* Порт не обесточивается: физическое извлечение — пользователь. Power-off — future.

### 11.5. Путь в госте

`resolve_path` переписывает `/usb0p1` в `/mnt/usb0p1`, если не добавить `"usb"` в
`ROOT_KEEPING_COMPONENTS` (`src/arch/x86_64/linux/io.rs:31`). Требование:

* добавить `"usb"` (или предикат `usb` + цифра) в список;
* host-свойство обновить: `/usb0p1` сохраняет корень, `/usbfoo` — тоже (простое
  совпадение компоненты), `/mnt/usb0p1` — под /mnt;
* без этой правки Files/композитор откроют `/mnt/usb0p1` и получат `ENOENT`.

### 11.6. Трёхправный контракт ABI

Для `mount`/`umount2`:

1. `pub const MOUNT: u64 = 165; UMOUNT2: u64 = 166;` в `nr` (`abi.rs`).
2. Ветки в `linux_dispatch` → `io_sys::sys_mount` / `sys_umount2`.
3. `SUPPORTED_SYSCALLS` и независимый `EXPECTED` дополняются (168 → 170);
   host-свойство `supported_set_is_exact` и `EXPECTED`-сверка обязаны пройти.

Никаких изменений в `is_supported`-логике, кроме новых номеров.

---

## 12. ext2 + WAL: переиспользование и границы

### 12.1. Что переиспользуется

* `Ext2Fs::mount_fs` — валидация суперблока, `Journal::open` + `recover`,
  `reconcile_free_counts`.
* `Ext2Fs::mount` → `root_node()` для `vfs::mount_at_named`.
* `BlockDevice::sector_count` — размер тома для `format`/`statfs`.
* `Tx`/`Journal` — без изменений; MSC только реализует `read_block`/`write_block`/
  `flush`.

### 12.2. Ограничение: только pagh-том

`mount_fs` пишет group descriptors (reconcile) и резервирует WAL-блоки. Поэтому
через `mount(2)` монтируется **только** том с маркером
`Ext2Fs::is_pagh_volume` (WAL superblock, который пишет `Ext2Fs::format`). Чужой ext2:
* классифицируется `ForeignExt2`;
* виден в uevent (`fs_class=foreign_ext2`), mount запрещён (`EINVAL`) с логом
  `foreign ext2; refusing to mount (mount would rewrite group descriptors)`;
* форматирование — только явный `disk format`/boot-menu, не USB-драйвер.

### 12.3. Flush и durability

`Journal::commit` уже вызывает `dev.flush()` после commit-записи и после checkpoint
(`src/fs/journal.rs:227,278`). Для MSC `flush` — реальная SCSI-команда (§7.8). Если
устройство её не поддерживает, commit возвращает `IoError`; это лучше, чем объявить
WAL-durable без flush. **Clean** `umount2` делает дополнительный `dev.flush()` перед
detach; на dead-устройстве flush невозможен — detach идёт с `unclean=1` (§11.4).

### 12.4. Unmount и открытые fd

`Ext2Fs` не имеет `Drop`; `umount2` снимает mount-узел, но `Arc<Ext2Fs>` остаётся у
открытых fd. Политика:

* живое устройство: busy-проверка не даёт detach при открытых fd (`EBUSY`), поэтому
  штатный путь не оставляет «висящих» писателей;
* dead-устройство: detach разрешён (`EIO`, `unclean=1`), старые fd получают `EIO`
  на любую запись (dead-флаг проверяется в `MscDevice` до BOT), UAF нет;
* новые `open` после detach не находят mount-точку — `ENOENT`.

---

## 13. Безопасное извлечение

### 13.1. Штатное (orderly)

1. Files просит композитор (PDP), root-композитор вызывает `umount2("/usb0p1", 0)`.
2. Ядро: busy → flush → detach → `UNMOUNTED` (`unclean=0`, `safe_to_remove=1`).
3. UI: «можно отключать»; пользователь выдёргивает.
4. Ядро (poll) видит CCS=0 → `REMOVE` (не surprise), слот teardown, `usbN` исчезает.

### 13.2. Внезапное (surprise)

1. CCS=0 без `umount2`.
2. `MscLink.dead = true` до teardown; текущая BOT-команда получает `EIO`.
3. `REMOVE` с `flags.surprise=1`; если был mount — `ERROR` (`error=io`), mount
   «stale»: `read`/`write` → `EIO`, `statfs` → `EIO`.
4. `umount2(target, 0)` на dead-устройстве: detach **разрешён**, возврат `EIO`,
   `unclean=1`, `detached=1`, `safe_to_remove=0`; новые записи запрещены. Это **не**
   clean unmount. `umount2(target, MNT_FORCE)` — явное подтверждение потери,
   `safe_to_remove=1`; на живом устройстве `MNT_FORCE` → `EINVAL`.
5. WAL: последняя закоммиченная транзакция durable (flush на commit); незавершённая
   отбрасывается; при следующем `mount(2)` того же тома (`uuid`) `Journal::recover`
   реплеит цепочку. Replay fail → `mount(2)` fail-closed (`EIO`), `error=replay_failed`,
   `safe_to_remove=0`.
6. ФС может требовать `e2fsck`? Нет: WAL + ordered data; но данные, писавшиеся в
   момент выдёргивания, могут быть частичными — это честно документируется.

### 13.3. Что НЕ делаем

* не обесточиваем порт (PP=0) в Phase 1;
* не делаем «принудительный unmount» с открытыми fd;
* не пытаемся писать в мёртвое устройство после `dead`.

---

## 14. События: `/dev/uevent`

### 14.1. Назначение и модель

`/dev/uevent` — символьный узел (как `/dev/input/event0`), из которого **композитор**
читает push-события о блочных устройствах и рассылает их клиентам по PDP
(`devices_subscribe` → `devices`). Files — чистый PDP-потребитель; ядро не знает про
окна.

Контракт: один читатель (композитор). Второй `open` → `EBUSY`; `open` первого читателя
кладёт в кольцо **initial snapshot** — по одной `ADD`-записи на каждое уже
перечисленное устройство с текущими `mount`/`flags`/`fs_class`/`error`/`uuid`. Иначе cold-plug
(флешка до старта композитора) невидим.

### 14.2. Запись v2: 144 байта (exact)

| Offset | Тип | Поле | Значение |
|---|---|---|---|
| 0 | u32 | `magic` | `0x56455550` = bytes `"PUEV"` (LE) |
| 4 | u16 | `version` | **2** (v1 128 B не смешивается с v2) |
| 6 | u8 | `kind` | 1 ADD, 2 REMOVE, 3 CHANGE, 4 ERROR, 5 MOUNTED, 6 UNMOUNTED |
| 7 | u8 | `flags` | bit0 removable, bit1 read_only, bit2 mounted, bit3 in_use (best-effort), bit4 safe_to_remove, bit5 surprise, bit6 unclean, bit7 detached; bits 0–5 не меняются, 6–7 append-only |
| 8 | u8 | `fs_class` | 0 unknown, 1 pagh_ext2, 2 foreign_ext2, 3 blank, 4 other_fs, 5 no_media |
| 9 | u8 | `error` | 0 none, 1 io, 2 mount_failed, 3 flush_failed, 4 unsupported_fs, 5 no_media, 6 unsupported_block, 7 capacity_overflow, 8 replay_failed, 9 bad_mount; 0–5 не перенумеровываются, 6–9 append-only |
| 10 | u16 | `reserved0` | 0 |
| 12 | u32 | `seq` | live: монотонный (wrap); snapshot: 0 |
| 16 | u64 | `sectors` | число логических блоков размера `sector_size`; Phase 1: 512-байтовые секторы (= `BlockDevice::sector_count`); `capacity = sectors * sector_size` |
| 24 | u32 | `sector_size` | размер логического блока; Phase 1 = 512 |
| 28 | [u8;16] | `dev` | `"usb0p1"`, NUL-padded ASCII |
| 44 | [u8;16] | `mount` | `"/usb0p1"` или `""`; NUL-padded ASCII; max 15 символов |
| 60 | [u8;16] | `vendor` | INQUIRY vendor, ASCII, `?` для не-ASCII |
| 76 | [u8;32] | `product` | INQUIRY product, ASCII |
| 108 | [u8;16] | `label` | метка тома (ext2 `s_volume_name`), если прочитана; иначе пусто |
| 124 | [u8;4] | `reserved1` | 0 |
| 128 | [u8;16] | `uuid` | stable id: pagh-ext2 `s_uuid`, иначе GPT unique partition GUID; MBR/unknown — нули (§10.1) |

Итого **144 байта**, `version=2`. Поля `mount[16]` и `uuid[16]` обязательны оба:
`mount` передаётся **явно** (не выводится из `dev`), потому что Files открывает точку
из события и future `/media/...` не совпадает с именем устройства; `uuid` — ключ
персистентности между reboot (`usbN` не стабилен, §10.1). Все `u16/u32/u64` —
little-endian (x86_64). Строки — NUL-padded ASCII; не-ASCII/непечатаемое → `?`;
если vendor/product/label пусты, UI fallback «USB-накопитель».
`capacity = sectors * sector_size`; в Phase 1 `sector_size = 512`, поэтому
`capacity = sectors * 512` и `sectors` совпадает с `BlockDevice::sector_count`.
В будущем (4K logical block) `sectors` — логические блоки, а не 512-байтовые
единицы; это фиксируется в ABI v3 вместе с `sector_size`.

**Совместимость v1/v2.** v1 — прежняя 128-байтовая запись без `uuid` (и с `mount`);
v2 — 144 байта. Размер и `version` разные, поэтому читатели не смешиваются:
* v2-читатель передаёт `count >= 144`, проверяет `magic` и `version == 2`; иначе
  `EINVAL`/пропуск записи;
* v1-читатель с `count == 128` получает `EINVAL` (не «первые 128 байт v2»);
* ядро пишет в ring только v2-записи фиксированного размера 144; смешанных записей
  в одном ring нет. При будущем v3 — новый `version` и новый размер, без in-ring
  mixing.

`uuid`: pagh-ext2 `s_uuid`; иначе GPT unique partition GUID; MBR/unknown — нули,
fallback персистентности `label + capacity` (§10.1).

**Почему `label`, а не `serial`:** `serial` (VPD 0x80) в Phase 1 не читается (лишний
SCSI-запрос ради диалога «Свойства»); `label` нужен Files для подписи тома и читается
из уже загруженного суперблока ext2 при classify. Serial остаётся future: при
необходимости он войдёт в ABI v3 отдельным полем, а не подменой `label`. Для
foreign-ФС без драйвера (exFAT) label пуст — UI использует vendor/product.

**Граница `mount[16]`:** 15 символов + NUL; `/usb0p12` (8) и `/media/usb0p12`
(14) влезают, более длинные — ABI v3. `dev[16]` имеет тот же лимит для имени
устройства.

### 14.3. Семантика kind/флагов

| Событие | Когда | Поля |
|---|---|---|
| `ADD` | устройство перечислено (snapshot или live) | dev, mount, uuid, capacity, vendor/product/label, fs_class, flags, error |
| `MOUNTED` | `mount(2)` успешен (композитор, root) | `mount=target`, `flags.mounted=1`, `safe_to_remove=0`, `error=0`; успешный replay снимает `unclean` |
| `UNMOUNTED` | clean `umount2` | `mount=""`, `mounted=0`, `detached=1`, `unclean=0`, `safe_to_remove=1` |
| `UNMOUNTED` (surprise) | dead-устройство, `umount2` | `mount=""`, `mounted=0`, `detached=1`, `unclean=1`, `safe_to_remove=0`; `EIO` при `flags=0` |
| `UNMOUNTED` (force) | dead + `MNT_FORCE` (явное подтверждение потери) | `mount=""`, `mounted=0`, `detached=1`, `unclean=1`, `safe_to_remove=1`; возврат 0 |
| `CHANGE` | смена состояния (fs_class после probe, read_only, in_use) | текущие поля |
| `ERROR` | I/O/mount/flush/replay/unsupported | `error`, флаги (`mounted` может остаться 1 при `flush_failed`) |
| `REMOVE` | устройство исчезло | `flags.surprise`; `mount=""`, `mounted=0`; `unclean=1`/`detached=1` только если том был смонтирован, иначе 0; слот teardown |

UI различает «смонтирован» (`flags.mounted`) и «ошибка» (`error != 0`) независимо от
kind; `kind=ERROR` — явный сигнал, но не единственный носитель ошибки. Устройство
**не исчезает** из UI при `mounted=0`: present/unmounted/foreign/blank/error — всё
это `ADD`/`CHANGE` с текущими полями. «Removing» — overlay композитора на время
`device_del`/teardown, не состояние ядра.

`unclean=1` означает «WAL не проверен». Успешный replay при следующем `mount(2)`
снимает `unclean`, но **не** ставит `safe_to_remove` на смонтированном томе (MOUNTED
явно сбрасывает его в 0): `safe_to_remove=1` — только после clean unmount (тогда же
`unclean=0`) либо явного `MNT_FORCE` на dead-устройстве (тогда `unclean=1` остаётся).

### 14.4. Чтение, poll, ioctl

* `open("/dev/uevent", O_RDONLY|O_NONBLOCK)` — эксклюзивно; initial snapshot в ring.
* `read(fd, buf, count)`: `count >= 144` → ровно одна запись, возврат 144;
  `count < 144` → `EINVAL`; пусто + `O_NONBLOCK` → `EAGAIN`; пусто + blocking →
  yield-цикл (как `read_evdev_event`, `io_sys.rs:470`), сигнал → `EINTR`.
* `poll`: `POLLIN` ровно когда ring непуст (ветка `is_uevent` в `poll_revents`,
  `io_sys.rs:3694` — рядом с evdev).
* `ioctl(fd, PUEV_IOC_SNAPSHOT)` — `0x5501` (type `'U'`, nr 1, no arg): снова кладёт
  snapshot (для `seq` gap). `PUEV_IOC_SNAPSHOT` — единственный ioctl; остальные →
  `ENOTTY`.
* Ring: 256 записей; переполнение → `dropped++`; счётчик выводится в логе и
  `usb`-команде, не в `/proc/partitions`.
* `seq`: live-события `seq++`; snapshot `seq=0`. Gap (`seq != last+1`, `seq != 0`) →
  `PUEV_IOC_SNAPSHOT`, затем применить snapshot по `dev` (идемпотентно).

### 14.5. Kernel-side реализация (план)

* `src/drivers/usb_uevent.rs` — чистое ядро: `Record` (144 B, v2), `encode`, `parse`,
  ring, `push`, `snapshot`, `seq`, `dropped`; host-tests фиксируют `size == 144`,
  каждый offset, `version == 2`, все значения `flags` (0..7) и `error` (0..9),
  round-trip, ring overflow, seq wrap. Snapshot кладёт те же 144-байтовые записи.
* `src/vfs/desktop_dev.rs` — узел `uevent` c `is_uevent()`; `VfsNode`-методы
  `read`/`poll`-предикат; `DevDirectory` получает его в `vfs::init`.
* `io_sys.rs`: ветки `read`/`poll`/`ioctl` для `is_uevent`; `open`/`close` —
  claim/release читателя.
* `usb_storage`-поток вызывает `uevent::push` на ADD/REMOVE/CHANGE/MOUNTED/UNMOUNTED/
  ERROR.
* Lock-дисциплина snapshot: список устройств собрать под `DeviceManager`, **отпустить**
  его, и только затем класть записи в ring под локом `uevent`. Держать оба лока сразу
  нельзя: `usb-storage` берёт `DeviceManager` при регистрации, и обратный порядок дал бы
  дедлок (спинлоки не реентерабельны).
* `/proc/partitions` — отдельный procfs-файл, рендер из `DeviceManager`.

### 14.6. Контракт с композитором/Files

* Композитор — единственный reader; читает до `EAGAIN`, применяет по `dev`, рассылает
  PDP `devices` (поля записи 1:1).
* Files показывает устройство по `ADD` с любым `fs_class`, если `dev` присутствует;
  `flags.mounted=0` → «не смонтировано», foreign → «exFAT не поддерживается»
  (причина из `fs_class`/`error`), `mounted=1` → клик открывает `mount` из события.
  `mount` — точка из события (не выводится из `dev`); `uuid` — ключ персистентности;
  `usbN` между reboot не сохраняется.
* `statfs` на точке монтирования даёт free space (nr 137); в записи его нет.
* При `REMOVE` композитор убирает устройство; если у Files открыт путь — I/O даёт
  `EIO`, окно показывает ошибку, не падает.

---

## 15. Приёмка

### 15.1. QEMU: `usb-storage`

Холодное подключение:

```sh
qemu-system-x86_64 ... \
  -device qemu-xhci,id=xhci \
  -drive file=usb-pagh.img,format=raw,if=none,id=usbstick \
  -device usb-storage,drive=usbstick,bus=xhci.0,removable=on
```

`usb-pagh.img` — GPT + один раздел, отформатированный **ядром** в ext2+WAL (не
host-`mkfs.ext2`: маркер WAL пишет только `Ext2Fs::format`). Практический путь:
`tools/mkimage.py --make-disk` → первый boot с этим образом как data-диск → boot-menu
`F` + имя устройства (или `disk format`) → образ с pagh-томом → подключить как
`usb-storage`.

Hotplug (QMP, тот же `Guest`/`QmpClient` из `tools/e2e.py`):

```json
{"execute":"device_add","arguments":{"driver":"usb-storage","bus":"xhci.0",
                                     "id":"stick1","drive":"usbstick"}}
{"execute":"device_del","arguments":{"id":"stick1"}}
```

Сценарий и маркеры (serial):

1. `usb-msc: dev=usb0 ...` — устройство найдено.
2. `usb-msc: inquiry vendor="..." product="..." rev="..." pdt=0` — SCSI работает.
3. `usb-msc: capacity last_lba=N block=512 sectors=N` — READ CAPACITY.
4. `part: usb0p1 -> N sectors at LBA ...` — разделы.
5. `usb-uevent: kind=ADD dev=usb0p1 fs_class=1 uuid=...` — событие **до** mount;
   ядро auto-mount не делает.
6. `mountprobe mount usb0p1 /usb0p1` (userspace `mount(2)`, §11.2) →
   `usb-storage: mounted usb0p1 at /usb0p1` + `usb-uevent: kind=MOUNTED`.
7. shell: `echo e2e > /usb0p1/e2e.txt; cat /usb0p1/e2e.txt` — read/write.
8. `mountprobe umount /usb0p1` (userspace `umount2`, §11.4) → `usb-uevent:
   kind=UNMOUNTED ... safe_to_remove=1`.
9. `device_del` → `usb-uevent: kind=REMOVE ... surprise=0`; нет panic.
10. `device_add` снова → `usb-msc: dev=usb1 ...`; `uuid` тот же (не `usbN`);
    `mountprobe mount usb1p1 /usb1p1`; файл `e2e.txt` читается (persistence через
    re-enumeration; для reboot-persistence — отдельный прогон с reboot).
11. Surprise removal: смонтировать, `device_del` без `umount2` → `REMOVE surprise=1`,
    `ERROR error=io`; `read` → `EIO`; `mountprobe umount` даёт `EIO` + `unclean=1`,
    `safe_to_remove=0`; `mountprobe umount --force` даёт `safe_to_remove=1`;
    повторный mount после `device_add` проходит `Journal::recover`; replay fail
    (подсунуть испорченный WAL) → mount fail-closed, `error=replay_failed`.
12. Негатив: host-тест SCSI-ответа с `block_length=4096` → `error=unsupported_block`,
    BlockDevice не регистрируется; `last_lba=0xFFFFFFFF` → `error=capacity_overflow`,
    ёмкость не усекается. QEMU `usb-storage` всегда 512 B, поэтому это unit/host
    проверка, не live.

E2E-режим: `python3 tools/e2e.py usb-storage` (новый mode; `usb`/`rndis` не ломаются).
Он обязан печатать `PASS`/`FAIL` по каждому шагу и сохранять serial; скриншот Files —
в прогоне с композитором, иначе `ueventprobe` dump как доказательство видимости.

### 15.2. Реальная Ventoy-флешка

По решению lead:

* **Ventoy/exFAT data-раздел**: устройство **видно** (`ADD`, `fs_class=other_fs`,
  vendor/product/capacity; `label` — если удалось прочитать, иначе пусто), mount
  **отключён** с честной причиной «exFAT пока не поддерживается»; никаких записей,
  никакого ложного mount.
* **Read/write** доказывается на **pagh-ext2 разделе** этой же флешки (или отдельной
  флешке): создать раздел, отформатировать ядром (`disk format`/boot-menu), воткнуть,
  root-композитор монтирует `usbNpM` через `mount(2)`, записать/прочитать/перезагрузиться/
  прочитать — идентифицируя том по `uuid`, а не по `usbN`.
* **Hotplug**: вынуть/вставить без перезагрузки; `REMOVE`/`ADD`, новый `usbN`.
* **Surprise removal**: вынуть смонтированный том; система жива, I/O `EIO`, повторный
  mount делает WAL-replay; файл, записанный до выдёргивания, читается (если commit
  успел).
* Доказательства: serial-лог с маркерами, `disk`/`/proc/partitions`, скриншот Files,
  `cargo`-гейты. Реальное железо тестирует **пользователь** после QEMU, по шагам;
  до этого в README/спеке не пишется «работает на Ventoy» — только «план проверки».

### 15.3. Никаких заглушек

Каждый шаг приёмки идёт через настоящий BOT/SCSI/partition/mount/WAL. Запрещено:
фейковый BlockDevice в boot-пути, «устройство есть» без INQUIRY, mount без flush,
событие без реального teardown. Host-тесты протокола не заменяют live-приёмку.

### 15.4. Гейты 6/6 на каждом этапе

```sh
cargo fmt --all -- --check
python3 tools/build.py build
python3 tools/build.py build --release
python3 tools/host_tests.py
python3 tools/check_safety.py
python3 tools/check_agents_md.py
```

Плюс `tools/e2e.py usb-storage` (QEMU) и live-доказательство этапа. DOC-only PR (этот
документ) не бампает версию; **MINOR bump делает финальный acceptance PR** (P8), не
промежуточные PR. Это решение lead, а не «кто-нибудь потом».

---

## 16. Риски и честные пробелы

| # | Риск | Влияние | Смягчение / гейт |
|---|---|---|---|
| R1 | Синхронный BOT под `Spinlock` (IF off) | HID/сеть/UI замирают на время команды; flush на дешёвой флешке — сотни мс | бюджет ≤250 мс на команду в Phase-гейте; таймауты; future sleepable-lock/async MSC |
| R2 | Нет хабов | флешка в переднем порту корпуса (через hub) невидима | назвать в README/UI; hub-драйвер — future |
| R3 | Нет hot-unplug в текущем xHCI | до реализации §6.5 выдёргивание оставляет слот | Phase 7 обязателен до заявления «hotplug работает» |
| R4 | Реальное железо не тестировано | USB2-кваки, CSW timeout, sense | приёмка пользователя; лог `usb`/`usb-msc`; retry |
| R5 | 64 KiB contiguous DMA может не выделиться | fallback 4 KiB (медленно) | лог + `disk`-проверка; не ошибка |
| R6 | `SYNCHRONIZE CACHE` не поддержан мостом | WAL-durability нельзя обещать | fail-closed: flush → Err; mount может остаться, но запись честно ошибочна |
| R7 | 4K logical block / >2 TiB | отказ | `error=unsupported_block` / `capacity_overflow`, без усечения; READ(16) — future |
| R8 | Multi-LUN | второй LUN невидим | `GET_MAX_LUN` лог; future |
| R9 | Чужой ext2 mount пишет | порча чужой ФС | `is_pagh_volume` gate; foreign только видим |
| R10 | exFAT Ventoy не монтируется | пользователь ждёт «флешку» | UI причина; exFAT future; acceptance явно на pagh-разделе |
| R11 | UAS-only устройства | невидимы | назвать; BOT — требование Phase 1 |
| R12 | Hostile-дескрипторы/таблицы | panic/OOB | существующие bounds-проверки + fuzz host-тесты |
| R13 | `usbN` не стабилен между reboot | UI не должен его запоминать | персистентность по `uuid`/`label` (§10.1); `usbN` — только рантайм |
| R14 | Один контроллер | второй xHCI не обслуживается | README; future |
| R15 | Bus-powered HDD | не хватит тока | назвать; флешки — цель |
| R16 | WAL replay после surprise падает | том нельзя монтировать | fail-closed `error=replay_failed`; `safe_to_remove=0`; ручной recovery — future |

---

## 17. Поэтапный план (узкие PR, base=`dev` после stable merge)

Каждый этап — отдельный PR; гейты 6/6; live-доказательство в описании PR. Версия — по
AGENTS.md (docs-only не бампает; feature-PR — MINOR).

### P0. Спека (этот документ)

* `docs/usb-mass-storage-spec.md`; review lead.
* Доказательство: документ + согласованный ABI/контракт; кода нет.

### P1. Чистые протоколы + host-свойства

* `src/drivers/usb_msc_proto.rs` — CBW/CSW encode/decode, signature/tag, residue.
* `src/drivers/usb_scsi.rs` — CDB builders (6/10/16), INQUIRY/CAPACITY/READ/WRITE/
  SENSE parse, sense→action.
* `src/drivers/usb_uevent_abi.rs` — 144-B v2 layout/encode/parse (host-property exact: size/offset/flags/error).
* `host-tests/src/lib.rs` + `properties/usb_msc.rs` (+ pNN): layout exact (size/offset),
  round-trip, hostile/truncated, sense matrix, CBW/CSW edge cases.
* Live: `host_tests` зелёный; кода ядра не трогаем.

### P2. Дескрипторы MSC

* `usb_desc::parse_msc_interfaces` (class 8/6/0x50), bulk-in/out, max packet.
* Host-тесты на реальных QEMU-дескрипторах (захваченных заранее) + усечённые.
* Live: unit-тесты; `enumerate_slot` пока не использует (no behavior change).

### P3. xHCI MSC transport + BlockDevice

* `MscLink`/`MscDevice`, bulk rings, CBW/data/CSW, GET_MAX_LUN, Reset Recovery,
  timeout, `read_block`/`write_block`/`flush`, `usb0`.
* `configure_endpoints` ветка MSC; `Slot.msc`.
* E2E QEMU: `usb-storage` с образом; serial `usb-msc: inquiry/capacity`; shell
  `disk` видит `usb0`.
* Live: QEMU + скриншот `disk`; host-тесты P1. Чтение данных (READ(10)) доказывается
  на P4: `register_partitions` читает LBA 0/1 и печатает `part: usb0p1 ...`.

### P4. Разделы и `/proc/partitions`

* `register_partitions` для `usbN`; GPT unique GUID в `uuid`; superfloppy fallback;
  `/proc/partitions`.
* E2E: `part: usb0p1 ...`, `disk` показывает `usb0p1`; `cat /proc/partitions`.
* Live: serial + shell.

### P5. mount(2)/umount2(2), VFS unmount, busy, userspace mount

* `nr 165/166`, dispatch, `SUPPORTED/EXPECTED` (168→170), `sys_mount`/`sys_umount2`.
* `vfs::unmount_at`; `MountOrigin`; `mount_id` на ext2-узлах; fd-scan busy.
* `ROOT_KEEPING_COMPONENTS += "usb"`; `mount_at` target checks.
* Root-композитор монтирует pagh-том через `mount(2)` после `ADD`; uevent MOUNTED/UNMOUNTED. Ядро на hotplug не монтирует.
* E2E: пользовательский `mountprobe` (маленький статический бинарник, как
  `grpprobe`/`netprobe` в `tools/`) вызывает `mount(2)`/`umount2(2)`; shell
  `echo/cat` и `statfs` free space; размонтирование с открытым fd → `EBUSY`; чужой
  том → `EINVAL`.
* Live: serial + shell + (если есть) Files.

### P6. `/dev/uevent` + desktop kernel-side

* `usb_uevent` ring, snapshot, ioctl, read/poll/open/close, `is_uevent`.
* E2E: `ueventprobe` (статический бинарник, как `mountprobe`) читает `/dev/uevent`
  через `read(2)`/`poll(2)` и печатает ADD/MOUNTED/REMOVE; cold-plug snapshot
  (загрузиться с воткнутой флешкой, открыть uevent после boot).
* Live: serial dump записей + (после UX-PR) скриншот Files.

### P7. Hot-unplug / surprise removal / eject

* `poll_port` disconnect detection, `disconnect_slot`, dead-флаг, REMOVE, stale mount,
  WAL replay.
* E2E: `device_del` смонтированного/несмонтированного; `device_add` снова (`usb1`);
  reboot-persistence проверяется по `uuid`, не по `usbN`.
* Live: QEMU; реальная флешка — пользователь после QEMU.

### P8. Реальное железо + docs + MINOR bump

* Ventoy-флешка (проверяет пользователь после QEMU): foreign exFAT виден, pagh-раздел
  mount/read/write, hotplug, surprise; до прогона — только «план проверки», без
  «работает на Ventoy».
* Обновить `README.md`, `src/drivers/README.md`, `AGENTS.md` (пробелы), `docs/`;
  `tools/README.md` (новый e2e mode).
* **MINOR bump** по AGENTS.md — в этом финальном acceptance PR.
* Live: serial/скриншоты; 6/6; отчёт пользователя по шагам §15.2.

---

## 18. Решения lead (зафиксированы) и остаток для реализации

### 18.1. Зафиксировано lead — не переспрашивать

* **Auto-mount — userspace-driven.** Ядро только enumerate→`ADD`; root-композитор
  монтирует **только** pagh-том через `mount(2)`. Ядро на hotplug не монтирует;
  boot-mount flow (`/mnt`) не меняется.
* **Acceptance Ventoy:** read/write — только на pagh-ext2 разделе; exFAT — visible/
  foreign, mount запрещён; exFAT-драйвер — future; до реального прогона пользователем
  «работает на Ventoy» не утверждается.
* **block size != 512 и >2 TiB:** явный reject (`error=unsupported_block` /
  `error=capacity_overflow`), без усечения; READ(16)/WRITE(16) — следующий этап.
* **`/dev/block/<name>`:** future, не Phase 1.
* **Surprise/dead:** `umount2` с `flags=0` → `EIO` + detach, `unclean=1`,
  `safe_to_remove=0`; `MNT_FORCE` (только dead) — явное подтверждение потери,
  `safe_to_remove=1`; запись запрещена; replay fail — mount fail-closed.
* **`usbN` не стабилен между reboot;** персистентность — по `uuid`/`label`, не по
  `usbN`.
* **uevent record v2 = 144 B:** `mount[16]` offset 44 (явно, не derived) + `uuid[16]`
  offset 128..143; `version=2`; v1 128 B не смешивается; flags 0–5 и error 0–5 не
  перенумеровываются, bit6/7 и error 6–9 append-only; `bad_mount` — error 9.
* **MINOR bump** — в финальном acceptance PR (не в промежуточных).
* **Реальное железо** — пользователь после QEMU, по шагам §15.2; до этого README не
  заявляет «работает».

### 18.2. Остаток для реализации (не меняет контракт)

1. **MBR без UUID:** `uuid[16]=0`; fallback персистентности — `label + capacity +
   vendor/product`. Подтвердить, что этого достаточно UI; иначе MBR-томам нужен
   синтетический id (хеш первых секторов) — отдельное решение.
2. **`MNT_FORCE`:** значение `1`, допустимо только при `msc.dead`. Проверить, что
   «явное подтверждение пользователем» — это именно флаг `umount2`, а не отдельный
   ioctl; UI должен показать предупреждение о возможной потере до вызова.
3. **bad_mount:** `mount(2)` с target длиннее 15 символов → `ENAMETOOLONG`, лог
   `bad_mount`, без uevent (состояние устройства не менялось). UI показывает ошибку
   имени, а не «устройство сломано».
4. **`error=replay_failed`:** как UI формулирует «том не восстановлен, не монтируйте»
   и что предлагает (только чтение? повторный replay? — пока fail-closed).
5. **E2E probes:** `mountprobe`/`ueventprobe` — статические бинарники в стиле
   `grpprobe`/`netprobe`; уточнить место сборки/стейджинга в `tools/`.

---

## 19. Приложение A: константы и раскладки

### A.1. USB/BOT

| Имя | Значение |
|---|---|
| CBW signature | `0x43425355` (`"USBC"`) |
| CSW signature | `0x53425355` (`"USBS"`) |
| CBW size / CSW size | 31 / 13 |
| GET_MAX_LUN | bmRequestType `0xA1`, bRequest `0xFE`, wLength 1 |
| Bulk-Only Reset | bmRequestType `0x21`, bRequest `0xFF`, no data |
| CLEAR_FEATURE(ENDPOINT_HALT) | bmRequestType `0x02`, bRequest 1, wValue 0, wIndex=ep |
| MSC interface | class 8, subclass 6, protocol `0x50` |
| CBW flag IN | bit 7 |
| CSW status | 0 passed, 1 failed, 2 phase error |
| CSW timeout | 30 s |

### A.2. xHCI completion codes (используемые)

| Code | Значение | Действие MSC |
|---|---|---|
| 1 | Success | ok |
| 6 | Stall | ClearFeature + Reset Endpoint |
| 13 | Short Packet | IN: ok; CSW: protocol error |
| 3 | Babble | Reset Recovery |
| 4 | USB Transaction Error | Reset Recovery |
| 5 | TRB Error | fail |
| 2 | Data Buffer Error | fail/recovery |

### A.3. CDB (Phase 1)

```
INQUIRY(6):        12 00 00 00 24 00
REQUEST SENSE(6):  03 00 00 00 12 00
TEST UNIT READY:   00 00 00 00 00 00
READ CAPACITY(10): 25 00 00 00 00 00 00 00 00 00
READ(10):          28 00 <lba:4BE> 00 <len:2BE> 00
WRITE(10):         2A 00 <lba:4BE> 00 <len:2BE> 00
SYNCHRONIZE(10):   35 00 00 00 00 00 00 00 00 00
READ CAPACITY(16): 9E 10 00 00 00 00 00 00 00 00 00 00 00 20 00 00
READ(16):          88 00 <lba:8BE> <len:4BE> 00 00
WRITE(16):         8A 00 <lba:8BE> <len:4BE> 00 00
```

### A.4. uevent flags/fs_class/error

См. §14.2. `capacity = sectors * sector_size`; Phase 1 `sector_size=512`.

---

## 20. Приложение B: что докажет каждый этап (сводка)

| Этап | Артефакт | Live-доказательство |
|---|---|---|
| P0 | эта спека | review lead |
| P1 | host-свойства | `host_tests` PASS |
| P2 | unit-тесты дескрипторов | `host_tests` PASS |
| P3 | QEMU `usb-storage` | serial inquiry/capacity; `disk` видит usb0 |
| P4 | разделы | `part:` + `/proc/partitions` |
| P5 | mount/unmount | `mountprobe` mount/umount + shell write/read + `statfs` |
| P6 | uevent | `ueventprobe` ADD/MOUNTED/REMOVE + cold-plug snapshot |
| P7 | hotplug | `device_del`/`device_add` + surprise + WAL replay |
| P8 | реальная флешка | serial + скриншот Files + reboot |

---

## 21. Приложение C: границы изменений (write scope)

| Область | Файлы (план) |
|---|---|
| Протокол | `src/drivers/usb_msc_proto.rs`, `usb_scsi.rs`, `usb_uevent_abi.rs` |
| xHCI | `src/drivers/xhci.rs` (MSC-ветка, disconnect, Configure Endpoint) |
| Дескрипторы | `src/drivers/usb_desc.rs` |
| BlockDevice | `src/drivers/usb_msc.rs` (MscDevice), `drivers/mod.rs` (регистрация) |
| Разделы | `src/drivers/partition.rs` (reuse; superfloppy helper) |
| События | `src/drivers/usb_uevent.rs`, `src/vfs/desktop_dev.rs`, `src/vfs/mod.rs` |
| Mount | `src/arch/x86_64/linux/abi.rs`, `mod.rs`, `io_sys.rs`, `io.rs`, `src/vfs/mod.rs` |
| Поток | `src/boot.rs` (spawn `usb-storage`), `src/usb_storage.rs` (или `drivers/usb_storage.rs`) |
| Тесты | `host-tests/src/lib.rs`, `host-tests/src/properties/usb_*.rs` |
| E2E/docs | `tools/e2e.py`, `README.md`, `src/drivers/README.md`, `AGENTS.md` |

Никаких изменений в `vendor/`, `/boot`, `dist/pagh-disk.img`, чужих worktree. Версия не
бампается в этом PR.