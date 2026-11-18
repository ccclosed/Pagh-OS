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
| `x86_64/mod.rs` | Корень x86_64: `acpi`, `apic`, `cpuid`, `gdt`, `idt`, `linux`, `pat`, `syscall` |
| `x86_64/gdt.rs` | GDT + TSS + IST-стеки, RSP0, селекторы |
| `x86_64/idt.rs` | IDT: все исключения CPU, IRQ-векторы 32–47, гейт `int 0x80`, постмортем page-fault/#GP |
| `x86_64/apic.rs` | LAPIC-таймер + I/O APIC роутинг + таблица обработчиков + EOI; гасит legacy 8259 PIC |
| `x86_64/acpi.rs` | Разбор MADT через крейт `acpi`; кэш `ApicAddrs` |
| `x86_64/pat.rs` | Обнаружение PAT (CPUID leaf 1 EDX bit 16) и однократное программирование `IA32_PAT` (MSR 0x277): записи 4/5 = WC, остальные сохранены, `CR0.CD` не меняется; чистая политика и PTE-флаги — в `memory::pat` |
| `x86_64/cpuid.rs` | CPUID-половина `/proc/cpuinfo`: читает листья в `CpuInfoData`, неподдерживаемые листья оставляет `None`; всё форматирование — в чистом `vfs::procfs_format` |
| `x86_64/syscall.rs` | MSRs SYSCALL/SYSRET (STAR/LSTAR/SFMASK/EFER), голые стабы `int80_stub`/`syscall_entry`, legacy-диспетчер, `sys_write`/`sys_exit` |
| `x86_64/linux/` | Слой совместимости с Linux x86_64 бинарями (см. ниже) |

## `linux/` — Linux compat

