# `src/task/` — планировщик, процессы, fd-таблица

Вытесняющий round-robin планировщик, контекст-свитчи на asm, создание ring-3 процессов,
загрузка Linux-бинарей, per-pid состояние совместимости и файловые дескрипторы.

Важно: **syscall-диспетчер и Linux compat layer живут не здесь** — в
`src/arch/x86_64/syscall.rs` и `src/arch/x86_64/linux/`. ELF-загрузчик — в `src/vfs/elf.rs`.
Все три являются основными контрагентами `task/`.

## Файлы

| Файл | Роль |
|---|---|
| `mod.rs` | Корень: реэкспорт 8 субмодулей |
| `scheduler.rs` | Round-Robin: `Tcb`, ready-queue, вытеснение по тику (`scheduler_tick_irq`), кооперативный yield, exit/reap, idle-задача, tripwire'ы повреждения фрейма |
| `switch.rs` | Контекст-свитч на asm: раскладка сохранённого фрейма, `yield_switch()`, `irq32_stub`, `kernel_thread_trampoline`, `scheduler_exit_thread` |
| `process.rs` | Создание user-процессов: ring-3 стартовые фреймы, `create_user_process`, `run_linux_binary`, `exec_linux_image`, `fork_linux_process`, `spawn_linux_thread` |
| `compat.rs` | Per-pid реестр `CompatState` (`COMPAT_STATES`): fd-таблица, VM-состояние, TLS, tid/tgid/ppid, cwd, umask, rlimits, exit-коды, зомби (`EXITED_CHILDREN`) |
| `fd.rs` | Ядровая fd-таблица: `OpenObject` (консоль, файл, dir, pipes, сокеты, epoll, eventfd), `FdTable`, `PipeEndpoint`, plumbing AF_UNIX-слушателей |
| `fd_alloc.rs` | Чистая bookkeeping fd-слотов (`FdSlots<T>`, `lowest_free_index`); core+alloc, шарится в host-tests |
| `fpu.rs` | FPU/SSE per-task: eager FXSAVE/FXRSTOR (`FxArea`, 512 B, align 64) |
| `stack.rs` | Чистый энкодер стартового стека System V + auxv (`build_initial_stack`, `AuxInputs`); хост-тестируемый |
| `stack_map.rs` | Эффектная половина: `map_initial_stack` маппит user-стек под user CR3 и копирует образ |

## Ключевые символы

- `scheduler`: `Tcb { pid, kernel_rsp, cr3 }`, `spawn(tcb)`, `schedule()`, `requeue(tcb)`,
  `scheduler_tick_irq(rsp) -> rsp`, `scheduler_yield_switch(rsp) -> rsp`,
  `kernel_thread_spawn(entry: fn())`, `yield_current()`, `exit_current() -> !`,
  `request_exit(pid)`, `next_pid()/current_pid()/set_current_pid()`, `ticks()/tick()`,
  `sleep_ticks(n)`, `check_frame(who, pid, rsp)`, `IDLE_PID=0`.
- `process`: `create_user_process(elf_data)`, `spawn_test_user_process()`,
  `run_linux_binary(path, argv, envp) -> Result<u64, RunError>`, `fork_linux_process(regs, clear_child_tid)`,
  `spawn_linux_thread(regs, user_rsp)`, `RunError::{ArgsTooLarge, NotFound, LoadFailed, StackFailed}`.
- `compat`: `CompatState`, `install_compat/remove_compat/with_current_compat`,
  `fork_current_compat/clone_current_compat`, `finish_compat_exit`, `reap_child/has_child`,
  `compat_is_raw(pid)`, `compat_lock_held()`.
- `fd`: `OpenObject::{Console, Stdin, InetTcp, InetUdp, File, PipeRead/PipeWrite, Eventfd, Socket, UnixListener, UnixSocketUnbound, Epoll, Dir}`,
  `FdTable::{alloc, get, close, dup, dup_min, dup_to, pipe, socketpair, set_cloexec}`,
  `PipeEndpoint` c `PipeReadResult`/`PipeWriteResult`.
- `fpu`: `FxArea`, `area_for(pid)`, `save_if_user/restore_if_user`.
- `stack`: auxv-теги `at::*`, `build_initial_stack(...)`, `StackImage`, `arg_gate` (≤256 аргументов, ≤4096 байт).

## Как работает

### Планировщик
- Одно CPU, строгий Round-Robin. Глобальный `READY_QUEUE: Spinlock<VecDeque<Tcb>>`.
  Запущенная задача представлена только `CURRENT_PID` + RSP CPU; `Tcb` пересобирается
  из `current_rsp` каждый тик.
- Linux-compat-состояние сознательно НЕ на `Tcb` — единственный источник правды `COMPAT_STATES`.
- Тик: LAPIC-таймер, вектор 32 (`irq32_stub`), `TICK_HZ=1000` (1 мс). От неё считаются
  `sleep_ticks`, таймауты futex, watchdog (зависший syscall ≥ 500 тиков = 5 c).

### Контекст-свитч: один канонический 21-словный фрейм
Все три пути (spawn / кооперативный / вытесняющий) используют одну раскладку (low→high):
```
[rsp+0]     RFLAGS для popfq (IF=0)
[+8..+120]  r15..rax (15 GPR)
[+128] RIP  [+136] CS  [+144] RFLAGS  [+152] RSP  [+160] SS   (iretq-фрейм)
```
- Кооперативный: `yield_current` → `switch::yield_switch` (asm) → `scheduler_yield_switch`.
  Задача ре-эн queue'ится **между** сохранением фрейма и переключением стека
  («stage-13.6 fix» — иначе clone-треды вечно висели в FUTEX_WAIT).
