# Контракт procfs для issue #11 — первый срез

**Статус:** требования (задача `t3`, kind=work). Документ — приёмка: реализация issue #11
обязана соответствовать ему по путям, содержимому и поведению syscall-поверхности.
**Реализовано** в задаче `t4` (ветка `vfs/procfs`); отступления от текста ниже, если они
есть, перечислены в коде и в `src/vfs/README.md`; DEVIATION-2 уточнён: рендер ленивый и
живёт один проход чтения (`read(offset == 0)` перерисовывает), а не снимок на `open()`.
Ссылки на код даны на `HEAD cd542bb` («Merge pull request #38 … fs/journal-live-seq-recovery»).

Источники решения: issue #11; `src/vfs/mod.rs` (трейт `VfsNode`); `src/vfs/ramfs.rs`;
`src/arch/x86_64/linux/io_sys.rs` (резолв путей, `open`/`stat`/`readlink`/`getdents64`);
`src/arch/x86_64/linux/misc.rs` (`sys_sysinfo`, `AT_RANDOM`); `src/task/compat.rs`
(`CompatState`); `src/memory/{pmm,heap}.rs`; `src/boot.rs`; плюс код реальных потребителей
(libuv, glibc, busybox/procps) — ссылки в §4.

---

## 0. Резюме для реализации

1. Добавляется **`/proc`**, смонтированный из `vfs::init()` рядом с `/dev` и `/tmp`, — без
   изменений в `boot.rs` и без единого изменения в `resolve_path`: `/proc` уже исключён из
   chroot-маппинга `/mnt` (см. §1).
2. Первый срез — ровно **7 файлов** + 2 каталога:
   `/proc`, `/proc/self`, `/proc/self/{exe,cmdline,status,maps}`, `/proc/{cpuinfo,meminfo,uptime}`.
   Всё остальное (`/proc/<pid>`, `/proc/stat`, `/proc/loadavg`, `/proc/mounts`,
   `/proc/self/fd`, `/proc/self/stat`, `/proc/sys`, `/proc/version`) — **ENOENT** в этом срезе,
   обоснование отложенности в §2.2.
3. Содержимое генерируется **снимком на `lookup()`/`open()`** в форме Linux-текста; `size()`
   файла возвращает длину снимка (это необходимо из-за `plan_read`-модели EOF — §3.2,
   DEVIATION-1).
4. Трейт `VfsNode` расширяется аддитивными `is_symlink()`/`read_link()` (§6.1, имена — из
   sibling-контракта `EXT2-LINKS.md` по issue #18, чтобы механизм ссылок был один);
5. Требуется precise-контракт для `lstat`/`stat` (`AT_SYMLINK_NOFOLLOW`), `getdents64`
   (`d_ino` = реальный inode, `d_type` = `DT_LNK`), `write` на procfs (`EACCES`) — §5, §6.3.
6. Плюс две мелкие правки-фикса по пути: предикат chroot-маппинга становится
   покомпонентным (§1.3, иначе гость не увидит `/mnt/process`, см. DEVIATION-9) и
   `getdents64.d_ino` перестаёт быть индексом (§6.3).

---

## 1. Как читатель попадает в `/proc` (минуя `/mnt`-маппинг)

### 1.1 Абсолютные пути из Linux-процесса

Единственная точка нормализации — `resolve_path` (`src/arch/x86_64/linux/io_sys.rs:686-733`).
Её хвост (`:716-733`) — это и есть «chroot»-маппинг гостя:

```rust
// io_sys.rs:720-731 (текущее состояние)
if out != "/"
    && !out.starts_with("/mnt")
    && !out.starts_with("/dev")
    && !out.starts_with("/proc")   // ← /proc НЕ переписывается в /mnt/proc
    && !out.starts_with("/sys")
    && !out.starts_with("/tmp")
{ … "/mnt" + out … }
```

`open`/`openat`/`access`/`chdir`/`newfstatat`/`statfs`/`readlink` вызывают `resolve_path`
(`io_sys.rs:774, 834, 1038, 1457, 1777, 2155, 1988`), поэтому абсолютный `/proc/...`
доходит до `vfs::lookup_path("/proc/...")` **без изменений в io_sys**.
Формулировка issue «open() на любой `/proc*` путь возвращает ENOENT из-за гарда
`!out.starts_with("/proc")`» неточна: этот гард не блокирует `/proc`, он снимает маппинг
`/mnt`; ENOENT сегодня — потому что узла `/proc` в дереве нет вовсе.

Требуется: `vfs::init()` (`src/vfs/mod.rs:283-304`) дополнительно делает
`mount_at("/proc", crate::vfs::procfs::root())`; `mount_at` (`:316-340`) кладёт в
`RootDirectory.children` узел-обёртку `MountNode` с `name() == "proc"`, поэтому
`ls /` показывает `dev tmp proc`, а `lookup_path("/proc/...")` делегирует в procfs.
Порядок безопасен: `vfs::init()` вызывается из `boot.rs:200` после `pmm::init` (`:125`),
`vmm::init` (`:134`), `heap::init` (`:140`) и `scheduler::init` (`:193`) — все источники
содержимого уже готовы; при этом узел ничего не вычисляет при монтировании (ленивый снимок,
§3.2), так что «фрагильный» порядок boot-фаз (`AGENTS.md`, инвариант 7) не затрагивается.
Монтирование из `vfs::init()` (а не из `boot::init_fs`) выбрано сознательно: `/proc` должен
работать даже когда ext2 не смонтирован/отказан `format_policy` — это диагностический
инструмент именно для такого случая.

### 1.2 Относительные пути

`resolve_path` склеивает `cwd + "/" + path`. `CompatState::cwd` по умолчанию `/mnt`
(`src/task/compat.rs:132`), поэтому:

| вызов (cwd = `/mnt`, по умолчанию) | результат |
|---|---|
| `open("/proc/cpuinfo")` | `/proc/cpuinfo` → procfs ✔ |
| `chdir("/proc")` затем `open("cpuinfo")` | `/proc/cpuinfo` → procfs ✔ |
| `open("proc/cpuinfo")` (cwd = `/mnt`) | `/mnt/proc/cpuinfo` → **ENOENT** (в ext2 такого файла нет) |

Это **осознанное ограничение первого среза** (DEVIATION-8): procfs виден только по
абсолютным путям либо после `chdir` внутрь `/proc`. Все реальные потребители
(`/proc/self/status`, `/proc/meminfo`, `readlink("/proc/self/exe")`) ходят абсолютными
путями; нормализация «гостевого корня» (когда `/` = VFS-root, а ext2 монтируется в `/`)
запредельна для этого среза.

### 1.3 Покомпонентность предиката (обязательная мелкая правка)

Сейчас исключения — это префиксы **строк**, а не компоненты пути: `"/processing"` тоже
не маппится в `/mnt/processing`. Для procfs это неважно, но предикат становится
контрактным (его проверяет свойство P55), поэтому требуется вынести его в чистую функцию
`src/arch/x86_64/linux/io.rs`:

```rust
/// Первый компонент абсолютного пути входит в набор исключений chroot-маппинга.
pub fn guest_path_keeps_root(abs: &str) -> bool {
    let first = abs.trim_start_matches('/').split('/').next().unwrap_or("");
    matches!(first, "mnt" | "dev" | "proc" | "sys" | "tmp")
}
```

`resolve_path` использует её вместо пяти `starts_with`; поведение для `/proc/...`
не меняется, для `/process/...` — исправляется на «маппится в `/mnt/process`».

### 1.4 Kernel-side читатели

Оболочка (`cat`, `ls`) и selftest'ы идут через `vfs::lookup_path` напрямую
(`src/shell/commands.rs:217`), то есть видят `/proc/...` независимо от `resolve_path`.
Нюанс: kernel-side `cat` не умеет следовать симлинкам, поэтому `cat /proc/self/exe`
напечатает пустоту (DEVIATION-6, необязательная правка — следование по `read_link()`).

---

## 2. Дерево первого среза

### 2.1 Что входит

```
/                     (VFS root; children: dev, tmp, proc, [mnt])
/proc                 каталог, readdir → ["self","cpuinfo","meminfo","uptime"]
/proc/self            каталог, readdir → ["exe","cmdline","status","maps"]
/proc/self/exe        симлинк (readlink → exe_path текущего процесса)
/proc/self/cmdline    обычный файл, снимок argv
/proc/self/status     обычный файл, Linux-поля процесса
/proc/self/maps       обычный файл, карта адресного пространства
/proc/cpuinfo         обычный файл, CPUID
/proc/meminfo         обычный файл, счётчики PMM/heap
/proc/uptime          обычный файл, тики планировщика
```

Обоснование вхождения каждого пути:

| Путь | Кто читает (проверено по коду потребителя) | Почему в срезе |
|---|---|---|
| `/proc/self/exe` | libuv `uv_exepath` (nvim `progpath`; уже есть в io_sys хардкодом), Go `os.Executable` (`readlink("/proc/self/exe")`), Rust `std::env::current_exe` | Переносит существующий хардкод в дерево и делает `ls -l /proc/self`, `lstat` корректными |
| `/proc/self/cmdline` | procps `ps`, busybox, любой `ps`-подобный код | Единственный способ узнать argv процесса; требует нового поля в `CompatState` (§4.4) |
| `/proc/self/status` | htop, procps, Go-runtime (дампы), gdb (`TracerPid`) | Базовая идентичность процесса + `Sig*`-маски, которые уже есть в `CompatState` |
| `/proc/self/maps` | ASan/TSan/UBSan, `addr2line`-символизатор, Go `runtime.dumpregs`, отладчики | Санитайзеры/Go отказываются или деградируют без него (issue #11, Impact) |
| `/proc/cpuinfo` | libuv `uv_cpu_info` (Node `os.cpus()`), Python `platform.processor()`, busybox | Единственный источник «какой это CPU» для userland |
| `/proc/meminfo` | libuv `uv_get_total_memory`/`uv_get_free_memory` (`MemTotal:`, `MemAvailable:`), busybox `free`, htop, psutil | glibc с 2.23 берёт физические страницы из `sysinfo(2)` (см. §4.2), поэтому meminfo нужен именно этим потребителям |
| `/proc/uptime` | libuv `uv_uptime` (`sscanf("%lf")`), busybox `uptime` | Тривиально и переиспользует уже существующий тик-клок |
| `/proc/stat` | libuv `uv_cpu_info` — **открывает первым, без него `os.cpus()` вообще падает** | **Сознательно отложен**: требует реального учёта user/sys/idle (см. §2.2) |

### 2.2 Что осознанно отложено и почему

| Путь | Причина отложенности |
|---|---|
| `/proc/<pid>` (перечисление), `/proc/<pid>/{status,stat,cmdline,maps}` | Нужен pid-keyed каталог, snapshot `COMPAT_STATES` и **чтение чужого процесса** (его `CompatState` + `VmRegionSet`) без удержания реестрового спинлока во время генерации содержимого. Плюс потребители (ps/htop) одновременно требуют `/proc/stat` и `/proc/<pid>/stat` — половина набора бесполезна. Отдельный срез. |
| `/proc/stat` | Нельзя отдавать выдуманное разбиение user/sys/idle. В ядре есть только глобальный `scheduler::ticks()` (`src/task/scheduler.rs:231`, `TICK_HZ = 1000`, `src/arch/x86_64/apic.rs:153`); для честного `/proc/stat` нужен учёт в `scheduler_tick_irq` (ring 3 → user, ring 0 → system, idle-pid → idle). Это самостоятельная правка с собственными тестами. **Первый кандидат в срез 2** — без него Node `os.cpus()` не заработает вовсе (libuv открывает `/proc/stat` первым и возвращает ошибку). |
| `/proc/loadavg` | Нет учёта загрузки (libuv падает обратно на `sysinfo(2)`, который уже реализован и возвращает `loads = [0;3]`). |
| `/proc/mounts`, `/proc/self/mountinfo` | В VFS нет таблицы монтирований (есть только `RootDirectory.children`); сначала нужен реестр монтирований. |
| `/proc/self/fd/*`, `/proc/self/task/*` | Требуют симлинков на произвольный fd/pid и полноценного следования по ссылкам (общая задача с #18), а не одного уровня (§5.4). |
| `/proc/self/stat`, `/proc/self/statm`, `/proc/self/cgroup` | Принадлежат семейству `/proc/<pid>/stat`; `statm` дёшев, но требует residency-учёта, которого нет (§4.5.1). |
| `/proc/sys/*`, `/proc/version`, `/proc/filesystems`, `/proc/devices` | Не нужны заявленным потребителям; `/proc/version` тривиален, но не проверяется ни одним E2E-сценарием — держим срез минимальным. |
| `[vdso]`, `[vvar]`, `[vsyscall]` в maps | В ядре их физически нет — строки не должны появляться (это не «не реализовано», а «отсутствует»). |

### 2.3 Идентичность узлов и inode-номера

- Дерево procfs **строится один раз**; `lookup(name)` возвращает **новый экземпляр**
  файлового узла (нужен для снимка на открытие — §3.2), но `name()`/`fs_ino()` у всех
  экземпляров одного пути **константны**.
- `fs_ino()` — из фиксированной таблицы (bit 63 = 0 — этот бит занят синтетическим
  FNV-хешем `synth_ino`, `io_sys.rs:908-915`; значения выше ramfs-диапазона
  `NEXT_INO = 0x0054_0000`, `src/vfs/ramfs.rs:25`):

```rust
pub const PROC_INO_BASE: u64 = 0x00F0_0000_0000; // 1.03e12, < 2^63
// /proc = base+0, /proc/self = base+1,
// /proc/self/{exe,cmdline,status,maps} = base+2..5,
// /proc/{cpuinfo,meminfo,uptime} = base+6..8
```

- Обёртка `MountNode` `fs_ino` не форвардит (`src/vfs/README.md`, «Грабли»), поэтому
  `stat("/proc")` вернёт `synth_ino("proc")` — стабильно и не конфликтует с inode'ами
  детей. Требований на изменение `MountNode` в этом срезе нет; при желании — отдельная
  правка (форвардить `fs_ino`, одна строка), но она не обязательна.
- `st_dev` у всех VFS-узлов один — `STAT_DEV_VFS = 0x0800` (`io_sys.rs:904`), менять не нужно.

---

## 3. Контракт узлов procfs (`VfsNode`)

### 3.1 Обязательные методы

| Метод | Поведение |
|---|---|
| `name()` | Компонент имени (`"proc"` даёт `MountNode`, дальше — `"self"`, `"cpuinfo"`, …) |
| `is_directory()` | `true` для `/proc` и `/proc/self`; `false` для всех файлов, включая `exe` |
| `readdir()` | Ровно дети из §2.1, стабильный порядок (см. §5.3); у файловых узлов — `NotSupported` (дефолт) |
| `lookup(name)` | Известное имя → узел; неизвестное → `Err(VfsError::NotFound)`; у файла — `NotFound` (дефолт трейта) |
| `read(offset, buf)` | `n = min(buf.len(), len - offset)`, копия байтов снимка; `offset >= len` → `Ok(0)` |
| `size()` | Длина снимка в байтах (см. DEVIATION-1) |
| `fs_ino()` | Константа из §2.3 |
| `write`/`truncate`/`create_dir`/`create_file`/`remove` | `Err(VfsError::NotSupported)` (дефолт трейта — не переопределять) |
| `is_symlink()`/`read_link()` | `true`/`Some(target)` только для `/proc/self/exe` (§6.1, §4.7) |
| `sync()` | no-op (дефолт) |

### 3.2 Снимок содержимого (ключевое проектное решение)

`sys_read` считает EOF из `node.size()`: `plan_read(size, off, count)` (`io.rs:37`) при
`size == 0` возвращает `copied = 0`, и `sys_read` отдаёт `Ok(0)` — то есть файл
с нулевым `size()` читается как пустой (`io_sys.rs:374-376`). Linux для файлов procfs
рапортует `st_size = 0`, и в этом ядре такое поведение сделало бы `cat /proc/meminfo`
пустым. Отсюда контракт:

1. `size()` = **длина сгенерированного текста**, а не 0 (DEVIATION-1).
2. Текст генерируется **один раз на экземпляр узла** и мемоизируется в
   `Spinlock<Option<Vec<u8>>>` самого экземпляра; `size()` и `read()` читают один и тот же
   буфер. Это устраняет рассогласование `size()`/`read()` внутри одного `read(2)`
   (для `/proc/uptime` текст меняется во времени, и без фиксации возможна обрезка числа
   на границе буфера).
3. Экземпляр создаётся в `lookup()` (и, следовательно, в `open_resolved`,
   `io_sys.rs:739-760`), поэтому снимок делается **от имени процесса, который открывает
   файл**. Общий узел-синглтон с мемоизацией запрещён: `/proc/self/cmdline`, открытый
   процессом A, не должен отдаваться процессу B.
4. Порядок блокировок: снимок формируется **до** взятия спинлока узла
   (сначала состояние: `compat::with_current_compat`, `pmm::total_frames`,
   `heap::stats`, `scheduler::ticks`; затем `node.data.lock()` только для записи готового
   `Vec<u8>` в `Option`). Вложенность «спинлок узла → `COMPAT_STATES`» запрещена
   (реестр — не реентрантный спинлок, `src/task/compat.rs:439-455`).
5. Генерация снимка не делает блокирующего I/O и не читает пользовательскую память —
   только kernel-state (§4).
6. Следствие: содержимое фиксируется на момент `open()`, а не на каждый `read()`
   (DEVIATION-2).

---

## 4. Точное содержимое файлов

Все файлы — байтовые, LF (`\n`) в конце каждой строки, без BOM и без NUL внутри текста.
Числа — десятичные, кроме явно указанных hex-полей. Форматы ниже — Linux-совместимые;
там, где байтовая точность load-bearing (парсеры `fscanf`/`sscanf`/`strstr`), это отмечено
явно.

Проверенные источники требований к формату (код потребителей):
libuv [`src/unix/linux.c`](https://raw.githubusercontent.com/libuv/libuv/v1.x/src/unix/linux.c)
(`uv__read_proc_meminfo`, `uv_uptime`, `uv_cpu_info`, `uv_loadavg`),
Linux [`fs/proc/meminfo.c`](https://elixir.bootlin.com/linux/latest/source/fs/proc/meminfo.c),
[`arch/x86/kernel/cpu/proc.c`](https://elixir.bootlin.com/linux/latest/source/arch/x86/kernel/cpu/proc.c),
[`fs/proc/array.c`](https://elixir.bootlin.com/linux/latest/source/fs/proc/array.c),
[`fs/proc/task_mmu.c`](https://elixir.bootlin.com/linux/latest/source/fs/proc/task_mmu.c),
glibc/`get_phys_pages(3)` (с 2.23 — через `sysinfo(2)`, а не `/proc/meminfo`):
[manpages](https://manpages.debian.org/trixie/manpages-dev/get_phys_pages.3.en.html).

### 4.1 `/proc/meminfo`

Формат строки (как в Linux `fs/proc/meminfo.c::show_val_kb`):
`label` дополняется пробелами до 16 символов, число — в поле шириной 8, затем `" kB\n"`.

```rust
write!(out, "{:<16}{:>8} kB\n", label_with_colon, kb)
```

Обязательный набор строк, в этом порядке (числовое поле — шириной 8 по правилу выше):

| Ключ | Значение (kB) |
|---|---|
| `MemTotal:` | `pmm::total_frames() * PAGE_SIZE / 1024` |
| `MemFree:` | `pmm::free_frames() * PAGE_SIZE / 1024` |
| `MemAvailable:` | `= MemFree` |
| `Buffers:`, `Cached:`, `SwapCached:`, `Active:`, `Inactive:`, `SwapTotal:`, `SwapFree:`, `Dirty:`, `Writeback:`, `AnonPages:`, `Mapped:`, `Shmem:`, `Slab:`, `SReclaimable:`, `SUnreclaim:` | `0` |

Пример (для 4 ГиБ, 1 ГиБ свободно):

```
MemTotal:        4194304 kB
MemFree:         1048576 kB
MemAvailable:    1048576 kB
Buffers:               0 kB
Cached:                0 kB
…
```

Требования и обоснование:

- Источники: `pmm::total_frames()`/`pmm::free_frames()` (`src/memory/pmm.rs:552,560`),
  `PAGE_SIZE` = 4096. **Те же источники, что у `sys_sysinfo`** (`src/arch/x86_64/linux/misc.rs:440-441`),
  поэтому обязательна согласованность: `MemTotal * 1024 == sysinfo.totalram` и
  `MemFree * 1024 == sysinfo.freeram` (проверяется selftest'ом, §8.2).
- Юнит — ровно `" kB"` (строчная `k`, заглавная `B`) после десятичного числа: libuv
  парсит файл как `strstr(buf, "MemTotal:")` + `sscanf(p, "%" PRIu64 " kB", &rc)`
  (libuv `src/unix/linux.c::uv__read_proc_meminfo`) и возвращает `rc * 1024`, то есть
  значение в поле — **килобайты**. Любое отклонение (`KB`, `kB `, таб вместо пробелов)
  ломает `uv_get_total_memory`.
- `MemAvailable:` обязан присутствовать и быть непустым: libuv читает его **первым**
  в `uv_get_free_memory` и при нуле падает обратно на `sysinfo(2)`. Значение `= MemFree`
  честно для ядра без страничного кэша и без reclaim (DEVIATION-3).
- `Buffers/Cached/Slab/SReclaimable/SUnreclaim/Shmem/Dirty/Writeback/Active/Inactive = 0`:
  ни страничного кэша, ни slab-аккаунтинга в ядре нет; ноль — правда. `SwapTotal/SwapFree = 0`
  (своп не поддерживается).
- Файл обязан укладываться в 4096 байт (буфер libuv — 4096, `char buf[4096]`).
  Фактический размер ≈ 550 Б.
- `heap::stats()` (`src/memory/heap.rs:229` → `(size, used, free)`) в meminfo **не входит**:
  ядровая куча — не Linux-память процесса; и нестандартные ключи не добавляем.

### 4.2 `/proc/cpuinfo`

Один блок на CPU; в этом ядре CPU один (single-CPU, `AGENTS.md`). Блок заканчивается
пустой строкой (`\n\n` в конце файла). Разделители — **табы**, как в Linux
`arch/x86/kernel/cpu/proc.c`. В блоке ниже `\t` и `\n` — это **байты TAB (0x09) и LF
(0x0A)**, а не литералы из двух символов (в коде Rust их пишут как `"processor\t: "`):

```
processor\t: 0\n
vendor_id\t: {brand-vendor}\n            (только если непустой)
cpu family\t: {family}\n
model\t\t: {model}\n
model name\t: {brand или "Unknown x86_64 CPU"}\n
stepping\t: {stepping}\n
cpu MHz\t\t: {mhz}.{frac:03}\n           (только если CPUID.16h доступен; frac = 000)
cache size\t: {cache_kb} KB\n            (только если CPUID.80000006h доступен)
physical id\t: 0\n
siblings\t: 1\n
core id\t\t: 0\n
cpu cores\t: 1\n
apicid\t\t: 0\n
initial apicid\t: 0\n
fpu\t\t: yes\n
fpu_exception\t: yes\n
cpuid level\t: {max_basic_leaf}\n
wp\t\t: yes\n
flags\t\t: {флаги через пробел}\n
bugs\t\t:\n
bogomips\t: {mhz * 2}.00\n              (только если mhz известно)
clflush size\t: 64\n
cache_alignment\t: 64\n
address sizes\t: {phys} bits physical, {virt} bits virtual\n
power management:\n
\n
```

Требования:

- **Байтовая точность разделителей load-bearing**: libuv делает
  `fscanf(fp, "processor\t: %u\n", &cpu)` и ищет литерал `"model name\t: "`
  (libuv `src/unix/linux.c::uv_cpu_info`). Значит `processor` и `model name`
  обязаны писаться ровно как `\t: ` (таб, двоеточие, пробел), а строка `processor`
  завершаться `\n`. Заголовок `flags\t\t: ` (два таба) — как в Linux.
- Значения из CPUID (`core::arch::x86_64::__cpuid`, тот же приём, что в
  `src/security/entropy.rs:14-23`; в коде уже используется):
  - `vendor_id` ← leaf 0 EBX/EDX/ECX (ASCII, NUL-терминированный);
  - `cpu family`/`model`/`stepping` ← leaf 1 EAX (+ extended family/model);
  - `model name` ← leaves `0x80000002..0x80000004` (48 байт бренда; обрезать NUL,
    свернуть внутренние пробелы, обрезать по краям); если бренд пуст →
    литерал `Unknown x86_64 CPU` (не выдумывать модель);
  - `cpu MHz` ← leaf `0x16` EAX (базовая частота, МГц); при отсутствии листа —
    **строку не печатать** (в QEMU `-cpu max` лист 0x16 может отсутствовать, поэтому
    E2E не должен требовать `cpu MHz`);
  - `cache size` ← leaf `0x80000006` ECX[31:16] → ` KB`; при отсутствии листа — не печатать;
  - `address sizes` ← leaf `0x80000008` EAX[7:0]/[15:8];
  - `cpuid level` ← EAX листа 0 (max basic leaf). **Все обращения к листам > max basic
    (и > max ext для 0x8000_000x) обязаны быть под гардом** — сейчас
    `entropy::capabilities()` (`:19`) читает leaf 7 без гарда; для cpuinfo это
    контрактное требование (иначе на старом CPU получаем мусор в отчёте).
  - `flags` — **только по битам CPUID**, из таблицы `(имя, слово, бит)`; таблица
    обязана включать: `fpu tsc msr pae cx8 apic sep mtrr pge mca cmov pat pse36 clflush
    mmx fxsr sse sse2 ssse3 sse4_1 sse4_2 popcnt cx16 aes xsave avx avx2 lm nx pdpe1gb
    rdtscp rdrand rdseed hypervisor` (биты 1/7/0x80000001 по SDM). Ложь в `flags`
    недопустима: userland и другие подсистемы ветвятся по этим именам (например,
    `rdrand`/`rdseed` — issue #16; `aes`/`sse4_2` — крипто). Обоснование выбора:
    эти имена — то, что читают диагностика и скрипты; glibc CPU-features берёт из CPUID,
    а не из cpuinfo (уточнение к impact в issue).
- Блоков ровно столько, сколько CPU (1): никаких фальшивых `processor\t: 1` для «больше
  параллелизма» — иначе `sched_getaffinity` и cpuinfo начнут противоречить друг другу.

### 4.3 `/proc/uptime`

```
{secs}.{centis:02} {idle_secs}.{idle_centis:02}\n
```

- `secs/centis` ← `scheduler::ticks()` и `TICK_HZ` (`apic.rs:153`; 1000 Гц):
  `secs = ticks / TICK_HZ`, `centis = (ticks % TICK_HZ) * 100 / TICK_HZ`.
- Второе поле (idle) = `0.00` при отсутствии учёта простоя (DEVIATION-4). libuv
  `uv_uptime` читает только первое поле (`sscanf(buf, "%lf", uptime)`), `uptime(1)`,
  htop и `busybox uptime` — тоже; обманчивых чисел не печатаем.
- Пример: `1234.56 0.00\n`. Ровно два знака после точки в обоих полях.
- Тот же источник, что у `sys_sysinfo.uptime` (`misc.rs:439`) — согласованность
  обязательна и проверяется selftest'ом.

### 4.4 `/proc/self/cmdline`

- Байты: `argv[0] \0 argv[1] \0 … argv[n-1] \0` — конкатенация аргументов, каждый
  завершён NUL; ничего больше (ни `\n`, ни пробелов). Пустой `argv` → пустой файл (0 байт).
- Байтовая точность обязательна: argv может быть не-UTF-8 (`&[&[u8]]` уже на входе
  `run_linux_binary`/`exec_linux_image`), поэтому хранить и отдавать надо `Vec<u8>`,
  без `String`-конверсий.
- Новое поле в `CompatState` (`src/task/compat.rs:29-96`):

```rust
/// `/proc/self/cmdline`: argv, склеенный NUL-ами и завершённый NUL
/// (Linux-формат). Наследуется fork/clone (derive(Clone)), переписывается exec.
pub cmdline: Vec<u8>,
```

- Лимит: `arg_gate` уже ограничивает argv 256 аргументами и 4096 суммарными байтами
  (`src/task/stack.rs:244-259`; вызывается первым шагом `run_linux_binary`, `process.rs:476-478`).
  Снимок строится с потолком **4096 байт полезной нагрузки** (сумма байтов аргументов — та же
  величина, что ограничивает `arg_gate`; NUL-терминаторы в потолок не входят, поэтому даже
  максимальный argv гейта представим целиком, а файл занимает ≤ 4096 + argc байт). Аргументы
  добавляются целиком: как только очередной не влезает — он и все последующие отбрасываются,
  после чего пишется финальный NUL. Никаких полу-обрезанных аргументов.
- Заполняется в `run_linux_binary` (шаг 5, `process.rs:519-531`) и в `exec_linux_image`
  (`process.rs:614-645`), из тех же `argv`, что уходят в `map_initial_stack`.
- Поток (`clone` с `CLONE_THREAD`) и форк наследуют поле как есть — это соответствует
  `CompatState::clone()`/`fork_current_compat`.
- `Name:` в `status` (§4.5) берётся из `exe_path`, а не из cmdline (Linux использует
  basename файла образа, а не argv[0]).
- **Auxv — не источник.** Проверено: энкодер начального стека (`src/task/stack.rs:49-62`,
  `AuxInputs`) кладёт только `AT_PHDR/AT_PHENT/AT_PHNUM/AT_ENTRY/AT_PAGESZ/AT_BASE/AT_RANDOM`
  + `AT_NULL`; `AT_EXECFN` в auxv нет вообще, а если бы и был — это указатель в
  пользовательской памяти, недоступный kernel-side чтению из VFS-узла без чужого CR3.
  Поэтому argv и путь образа хранятся в `CompatState` (kernel-owned копия), а не
  вычитываются из стека. (`AT_RANDOM` — зона issue #16 и на procfs не влияет; в
  `/proc/cpuinfo` виден только его аппаратный базис — флаги `rdrand`/`rdseed` из CPUID.)

### 4.5 `/proc/self/status`

Табы после двоеточия (`"Name:\t"`), как в Linux `fs/proc/array.c`. Обязательные строки:

| Строка | Точный формат | Источник |
|---|---|---|
| `Name` | `Name:\t{name}\n`, ≤ 15 байт | basename(`cs.exe_path`), иначе basename(argv[0]); иначе `pagh`; обрезка ровно 15 байт (Linux `TASK_COMM_LEN-1`) |
| `Umask` | `Umask:\t{:04o}\n` | `cs.umask` (022) |
| `State` | `State:\tR (running)\n` | процесс читает сам себя ⇒ всегда running |
| `Tgid` | `Tgid:\t{tgid}\n` | `cs.tgid` |
| `Pid` | `Pid:\t{pid}\n` | `scheduler::current_pid()` |
| `PPid` | `PPid:\t{ppid}\n` | `cs.ppid` |
| `TracerPid` | `TracerPid:\t0\n` | трассировки нет |
| `Uid`,`Gid` | `Uid:\t0\t0\t0\t0\n` (и `Gid` так же) | всё выполняется как root (модель ядра) |
| `FDSize` | `FDSize:\t{slots}\n` | `FdTable` → `FdSlots::len()` (`src/task/fd_alloc.rs:118`); нужен однострочный аксессор `FdTable::capacity()` |
| `Threads` | `Threads:\t{n}\n` | число pid в `COMPAT_STATES` с `tgid == cs.tgid` (через существующий `group_member_pids`, `compat.rs:355`) |
| `SigPnd` | `SigPnd:\t{:016x}\n` | `cs.sig_pending` |
| `ShdPnd` | `ShdPnd:\t0000000000000000\n` | групповых pending нет |
| `SigBlk` | `SigBlk:\t{:016x}\n` | `cs.sig_blocked` |
| `SigIgn` | `SigIgn:\t{:016x}\n` | бит i = disposition == `SIG_IGN` |
| `SigCgt` | `SigCgt:\t{:016x}\n` | бит i = handler != `SIG_DFL` |
| `Cap*` | `CapInh/CapPrm/CapEff/CapBnd/CapAmb:\t0000000000000000\n` | модель capability в ядре отсутствует — нули, а не выдуманный root-набор |
| `NoNewPrivs`,`Seccomp` | `…:\t0\n` | не поддерживаются |
| `Cpus_allowed_list`/`Mems_allowed_list` | `…:\t0\n` | одна CPU, одна memory-нода |
| `VmPeak`,`VmSize` | `VmPeak:\t{vm_size:>8} kB\n` | VmSize = образ + heap + mmap'ы + стек (§4.5.1); VmPeak = VmSize (high-water не отслеживается, DEVIATION-5) |
| `VmLck`,`VmPin`,`VmSwap`,`VmPTE` | `…:\t       0 kB\n` | не поддерживаются |
| `VmHWM`,`VmRSS` | `VmHWM:\t{vm_rss:>8} kB\n` | VmRSS = сумма **eagerly-backed** регионов: сегменты образа, стек, file-backed mmap'ы (anon-mmap и heap — demand-paged, residency не отслеживается; DEVIATION-5) |
| `VmData` | `VmData:\t{heap_kb:>8} kB\n` | span heap'а |
| `VmStk`,`VmExe`,`VmLib` | `…:\t{…:>8} kB\n` | стек (2048 страниц), сегменты образа, сегменты интерпретатора |
| `voluntary_ctxt_switches`,`nonvoluntary_ctxt_switches` | `…:\t0\n` | счётчиков нет |

Правила:

- Набор полей — **надмножество** того, что читают htop/procps (`Pid`, `PPid`, `Name`,
  `State`, `Threads`, `VmRSS`, `Uid`, `Gid`, `SigIgn`, `SigCgt`, `SigBlk`) и дампы
  Go/отладчики (`TracerPid`, `VmSize`, `VmRSS`). Строки, которых нет в таблице, печатать
  нельзя; строки из таблицы обязаны присутствовать ровно один раз.
- `Name` — не UTF-8-валидируется, обрезка по байтам (Linux делает то же).
- Если у вызывающего нет compat-состояния — см. §5.5 (весь `/proc/self` невидим).

#### 4.5.1 Формулы Vm*

- `image_kb` = сумма страничных span'ов PT_LOAD-сегментов образа (exe) + интерпретатора;
- `heap_kb` = `page_up(cs.vm.current_brk) - page_down(cs.vm.initial_brk)`;
- `stack_kb` = `USER_STACK_PAGES * PAGE_SIZE / 1024` = 2048 * 4 = 8192 КиБ
  (`memory/layout.rs:73,85`; стек маппится жадно в `process.rs:137-153`);
- `mmap_kb` = Σ `pages * PAGE_SIZE / 1024` по `cs.vm.mmaps` (`mem.rs:36-54`);
- `VmSize` = `image_kb + heap_kb + stack_kb + mmap_kb`;
- `VmRSS` = `image_kb + stack_kb + file-backed mmap_kb` (anon-mmap/heap не считаются —
  они подкачиваются по fault'у, а постраничного residency-учёта нет).

### 4.6 `/proc/self/maps`

Строка (Linux `fs/proc/task_mmu.c::show_map_vma`), байтовая точность полей:

```
{start:08x}-{end:08x} {r}{w}{x}{p} {offset:08x} {maj:02x}:{min:02x} {ino} {path}\n
```

- `start`/`end` — строчная hex без `0x`, минимум 8 цифр (`{:08x}`);
  `start < end`, страницы 4 КиБ;
- `perms` — четыре символа: `r`/`-`, `w`/`-`, `x`/`-`, затем `p` (все маппинги private;
  `s` не эмитится);
- `offset` — 8 hex-цифр (для анонимных 0);
- `maj:min` — `00:00` для анонимных и для file-backed, у которых (dev, ino) не захвачены
  (DEVIATION-7); `ino` — десятичный;
- `path` — абсолютный гостевой путь образа/интерпретатора, либо метка в квадратных
  скобках (`[heap]`, `[stack]`), либо отсутствует (анонимные). Перед `path` ровно один
  пробел; лишнее выравнивание не нужно (парсеры — `sscanf` по whitespace).
  Для анонимной строки **пробела в конце нет**.

Состав и порядок:

1. PT_LOAD-сегменты образа (в порядке возрастания адреса): span = page-округление
   `[p_vaddr, p_vaddr + p_memsz)`, `perms` из `p_flags`, `offset` = `p_offset`
   (выровненный вниз по странице), `path` = `exe_path`;
2. PT_LOAD-сегменты интерпретатора (если `PT_INTERP` был): `path` = путь интерпретатора;
3. `[heap]`: `[page_down(initial_brk), page_up(current_brk))`, `rw-p`, печатать только
   если span непуст;
4. каждый `cs.vm.mmaps` (`mem.rs:36-54`): `perms` из `prot` (`PROT_READ/WRITE/EXEC`),
   `p`; без пути; file-backed — тоже без пути (путь не хранится в `VmRegionSet`; §2.2);
5. `[stack]`: `[USER_STACK_TOP - USER_STACK_PAGES*4096, USER_STACK_TOP)`, `rw-p`.
   Linux печатает только «использованную» часть — мы печатаем весь замапленный регион
   (DEVIATION-7);
6. Результат **отсортирован по `start`**; пересекающиеся входы сливаются в один регион
   (объединение прав, приоритет — первое имя), чтобы вывод всегда был парсабельным.
   `[vdso]`/`[vvar]`/`[vsyscall]` не печатаются (их нет).

Для пунктов 1–2 нужны данные, которых в ядре сейчас нет: `ElfProcess`
(`src/vfs/elf.rs:73-90`) не хранит сегменты. Требуется:

```rust
/// Один загруженный PT_LOAD-сегмент (для /proc/self/maps).
#[derive(Clone, Copy)]
pub struct LoadSegment { pub start: u64, pub end: u64, pub prot: u32, pub file_offset: u64 }

// ElfProcess { …, pub segments: Vec<LoadSegment> }  — заполняется в load_linux;
// интерпретатор добавляет свои сегменты (map_interpreter уже знает их).
// CompatState { …, pub image_segments: Vec<LoadSegment>, pub interp_path: String }
```

Захват (dev, ino) для строк образа — необязателен (иначе `00:00 0`).

### 4.7 `/proc/self/exe`

- Узел не является каталогом; `is_symlink()` → `true`, `read_link()` → `Some(cs.exe_path.clone())`.
- Целевой путь — тот, из которого загружен образ (сейчас `run_linux_binary` пишет
  переданный путь `process.rs:530`, `exec_linux_image` — результат `resolve_exec_path`,
  `process.rs:640`). В обоих случаях путь **самодостаточен**: `resolve_path(exe_path)`
  находит образ, поэтому `nvim`/python могут его ре-exec'нуть. Канонизация к «гостевому»
  виду (`/mnt/usr/bin/nvim` → `/usr/bin/nvim`) в этот срез не входит (DEVIATION-8,
  косметика для `ps`-подобных инструментов).
- Если `cs.exe_path` пуст — узел не существует для вызывающего (§5.5).

---

## 5. Поведение syscall-поверхности

### 5.1 Сводная таблица

| Операция | Результат |
|---|---|
| `open("/proc")`, `open("/proc/self")` | каталог-fd (`OpenObject::Dir`), `getdents64` отдаёт детей, `read(2)` → `EISDIR` (`io_sys.rs:320`) |
| `open("/proc/meminfo")` и прочие файлы | файл-fd, `read(2)` отдаёт снимок, EOF по `size()` |
| `open("/proc/self/exe")` | **следование по ссылке** (один уровень) → fd настоящего файла образа |
| `open("/proc/<неизвестное>")` | `ENOENT` (маппинг `VfsError::NotFound` → `ENOENT` в `open_path`, `io_sys.rs:790`) |
| `open("/proc/cpuinfo/child")` | `ENOENT` (DEVIATION-10: Linux даёт `ENOTDIR`; дефолтный `lookup` трейта возвращает `NotFound`) |
| `write(2)` на fd procfs-файла | `EACCES` (требуется правка маппинга, §6.3) |
| `stat`/`lstat` (`newfstatat`) | см. §5.2 |
| `readlink("/proc/self/exe")` | путь образа без NUL, обрезанный по `bufsiz`; `ENOENT`, если у вызывающего нет exe |
| `readlink("/proc/self")` | `EINVAL` (DEVIATION-8; Linux вернул бы `/proc/<pid>`) |
| `readlink(любой другой существующий узел)` | `EINVAL` (сохраняется текущее поведение `io_sys.rs:1989-1991`) |
| `readlink(несуществующее)` | `ENOENT` |
| `access("/proc/<известное>")` | `0` (мод не проверяется — текущее поведение `io_sys.rs:1455-1460`) |
| `chdir("/proc")` | `0`; `chdir("/proc/meminfo")` → `ENOTDIR` (`io_sys.rs:1779`) |
| `statfs("/proc")` | успех с дефолтным `statfs` (`fs_stat()` → `None`; тип ФС не репортится, оставляем как есть) |
| `getdents64` | см. §5.3 |
| `mkdir`/`unlink`/`rename`/`create` внутри `/proc` | `NotSupported` → текущие errno слоя (`ENOENT`/`EIO`/`EACCES`), поведение не ухудшается |

### 5.2 `stat` / `lstat` (`newfstatat`)

Сейчас `sys_newfstatat` игнорирует флаги (`io_sys.rs:1035`, `_flags`). Требуется:

- `flags & AT_SYMLINK_NOFOLLOW (0x100) != 0` и `node.is_symlink()`:
  `st_mode = S_IFLNK | 0o777`, `st_size = target.len()`, `st_nlink = 1`,
  `st_dev/st_ino` — как обычно (`STAT_DEV_VFS`, `node_ino(node)`);
- флаг не задан и узел — ссылка: **следование** тем же резолвером, что и у `open`
  (§5.4; `ENOENT` при висячей цели, `ELOOP` при цикле — из t15);
- остальные узлы: без изменений (`write_stat`, `io_sys.rs:968-984`).
- Точные значения полей для ссылок (`st_nlink` из `i_links_count`, `st_blocks` из
  `i_blocks` и т.п.) — зона t15 (`EXT2-LINKS.md` §4.3); для `/proc/self/exe`
  действует строка таблицы ниже: `st_nlink = 1`, `st_blocks = 0`.

Итоговые значения для procfs:

| Путь | `st_mode` | `st_size` | `st_nlink` |
|---|---|---|---|
| `/proc` | `S_IFDIR \| 0700` | 0 | 1 |
| `/proc/self` | `S_IFDIR \| 0700` | 0 | 1 |
| `/proc/self/exe` (`lstat`) | `S_IFLNK \| 0777` | длина `exe_path` | 1 |
| `/proc/self/exe` (`stat`) | по цели | по цели | по цели |
| `/proc/{cpuinfo,meminfo,uptime}`, `/proc/self/{cmdline,status,maps}` | `S_IFREG \| 0755` | длина снимка | 1 |

DEVIATION-1 (size ≠ 0), DEVIATION-11 (права: `stat_mode_for` даёт дефолты 0700/0755,
тогда как Linux для procfs — 0555/0444; режим в этом ядре не влияет на `access`,
поскольку `sys_access` его игнорирует; введение `VfsNode::perms()` в срез не входит).

### 5.3 `readdir` / `getdents64`

- `readdir()` каталогов procfs возвращает ровно детей из §2.1 в фиксированном порядке:
  `/proc` → `["self","cpuinfo","meminfo","uptime"]`,
  `/proc/self` → `["exe","cmdline","status","maps"]`.
  Порядок стабилен между вызовами при неизменном дереве (Linux порядок не гарантирует;
  нашим потребителям он безразличен, а тесты на нём стоят).
- `getdents64` (`io_sys.rs:1706-1750`) отдаёт записи `linux_dirent64`
  (`dirent.rs::encode_dirent64`), уже снапшотнутые при `open()`.
  **Обязательная правка**: сейчас `d_ino = index + 1` (`io_sys.rs:1727`) — это не inode,
  а позиция, из-за чего `readdir` и `stat` противоречат друг другу (`ls -i /proc`
  показывает не то, что `stat`). Требуется `d_ino = node_ino(child)`, `d_off = index + 1`.
  Риска нет: ни один потребитель в проекте не полагается на индексные значения, а
  glibc-`readdir` отдаёт `d_ino` в `struct dirent`, который проверяют `find`/`ls -i`.
- `d_type`: `DT_DIR (4)` для каталогов, `DT_REG (8)` для обычных файлов и
  **`DT_LNK (10)`** для `/proc/self/exe` → нужно добавить `DT_LNK` в `dirent.rs` и
  маппинг в хендлере (`dir → DT_DIR; is_symlink() → DT_LNK; иначе DT_REG`).
- Записи `.` и `..` не эмитятся ни в одном каталоге VFS (и сейчас) — не добавляем
  (DEVIATION-12; `ls`/`find` работают без них).

### 5.4 `open` и следование по ссылкам

`open_path` (`io_sys.rs:773-816`) сейчас не различает ссылки. Нужна follow-семантика, и её
владелец — поток t15 из контракта по issue #18 (`EXT2-LINKS.md` §4.1/§4.6): общий резолвер
с бюджетом `SYMLOOP_MAX = 40` переходов, `Errno::ELOOP`, `lookup_path_no_follow` для
`lstat`/`readlink` и таблица «что следует за конечным компонентом, а что нет».

Контракт procfs для этого среза:

1. **Если t15 уже лендирован** — `open`/`openat` (без `O_NOFOLLOW`), `stat` без
   `AT_SYMLINK_NOFOLLOW`, `access`, `chdir` используют общий follow-резолвер t15;
   `readlink`/`lstat` — резолвер без следования. `/proc/self/exe` — это просто узел
   с `is_symlink() == true` и `read_link() == Some(exe_path)`, и он получает всю эту
   семантику бесплатно (у t15 это записано как ожидаемый случай: «синтетический узел с
   `is_symlink()`/`read_link()`»). Отдельного хардкода `/proc/self/exe` в `sys_readlink`
   не остаётся.
2. **Если t15 ещё не лендирован, а procfs садится первым** — минимальный собственный шов
   (его потом заменяет общий резолвер): один уровень следования в `open_path` по
   `read_link()`, `lookup_path` для остального, исчерпание бюджета (8) → `ENOENT` +
   `crate::warn!` вместо `ELOOP`. Тогда DEVIATION-13 действует только до мержа t15.
3. `O_CREAT|O_EXCL` при существующем имени → `EEXIST` (в т.ч. если это висячая ссылка —
   как в `EXT2-LINKS.md` §4.5); `O_CREAT`/`O_TRUNC` по ссылке действуют на цель;
   `readlink(2)` резолвит **без** следования (иначе нельзя прочитать саму ссылку).

### 5.5 Отсутствие процесса

`/proc/self` существует только для задачи, у которой есть `CompatState`
(`compat::current_has_compat()`, `compat.rs:434`). Правило: **если compat-состояния нет,
`lookup("self")` → `NotFound` (→ `ENOENT` во всех путях)**, и то же для всех детей
`/proc/self/*`. Для compat-процесса `exe` дополнительно требует непустого `exe_path`
(иначе `NotFound`, что сохраняет сегодняшний `ENOENT` в `sys_readlink`).

Следствия, которые обязаны быть в тестах:

- kernel-side читатель (shell `cat /proc/self/status`, selftest на boot-thread) получает
  `ENOENT` — это ожидаемое поведение, а не баг;
- `ls /proc` у такого читателя всё равно покажет `self` (запись есть в каталоге), но
  `ls /proc/self` → `ENOENT`. В Linux такой ситуации не бывает (там syscall'ы делает
  только процесс), поэтому сравнения с Linux тут нет: мы сознательно не выдумываем
  фальшивый процесс для задачи без `CompatState`.

### 5.6 Неизвестные `/proc`-пути

Инвариант: **никакого wildcard/catch-all**. Любой путь под `/proc`, не перечисленный в
§2.1, даёт `VfsError::NotFound` → `ENOENT` — и никогда `EISDIR`, `EIO` или «успех с пустым
файлом». Отдельный нюанс: `open("/proc/cpuinfo/child")` — тоже `ENOENT` (DEVIATION-10),
а не Linux-овский `ENOTDIR` (дефолтный `lookup` трейта возвращает `NotFound`).
Обязательно покрыть тестами: `/proc/1`, `/proc/1234`, `/proc/self/fd`, `/proc/self/status/x`,
`/proc/sys/kernel/ostype`, `/proc/version`, `/proc/nonexistent`.

---

## 6. Изменения в общих местах (по файлам)

### 6.1 `src/vfs/mod.rs`
- `pub mod procfs;` (+ `pub mod procfs_format;` — §7);
- `vfs::init()` монтирует `/proc` (§1.1);
- **методы ссылок в трейте `VfsNode`** — аддитивные, с дефолтами (ни один существующий
  узел не меняется). Имена берутся из sibling-контракта по issue #18 (`EXT2-LINKS.md`,
  §1.2/§4.2), чтобы не завести два имени для одного механизма:

```rust
/// Узел — символическая ссылка? (дефолт `false`: в этом VFS ссылок нет; procfs
/// переопределяет для `/proc/self/exe`, ext2 — по issue #18).
fn is_symlink(&self) -> bool { false }
/// Цель ссылки как путь. Значимо только при `is_symlink() == true`.
fn read_link(&self) -> Option<String> { None }
```

- `MountNode` форвардит **каждый** метод трейта явно (`src/vfs/mod.rs:202-242`) — оба
  новых метода обязаны быть провардены, иначе `/mnt/<ссылка>`-корень и любой будущий
  procfs-подобный mount потеряют тип ссылки;
- резолверы путей: `lookup_path` (без следования, семантика не меняется) и общий
  follow-резолвер из t15 (§5.4).

### 6.2 `src/vfs/procfs.rs` (новый)
- Узлы `ProcDir`/`ProcFile`/`ProcSymlink`, `root()`; источники: `pmm`, `heap`,
  `scheduler::ticks`, `apic::TICK_HZ`, `compat::with_current_compat`,
  `arch::x86_64::cpuid`, `memory::layout` (стек), `task::process` (образ).
- Никакого блокирующего I/O; снимок — §3.2.

### 6.3 `src/arch/x86_64/linux/io_sys.rs`
- `sys_readlink` (`:1975-1993`): удалить хардкод `/proc/self/exe`, резолвить путь через
  `lookup_path` и брать `node.is_symlink()`/`node.read_link()`; пустая цель/нет узла → `ENOENT`;
  не-ссылка → `EINVAL`;
- `sys_newfstatat` (`:1035-1041`): обработать `AT_SYMLINK_NOFOLLOW` (§5.2);
- `open_path` (`:773-816`): следование по ссылке через общий follow-резолвер t15 (§5.4),
  либо (если t15 ещё не лендирован) — минимальный одноуровневый шов §5.4(2);
- `sys_getdents64` (`:1726-1729`): `d_ino = node_ino(child)`, `d_type` → `DT_LNK` для ссылок;
- `sys_write` (File-ветка, `:456-459`): маппинг `VfsError::NotSupported` → `EACCES`
  (сейчас всё превращается в `EINVAL`); `writev`/`pwrite64` (`:549`, `:2071`) — та же
  правка для консистентности;
- `resolve_path` (`:716-733`): использовать `guest_path_keeps_root` (§1.3).

### 6.4 `src/arch/x86_64/linux/{io.rs,dirent.rs}`
- `io.rs`: `guest_path_keeps_root` (+ свойство P55);
- `dirent.rs`: `DT_LNK = 10`, помощник `d_type_for(is_dir, is_link)` (+ расширение P32).

### 6.5 `src/vfs/elf.rs`, `src/task/{process.rs,compat.rs,fd.rs}`
- `LoadSegment` + `ElfProcess::segments` (образ и интерпретатор) — §4.6;
- `CompatState::{cmdline, image_segments, interp_path}`; заполнение в
  `run_linux_binary`/`exec_linux_image`; `fork_current_compat`/`clone_current_compat`
  наследуют (derive(Clone) уже есть);
- `FdTable::capacity()` (одна строка, `FdSlots::len()`).

### 6.6 `src/arch/x86_64/cpuid.rs` (новый, kernel-only)
- `pub struct CpuIdReport { vendor, brand, family, model, stepping, max_basic, max_ext,
  cache_kb: Option<u32>, mhz: Option<u32>, phys_bits, virt_bits, w1edx, w1ecx,
  w7ebx, w7ecx, w81edx, w81ecx }` + `pub fn read() -> CpuIdReport`;
- каждый `__cpuid` — под гардом max-leaf и с `// SAFETY:`-комментарием (правило
  `AGENTS.md` §10 распространяется на «все остальные» файлы добровольно, но
  `tools/check_safety.py` проверяет 6 критических — здесь комментарии всё равно
  обязательны по стилю);
- модуль **не** включается в host-tests (архитектурный asm); вся логика вывода — в
  `procfs_format` (§7).

### 6.7 Документация
- `src/vfs/README.md` — раздел procfs (+ ссылка на этот файл);
- `README.md` (список ограничений, ~:459) и `LINUX-USERLAND.md` (~:82) — переписать
  «нет procfs» на фактическое состояние и перечислить отложенное (§2.2);
- `docs/procfs.md` — этот файл, обновлять при изменении дизайна.

---

## 7. Host-tests: что выносится и какие свойства нужны

### 7.1 Раскладка

| Файл | Зависимости | В host-tests? |
|---|---|---|
| `src/vfs/procfs_format.rs` | только `core` + `alloc` (форматирование текста, таблица путей/inode'ов, чистые формулы) | **да**, `#[path]`-include в `host-tests/src/lib.rs` |
| `src/vfs/procfs.rs` | `pmm`/`heap`/`scheduler`/`compat`/`cpuid`/`vmm` | нет (kernel-only) |
| `src/arch/x86_64/cpuid.rs` | `core::arch::__cpuid` | нет (asm/архитектура) |
| `src/arch/x86_64/linux/io.rs` | чистый | уже включён: добавить `guest_path_keeps_root` |
| `src/arch/x86_64/linux/dirent.rs` | чистый | уже включён: `DT_LNK`, `d_type_for` |

Публичный чистый API (входы — только скаляры/срезы, чтобы генерация proptest'ом была
тривиальной):

```rust
pub const PROC_INO_BASE: u64;
pub enum ProcKind { SelfExe, SelfCmdline, SelfStatus, SelfMaps, CpuInfo, MemInfo, Uptime }
pub struct ProcEntry { pub name: &'static str, pub ino: u64, pub kind: ProcKind }
pub const PROC_ENTRIES: [ProcEntry; 4];        // self, cpuinfo, meminfo, uptime
pub const PROC_SELF_ENTRIES: [ProcEntry; 4];   // exe, cmdline, status, maps
pub fn proc_entry(name: &str) -> Option<ProcEntry>;

pub struct CpuIdReport { /* слово-в-слово как в 6.6, но без Option-API ядра */ }
pub fn format_cpuinfo(cpus: &[CpuIdReport]) -> Vec<u8>;

pub struct MemInfoKb { pub total_kb: u64, pub free_kb: u64 }
pub fn format_meminfo(m: &MemInfoKb) -> Vec<u8>;
pub fn format_uptime(ticks: u64, tick_hz: u64) -> Vec<u8>;

pub struct StatusInputs<'a> { pub name: &'a [u8], pub umask: u32, pub pid: u64, pub tgid: u64,
    pub ppid: u64, pub threads: u64, pub fd_size: u64, pub sig_pending: u64,
    pub sig_blocked: u64, pub sig_ign: u64, pub sig_cgt: u64, pub vm: VmKb }
pub struct VmKb { pub size: u64, pub rss: u64, pub data: u64, pub stk: u64, pub exe: u64, pub lib: u64 }
pub fn format_status(s: &StatusInputs) -> Vec<u8>;
pub fn task_name(exe_path: &str, argv0: &[u8]) -> Vec<u8>;   // ≤15 байт
pub fn format_cmdline(argv: &[&[u8]], cap: usize) -> Vec<u8>;
pub fn sig_masks(handlers: &[SignalAction]) -> (u64 /*ign*/, u64 /*cgt*/);

pub struct MapsRegion { pub start: u64, pub end: u64, pub prot: u32, pub file_offset: u64,
    pub dev: (u8, u8), pub ino: u64, pub path: Option<&'static str> }
pub fn format_maps(regions: &[MapsRegion]) -> Vec<u8>;       // сортирует и сливает
```

### 7.2 Новые свойства (нумерация продолжает `p50`)

| Файл | Свойство | Что проверяет |
|---|---|---|
| `properties/p51.rs` | meminfo | каждая строка матчится `^[A-Za-z_0-9]+:[ ]+[0-9]+ kB$`; присутствуют ровно обязательные ключи в контрактном порядке, каждый один раз; `MemTotal ≥ MemFree ≥ 0`; `MemAvailable == MemFree`; `SwapTotal == SwapFree == 0`; в выводе есть подстроки `"MemTotal:"`, `"MemAvailable:"`, `"MemFree:"`, `"Buffers:"`, `"Cached:"` (контракт libuv `strstr`); `len ≤ 4096` |
| `properties/p52.rs` | cpuinfo | для случайного набора CPUID-слов: ровно один блок на CPU, блок кончается `\n\n`; первая строка каждого блока ровно `processor\t: {i}\n`; есть литерал `model name\t: `; `flags` — **точное равенство** «таблица ∩ биты» (для всех нулевых слов список имён пуст — `flags\t\t: \n`, ни одного выдуманного имени); `model name` не пуст; `cpu MHz`/`cache size`/`bogomips` присутствуют ⇔ соответствуют `Some`; `bugs\t\t:` присутствует; ключи не дублируются |
| `properties/p53.rs` | uptime + status | `uptime`: regex `^\d+\.\d{2} \d+\.\d{2}\n$`, `secs*100+centis == ticks*100/hz`, монотонность по `ticks`, `idle == 0`; `status`: каждая обязательная строка ровно один раз, `Pid`/`Tgid`/`PPid` равны входу, `SigPnd`/`SigBlk`/`SigIgn`/`SigCgt` — ровно 16 строчных hex, `Name` = `task_name` и ≤ 15 байт, `VmSize ≥ VmRSS`, нет NUL, каждая строка кончается `\n` |
| `properties/p54.rs` | cmdline | `format_cmdline(a)` == конкатенация `a[i] + \0`; расщепление по NUL (без последнего пустого) байт-в-байт возвращает `a`; пустой argv → пустой вывод; при лимите отбрасываются только целые аргументы, вывод кончается NUL, `len ≤ cap`; аргументы с невалидным UTF-8 и с внутренними `\n` проходят без изменений |
| `properties/p55.rs` | maps + таблица + путём | maps: результат парсится regex'ом `^[0-9a-f]{8,16}-[0-9a-f]{8,16} [r-][w-][x-]p [0-9a-f]{8} [0-9a-f]{2}:[0-9a-f]{2} [0-9]+( \S.*)?$`, отсортирован, `start<end`, не пересекается, ни одного `0x`, пустой путь ⇒ нет хвостового пробела; таблица: `proc_entry` для всех имён из контракта → `Some`, для `"1"`, `"self/fd"`, `"cpuinfo/x"`, `""`, `"version"` → `None`; все inode'ы из таблицы различны, bit 63 = 0, `> 0x540000`; `guest_path_keeps_root`: `true` ⇔ первый компонент ∈ {mnt,dev,proc,sys,tmp}, и `/proc/...` **всегда** true |
| `properties/p32.rs` (расширить) | dirent | `DT_LNK == 10`, `d_type_for(dir=true)=DT_DIR`, `d_type_for(link=true)=DT_LNK`, иначе `DT_REG`; `encode_dirent64` сохраняет `d_ino`/`d_type`/имя (уже есть — дополнить новыми кейсами) |

Итого 5 новых property-файлов + правка одного; регистрация в `host-tests/src/lib.rs`
(`#[path]`-include `procfs_format` + `mod p51..p55` в `properties`). `procfs_format`
ссылается на `super::signal_frame` (для `sig_masks`) — ровно как `io.rs` на
`super::errno`; в host-tests `signal_frame` уже объявлен на crate root (`P42`), так что
дополнительной обвязки не нужно, но `#[path]`-include должен стоять как crate-root
sibling, а не внутри вложенного модуля.

### 7.3 Что host-tests принципиально не покрывают

`VfsNode`/`Arc<dyn Trait>` в host-крейте не компилируется (`fd_alloc.rs` включён именно
поэтому — см. комментарий `host-tests/src/lib.rs:158-164`). Поэтому поведение дерева
(`lookup`/`readdir`/ENOENT/снимок), `newfstatat`-флаги, `getdents64.d_ino` и следование по
ссылке проверяются только в-QEMU (§8.2) и E2E (§8.3).

---

## 8. Верификация (критерии приёмки реализации)

### 8.1 Обязательные гейты репозитория (все зелёные, `AGENTS.md`)
```
cargo build
python3 tools/build.py build --release
cargo fmt --all -- --check
python3 tools/check_safety.py
python3 tools/host_tests.py          # + 5 новых property-наборов
```

### 8.2 In-QEMU selftest (`src/test.rs`)
Новый недеструктивный рутин (`all_tests()` в `src/test.rs:3158`, никаких мутаций
`COMPAT_STATES`/PMM/IF/VFS и никаких spawn'ов):

- `lookup_path("/proc")` → каталог; имена `readdir()` == множество
  `{self, cpuinfo, meminfo, uptime}`;
- `/proc/cpuinfo`: непусто, первая строка `processor\t: 0`, есть `vendor_id\t: ` и
  `model name\t: `, файл кончается `\n\n`;
- `/proc/meminfo`: каждая строка парсится, `MemTotal ≥ MemFree > 0`,
  `MemTotal * 1024 == pmm::total_frames() * 4096`,
  `MemFree * 1024 == pmm::free_frames() * 4096`;
- `/proc/uptime`: формат `\d+\.\d{2} \d+\.\d{2}\n`; значение в пределах
  `[ticks/TICK_HZ - 1, ticks/TICK_HZ + 1]`;
- `read(offset=0, 8 KiB)` на `/proc/meminfo` возвращает полный снимок, повторный `read`
  с `offset == size` → `Ok(0)`, `read` с `offset > size` → `Ok(0)`;
- `lookup_path("/proc/1")`, `"/proc/self/fd"`, `"/proc/version"`,
  `"/proc/cpuinfo/child"` → `Err(VfsError::NotFound)`;
- `lookup_path("/proc/self")` → `Err(NotFound)` **в контексте boot-thread** (нет compat
  состояния — §5.5), и это утверждение фиксирует контракт.
- Вердикт рутина виден как `RUN`/`ok` + `SELFTEST SUMMARY: N routines, 0 failed checks`
  (`src/test.rs:3364-3389`), то есть проверяется штатным
  `python3 tools/e2e.py selftest` (`tools/e2e.py:872-914`, режим добавлен t1).

### 8.3 E2E из гостя (`tools/e2e.py shell`)
`--expect` — это regex, ищется по всему логу (`re.search`), поэтому без `^`/`$`-анкоров:
- `python3 tools/e2e.py shell --cmd 'cat /proc/meminfo' --expect 'MemTotal:' --expect 'MemAvailable:'`
- `--cmd 'cat /proc/uptime' --expect '[0-9]+\.[0-9]{2} [0-9]+\.[0-9]{2}'`
- `--cmd 'cat /proc/cpuinfo' --expect 'model name'`
- `--cmd 'ls /proc' --expect 'cpuinfo'` (+ `self`)
- `/proc/self/*` проверяется **только из Linux-процесса** (shell не имеет compat-состояния,
  §5.5): установленный `busybox-static` из локального зеркала (`/mnt/bin/busybox`,
  `src/selftest_lx.rs:359-386`):
  `--cmd 'lxrun /mnt/bin/busybox cat /proc/self/cmdline' --expect 'busybox'`,
  `… /proc/self/status' --expect 'Pid:'`, `… /proc/self/maps' --expect '[stack]'`,
  `--cmd 'lxrun /mnt/bin/busybox ls -l /proc/self' --expect 'exe'`.

### 8.4 Обязательные обновления документации
`src/vfs/README.md`, `README.md`, `LINUX-USERLAND.md` — см. §6.7 (по `AGENTS.md`
устаревшая документация = баг). Версия: срез — новая user-visible возможность ⇒
**MINOR bump** (`Cargo.toml` + `Cargo.lock` в том же коммите, `2.4.2 → 2.5.0`;
`AGENTS.md` в разделе Versioning всё ещё называет «Current 2.3.0» — это устаревшая строка,
источник истины — `Cargo.toml`).

---

## 9. Осознанные отклонения от Linux (все — задокументировать в коде)

| # | Отклонение | Причина / допустимость |
|---|---|---|
| 1 | `st_size` файлов procfs = длина содержимого, а не 0 | Иначе `sys_read` (`plan_read`) отдаёт мгновенный EOF; все потребители читают до `Ok(0)` |
| 2 | Содержимое фиксируется на `open()`, а не генерируется на каждый `read()` | Гарантирует согласованность `size()`/`read()` и привязку к процессу-открывателю; долго живущий fd `/proc/uptime` вернёт устаревшее значение |
| 3 | `MemAvailable = MemFree`, `Buffers/Cached/…= 0` | Кэша и reclaim в ядре нет — это правда, а не заглушка |
| 4 | `/proc/uptime` второе поле = `0.00` | Учёта idle нет; потребители читают первое поле |
| 5 | `VmPeak == VmSize`, `VmHWM == VmRSS`, RSS без residency-учёта | Постраничного учёта нет; врут оба варианта, но этот — минимально |
| 6 | `cat /proc/self/exe` из kernel-shell печатает пусто | Kernel-side `cat` не следует ссылкам; syscall-путь (`open`) следует (§5.4) |
| 7 | `[stack]` — весь регион; `maj:min/ino` образа могут быть `00:00 0`; file-backed mmap без пути | Не хранится residency/путь в `VmRegionSet`; диапазон и права при этом верны |
| 8 | Относительный `proc/...` из cwd `/mnt` не виден; `readlink("/proc/self")` → `EINVAL`; `exe_path` с префиксом `/mnt` | Следствие chroot-маппинга и отсутствия общего следования по ссылкам; абсолютные пути покрывают всех потребителей |
| 9 | Покомпонентный предикат исключений (`/process` теперь маппится в `/mnt/process`) | Исправление латентного дефекта префиксного сравнения |
| 10 | `open("/proc/cpuinfo/x")` → `ENOENT`, а не `ENOTDIR` | Дефолтный `lookup` трейта; `ENOTDIR` потребует отдельной проверки в `lookup_path` |
| 11 | Права procfs = дефолты слоя (0700/0755), а не 0555/0444 | `stat_mode_for` не имеет per-node режима; `access(2)` режим не проверяет |
| 12 | Нет записей `.`/`..` в `getdents64` | Общее свойство VFS этого ядра, не специфика procfs |
| 13 | До мержа t15: цикл/лишние ссылки → `ENOENT` (нет `ELOOP` в `errno.rs`), вместо Linux-овского `ELOOP` | Бюджет 8 + `warn!` как временный шов; t15 (`EXT2-LINKS.md` §4.6) добавляет `Errno::ELOOP = 40` и `SYMLOOP_MAX = 40`, после чего procfs переиспользует общий резолвер |
| 14 | `/proc/<pid>` (в т.ч. `/proc/1`) → `ENOENT` | Перечисление отложено (§2.2); `ps`/`htop` в этом срезе по-прежнему не работают |

---

## 10. Совмещение с issue #18 (ext2 symlink/hardlink) и открытые вопросы

1. **Общий трейт.** `VfsNode::is_symlink()`/`read_link()` (§6.1) — тот же примитив, который
   нужен #18 для симлинков ext2; имена взяты из `EXT2-LINKS.md` (§1.2), чтобы не появилось
   двух названий одного механизма. Кто первым мержит — тот добавляет методы и их форвард
   в `MountNode`; вторая задача использует их без изменений. Follow-семантика, бюджет
   переходов и `ELOOP` — зона t15 (`EXT2-LINKS.md` §4.1/§4.6); procfs их только
   переиспользует (§5.4).
2. **`readlink`-тип возврата.** `Option<String>` (не `Vec<u8>`) выбран потому, что все
   пути в этом ядре уже `String` (`read_user_cstr`, `resolve_path`); ext2-тайргеты
   читаются из inode-байтов, но и там путь нормализуется в `String`.
3. **Порядок мержа.** Если #18 сядет первым и тронет `sys_readlink`/`newfstatat`/
   `open_path` иначе, чем §5.2/5.4, приоритет у этого контракта для `/proc`-путей;
   расхождение — предмет ревью, а не «кто первый».
4. **Открытый вопрос (не блокирует срез):** захват `(dev, ino)` для строк maps.
   Решение по умолчанию — `00:00 0`; если реализация может дёшево получить узел образа,
   лучше печатать `08:00 {ino}`.
5. **Открытый вопрос:** `cpu MHz` при отсутствии CPUID.16h. Решение — не печатать строку
   (не выдумывать частоту). Если позже появится калибровка по TSC/LAPIC — печатать
   измеренное значение и добавить его в E2E.

## 11. Definition of Done для реализации

1. Все 9 узлов §2.1 существуют и ведут себя по §3–§5; неизвестные пути → `ENOENT` (§5.6).
2. `VfsNode::{is_symlink, read_link}` (+ форвард в `MountNode`), follow-резолвер (t15 или
   минимальный §5.4(2)), `FdTable::capacity`, `LoadSegment`,
   `CompatState::{cmdline,image_segments,interp_path}`, `cpuid::read`,
   `procfs_format::*` — на месте.
3. 5 новых property-наборов (p51–p55) + расширение p32 зелёные в
   `python3 tools/host_tests.py`; все 4 гейта `AGENTS.md` зелёные.
4. Новый selftest-рутин (§8.2) даёт 0 failed checks в `python3 tools/e2e.py selftest`;
   E2E-команды §8.3 проходят (включая `/proc/self/*` через busybox).
5. Обновлены `src/vfs/README.md`, `README.md`, `LINUX-USERLAND.md`; MINOR-бамп версии
   вместе с `Cargo.lock`.
6. Каждое отклонение §9 имеет комментарий в коде рядом с местом отклонения.
