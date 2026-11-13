# `src/vfs/` — виртуальная файловая система + ELF-загрузчик

Единый node-трейт `VfsNode`, резолв путей, монтирование, синтетический `/dev`, ramfs для
`/tmp` и загрузчик ELF64 (нативные и Linux-бинари). Инициализация — `vfs::init()` из `boot.rs`,
затем `boot.rs` подключает ext2-корень: `vfs::mount_at_named("/mnt", root, "ext2")`.

Синтетический `/proc` (issue #11) реализован срезами 1–7: контракт — пути, байтовые
форматы файлов, семантика `lstat`/`readdir`/`getdents64`/`readlink`, список отложенного —
зафиксирован в **`docs/procfs.md`**, он же приёмка. Срез 5 добавил
`/proc/<pid>/fdinfo/<n>` (и `self`-алиас) и `/proc/<pid>/limits` (и `self`-алиас),
срез 6 (`2.19.0`) — per-thread view `/proc/<pid>/task/` и `/proc/self/task/`
(каталог треда — тот же VFS-объект, что `/proc/<tid>`), срез 7 (`2.20.0`) — реальные
`dev`/`ino`/`offset`/`pathname` в `/proc/<pid>/maps` для file-backed-строк
(DEVIATION-7 сужена до анонимных регионов); отложены необязательные поля
mountinfo/statfs и не смоделированные Linux-файлы per-thread view (`sched`, `syscall`,
`comm`, ...).

## Файлы

| Файл | Роль |
|---|---|
| `mod.rs` | Ядро VFS: трейт `VfsNode`, `VfsError`, `FsStat`, резолв путей (`lookup_path`), монтирование (`mount_at`/`MountNode`), синтетический `/dev` (`NullDevice`, `SerialDevice`, `DevDirectory`), корень (`RootDirectory`), `init()` |
| `ramfs.rs` | In-memory ФС для `/tmp`; единственная in-memory-ФС с настоящими `create_dir`/`create_file`/`remove` (на диске то же умеет ext2-каталог под `/mnt`) |
| `procfs.rs` | Синтетический `/proc`: узлы `ProcDir`/`ProcFile`, ленивый рендер текста на `size()`/`read()`, источники — PMM/heap/тики/счётчики планировщика/`CompatState`/CPUID. `DirKind::TaskDir(tgid)` листает живые треды группы из `compat::thread_group_pids`, узел `task/<tid>` — это `pid_dir(tid)`. Kernel-only |
| `procfs_format.rs` | Чистое (`core`+`alloc`) форматирование текстов procfs (включая `/proc/version`, `/proc/filesystems`, `/proc/loadavg`, `/proc/sys/kernel/*`, `fdinfo` и `limits`), таблица путей и inode'ов (`pid_task_dir_ino` — срез 6) — включается в host-tests (свойства `procfs_*`) |
| `elf.rs` | Эффектный загрузчик ELF64: `ElfLoader::load` (нативный `ET_EXEC`), `ElfLoader::load_linux` (`ET_EXEC`, static-PIE `ET_DYN` и glibc-dynamic образы — `PT_INTERP` подшивается отдельно через `map_interpreter`), `ElfLoader::map_interpreter`; `ElfProcess` |
| `elf_classify.rs` | Чистый core-only классификатор ELF (`classify_elf`, `ElfKind`, `ElfVerdict`) и выбор bias для static-PIE (`choose_bias`, `PIE_BASE`); включается в host-tests |
| `link_walk.rs` | Чистый (`core`+`alloc`, self-contained) резолвер симлинков (issue #18): компонентный обход, стек `..`, относительные/абсолютные цели, бюджет `SYMLOOP_MAX=40`, `MAX_EXPANDED_BYTES`; host-свойства `link_walk_paths` (включая сравнение с независимым оракулом) |

## Ключевые символы

- `VfsResult<T>`, `VfsError::{NotFound, NotSupported, InvalidArgument, IoError, AlreadyExists}`.
- `FsStat { block_size, blocks_total, blocks_free, inodes_total, inodes_free }` — для `statfs`/`fstatfs`.
- `trait VfsNode: Send + Sync` — обязательные только `name`/`is_directory`; дефолты:
  `read/write/truncate/readdir/create_dir/create_file/remove/create_symlink/link` →
  `Err(NotSupported)`, `lookup` → `Err(NotFound)`, `fs_stat` → `None`, `size`/`fs_ino` → `0`,
  `is_symlink` → `false`, `read_link` → `None`, `nlink` → `1`, `sync` → no-op.
- `init()`, `mount_at(path, node)`, `lookup_path(path)` (одношаговый, **не** следует за ссылкой),
  `lookup_path_walk(path, follow_final)` (обход с переходом по ссылкам → `link_walk::WalkError`)
  и `lookup_path_walk_checked(path, follow_final, may_search)` — тот же обход с предикатом
  поиска (`x`) на каждом каталоге (его использует syscall-слой; ядро-внутренние вызовы
  остаются на непроверяемом варианте).
- `link_walk::{resolve, LinkTree, WalkError, SYMLOOP_MAX, map_guest_target, guest_path_keeps_root}`;
  `LinkTree::may_search` — хук прав поиска, который спрашивает чистый walker перед каждым lookup.
- `VfsNode::is_symlink()` / `read_link() -> Option<String>` — понятие ссылки в трейте
  (дефолты `false`/`None`; переопределён у `/proc/self/exe`, будет переиспользован
  ext2-симлинками из issue #18). `MountNode` форвардит оба метода.
- `VfsNode::{mode, owner, set_mode, set_owner}` — **метаданные**, не политика:
  дефолты `0o755`/`0o644` по `is_directory`, владелец `(0, 0)`, set*-методы →
  `NotSupported`. ext2 читает/пишет `i_mode`, `i_uid`, `i_gid` через журнал;
  `MountNode` форвардит все четыре. Права проверяет ТОЛЬКО syscall-слой
  (`docs/security-model.md`), не VFS — boot/installer/shell ходят мимо него.
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
- Срезы 1–7: `/proc` + `/proc/self` + `self/{exe,cmdline,status,maps,stat,statm,mounts,cgroup,mountinfo,limits}`
  + `{cpuinfo,meminfo,uptime,stat,version,filesystems,loadavg,mounts}` + read-only
  `/proc/sys/kernel/{ostype,osrelease,hostname}` + per-pid
  `{cmdline,stat,status,maps,cgroup,mountinfo,limits,fd/*,fdinfo/*,task/*}`.
  `/proc/<pid>/task/` (и `/proc/self/task/`) — per-thread view: по каталогу на каждый
  живой тред группы (`COMPAT_STATES`-запись с `tgid == pid`), сортировка по tid;
  `task/<tgid>/task/<tid>` резолвится в `pid_dir(tid)`, то есть это ТОТ ЖЕ VFS-объект/
  inode, что `/proc/<tid>` (Linux-идентичность «один тред = один inode»), поэтому
  `stat`/`status`/`cmdline` (и остальные pid-файлы) рендерятся существующими
  рендерерами per-thread. Мёртвый pid, тред чужой группы и неназванный tid — `ENOENT`
  (`docs/procfs.md` §13). Срез 7: строки maps для PT_LOAD-сегментов образа/
  интерпретатора и file-backed `mmap` печатают реальные `08:00`/`st_ino`/offset/pathname
  из VFS (`CompatState::image_ino`/`interp_ino`, `MmapBacking`); `[heap]`/`[stack]`/
  анонимные — честные `00:00 0` (§14).
  `/proc/mounts` читает реестр `vfs::mount_snapshot()`, `/proc/*/fd` — снимок fd-таблицы,
  `/proc/*/fdinfo/<n>` — живые offset/flags дескриптора и `node_ino` его VFS-узла
  (`mnt_id` — тот же отсортированный mount-реестр, что у mountinfo; 0 для объектов без
  VFS-монтирования). `/proc/*/limits` печатает 16 Linux-строк из
  `CompatState::rlimits` (override) или `misc::default_rlimit` (иначе `unlimited`).
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
- `/proc/version` — `pagh <CARGO_PKG_VERSION> (x86_64) #1 SMP\n`: реальная версия
  крейта (`env!("CARGO_PKG_VERSION")`), а не поддельная Linux-строка. `#1 SMP` —
  фиксированный build-тег формы; ядро однопроцессорное и SMP не заявляет.
- `/proc/filesystems` — только реально реализованные драйверы: `nodev\tproc`,
  `nodev\tramfs`, `\text2`. Это статическая таблица (`procfs_format::FILESYSTEMS`),
  а не wildcard; новый драйвер обязан добавить строку в тот же коммит.
- `/proc/loadavg` — `X.XX X.XX X.XX <runnable>/<total> <last_pid>\n`. Средние —
  реальные экспоненциально затухающие 1/5/15-минутные значения в fixed point 16.16
  (`scheduler::loadavg_*`, ленивое обновление, без FPU), а не заглушка `0.00`;
  `runnable/total` — живые счётчики планировщика (ready+running против
  ready+running+parked), `last_pid` — последний выданный pid (`0` до первого).
  Доступ — `scheduler::loadavg_counts()`.
- `/proc/sys/kernel/{ostype,osrelease,hostname}` (+ каталоги `/proc/sys`,
  `/proc/sys/kernel`) — read-only константы: `Pagh`, реальная версия, `pagh`
  (совпадает с `uname(2).nodename`). Записываемого sysctl нет, wildcard нет.
- `/proc/self/exe` — первый узел с `is_symlink() == true`: `readlink` отдаёт путь образа,
  `lstat` — `S_IFLNK|0777` с длиной цели, `open`/`stat` следуют по ссылке (общий резолвер
  `lookup_path_walk`, `SYMLOOP_MAX = 40` → `ELOOP`), а `getdents64` помечает запись `DT_LNK`.
- `MountNode.fs_ino` не форвардится, поэтому `stat("/proc")` отдаёт синтетический
  FNV-inode имени `proc`; inode'ы детей — из фиксированной таблицы (`PROC_INO_BASE`),
  выше ramfs-диапазона и с нулевым старшим битом (он занят `synth_ino`).
- Тесты: `procfs_format.rs` покрыт host-свойствами (`procfs_meminfo`/`procfs_cpuinfo`/
  `procfs_status`/`procfs_cmdline`/`procfs_maps`/`procfs_misc`/`procfs_fdinfo_limits`),
  дерево и тексты — in-QEMU рутинами `procfs::tree, rendered files and ENOENT matrix` и
  `procfs::fdinfo and limits (slice 5)` в `src/test.rs`.

### Симлинки и обход путей (issue #18, контракт `EXT2-LINKS.md` §4/§7)
- Трейт расширен аддитивно: `is_symlink()`, `read_link() -> Option<String>`, `nlink()`,
  `create_symlink(name, target)`, `link(name, target_node)`; `MountNode` форвардит их
  (`nlink` — чтобы `stat("/mnt")` показывал `i_links_count` корня ext2).
  **`lookup_path` семантику не меняет** (одношаговый, без перехода): на нём остаются
  `readlink`, `lstat`, `unlink`, `rmdir`, `rename`, `chmod`-исключения. Всё, что говорит
  «файл, на который указывает путь» (`stat`, `open`, `access`, `chdir`, `statfs`, `execve`),
  идёт через `lookup_path_walk(p, true)`; родительские каталоги в `mkdir`/`unlink`/`rmdir`/
  `rename`/`O_CREAT` — тоже follow.
- **Алгоритм живёт в чистом `link_walk.rs`**, ядро подставляет адаптер над `dyn VfsNode`
  (интернирование узлов в `usize`-хендлы). Один обход: стек каталогов для `..`; промежуточные
  ссылки следуются всегда, конечная — по `follow_final`; переход по ссылке **вставляет
  компоненты цели перед оставшимися** (абсолютная цель сбрасывает стек к корню, относительная
  продолжает от каталога ссылки — не от cwd!); 41-й переход → `ELOOP`, разросшийся путь →
  `ENAMETOOLONG`.
- Отображение цели: абсолютная цель — гостевая (`/x` → `/mnt/x`, кроме `dev/tmp/proc/sys/mnt`),
  поэтому `lib64 → /usr/lib64` и `python3 → /mnt/usr/bin/python3.11` работают; относительная —
  дословно от каталога ссылки. `map_guest_target` **не** схлопывает `..` лексически: стек
  должен его увидеть (`/a/link/..` — это каталог цели, а не каталога ссылки).
- `lstat` (`AT_SYMLINK_NOFOLLOW`): `S_IFLNK|0777`, `st_size` = длина цели, `st_nlink` =
  `i_links_count`, `st_ino` — сам inode ссылки. `stat` следует (висячая цель → `ENOENT`,
  цикл → `ELOOP`); `open` следует, `O_NOFOLLOW` на ссылке → `ELOOP`, `O_CREAT|O_EXCL` при
  существующем имени (даже висячей ссылке) → `EEXIST`; `readlink` отдаёт `min(bufsiz, len)`
  байт без NUL, `bufsiz == 0` → `EINVAL`, не-ссылка → `EINVAL`.
- **Права поиска на обходе (аудит 2.22.2, F-x86v-3):** чистый `link_walk` перед каждым
  `lookup` спрашивает `LinkTree::may_search`; ядро-адаптер `io_sys::walk_as` отвечает
  `Creds::may(node.mode(), node.owner(), Access::Execute)` эффективных id (`access(2)` —
  реальных). Проверяются все пройденные каталоги, **включая компоненты цели ссылки**,
  вставленные обходом; отказ → `EACCES`. `chdir(2)` дополнительно требует `x` на самом
  каталоге; `lstat`/`readlink` сохраняют no-follow-семантику. Host-свойства —
  `link_walk_paths::{traversal_requires_search_permission_on_each_directory,
  search_permission_follows_spliced_link_targets, final_directory_name_needs_only_the_parent_search_bit}`.
- `getdents64`: `d_type = DT_LNK` для ссылок, `d_ino` — реальный номер inode.
- Известные отклонения: цель хранится как `String`, поэтому не-UTF-8 цель чужого образа
  отдаётся lossy; `st_blocks` ссылки — 0 (у slow-ссылки реально 1 блок); `rename`
  по-прежнему копирует файлы и на ссылке-источнике отказывает (`EIO`), а не переносит dirent.

### ELF-загрузчик
- **Валидация до аллокации и откат при ошибке** (security-фикс по утечке PMM): суммарный
  `p_memsz` всех `PT_LOAD` ограничен `ElfLoader::MAX_IMAGE_BYTES = 64 МиБ` — образ с
  «легальным» на вид, но огромным `p_memsz` отвергается до первого `alloc_frame`
  (иначе один неудачный `load` вычерпывал весь PMM: 443 МиБ за вызов, после чего гость не
  мог запустить процесс). Всё, что замаплено, регистрируется в `MappedFrames`: при любом
  раннем возврате кадры возвращаются в PMM, причём **сначала `unmap`, потом `free`**
  (иначе живая PTE указывала бы на освобождённую память). Пользовательский PML4 и
  промежуточные таблицы страниц не освобождаются — у VMM нет teardown-API (то же и на
  выходе процесса); это осознанное ограничение, а не забытое.
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
- Абсолютная цель ссылки **не** очищает оставшиеся компоненты пути: `/a/link/..` обязан
  применить `..` к каталогу цели (`/b/c/..` = `/b`). Ранний вариант резолвера чистил список
  компонентов при абсолютной цели и возвращал `/b/c` — поймано host-свойством против
  независимого оракула (`link_walk_paths`).
- Адаптер `WalkNodes` интернирует узлы по `Arc::ptr_eq`: узел, возвращённый `lookup`,
  должен быть тем же `Arc`, иначе один и тот же inode получит разные id и решение
  «туда ли мы пришли» разъедется.
- `readlink`, `lstat`, `unlink`, `rmdir` **нельзя** переводить на `lookup_path_walk(_, true)`:
  они должны видеть саму ссылку (иначе удаление ссылки в каталог вернёт `EISDIR`).
