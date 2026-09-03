# `src/arch/` — архитектурный слой x86_64

Всё, что касается CPU и привилегированных инструкций: GDT/IDT/TSS/IST, прерывания,
LAPIC + I/O APIC, ACPI, вход в syscall и слой совместимости с Linux-бинарями.
Безопасные обёртки над priv-инструкциями — в `cpu.rs`; serial и port I/O живут в
`src/drivers/serial.rs`, не здесь.

## Файлы

| Файл | Роль |
|---|---|
| `mod.rs` | Корень: `cpu`, `x86_64` |
| `cpu.rs` | Безопасные обёртки: `enable_sse()`, `hlt`, `cli/sti`, `interrupts_enabled()`, `without_interrupts(f)`, `read_msr/write_msr` |
| `x86_64/mod.rs` | Корень x86_64: `acpi`, `apic`, `gdt`, `idt`, `linux`, `syscall` |
| `x86_64/gdt.rs` | GDT + TSS + IST-стеки, RSP0, селекторы |
| `x86_64/idt.rs` | IDT: все исключения CPU, IRQ-векторы 32–47, гейт `int 0x80`, постмортем page-fault/#GP |
| `x86_64/apic.rs` | LAPIC-таймер + I/O APIC роутинг + таблица обработчиков + EOI; гасит legacy 8259 PIC |
| `x86_64/acpi.rs` | Разбор MADT через крейт `acpi`; кэш `ApicAddrs` |
| `x86_64/syscall.rs` | MSRs SYSCALL/SYSRET (STAR/LSTAR/SFMASK/EFER), голые стабы `int80_stub`/`syscall_entry`, legacy-диспетчер, `sys_write`/`sys_exit` |
| `x86_64/linux/` | Слой совместимости с Linux x86_64 бинарями (см. ниже) |

## `linux/` — Linux compat

| Файл | Роль |
|---|---|
| `mod.rs` | Корень; `linux_dispatch(regs: *mut SavedRegs)` — главный вход; `check_user_ptr` — единая точка валидации user-указателей; гейт поддерживаемого набора; watchdog зависших syscall'ов |
| `regs.rs` | `SavedRegs` — 15-GPR фрейм, общий контракт обоих входных стабов |
| `abi.rs` | Чистый маршаллинг аргументов (`marshal_args`), membership поддерживаемого набора, константы номеров |
| `validate.rs` | Чистая валидация user-указателей (`spanned_pages`, границы/переполнение, без разыменований) |
| `errno.rs` | Модель errno; ядро складывает `Err(e)` в rax как `-errno` (`-4095..=-1`) |
| `io.rs` | Чистое планирование `read`/`lseek` |
| `io_sys.rs` | Эффектные обработчики файлового I/O (FdTable, консоль, VFS/ext2) |
| `mem.rs` | Чистое планирование `brk`/`mmap`/`munmap`, `prot_to_flags` |
| `mem_sys.rs` | Эффектные memory-syscall'ы: `handle_user_page_fault` (demand paging для anon mmap/brk), VmRegionSet + VMM/PMM |
| `misc.rs` | `getpid`/`uname`/`arch_prctl`/`set_tid_address`/`clock_gettime`/`getrandom`/`exit`/`exit_group` |
| `stat.rs` | Кодирование `struct stat` (`LinuxStat`) для `fstat`/`newfstatat` |
| `dirent.rs` | Чистая упаковка `linux_dirent64` для `getdents64` |
| `epoll_sys.rs` | `clock_getres`, `eventfd2`, `epoll_create1/ctl/wait` |
| `process_sys.rs` | Процессные syscall'ы: `execve`, `clone`, семейство futex |
| `unix_sock.rs` | AF_UNIX stream-сокеты: socket/connect/accept/bind/listen/socketpair; реестр слушателей по пути, байтовые очереди |
| `inet_sock.rs` | AF_INET TCP/UDP поверх собственного стека (`net::`) — curl, glibc resolver/DNS |
| `rtc.rs` | Чтение CMOS RTC (порты 0x70/0x71) |
| `timeconv.rs` | Чистый BCD-декод и civil-date → Unix-seconds (хост-тестируемый) |
| `rand_clock.rs` | Чистое планирование `getrandom`/`ticks_to_timespec` |
| `diag.rs` | Чистая дедупликация nosys-логов per-process и нормализация exit-кодов (139 = 128+SIGSEGV) |

Чистые модули (`abi`, `diag`, `dirent`, `errno`, `io`, `mem`, `rand_clock`, `stat`, `timeconv`,
`validate`) через `#[path]` включаются в хост-крейт `host-tests` — они обязаны быть
только `core`+`alloc`.

## Ключевые символы