| Файл | Роль |
|---|---|
| `mod.rs` | Корень; `linux_dispatch(regs: *mut SavedRegs, reentry_allowed: u64)` — главный вход (бит 0 `REENTRY_ALLOWED` разрешает размаскировать IF: его передают оба входных стаба, boot-selftest передаёт 0; бит 1 `INT80_ENTRY` помечает вход через `int 0x80`, см. инвариант 2); `check_user_ptr` (диапазон + PRESENT/`USER_ACCESSIBLE` на каждой странице), `check_user_ptr_writable` (плюс право записи) и `check_user_ptr_bounds` (только диапазон, без PTE — для путей, копирующих `min(count, доступное)`); гейт набора `abi::SUPPORTED_SYSCALLS`; watchdog зависших syscall'ов |
| `regs.rs` | `SavedRegs` — 15-GPR фрейм, общий контракт обоих входных стабов |
| `abi.rs` | Чистый маршаллинг аргументов (`marshal_args`), полный перечень поддерживаемого набора `SUPPORTED_SYSCALLS` (единственный источник истины, `is_supported` — бинарный поиск по нему), константы номеров |
| `validate.rs` | Чистая валидация user-указателей (`spanned_pages`, границы/переполнение, без разыменований) |
| `trap_frame.rs` | Чистое зеркало фрейма `irq32_stub` (21 слово: popfq, 15 GPR в порядке `SavedRegs`, iret-слова) и план доставки сигнала с тика: ring-3 классификация, точный набор мутаций входа в обработчик, проверка «доставлен тот же сигнал» (хост-тестируемый, свойство `irq_frame`; const-asserts против индексов из `src/test.rs`) |
| `kill.rs` | Чистая модель `kill(2)`: 32-битное декодирование `pid_t`/`int`, классификация цели (`pid>0`, `0`, `-1`, `-pgid`, `INT_MIN → ESRCH`), политика прав `kill_permits` (root / real uid / effective uid цели; EPERM реально достижим), выбор одного потока на группу (хост-тестируемая, свойство `kill_target`) |
| `tty.rs` | Чистая модель управления терминалом (job control): foreground-pgid + `TOSTOP`, вердикт доступа (`SIGTTIN` на фоновое чтение, `SIGTTOU` на фоновую запись при `TOSTOP`), декодирование `int`-аргумента `TIOCSPGRP` (отрицательный → `EINVAL`) и `may_set_foreground` (живая группа / текущая / группа shell; иначе `EPERM`), константы `TIOCGPGRP` 0x540F / `TIOCSPGRP` 0x5410 и `TOSTOP` (хост-тестируемая, свойство `tty_job`; состояние — в `drivers/tty.rs`) |
| `cred.rs` | Чистая модель креденшелов: `Creds` (6 id), `Access`, POSIX `may` (owner/group/other по effective id, root bypass r/w, execute требует x), переходы setuid/setgid/setreuid/setregid/setresuid/setresgid, `may_chmod`/`may_chown` (хост-тестируемая, свойство P56) |
| `errno.rs` | Модель errno; ядро складывает `Err(e)` в rax как `-errno` (`-4095..=-1`) |
| `io.rs` | Чистое планирование `read`/`lseek` |
| `io_sys.rs` | Эффектные обработчики файлового I/O (FdTable, консоль, VFS/ext2) |
| `mem.rs` | Чистое планирование `brk`/`mmap`/`munmap`, `prot_to_flags` |
| `mem_sys.rs` | Эффектные memory-syscall'ы: `handle_user_page_fault` (demand paging для anon mmap/brk), VmRegionSet + VMM/PMM |
| `misc.rs` | `getpid`/`uname`/`arch_prctl`/`set_tid_address`/`clock_gettime`/`getrandom`/`exit`/`exit_group`, а также `get[res]{u,g}id`/`set[res]{u,g}id` (читают и меняют `CompatState::creds`) |
| `stat.rs` | Кодирование `struct stat` (`LinuxStat`) для `stat`/`fstat`/`newfstatat` |
| `dirent.rs` | Чистая упаковка `linux_dirent64` для `getdents64` |
| `epoll_sys.rs` | `clock_getres`, `eventfd2`, `epoll_create1/ctl/wait` |
| `process_sys.rs` | Процессные syscall'ы: `execve`, `clone`, семейство futex |
| `unix_sock.rs` | AF_UNIX stream-сокеты: socket/connect/accept/bind/listen/socketpair; реестр слушателей по пути, байтовые очереди; `endpoint_wait_verdict` — пробуждение блокирующего ЧТЕНИЯ (`EBADF`/EOF), `endpoint_write_verdict` — блокирующей ЗАПИСИ (закрытый пишущий fd → `EBADF`); `sys_shutdown` (nr 48) — полузакрытие направления |
| `inet_sock.rs` | AF_INET TCP/UDP поверх собственного стека (`net::`) — curl, glibc resolver/DNS; `tcp_apply_shutdown` — `shutdown(SHUT_WR)` шлёт FIN, оставляя сокет принимающим |
| `sock_shutdown.rs` | Чистая политика `shutdown(2)` (nr 48): порядок отказов (`EBADF` → `ENOTSOCK` → `EINVAL` → `ENOTCONN`) и что значит каждый флаг направления для read/write/poll (хост-тестируемая, свойство `p59`) |
| `rtc.rs` | Чтение CMOS RTC (порты 0x70/0x71) |
| `timeconv.rs` | Чистый BCD-декод и civil-date → Unix-seconds (хост-тестируемый) |
| `rand_clock.rs` | Чистое планирование `getrandom`/`ticks_to_timespec` |
| `diag.rs` | Чистая дедупликация nosys-логов per-process и нормализация exit-кодов (139 = 128+SIGSEGV) |
| `signal.rs` | Доставка POSIX-сигналов: очередь pending, `deliver_one_pending_syscall` в эпилоге `linux_dispatch` (возврат из syscall) и `tick_action` на возврате тика (только текущей задаче, ring-3 проверка ДО выбора сигнала, `sys_exit_group` из IRQ не вызывается), `sys_kill` (nr 62: pid / своя группа / `-pgid` / `-1`, по одной копии на группу), `send_signal_to_pgid` (одна копия на группу — для `kill`) и `send_signal_to_pgid_all` (по одной копии на КАЖДУЮ thread-группу внутри pgid — для терминальных `SIGTTIN`/`SIGTTOU`/`SIGTSTP`/`SIGCONT` и `^C`/`^Z`: групповая остановка в планировщике per-thread-group, лидеру её недостаточно), `SIGSTOP`/`SIGCONT` (стоп — действие доставки, парковка фреймов; продолжение — генерация `SIGCONT` отправителем + сброс pending stop-битов), `rt_sigaction`/`rt_sigprocmask`/`rt_sigreturn`, `tgkill`, маски, guard'ы инварианта 2 (флаг int80-входа + проверка кадра) |
| `signal_frame.rs` | Чистая сборка/разбор `rt_sigframe` на user-стеке (хост-тестируемая) |

Чистые модули (`abi`, `cred`, `diag`, `dirent`, `errno`, `io`, `kill`, `mem`, `rand_clock`, `regs`,
`signal_frame`, `sock_shutdown`, `sock_wait`, `stat`, `timeconv`, `trap_frame`, `tty`, `validate`) через `#[path]` включаются в
хост-крейт `host-tests` — они обязаны быть только `core`+`alloc`.

## Ключевые символы

