# `src/drivers/` — драйверы и device manager

Все устройства: PCI-энумерация, e1000, virtio-blk, NVMe, PS/2 клавиатура и мышь,
framebuffer-консоль, VT-эмулятор, курсор, serial. Координируются `drivers::mod`.

## Файлы

| Файл | Роль |
|---|---|
| `mod.rs` | Device manager: трейты `Console`/`CharacterDevice`/`BlockDevice`, реестр `DeviceManager` (`register_char/register_block/get_char/get_block`), boot-`init()` (kbd → framebuffer → mouse) |
| `pci/mod.rs` | Legacy PCI config-space через порты 0xCF8/0xCFC; `enumerate()` по всем 256 шинам; `enable_bus_master`; узнаёт virtio (0x1AF4) и Intel (0x8086) |
| `e1000.rs` | Intel 8254x NIC — поллинг, без IRQ; MMIO-регистры, EEPROM MAC, легаси 16-слотовые TX/RX-кольца |
| `virtio/mod.rs` | Корень virtio-обвязки (`blk`, `hal`) |
| `virtio/hal.rs` | `PaghHal` — реализация `virtio_drivers::Hal`: DMA/MMIO через `pmm`/`vmm`; автоматические bounce-буферы для неконтинуальных heap-буферов |
| `virtio/blk.rs` | virtio-blk: `PciTransport` + `VirtIOBlk`, обёрнут в `BlockDevice` «virtio-blk0», секторный кэш 2 MiB |
| `nvme.rs` | NVMe поверх PCIe — поллинг (phase-bit), BAR0 MMIO, PRP scratch-frame; регистрируется как `BlockDevice` |
| `ps2_kbd.rs` | PS/2 клавиатура (IRQ1): 128-байтное кольцо сканкодов, трекинг Ctrl+C, лATCHи `CTRL_C`/`FG_PID` |
| `ps2_mouse.rs` | PS/2 мышь (IRQ12) через 8042 aux: сборка 3-байтовых пакетов, зажатые координаты + кнопки + `seq` |
| `framebuffer.rs` | Limine framebuffer текстовая консоль + 2D-графика: шрифт 8x16 (`assets/font8x16.bin`), `FbWriter`, скролл, status bar, макросы `fb_print!` |
| `vt.rs` | ANSI/VT-100 эмулятор поверх framebuffer (`Vt`: CSI/OSC/DCS, 256 цветов, scroll regions, ответы на запросы) — stdout compat-программ |
| `cursor.rs` | Программный курсор мыши 12×19 (save/restore фона); `text_begin`/`text_end` для курсор-безопасного текста |
| `serial.rs` | COM1 UART консоль (`init`, `write_bytes`, `console()`, макросы `sprint!`, `kprint!`/`kprintln!`) |

## Ключевые символы

- `mod`: трейт `BlockDevice { name, read_block, write_block, sector_count }` (секторы 512 B);
  `init()`, `get_block(name)`.
- `pci`: `enumerate() -> Vec<PciDevice>`, `config_read_u32/write_u32`, `enable_bus_master`,
  `PciDevice::is_virtio()`.
- `e1000`: `attach(&[PciDevice]) -> Result<[u8;6], ()>`, `send(&[u8]) -> usize`,
  `recv(&mut [u8]) -> Option<usize>`, `mac_address()`, `is_attached()`.
  Ключевые регистры: CTRL 0x0000, EERD 0x0014, ICR 0x00C0, IMC 0x00D8, RCTL 0x0100, TCTL 0x0400,
  RX-кольцо 0x2800.., TX-кольцо 0x3800..; `DESC_COUNT=64`, `BUF_SIZE=2048`.
- `virtio::blk`: `init_blk(&[PciDevice])`, `VirtioBlkDevice` (реализует `BlockDevice`);
  `SECTOR_CACHE_MAX=4096` секторов, write-through.
- `ps2_kbd`: `init()`, `irq_handler()`, `has_pending()`, атомики `CTRL_C`, `FG_PID`.
- `ps2_mouse`: `init(screen_w, screen_h) -> bool`, `irq_handler()`, `poll()`,
  `MouseState { x, y, left, right, middle, seq }`.
- `framebuffer`: `init()`, `console()`, `clear_screen()`, `set_fg_color()`, `dimensions()`,
  `with(f)`, `draw_status_bar(left, right)`, `fb_print!/fb_println!`;
  константы `CHAR_WIDTH=8`, `CHAR_HEIGHT=16`, `CHARS_PER_LINE=100`, `MAX_LINES=37`,
  `STATUS_BAR_HEIGHT=18`.
- `vt`: `init()`, `write(bytes)`, `dimensions()`, `take_input_responses()`.
- `cursor`: `hide()`, `move_to()`, `text_begin()`, `text_end()`.
- `serial`: `init()`, `write_bytes()`, `console()`.

## Как работает

