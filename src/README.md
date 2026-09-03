# `src/` — карта кодовой базы ядра pagh

Ядро — `#![no_std]`-крейт `pagh` (crate-type `staticlib`), target `x86_64-unknown-none`,
boot через Limine. Корень крейта — `lib.rs`, вся init-последовательность — в `boot.rs`.
Документация по подсистемам — в `README.md` внутри каждой папки.

## Папки

| Папка | Назначение | Документация |
|---|---|---|
| `arch/` | CPU-слой: GDT/IDT/TSS/IST, APIC/ACPI, syscall-вход, Linux compat layer | [arch/README.md](arch/README.md) |
| `debug/` | Stack trace без heap (panic/фолт-контексты) | [debug/README.md](debug/README.md) |
| `drivers/` | Все устройства: PCI, e1000, virtio-blk, NVMe, PS/2, framebuffer, VT, курсор, serial | [drivers/README.md](drivers/README.md) |
| `fs/` | ext2 + WAL-журнал | [fs/README.md](fs/README.md) |
| `memory/` | PMM (битмап + COW), VMM (4-уровневый paging), куча, layout | [memory/README.md](memory/README.md) |
| `net/` | Собственный TCP/IP-стек, DNS, HTTP/HTTPS(TLS) | [net/README.md](net/README.md) |
| `pkg/` | apt-style пакетный менеджер (.deb, индексы, tar) | [pkg/README.md](pkg/README.md) |
| `security/` | Аппаратная энтропия (fail-closed) | [security/README.md](security/README.md) |
| `shell/` | Интерактивный shell, paint, nano, мини-Rust toolchain | [shell/README.md](shell/README.md) |
| `sync/` | Спинлок с маскированием IF | [sync/README.md](sync/README.md) |
| `task/` | Планировщик RR, процессы, fd-таблица, Linux compat-состояние | [task/README.md](task/README.md) |
| `vfs/` | VFS-трейт, ramfs, ELF-загрузчик | [vfs/README.md](vfs/README.md) |

## Верхнеуровневые файлы

### `lib.rs` — корень крейта
- Атрибуты: `no_std`, `no_main`, фичи (`abi_x86_interrupt`, `allocator_api`,
  `custom_test_frameworks`, `sync_unsafe_cell`); `#![test_runner]` — пустой стаб,
  реальный харнесс — `test::run_all()` из shell.
- Дерево модулей: `arch, boot, debug, drivers, fs, log, memory, net, pkg, provision,
  security, shell, sync, task, test, vfs`; `selftest_lx` — под feature-гейтами
  (`lx_selftest`/`lx_livetest`/`lx_bigindex`).
- Limine-запросы в секции `.requests`: `BASE_REVISION` (rev 2), `HHDM_REQUEST`,
  `MEMMAP_REQUEST`, `KERNEL_ADDR_REQUEST`, `FRAMEBUFFER_REQUEST`, `RSDP_REQUEST`.
- Глобалы: `HHDM_OFFSET`, `KERNEL_BASE`, `KERNEL_SIZE` (заполняет `boot::start`).
- `_start() -> !` → `boot::start()`.
- `#[panic_handler]`: cli → `[PANIC] file:line — msg` → `debug::unwind::stack_trace()` → halt.

### `boot.rs` — оркестратор загрузки
Инициализация по фазам, каждая — приватная fn, ошибка → `fatal(step)` (error + halt).
Точный порядок:
1. `enable_sse()` — первым (Limine отдаёт управление с CR4.OSFXSR=0, а прологи
   `x86-interrupt` эмитят `movaps` — иначе `#UD`-хендлер рекурсирует в triple fault).
2. `serial::init` (COM1).
3. Проверка Limine base revision; чтение HHDM/KERNEL_ADDR ответов.
4. `gdt::init` + `idt::init`.
5. `syscall::init` (после GDT — STAR читает селекторы).
6. `pmm::init` → `vmm::init` → `heap::init`.
7. `apic::init` + роутинг IRQ1/IRQ12 (клавиатура/мышь).
8. `drivers::init` (PS/2 + framebuffer).
9. virtio: `pci::enumerate()` → `virtio::blk::init_blk` (после кучи — enumerate аллоцирует Vec).
10. `scheduler::init`, `vfs::init`.
11. `init_fs`: virtio-blk → NVMe фоллбэк; `Ext2Fs::mount`; формат только genuinely-blank диска;
    `vfs::mount_at("/mnt", root)`; `fs_boot_demo()`; `provision::seed()`.
12. `net::init()` + echo-сервисы на порту 7.
`kernel_main`: спавн `shell_thread` (PID 1) и `net::net_thread`; feature-гейтнутые selftest-хуки;
`enable_interrupts()`; main-поток — `halt_loop()`. `shell_thread` после стабилизации
спрашивает `provision::prompt_base_packages()` (Y/n) и запускает shell.

Грабли: `create_user_process` обязан идти с выключенными прерываниями (тик, увидев чужой CR3,
ломает планирование); отсутствие диска/NIC — warn, не fatal; ext2-маунт до включения прерываний.

