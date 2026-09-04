# `src/memory/` — память: PMM, VMM, куча, layout

Управление памятью ядра: физические кадры, 4-уровневая страница-таблица, куча и карта
фиксированных виртуальных регионов. Инициализируется из `boot.rs` в жёстком порядке:
`pmm::init` → `vmm::init` → `heap::init`.

## Файлы

| Файл | Роль |
|---|---|
| `mod.rs` | Корень модуля: `heap`, `layout`, `pmm`, `vmm` |
| `layout.rs` | Единственный источник правды о фиксированных виртуальных регионах: стеки ядра per-PID, user-стек, mmap-база, размер кучи; хелперы границ образа ядра |
| `pmm.rs` | Physical Memory Manager — битовый аллокатор кадров 4 KiB + счётчики COW |
| `vmm.rs` | Virtual Memory Manager — 4-уровневый paging, map/unmap/translate, clone/fork/drop user-space, MMIO, управление CR3 |
| `heap.rs` | Ядровая куча, `#[global_allocator]` (обёртка над `good_memory_allocator` с маскировкой прерываний) |

## Ключевые символы

- `layout`: `PAGE_SIZE=4096`, `KERNEL_STACK_REGION_BASE`, `KERNEL_STACK_PAGES=64` (256 KiB на PID),
  `KERNEL_STACK_GUARD_PAGES=1`, `kernel_stack_for_pid(pid)`, `USER_STACK_TOP=0x7000_8000_0000`,
  `USER_STACK_PAGES=2048` (8 MiB), `USER_MMAP_BASE=0x2000_0000_0000`, `HEAP_INITIAL_PAGES` (512 MiB),
  `kernel_start()/kernel_end()/kernel_size()/heap_base()` (из linker-символов).
- `pmm`: `init(&MemmapResponse)`, `alloc_frame()`, `free_frame()`, `alloc_frames_contiguous(n)`,
  `free_frames_contiguous()`, `cow_ref()/cow_unref()`, `total_frames()/free_frames()`.
- `vmm`: `init(hhdm_offset)`, `kernel_pml4_phys()`, `phys_to_virt()/virt_to_phys()`,
  `virt_to_phys_in(pml4, virt)`, `map()/unmap()`, `map_mmio(phys, len)`, `load_cr3(phys)`,
  `new_user_pml4()`, `clone_user_space()`, `fork_user_space_cow()`, `cow_copy_page()`, `drop_user_space()`.
- `heap`: `init()`, `stats() -> (size, used, free)`.

## Как работает

### PMM (`pmm.rs`)
- Битовый массив: 1 бит на кадр 4 KiB, **1 = свободен, 0 = занят**. Инициализация по
  `MEMMAP_USABLE`-записям Limine; диапазон = от самой низкой до самой высокой usable-границы.
- Размещение битмапа: первый usable-регион, куда он целиком влезает (предпочтение `entry.base + 0x200000`).
- Резервируется: всё ниже 1 MiB (`LOW_RESERVED`), сам битмап; образ ядра резервируется косвенно —
  Limine помечает его `MEMMAP_EXECUTABLE_AND_MODULES`. Захватывается до `MAX_KERNEL_RANGES=16` диапазонов.
- `alloc_frame` — линейный поиск первого ненулевого слова + `trailing_zeros`. Double free
  детектится и логируется (`[PMM] DOUBLE FREE`), но не фатален.
- COW-счётчики: `COW_REFS: Spinlock<BTreeMap<u64, u32>>` (адрес кадра → число шарящих).
  Входит при fork со значением 2; `cow_unref` возвращает `true`, когда вызывающий должен освободить кадр.

### VMM (`vmm.rs`)
- Ходит по **активному** CR3 PML4 (не по кэшированному корню). Все разыменования таблиц —
  через `PageTableWalker` (адреса только через HHDM: `phys + hhdm`).
- Промежуточные таблицы выделяются из PMM и обнуляются; получают `PRESENT|WRITABLE`,
  `USER_ACCESSIBLE` — только если лист его запросил (с апгрейдом существующих).