### IRQ
Ровно два device-IRQ подключены в `boot.rs`: вектор 33 = IRQ1 клавиатура, вектор 44 = IRQ12 мышь.
Всё остальное — **e1000 и NVMe — поллится, прерывания замаскированы** (`REG_IMC=0xFFFFFFFF`
у e1000; phase-bit у NVMe). Это несущая предпосылка локинга `net::` — сетевой стек вообще
не живёт в IRQ-контексте.

### Клавиатура
Сканкод с порта 0x60 → трекинг 0xE0-префикса → Ctrl+C-make ставит `CTRL_C` и, если `FG_PID`
указывает на живой не-raw compat-процесс, зовёт `scheduler::request_exit(pid)` прямо из IRQ
(^C работает даже когда shell заблокирован в read()) → байт в кольцо (аллокации нет,
при переполнении дроп).

### Мышь
Байт с 0x60 → ресинк по биту 3 нулевого байта → сборка 3 байтов → 9-битные знаковые дельты,
Y инвертирован, кламп в экран, инкремент `seq`. Рендерингом не занимается — потребляют
`cursor` и `paint`.

### e1000
`attach` находит 0x8086:0x100E/0x100F → bus master → BAR 0 → `vmm::map_mmio(phys, 0x2_0000)` →
маскирование IRQ → `CTRL_RST` → `CTRL_SLU` (link up) → континуальные кольца и буферные пулы
из `pmm::alloc_frames_contiguous` через HHDM → программирование RDBA/TDBA/LEN →
`RDT = DESC_COUNT` → enable → MAC из EEPROM (фоллбэк `52:54:00:12:34:56`).
RX-поллинг по биту `STATUS.DD`; TX: копия в слот-буфер, дескриптор `EOP|IFCS|RS`, kick `TDT`.
Один кадр = один дескриптор = один буфер 2048; oversized **дропается**, не режется.

### virtio-blk
Ручных virtqueue нет — крейт `virtio-drivers` 0.11. `PciTransport` находит virtio-капабилити
в BAR'ах и мапит через `PaghHal::mmio_phys_to_virt` (`NO_CACHE|NO_EXECUTE`). Буферы запросов
проходят `PaghHal::share`: прямой DMA при физической континуальности, иначе bounce-буфер
+ reconcile на `unshare`. Весь доступ — под одним `Spinlock` на время обмена.

### Консоль / VT / курсор
- Текстовая сетка 8×16, скролл — `copy` текстовой области на строку вверх (status bar не трогается);
  контрольные символы: `\n`, `\r`, `0x08`, печатные 32..=126.
- `FbWriter` = `Spinlock<Option<FramebufferWriter>>`; `init()` строит writer **вне** локa
  (лог внутри `new()` ре-лочит нереентерабельный спинлок — дедлок).
- Каждый текстовый вывод идёт `cursor::text_begin()` → write → `text_end()` — стрелка мыши
  прячется/восстанавливается из любого потока.
- VT-эмулятор: сетка ячеек с fg/bg, scroll regions, 16 ANSI + 256 цветов; отвечает на
  DA1/DSR/DECRQM/OSC через очередь, дренируемую в compat-stdin. `vt::init()` зовётся
  перед запуском Linux-процесса (`cmd_lxrun`).

## Зависимости

- **От:** `memory::pmm` (континуальные кадры), `memory::vmm` (`phys_to_virt`, `map_mmio`),
  `sync::spinlock`, порт I/O крейта `x86_64`, Limine `FRAMEBUFFER_REQUEST`,
  `virtio-drivers` (только virtio), `task::compat`/`task::scheduler` (^C).
- **На неё:** `net` (e1000), `boot.rs` (storage, IRQ wiring), `vfs`/`fs` (BlockDevice),
  `log.rs` (serial + framebuffer mirror), весь `shell/`, `task/compat` (stdout → vt).

## Грабли

- TX-лимит e1000: `BUF_SIZE - 4 = 2044` байта на кадр.
- `enable_bus_master` пишет назад весь command dword — старшая половина write-1-to-clear,
  не заляпать.
- `PaghHal::dma_alloc`/`mmio_phys_to_virt` — `expect` при неудаче (boot-fatal by design).
- Инвариант bounce-буферов: virtually-континуальный heap-буфер может быть физически
  фрагментирован — `share` проверяет континуальность постранично через `virt_to_phys`.
- Секторный кэш когерентен только потому, что весь disk I/O идёт через `read_block`/`write_block`.
- Замыкание `framebuffer::with` НЕ должно логировать (ре-лок той же консоли → дедлок).
- Шрифт покрывает только ASCII 0x20–0x7E; остальное — fallback-бокс.
- `CHARS_PER_LINE=100` — фиксированная ширина переноса независимо от разрешения.
- DMA-буферы e1000/NVMe — PMM-кадры через HHDM; heap-аллокаций на I/O-пути нет.