- Вытесняющий: `irq32_stub` (CPU уже пушнул iret-фрейм; стаб пушит 15 GPR + pushfq) →
  `scheduler_tick_irq`: реап минимум одной exit-задачи за тик, EOI, выбор следующего,
  `activate_task` (TSS RSP0 + зеркало syscall-стека + FS base), `vmm::load_cr3` (заодно TLB flush),
  FPU restore, возврат нового RSP.
- Long-mode грабля: `iretq` ВСЕГДА попает 5 слов даже ring0→ring0; синтетические 3-словные
  фреймы ловили фолт на guard-странице.
- Kernel-стек per-pid: `memory::layout::kernel_stack_for_pid(pid)` — 64 страницы + 1 guard.
- Exit: `exit_current` помечает `EXITING_PIDS`; следующий тик выкидывает задачу и ставит
  `PENDING_REAPS`; тик позже освобождает kernel-стек, приватный user PML4 (только при
  эксклюзивном владении — шареный CR3 = треды) и FPU-область. Реап отложен на тик,
  потому что дропающий тик исполняется НА стеке умирающей задачи.

### Ring 3
- `build_ring3_frame` пишет iretq-фрейм с user CS/SS (`user_code/user_data | 3`),
  RFLAGS `0x202`, entry RIP, user RSP; смену привилегии делает `iretq`.
- Ring 3 → ring 0 приходит через TSS RSP0 (`int 0x80`, таймер) или зеркало
  `SYSCALL_KERNEL_RSP` (`syscall` не переключает стек автоматически).

### Linux-процессы
- `run_linux_binary` → `exec_linux_image`: чтение файла, ELF-загрузка (`vfs::elf::ElfLoader::load_linux`,
  ET_EXEC и ET_DYN/static-PIE с bias), поиск интерпретатора (`/lib64/ld-linux-x86-64.so.2`
  с фоллбэком в merged-usr пути), маппинг стека через `stack_map::map_initial_stack`.
- `sys_clone` (в `arch/.../linux/process_sys.rs`) различает тред (`CLONE_VM|CLONE_THREAD` →
  общий CR3, `spawn_linux_thread`) и fork (→ `fork_linux_process`, COW через
  `vmm::fork_user_space_cow`, с eager-фоллбэком при huge pages). `CLONE_VM` без
  `CLONE_THREAD` (vfork/posix_spawn) — сознательно ENOSYS, чтобы libuv шла через fork+exec.
- Зомби: `finish_compat_exit` кладёт детей в `EXITED_CHILDREN`; `sys_wait4` опрашивает
  `reap_child`, уступая между проверками.

## Реализованные Linux syscall'ы (кратко)

Полный список — в `dispatch_supported` (`arch/x86_64/linux/mod.rs`):
I/O (read/write/writev, open/openat, lseek, rename, fstat/statx, ioctl, poll/select, pipe,
getdents64, getcwd/chdir, dup/fcntl, socketpair), память (brk, mmap, munmap, mremap, mprotect),
сокеты AF_UNIX и AF_INET/6 (TCP/UDP), epoll/eventfd, время (clock_gettime, nanosleep,
gettimeofday), процессы (getpid/getppid/gettid, wait4, clone, execve, exit/exit_group,
futex WAIT/WAKE, arch_prctl, set_tid_address, robust list, prlimit64, umask, getrandom,
sched_yield, tgkill и др.).

## ELF-загрузчик — `src/vfs/elf.rs`

Используется `task::process`. Валидирует ELF64/LSB/`EM_X86_64`, отвергает user VA ≥
`USER_ADDR_MAX = 0x0000_8000_0000_0000`, маппит `PT_LOAD` в свежий user PML4
(высшая половина ядра — по ссылке). Два входа: `ElfLoader::load` (нативный, только `ET_EXEC`)
и `ElfLoader::load_linux` (+ static-PIE через `vfs::elf_classify::{classify_elf, choose_bias}`).
`map_interpreter` маппит PT_INTERP на `INTERP_BASE = 0x0000_7000_0000_0000`.

## Константы

| Константа | Значение |
|---|---|
| `IDLE_PID` | 0 (boot/main-поток, всегда runnable) |
| `FIRST_DYNAMIC_FD` | 3 (0/1/2 — stdin/консоль/консоль) |
| `PIPE_CAPACITY` | 64 KiB |
| FXSAVE | 512 B, align 64, `FCW_INIT=0x037F`, `MXCSR_INIT=0x1F80`, eager-политика |
| `cwd`/`umask` по умолчанию | `/mnt`, `0o022` |

## Грабли

- `COMPAT_STATES` спинлок **нереентерабелен**: замыкания `with_current_compat` не должны
  блокироваться на device I/O и не должны реэнтерить реестр; page-fault-обработчик
  проверяет `compat_lock_held()` (`COMPAT_DEPTH`), чтобы не дедлокнуться.
- `spawn`/`requeue` дропают дубликат уже стоящего в очереди pid (лог `[SCHED] DOUBLE ENQUEUE`).
- `setup_task_kernel_stack` перепрограммирует единственный глобальный слот RSP0/syscall-стека —
  fork/clone обязаны вернуть его на РОДИТЕЛЯ перед возвратом.
- CR3-чувствительные шаги запуска — внутри `without_interrupts` (тик, увидевший чужой CR3,
  ломает планирование); ext2-чтения до guard'а, потому что могут блокироваться.
- futex-очереди — тикетные, кооперативные, ключ `(pml4_phys, uaddr)`.