- Транслируются и 1 GiB, и 2 MiB huge pages.
- `map` при ремапе-перезаписи логирует утечку, но не падает.
- COW: `COW_BIT = BIT_9` (software-бит PTE). `fork_user_space_cow` двухпроходный:
  проход 1 отбрасывает huge pages до мутаций, проход 2 шарит листы, снимает WRITABLE,
  ставит COW_BIT обеим сторонам, `cow_ref` + полный TLB flush. Промежуточные таблицы НЕ шарятся.
- `drop_user_space`: освобождает листы (с `cow_unref` для COW), затем PT/PD/PDPT/PML4 —
  по одному кадру промежуточного уровня строго **после** внутреннего цикла (ранний баг
  освобождал PD на каждый PT-entry и ловил double-free детектор).
- `map_mmio` маппит `PRESENT|WRITABLE|NO_CACHE|NO_EXECUTE` в `virt = phys_to_virt(phys)`
  (конвенция: MMIO живёт в HHDM-окне, отдельного окна нет).
- `load_cr3` — **единственное место записи CR3** (оба пути, tick и yield, идут через него).

### Куча (`heap.rs`)
- `IrqSafeAllocator` оборачивает `good_memory_allocator::SpinLockedAllocator`: внутренний лок
  galloc **не** глушит прерывания, а прерывание посреди аллокации при чужом irq-disabling
  спинлоке = дедлок. Обёртка маскирует IF на каждую операцию.
- Куча — фиксированный регион: `init()` маппит `HEAP_INITIAL_PAGES` страниц по `layout::heap_base()`
  (flags `PRESENT|WRITABLE|NO_EXECUTE`) и отдаёт аллокатору. Роста нет → OOM = null → alloc-error abort.
- Кап по RAM: если свободных кадров меньше, чем запрошено, `init()` капит кучу — четверть
  свободных кадров (минимум 8 MiB) остаётся PMM под user-страницы/COW/page tables, остальное
  уходит куче; при срабатывании капа пишется `[WARN] "Kernel heap capped"`. На конфигурации
  ≥1 GiB кап не срабатывает и куча полные 512 MiB.

## Важные константы

| Константа | Значение |
|---|---|
| Образ ядра (higher half) | `0xffffffff80000000` (`linker.ld`) |
| `KERNEL_STACK_REGION_BASE` | `0xFFFF_FE00_0000_0000`; слот на PID = 1 guard + 64 страниц |
| `USER_STACK_TOP` | `0x7000_8000_0000` (8 MiB, как Linux RLIMIT_STACK) |
| `USER_MMAP_BASE` | `0x2000_0000_0000` |
| Верхняя граница user VA | `0x0000_8000_0000_0000` (`USER_CANONICAL_LIMIT` в arch) |
| `HEAP_INITIAL_PAGES` | 512 MiB (нужно ≥1 GiB RAM; все раннеры дают `-m 1024M`; на меньшей RAM куча капится с `[WARN]`) |

## Зависимости

- **От:** `sync::spinlock::Spinlock`, Limine (`MemmapResponse`), `crate::HHDM_OFFSET` (lib.rs),
  крейт `x86_64` (paging, TLB), `arch::cpu` (контроль прерываний в куче).
- **На неё:** `boot.rs` (порядок init), `task` (стеки, fork/exit, COW-фолты), `arch::apic`/`acpi`
  (`map_mmio`), `arch::idt` (`virt_to_phys` для дампа стека), syscall-слой (`mem_sys`),
  драйверы (MMIO), `debug::unwind`.

## Грабли

- Куча не растёт: полный Debian-индекс (`apt update`) специально упирается в лимит
  `pkg::deb::MAX_INDEX_*`, чтобы падать чисто, а не OOM-абортом.
- `fork_user_space_cow` требует, чтобы родительское адресное пространство было АКТИВНО в CR3
  (нужен TLB-invalidate).
- Некоторые элементы `layout.rs` помечены `dead_code` до миграции вызовов планировщика.