- `cpu`: `enable_sse()`, `halt_loop() -> !`, `enable/disable_interrupts()`, `without_interrupts(f)`.
- `gdt`: `init()`, `Selectors::{kernel_code, kernel_data, user_code, user_data}`, `set_kernel_stack(rsp0)`, `IST_DOUBLE_FAULT=1`, `IST_PAGE_FAULT=2`.
- `idt`: `init()`, `STACK_DUMP: AtomicBool`, `note_syscall(pid, nr)`.
- `apic`: `init()`, `register_irq(vector, handler)`, `irq_dispatch(vector)`, `send_eoi()`, `route_irq()`, `TICK_HZ=1000`, `ms_to_ticks()`.
- `acpi`: `ApicAddrs { lapic_phys, ioapic_phys, gsi_base }`, `apic_addresses()`.
- `syscall`: `init()`, `SYS_WRITE=1/SYS_EXIT=2/SYS_YIELD=3`, `legacy_dispatch`, `extern "C" int80_stub/syscall_entry`.
- `linux`: `linux_dispatch`, `check_user_ptr`.

## Как работает

### GDT/TSS/IST
- Статики в `SyncUnsafeCell` (init-once). Два IST-стека по 16 KiB: IST1 — double fault, IST2 — page fault.
- Порядок селекторов важен: **user_data стоит непосредственно перед user_code**, потому что
  `sysretq` вычисляет `SS = STAR[63:48]+8` и `CS = STAR[63:48]+16` без обращения к GDT.
- После `gdt.load()` код-сегмент перезагружается через far `retfq`, и **SS тоже надо перезагрузить**:
  Limine оставляет SS=0x30, что после swap указывает в дескриптор TSS → `#GP` на первом `iretq`.
- RSP0 программируется per-task через `set_kernel_stack`. **Ограничение: один слот RSP0 =
  один ring-3 процесс единовременно.**

### IDT
- Вектор 32 — голый стаб `task::switch::irq32_stub` (вытеснение по таймеру); векторы 33–47
  генерируются макросом `irq_handler!` (вызов `apic::irq_dispatch` + `send_eoi`); вектор 0x80 —
  `int80_stub` с DPL=3.
- `page_fault_handler`: сначала пробует demand paging через `linux::mem_sys::handle_user_page_fault`;
  kernel-mode фолт → backtrace + halt; ring-3 фолт → SIGSEGV (код 139), убивает только Compat_Process.
- `gp_fault_handler` дамплит слова вокруг RSP (с проверкой `virt_to_phys`, чтобы не каскадить
  PF на guard-странице).

### APIC / ACPI
- 8259 замаскирован; LAPIC включается через MSR 0x1B. MMIO LAPIC/IOAPIC маппится самим модулем
  через `vmm::map_mmio`; в `LAPIC_BASE`/`IOAPIC_BASE` лежат **уже виртуальные** HHDM-адреса —
  второй раз HHDM прибавлять нельзя.
- LAPIC-таймер: periodic, вектор 32, divider 16; `TICK_HZ=1000` — константа отсчёта времени всего ядра.
- MADT парсится один раз под спинлоком; при неудаче — дефолты `lapic_phys=0xFEE0_0000`, `ioapic_phys=0`.
  RSDP берётся из `RSDP_REQUEST` lib.rs. Парсинг требует кучу (идёт после `heap::init`).

### Вход в syscall
- `init()` ставит `EFER.SCE`, пишет STAR (fail-loudly: регрессия, когда STAR молча обнулялся,
  роняла ring 3 с `#GP` на первом тике), LSTAR=`syscall_entry`, SFMASK глушит IF.
- Два пути, оба собирают одинаковый `SavedRegs` и зовут `linux_dispatch`:
  - `int 0x80`: CPU сам переключается на TSS RSP0, пушит 5 qword'ов + 15 GPR, фрейм в rdi.
  - `syscall`: переключения стека НЕТ; стаб прячет user RSP в scratch, поднимает kernel-стек
    из глобального зеркала, пушит per-task слот user RSP и 15 GPR.
- Конвейер `linux_dispatch`: включить IF → `abi::marshal_args` → `note_syscall` →
  шим нативных задач (nr 1/2/3 без compat → `legacy_dispatch`) → гейт `is_supported`
  (`-ENOSYS` до любых проверок указателей) → роутинг → фолдинг errno в rax.
- Legacy `sys_write`: только fd==1, `len <= 4096`, обе границы ниже `USER_CANONICAL_LIMIT`,
  каждая страница проверена `virt_to_phys` до разыменования.

## Зависимости

- **От:** `memory::vmm` (map_mmio, virt_to_phys), `memory::layout`, `task::switch/scheduler/process/compat`,
  `drivers::ps2_kbd/ps2_mouse`, `sync::spinlock`, request-статики lib.rs.
- **На неё:** фактически всё ядро — `sync::spinlock` построен на `without_interrupts`;
  планировщик живёт на векторе 32; boot.rs задаёт порядок init.

## Грабли

- `set_kernel_stack` + `set_syscall_kernel_stack` обязаны получать одно значение при спавне ring-3 задач.
- `syscall_entry` не использует GS base/`swapgs`; зеркала стеков — глобальные, точные только
  в однозадачной RSP0-модели.
- `page_fault_handler` после успешного demand paging делает return (переисполнение инструкции);
  всё остальное — park.
- Комментарий в `apic.rs` про маппинг LAPIC в lib.rs устарел: маппинг теперь внутри `apic::init`.