- `cpu`: `enable_sse()`, `halt_loop() -> !`, `enable/disable_interrupts()`, `without_interrupts(f)`.
- `gdt`: `init()`, `Selectors::{kernel_code, kernel_data, user_code, user_data}`, `set_kernel_stack(rsp0)`, `IST_DOUBLE_FAULT=1`, `IST_PAGE_FAULT=2`.
- `idt`: `init()`, `STACK_DUMP: AtomicBool`, `note_syscall(pid, nr)`.
- `apic`: `init()`, `register_irq(vector, handler)`, `irq_dispatch(vector)`, `send_eoi()`, `route_irq()`, `TICK_HZ=1000`, `ms_to_ticks()`.
- `acpi`: `ApicAddrs { lapic_phys, ioapic_phys, gsi_base }`, `apic_addresses()`.
- `syscall`: `init()`, `SYS_WRITE=1/SYS_EXIT=2/SYS_YIELD=3`, `legacy_dispatch`, `extern "C" int80_stub/syscall_entry`.
- `linux`: `linux_dispatch`, `check_user_ptr`, `check_user_ptr_bounds`,
  `mem_sys::populate_user_in/populate_user_out` (подпирание ленивого буфера перед копией).

### Ленивые пользовательские буферы (read/write)
- `read(2)`/`pread(2)`/`readv(2)` копируют `min(count, доступное)`, а `write(2)`/
  `pwrite(2)`/`writev(2)` читают весь `count`, поэтому требовать PRESENT на всех
  страницах `count` нельзя: свежий `Vec`/`Box` на ленивой куче `brk` или анонимный
  `mmap` (`vec![0u8; total]`) отвергался `EFAULT`'ом на любой длине больше страницы.
  Приёмник/источник подпирается `mem_sys::populate_user_in/out` — той же политикой,
  что у `handle_user_page_fault`, через чистое правило `validate::page_acceptance`
  (host-свойство `user_buffer_paging`): отсутствующая страница подпирается только в
  ленивом `brk`/anon-регионе, а `PROT_NONE`, eager-файловый регион и адрес вне
  регионов остаются `EFAULT`. Права записи по-прежнему проверяет `copy_out_checked`
  после подпирания, поэтому TOCTOU-контракт (F-r4-arch-1) не ослаблен.
- Буфер под копию наружу/внутрь выделяется fallible (`try_user_buf`): `count`
  приходит от пользователя (до `COUNT_MAX` = 2 ГиБ), куча ядра фиксирована, а
  неудачная аллокация в `no_std` паникует — `panic = "abort"` останавливает
  машину. Отказ — `ENOMEM`, как нехватка памяти в Linux.
- Остаток, названный явно: `signalfd`-чтение и `getdents64` сохраняют старую
  проверку (первый — вне общего пути, второй исполняется под `COMPAT_STATES`, где
  `handle_user_page_fault` отказывает по реентерабельности).
- `pat`: `init()` — PAT-детект и WC-записи `IA32_PAT`; `memory::pat::{wc_supported, wc_pte_flags, fallback_pte_flags}`.

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
  kernel-mode фолт → backtrace + halt; ring-3 фолт → SIGSEGV (код 139) и завершение виновной
  задачи — **всегда**, в том числе у нативной ring-3 задачи без `CompatState` (S4b: раньше
  такой отказ уходил в `halt()` и уносил машину).
- `gp_fault_handler` дамплит слова вокруг RSP (с проверкой `virt_to_phys`, чтобы не каскадить
  PF на guard-странице).
- **Политика пользовательских исключений (см. `SECURITY.md`).** `#DE`/`#DB`/`#UD`/`#OF`/`#BR`/
  `#NM`/`#AC` (и user-reachable `#MF`/`#XM`) идут через чистую `linux::trap_frame::exception_action`
  (host-свойство `exception_trap`): исключение из ring 3 завершает виновную задачу —
  `exit_code = 128 + signal` для Compat_Process и `exit_current()` для любой ring-3 задачи
  («[EXC #N] killing task …»), машина живёт даже без compat-состояния; ring 0 на kernel-RIP
  (и ring 0 без compat, чтобы кого-то винить) → `halt()`, настоящая ошибка ядра остаётся
  фатальной. `syscall`
  маскирует TF в kernel-RFLAGS (`SFMASK`), чтобы пользовательский single-step не превращался
  в kernel-mode `#DB`; сам TF сохраняется в r11/sysretq.

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
- Конвейер `linux_dispatch`: размаскировать IF, если вызывающий передал `REENTRY_ALLOWED`
  → `abi::marshal_args` → `note_syscall` →
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
