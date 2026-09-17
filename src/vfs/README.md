# `src/vfs/` — виртуальная файловая система + ELF-загрузчик

Единый node-трейт `VfsNode`, резолв путей, монтирование, синтетический `/dev`, ramfs для
`/tmp` и загрузчик ELF64 (нативные и Linux-бинари). Инициализация — `vfs::init()` из `boot.rs`,
затем `boot.rs` подключает ext2-корень: `vfs::mount_at("/mnt", root)`.

Синтетический `/proc` (issue #11) реализован: контракт — пути, байтовые форматы файлов,
семантика `lstat`/`readdir`/`getdents64`/`readlink`, список отложенного — зафиксирован в
**`docs/procfs.md`**, он же приёмка. Отложены `/proc/<pid>`, `/proc/stat`, `/proc/loadavg`,
`/proc/mounts`, `/proc/self/fd` (все дают `ENOENT`).

## Файлы

| Файл | Роль |
|---|---|
| `mod.rs` | Ядро VFS: трейт `VfsNode`, `VfsError`, `FsStat`, резолв путей (`lookup_path`), монтирование (`mount_at`/`MountNode`), синтетический `/dev` (`NullDevice`, `SerialDevice`, `DevDirectory`), корень (`RootDirectory`), `init()` |
| `ramfs.rs` | In-memory ФС для `/tmp`; единственная in-memory-ФС с настоящими `create_dir`/`create_file`/`remove` (на диске то же умеет ext2-каталог под `/mnt`) |
| `procfs.rs` | Синтетический `/proc`: узлы `ProcDir`/`ProcFile`, ленивый рендер текста на `size()`/`read()`, источники — PMM/heap/тики/`CompatState`/CPUID. Kernel-only |
| `procfs_format.rs` | Чистое (`core`+`alloc`) форматирование текстов procfs, таблица путей и inode'ов — включается в host-tests (свойства `procfs_*`) |
| `elf.rs` | Эффектный загрузчик ELF64: `ElfLoader::load` (нативный `ET_EXEC`), `ElfLoader::load_linux` (`ET_EXEC`, static-PIE `ET_DYN` и glibc-dynamic образы — `PT_INTERP` подшивается отдельно через `map_interpreter`), `ElfLoader::map_interpreter`; `ElfProcess` |
| `elf_classify.rs` | Чистый core-only классификатор ELF (`classify_elf`, `ElfKind`, `ElfVerdict`) и выбор bias для static-PIE (`choose_bias`, `PIE_BASE`); включается в host-tests |

## Ключевые символы

- `VfsResult<T>`, `VfsError::{NotFound, NotSupported, InvalidArgument, IoError, AlreadyExists}`.
- `FsStat { block_size, blocks_total, blocks_free, inodes_total, inodes_free }` — для `statfs`/`fstatfs`.
- `trait VfsNode: Send + Sync` — обязательные только `name`/`is_directory`; дефолты:
  `read/write/truncate/readdir/create_dir/create_file/remove` → `Err(NotSupported)`,
  `lookup` → `Err(NotFound)`, `fs_stat` → `None`, `size`/`fs_ino` → `0`, `sync` → no-op.
- `init()`, `mount_at(path, node)`, `lookup_path(path)`.
- `VfsNode::is_symlink()` / `read_link() -> Option<String>` — понятие ссылки в трейте
  (дефолты `false`/`None`; переопределён у `/proc/self/exe`, будет переиспользован
  ext2-симлинками из issue #18). `MountNode` форвардит оба метода.
- `elf`: `ElfLoader::{load, load_linux, map_interpreter}`,
  `ElfProcess { entry, pml4_phys, load_bias, phdr_vaddr, phent, phnum, initial_brk }`.
- `elf_classify`: `USER_ADDR_MAX = 0x0000_8000_0000_0000`, `PIE_BASE = 0x1_0000`.

## Как работает

### Абстракция
- Один плоский трейт нод; каталоги — ноды с `readdir`/`lookup`. Никакого dcache;
  fd-таблица живёт в `task::fd`. `lookup_path` режет по `/`, пустые компоненты пропускает.
- `mount_at` подсоединяет поддерево под одно-компонентное имя верхнего уровня (`"/mnt"`),
  оборачивая в `MountNode` (форвард методов; **`fs_ino` не форвардится** — сам корень
  монтирования отдаёт 0, хотя нижележащий каталог имеет настоящий inode). Повторный mount
  того же имени заменяет.
  **Ограничение v1: только одноуровневые монтирования** (`"/a/b"` → `InvalidArgument`).

### Синтетика и ramfs
- `/dev/null` (read=0, write всасывает), `/dev/serial` (COM1; read — поллинг LSR 0x3FD / DATA 0x3F8,
  write через `drivers::serial::write_bytes` — побайтово, без UTF-8-энкода).
- ramfs: `RamDir` = `Spinlock<BTreeMap<String, Arc<dyn VfsNode>>>`, `RamFile` = `Spinlock<Vec<u8>>`;
  запись за EOF заполняет дыры нулями (`try_reserve` заранее — чтобы огромный offset не абортнул
  кучу). `remove` непустого каталога → `NotSupported`. Счётчик inode стартует с `0x0054_0000` —
  выше диапазона, который выдаёт ext2-форматтер на дисках примерно до 84 ГиБ (`st_dev` у всех
  VFS-файлов один, поэтому пары `(st_dev, st_ino)` не должны пересекаться), чтобы glibc ld.so
  дедуплицировал корректно.
- `/tmp` появился из-за nvim: `vim_mktempdir` делает `mkdir("/tmp/nvim.XXXXXX")`.

### procfs (issue #11 — контракт `docs/procfs.md`)
- Первый срез: `/proc` + `/proc/self` + `self/{exe,cmdline,status,maps}` +
  `{cpuinfo,meminfo,uptime}`. `/proc/<pid>` и `/proc/stat` отложены (`/proc/stat` требует
  честного учёта user/sys/idle в тике планировщика).
- Монтируется из `vfs::init()` (не из `boot::init_fs`): `/proc` обязан работать даже когда
  ext2 отказан `format_policy`. Готовые источники к этому моменту — PMM, heap, тик-клок;
  при монтировании не рендерится ничего.
- **Рендер ленивый и живёт один проход чтения**: узел создаётся на `lookup()`/`open()`,
  текст строится при первом `size()`/`read()`, а `read(offset == 0)` (начало нового прохода)
  перерисовывает его — программа, опрашивающая `/proc/uptime`/`/proc/meminfo`, видит
  актуальные значения, а многоблочное чтение одного файла не склеивает два разных рендера.
  `size()` возвращает длину рендера, а не 0: иначе `plan_read`-модель EOF в `sys_read`
  отдала бы мгновенный конец файла (DEVIATION-1). Экземпляр на `lookup` (не синглтон)
  обязателен ещё и потому, что `/proc/self/*` привязан к вызвавшему процессу.
- Спинлок узла никогда не берётся до `COMPAT_STATES`: сначала рендер (kernel-state), потом
  запись готового `Vec<u8>` под спинлоком узла (`AGENTS.md`, инвариант 4).
- `/proc/self` невидим (ENOENT) для задачи без `CompatState` — kernel-side `cat` поэтому
  получает ENOENT, а не выдуманное содержимое.
- Неизвестный `/proc`-путь — всегда `NotFound` → `ENOENT`; никакого catch-all.
- `/proc/self/exe` — первый узел с `is_symlink() == true`: `readlink` отдаёт путь образа,
  `lstat` — `S_IFLNK|0777` с длиной цели, `open`/`stat` следуют по ссылке (бюджет 8 переходов
  до общего резолвера issue #18), а `getdents64` помечает запись `DT_LNK`.
- `MountNode.fs_ino` не форвардится, поэтому `stat("/proc")` отдаёт синтетический
  FNV-inode имени `proc`; inode'ы детей — из фиксированной таблицы (`PROC_INO_BASE`),
  выше ramfs-диапазона и с нулевым старшим битом (он занят `synth_ino`).
- Тесты: `procfs_format.rs` покрыт host-свойствами (`procfs_meminfo`/`procfs_cpuinfo`/
  `procfs_status`/`procfs_cmdline`/`procfs_maps`), дерево и тексты — in-QEMU рутином
  `procfs::tree, rendered files and ENOENT matrix` в `src/test.rs`.

### ELF-загрузчик
- `load` (legacy): валидация хедера и program headers с overflow-safe арифметикой **до**
  аллокаций; создание user PML4 (`vmm::new_user_pml4()`), временный `load_cr3`
  (вызывающий обязан идти с выключенными прерываниями); маппинг `PT_LOAD`
  (`PF_W`→WRITABLE, `!PF_X`→NO_EXECUTE, всегда USER_ACCESSIBLE); копирование filez
  страница-за-страницей через HHDM (кадры не континуальны — один memcpy бы попортил память);
  обнуление BSS; page-округление `brk`.
- `load_linux`: + чистый гейт `classify_elf` (отказ до аллокаций) и поддержка static-PIE:
  `choose_bias(max_load_vaddr_end)` от `PIE_BASE`, biased-маппинг, вычисление `AT_PHDR`.
- `map_interpreter` маппит ET_DYN-образ интерпретатора в существующее адресное пространство
  по caller-заданному bias.

## Зависимости

- **От:** `sync::spinlock`, `alloc`, `drivers::serial`, порт I/O (`x86_64`) для COM1;
  `elf.rs` → `memory::vmm` + `memory::pmm`.
- **На неё:** почти всё ядро — `boot.rs`, `task/process.rs`, `arch/.../io_sys.rs`
  (вся резолвка путей syscall'ов), `shell/*`, `pkg/*`, `provision.rs`, `selftest_lx.rs`, `test.rs`.

## Грабли

- glibc ld.so дедуплицирует загруженные объекты по `(st_dev, st_ino)` — разные файлы не должны
  делить пару; отсюда старт ramfs-ino с `0x0054_0000`.
- `lookup_path` не должен держать `VFS_ROOT`-спинлок во время `lookup` (дедлок).
- Serial-write раньше портил байты ≥ 0x80 через `format_args!` (UTF-8) — фикс: байтовый API.
- `elf_classify.rs` сознательно дублирует `USER_ADDR_MAX` вместо импорта (иначе в host-tests
  протекают kernel-зависимости).
- Нулевой `e_phnum` допустим; валидация phdr идёт до аллокаций — битые бинари не трогают память.
