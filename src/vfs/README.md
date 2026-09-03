# `src/vfs/` — виртуальная файловая система + ELF-загрузчик

Единый node-трейт `VfsNode`, резолв путей, монтирование, синтетический `/dev`, ramfs для
`/tmp` и загрузчик ELF64 (нативные и Linux-бинари). Инициализация — `vfs::init()` из `boot.rs`,
затем `boot.rs` подключает ext2-корень: `vfs::mount_at("/mnt", root)`.

## Файлы

| Файл | Роль |
|---|---|
| `mod.rs` | Ядро VFS: трейт `VfsNode`, `VfsError`, `FsStat`, резолв путей (`lookup_path`), монтирование (`mount_at`/`MountNode`), синтетический `/dev` (`NullDevice`, `SerialDevice`, `DevDirectory`), корень (`RootDirectory`), `init()` |
| `ramfs.rs` | In-memory ФС для `/tmp`; единственное дерево с настоящими `create_dir`/`create_file`/`remove` |
| `elf.rs` | Эффектный загрузчик ELF64: `ElfLoader::load` (нативный `ET_EXEC`), `ElfLoader::load_linux` (static + static-PIE), `ElfLoader::map_interpreter`; `ElfProcess` |
| `elf_classify.rs` | Чистый core-only классификатор ELF (`classify_elf`, `ElfKind`, `ElfVerdict`) и выбор bias для static-PIE (`choose_bias`, `PIE_BASE`); включается в host-tests |

## Ключевые символы

- `VfsResult<T>`, `VfsError::{NotFound, NotSupported, InvalidArgument, IoError, AlreadyExists}`.
- `FsStat { block_size, blocks_total, blocks_free, inodes_total, inodes_free }` — для `statfs`/`fstatfs`.
- `trait VfsNode: Send + Sync` — методы с дефолтом `Err(NotSupported)`:
  `name, is_directory, fs_stat, read, write, truncate, readdir, lookup, size, fs_ino,
  create_dir, create_file, remove, sync`.
- `init()`, `mount_at(path, node)`, `lookup_path(path)`.
- `elf`: `ElfLoader::{load, load_linux, map_interpreter}`,
  `ElfProcess { entry, pml4_phys, load_bias, phdr_vaddr, phent, phnum, initial_brk }`.
- `elf_classify`: `USER_ADDR_MAX = 0x0000_8000_0000_0000`, `PIE_BASE = 0x1_0000`.

## Как работает

### Абстракция
- Один плоский трейт нод; каталоги — ноды с `readdir`/`lookup`. Никакого dcache;
  fd-таблица живёт в `task::fd`. `lookup_path` режет по `/`, пустые компоненты пропускает.
- `mount_at` подсоединяет поддерево под одно-компонентное имя верхнего уровня (`"/mnt"`),
  оборачивая в `MountNode` (форвард всех методов). Повторный mount того же имени заменяет.
  **Ограничение v1: только одноуровневые монтирования** (`"/a/b"` → `InvalidArgument`).

### Синтетика и ramfs
- `/dev/null` (read=0, write всасывает), `/dev/serial` (COM1; read — поллинг LSR 0x3FD / DATA 0x3F8,
  write через `drivers::serial::write_bytes` — побайтово, без UTF-8-энкода).
- ramfs: `RamDir` = `Spinlock<BTreeMap<String, Arc<dyn VfsNode>>>`, `RamFile` = `Spinlock<Vec<u8>>`;
  запись за EOF заполняет дыры нулями (`try_reserve` заранее — чтобы огромный offset не абортнул
  кучу). `remove` непустого каталога → `NotSupported`. Счётчик inode стартует с `0x0054_0000`
  (далеко за диапазоном ext2), чтобы glibc ld.so дедуплицировал корректно.
- `/tmp` появился из-за nvim: `vim_mktempdir` делает `mkdir("/tmp/nvim.XXXXXX")`.

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