### `log.rs` — логгирование
Leveled-фасад: `error!/warn!/info!/debug!/trace!` + runtime-фильтр (`ACTIVE_LEVEL`, дефолт Info).
Проверка уровня до форматирования; запись потоковая в sinks, без heap-String.
Sinks: serial всегда, framebuffer условно. Гейты fb-зеркала: `FB_MIRROR_PAUSED`
(фоновые задачи глушат fb-чат) и `FB_WARN_MIRROR` (дефолт false — warn/info на fb заглушены,
тумблер `warn on|off` в shell; чтобы не ломать TUI). `[ERROR]` зеркалится всегда.

### `provision.rs` — первичный userland
- `seed()` — идемпотентный посев ФС первого бута (guard `/mnt/etc/pagh-release`):
  `etc/`, `home/user/`, `usr/share/pagh/`, `examples/hello/`; `resolv.conf` (10.0.2.3),
  `hosts`, `motd`, README, LICENSE-NOTICE, hello-пример. `write_once` — не перезаписывает.
- `prompt_base_packages() -> bool` — Y/n на консоли (raw PS/2 set-1: y=0x15, Enter=0x1C,
  n=0x31); false, если python уже стоит.
- `ensure_base_packages_thread()` — фон-поток: пауза fb-зеркала, sleep ~10 c (DHCP),
  до 6 ретраев `apt::update()`, затем `apt::install("python3")` (тянет libc6, libpython).
  Зависит от `pkg::apt`, `vfs`, `scheduler::sleep_ticks`, `drivers` (клавиатура).

### `selftest_lx.rs` — Linux-совместимость selftests (feature-gated)
Компилируется только под `lx_selftest`/`lx_livetest`/`lx_bigindex`.
- `run()` — 13 проверок до включения прерываний: end-to-end запуск hand-assembled Linux ELF
  (write + exit_group), изоляция exit, ENOENT, arch_prctl/uname/tid, сохранность регистров
  через `linux_dispatch`, OOM-rollback brk/mmap, fetch без сети, ext2 install roundtrip,
  getcwd/chdir/dup/gettimeofday/getdents.
- `run_post_net_checks()` — apt E2E через локальное мини-зеркало (`tools/mini_repo.py`) +
  HTTPS smoke (реальный TLS 1.3 GET на deb.debian.org).
- `run_live_update_check()` (`lx_livetest`) — полный live `apt update` (HTTP, не HTTPS —
  у embedded-tls детерминированный хэнг на больших стримах), assert count ≥ 50 000.
- `run_bigindex_check()` (`lx_bigindex`) — репро parse-краша #14; вариант in-RAM
  (`lx_bigindex_inram`) отделяет parse/heap от net/scheduler.
Все проверки печатают `LXSELFTEST <name> PASS/FAIL` и возвращаются — паника в проверке
убила бы харнесс (`panic = "abort"`). Хелпер `with_synth_compat` ставит временный
`CompatState`; scratch-страница на `0x0000_4000_0000_0000` для user-указателей.

### `test.rs` — in-QEMU kernel self-test suite (~45 рутин)
- `assert_kernel!`/`assert_eq_kernel!` печатают `FAIL: file:line: msg` и продолжают (не аборт).
- `all_tests() -> Vec<(&'static str, fn())>`; `run_all()` — вызывается вручную командой
  `selftest` в shell, не на буте. Все рутины неразрушающие (восстанавливают PMM, кучу, IF, VFS).
- `mock_block` — RAM-блокдевайс с crash-инъекцией (`set_crash_after(n)` — тишие дропы записей
  после N, симуляция потери питания для тестов журнала).
- Покрытие: PMM (P1/P2/P15), VMM (P3/P4), heap (P5), spinlock (P6), планировщик + 21-словный
  фрейм (P7), ELF reject-матрица + фаззинг (P8), log-монотонность (P9), journal
  replay/atomicity/idempotence/corruption (P10–P13), virtio-blk roundtrip (P14/P16),
  net RX-ring модель (P17), ext2 (P18–P20), shell-инварианты (P21–P27).
- Детерминизм: приватный XorShift64 с фиксированными сидами; near-OOM — skip, не fail.

## Зависимости крейта (кратко)

`limine 0.6`, `spin 0.9`, `x86_64 0.15` (запатчен: `[patch.crates-io]` → `third_party/x86_64`),
`lazy_static (spin_no_std)`, `bitflags`, `uart_16550`, `volatile`,
`good_memory_allocator 0.1` (заменил `linked_list_allocator` — тот деградировал до ~O(n²)
под churn парсера apt-индекса), `virtio-drivers 0.11`, `miniz_oxide =0.8.9`, `ruzstd =0.8.3`,
`xz4rust =0.2.1`, `embedded-tls =0.19.0` (NoVerify — см. SECURITY), `embedded-io(-async)`,
`rand_core`, `acpi 5.0`. Профили: `panic = "abort"`, release: `lto = true, opt-level = "z"`.

Feature-флаги: `default = ["network_packages"]` (apt включён; fail-closed — `--no-default-features`),
`lx_selftest`, `lx_livetest`, `lx_bigindex`, `lx_bigindex_inram`, `insecure_network_demo`.
