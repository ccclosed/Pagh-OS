# `src/fs/` — ext2 + WAL-журнал

Совместимая с ext2 (Linux-маунтабельная) файловая система на реальном диске с собственным
write-ahead-log журналом для crash-consistency. Чистая логика над трейтом `BlockDevice`
(`drivers::mod`) — ядро-тесты гоняют её через RAM-мок.

## Файлы

| Файл | Роль |
|---|---|
| `mod.rs` | Корень; enum `FsError` |
| `format_policy.rs` | Политика «можно ли форматировать это устройство»: чистый `core`-модуль **без I/O** — `Probe` (инкрементальный классификатор boot-области: ext2-magic / MBR+GPT / blank / foreign) и `format_allowed(layout, has_valid_superblock, allow_destructive)`; host-тесты гоняют его напрямую (P50, issue #33) |
| `journal.rs` | WAL-журнал: `Journal`, `Txn`, `JournalArea` — кольцевой лог, атомарные мультиблочные транзакции, CRC-коммиты, crash-consistent `recover()` |
| `ext2/mod.rs` | Ядро драйвера: `Ext2Fs` (format/mount), `Tx` (dirty-набор, аллокаторы, block-map), файловые операции, VfsNode-адаптеры `Ext2Dir`/`Ext2File`/`Ext2Symlink`/`Ext2Opaque`, маппинг ошибок `fs_to_vfs` |
| `ext2/structs.rs` | On-disk `#[repr(C)]`-структуры с compile-time фиксацией размеров (`Ext2SuperBlock` 0x88 B, `Ext2GroupDesc` 32 B, `Ext2Inode` 128 B, `JournalSuper` 56 B и др.), магии (`S_IFREG/S_IFDIR/S_IFLNK`), unaligned read/write, CRC32 |
| `ext2/alloc.rs` | Битмап-примитивы: `alloc_bit`, `set/clear/test_bit`, `count_set_bits`; `alloc_block`/`free_block`/`alloc_inode`/`free_inode` с синком счётчиков sb/GD |
| `ext2/dir.rs` | Движок каталогов: `iter_entries` (строгая валидация тайлинга `rec_len`), `find`, `init_dot_entries`, `insert_into_block` (расщепление slack донора), `remove_from_block` (слияние `rec_len` с предыдущей), `DirEntry`, `min_rec_len` |
| `ext2/inode.rs` | Чтение block map: `block_for_offset` — 12 direct + single/double/triple indirect (`PTRS_PER_BLOCK=1024`); дыры → `None` |
| `ext2/symlink.rs` | Чистый (`core`-only, host-тестируемый) расчёт формы симлинка и учёта ссылок: `plan` (fast ⟺ цель+NUL ≤ 60 байт, иначе slow), `is_fast`, `fast_target`, `inline_words`/`inline_bytes`, `unlink_action`, `link_bump`; host-свойства — `host-tests/src/properties/ext2_links.rs` (issue #18) |

## Ключевые символы

- `FsError::{BadSuperBlock, BadJournal, OutOfSpace, NotFound, AlreadyExists, IoError, NameTooLong, Corrupt, FileTooBig}`.
- `Ext2Fs`: `format(dev)`, `mount(dev) -> Arc<dyn VfsNode>`, `mount_fs(dev) -> Arc<Ext2Fs>`,
  `has_valid_superblock(dev)`, `read_file/write_file/truncate_file/create/unlink`, `sync()` (no-op),
  `read_fs_block/read_inode/lookup_entry/read_dir_entries`,
  `create_symlink(parent, name, target) -> ino`, `read_symlink(ino) -> Vec<u8>`,
  `link(parent, name, target_ino)` (issue #18).
- `Journal`: `format`, `open`, `begin() -> Txn`, `log_block`, `commit`, `recover() -> u32`, `next_seq()`.

## Константы

| Константа | Значение |
|---|---|
| `BS` | 4096 (размер блока), `SECTORS_PER_BLOCK=8` |
| `EXT2_MAGIC` | `0xEF53`, `EXT2_ROOT_INO=2`, `EXT2_FIRST_INO=11` |
| `FMT_LOG_BLOCKS` / `JOURNAL_RESERVE_BLOCKS` | 64 / 65 (журнал сразу после ext2-региона) |
| `MAX_GROUP_BLOCKS/INODES` | 32768 (группа 128 MiB, u16-счётчики) |
| `BYTES_PER_INODE` | 16 KiB (floor `MIN_INODES=32`) |
| `TX_DATA_BLOCKS` | 64 (чанк большой записи на транзакцию) |
| `JDESC_MAX_TARGETS` | 254 |
| Магии журнала | `JNL_MAGIC = "PAGHJNL\1"` — **свой формат, не jbd2** |

## Как работает

### On-disk
- Суперблок на байте 1024, 4 KiB-блоки, `rev_level=1`, все feature-флаги сняты — Linux
  маунтит как обычный ext2. GD-таблица на блоке 1 + реплика в начале каждой группы.
- Журнал живёт после ext2-региона: журнал-суперблок + 64 кольцевых блока лога.
  Таргеты транзакций обязаны быть `< fs_blocks`.

### Block groups
- Формат многогрупповой: всё устройство минус 65 блоков журнала, группы по 32768 блоков
  (регион дополнительно ограничен `u32::MAX` блоков, слишком маленькая хвостовая группа
  отбрасывается).
- `Tx::alloc_zeroed_block`/`alloc_new_inode` сканируют ВСЕ группы (раньше только группу 0 —
  каждая ФС упиралась в 128 MiB).
- `reconcile_free_counts` на монтировании пересчитывает свободные счётчики по битмапам
  и чинит sb/GD, если разъехались.

### Чтение / запись
- **Read**: чанками по `BS`; `block_for_offset` резолвит блок (дыры читаются нулями); кламп в `i_size`.
- **Write**: отказ при записи за `u32::MAX` (`FileTooBig`). Большие записи режутся на транзакции
  по 64 data-блока. `map_or_alloc` выделяет direct/indirect блоки; при полной перезаписи блока
  disk-read пропускается (RMW резал throughput вдвое).
- **Ordered-mode journaling**: file data пишется на финальные места ДО метадата-транзакции
  (коммиченные метаданные никогда не указывают на несуществующие данные); по WAL едут
  только метаданные (битмапы, inode, indirect, sb, GD).
- **Truncate**: рост — zero-fill через write-путь чанками 64 KiB; усадка — одна journal-транзакция.
- **Create/unlink**: `create` — inode + dir-блок с `.`/`..` для каталогов; `unlink` требует
  пустой каталог. Сканы lookup'ов read-only — `map_or_alloc` на дырах не зовётся
  (аллокация «просто для поиска» текла на ранних ошибках).

### Симлинки и хардлинки (issue #18, контракт `EXT2-LINKS.md`)
- **Форма симлинка** выбирается чистой `ext2/symlink.rs::plan`: цель ≤ 59 байт (то есть цель
  вместе с терминирующим NUL укладывается в 60 байт `i_block`) — **fast**, `i_blocks = 0`,
  блока данных нет; цель ≥ 60 байт — **slow**, один блок данных (`i_blocks = BS/512`), цель с
  offset 0, хвост блока нули (NUL после цели получается сам). Граница проверена на e2fsprogs
  1.47.3: 59 — fast, 60 — slow; ошибка на единицу теряет последний символ цели на Linux-маунте.
- **Чтение**: `read_symlink(ino)` отдаёт ровно `i_size` байт цели. Fast ⟺ `i_blocks == 0 &&
  i_size ≤ 60` (у нас xattr выключен, `i_file_acl == 0`, поэтому linux-тест
  `i_blocks - ea_blocks == 0` сводится к `i_blocks == 0`). Нет первого блока у slow-ссылки или
  inode не симлинк → `Corrupt` (обрезанная цель молча увела бы вызывающего на другой путь).
- **Цель хранится дословно** — это сырые байты (не обязательно UTF-8), никакой нормализации.
- **Хардлинк**: `link(parent, name, target_ino)` — тот же inode, `i_links_count += 1`, один
  dirent, ни одного нового блока (проверяется host-свойством и in-guest тестом). Цель-каталог
  отвергается (`AlreadyExists`): у каталога `i_links_count` считает `.`/`..`, а не имена.
- **Удаление по учёту ссылок**: для не-каталога `unlink` смотрит `i_links_count` — при `> 1`
  только уменьшает счётчик (блоки и inode не трогаются: их читают оставшиеся имена), при `== 1`
  освобождает блоки+inode и обнуляет счётчик. Каталог освобождается **целиком всегда** (его
  счётчик — про `.`/`..`, хардлинков на каталоги не бывает), иначе `rmdir` тёк бы inode и блок.
  Всё — в одной транзакции с удалением dirent.
- **Транзакции**: `create_symlink` — один `Tx`; slow-блок берётся через `Tx::data_block`, то есть
  пишется на финальное место до коммита метаданных (ordered mode): коммитнутая ссылка не может
  указывать на незаписанный блок. `link` — один `Tx` (inode цели + dirent). Операция трогает
  ≤ 7 метаблоков при лимите WAL в 61.
- **Тип узла**: `node_for` диспетчеризует явно — `Ext2Dir` / `Ext2Symlink` / `Ext2File` /
  `Ext2Opaque` (чужой inode: device/fifo). Симлинк **никогда** не отдаётся как `Ext2File`:
  `read` на fast-ссылке истолковал бы текст цели как номера блоков и вернул бы произвольный
  блок диска. `write_file`/`truncate_file` отказывают любому не-`S_IFREG` inode.
- `link_target` и guest-видимые `readlink`/`lstat` — задача t15; VfsNode-метод `readlink()`
  добавляет срез procfs (`docs/procfs.md` §6.1), ext2 переопределяет его через
  `Ext2Fs::read_symlink`.

### Журнал
- On-disk транзакция = `[Descriptor][Data]*N[Commit]`; Descriptor держит до 254 таргетов,
  Commit — magic + seq + CRC32 по всем data-блокам. Коммит-запись — точка **атомарности**: по ней
  `recover()` отличает закоммиченную транзакцию от оборванной. **Дюрабильность**: `commit` шлёт
  `BlockDevice::flush()` (NVMe FLUSH / VIRTIO_BLK_T_FLUSH) дважды — после коммит-записи (журнал
  доехал до носителя → транзакция реплеится после краха) и после checkpoint'а, до сдвига `head`
  (иначе сдвиг мог бы объявить лог пустым, пока данные ещё в кэше устройства — issue #15).
  `recover()` флашит реплейнутые образы перед записью опустошённого лога. На RAM-моке и на
  устройствах без кэша `flush` — no-op по умолчанию в трейте.
  **Best-effort:** если устройство не объявляет `VIRTIO_BLK_F_FLUSH`, крейт завершает запрос как
  no-op (`vendor/virtio-drivers/src/device/blk.rs:143`), и барьер на нём ничего не гарантирует —
  ядро этого не проверяет и не логирует. NVMe FLUSH (opcode 0x08) шлётся безусловно, там барьер
  честный. P24 проверяет, что барьеры действительно дают crash-consistency при волатильном кэше.
- `commit`: проверка лимитов, reclaim всего лога при нехватке места (чекпойнтинг синхронный),
  валидация таргетов. Reclaim персистит новый `tail` **до** записи транзакции: завёрнутая запись
  ложится ровно на слот, который называл старый `tail`, и без этого крах после коммит-записи
  оставлял бы on-disk `tail`, не указывающий на дескриптор, — `recover` остановился бы на нём и
  потерял закоммиченную транзакцию (то же окно #35, но через кольцо).
- `Journal::format` пишет пустой суперблок **и обнуляет первый блок лога**: кольцо переживает
  свою ФС, а `recover` всегда стартует с `tail == 0`, поэтому без этой инвалидации первая запись
  прошлой инкарнации (её `seq == 1` совпадает с новым `next_seq`) прошла бы проверку цепочки и её
  старые образы легли бы в свежую ФС.
- `recover()` (на mount, до построения корня): скан от `tail`, реплей только валидных
  descriptor+commit с совпадением seq и CRC32; реплей идемпотентен. **Живость записи определяет
  `seq` против персистентного `next_seq`, а не `head == tail`** (issue #35): сдвиг `head`
  персистится *после* checkpoint'а, поэтому `head == tail` после краха означает «суперблок не
  двигался», а не «лог пуст». Записи с `seq < next_seq` подтверждены суперблоком (уже
  зачекпойнчены) и **пропускаются** — иначе старый образ лёг бы поверх нового; реплей идёт по
  цепочке от `next_seq`, первая битая/незакоммиченная запись останавливает скан.
- Тот же таргет, залогированный дважды: побеждает поздняя запись.

### Монтирование (из `boot.rs::init_fs`)
`has_valid_superblock` → `mount` → валидация (magic, block size, счётчики, `s_inode_size`) →
`Journal::open` + `recover()` → `reconcile_free_counts` → `Arc<Ext2Fs>`. Boot выбирает
virtio-blk, фоллбэк NVMe; маунт в `/mnt`.

Форматирование разрешено **только для genuinely blank устройства** — либо для устройства с
явным opt-in маркером (`PAGH-FORMAT` в секторе 0, см. `format_policy::OPT_IN_MARKER`) — и
решается в `format_policy` (issue #33): провал `mount` больше не означает «диск пустой».
Порядок правил — валидный (парсящийся) суперблок → отказ (`Refusal::ExistingFilesystem`,
убитый WAL не даёт права стирать); таблица разделов MBR/GPT → отказ
`Refusal::PartitionTable`; любые ненулевые данные → `Refusal::ForeignData`; формат — только
если все спроецированные 1 MiB нулевые либо в секторе 0 стоит маркер. Зонд идёт по 4 KiB с
ранним выходом и запускается **лишь после неудачного mount**, так что happy path ничего не
платит.

## Зависимости

- **От:** `drivers::BlockDevice` (`read_block`/`write_block` по 512-байтным секторам;
  реализуют `virtio::blk`, `nvme` и тестовый мок), `sync::spinlock`, `vfs` (VfsNode/VfsError/FsStat).
  Направление vfs←fs инвертировано намеренно: fs зависит от vfs, а boot подшивает корень fs в vfs-дерево.
- **На неё:** `boot.rs` (`init_fs`: mount/format), shell-команда `fscrash` (демо mount +
  journal recover), `test.rs` (fs_prop_tests, mock_block).

## Грабли

- `inode_location` отвергает мусорные inode-номера через `Corrupt`, а не клампит — кламп
  алиасил чужой слот inode-таблицы и освобождал произвольные блоки.
- `free_block` игнорирует блоки ниже `s_first_data_block` — иначе заворачивался и освобождал
  мусорный бит.
- `iter_entries` требует точного тайлинга `rec_len` по `[0, BS)` — любое отклонение → `Corrupt`.
- VfsNode-адаптеры держат `cached_size` как фоллбэк для `size()`, если `read_inode` упал
  (report 0 заставлял инсталляторы копировать пустой ld-linux.so).
- Симлинк нельзя отдавать как `Ext2File`: у fast-ссылки `i_block` — это **текст цели**, и
  `read_file`/`block_for_offset` прочитали бы его как номера блоков (чтение произвольного блока).
  Поэтому `node_for` диспетчеризует по `i_mode`, а `write_file`/`truncate_file` отвергают всё,
  кроме `S_IFREG`.
- `i_links_count` никто не восстанавливает: `recover()` реплеит только закоммиченные транзакции,
  `reconcile_free_counts` чинит лишь счётчики свободного по битмапам, fsck нет. Ошибка учёта
  ссылок необратима — правила зафиксированы в `ext2/symlink.rs` и покрыты host-свойствами.
- Удаление последней ссылки при **открытом** fd по-прежнему не защищено (узел живёт в `Arc`,
  блоки могут быть переиспользованы) — так было и до #18, это известное ограничение.
- Имя вставляется в каталог с повторной проверкой **внутри** транзакции (`insert_dirent`):
  внешний `lookup_entry` в `create/link` работает вне tx-лока, и двойная вставка одного имени
  создала бы два dirent на один inode, сломав учёт ссылок.
- Имя с `/` отвергается писателем (`check_new_entry`): путь всегда разбивается до вызова, а
  запись с разделителем в имени больше никогда не найдётся (`lookup_entry` режет по `/`) —
  недостижимый и неудаляемый файл. Найдено in-QEMU рутиной резолвера ссылок.
