# `src/fs/` — ext2 + WAL-журнал

Совместимая с ext2 (Linux-маунтабельная) файловая система на реальном диске с собственным
write-ahead-log журналом для crash-consistency. Чистая логика над трейтом `BlockDevice`
(`drivers::mod`) — ядро-тесты гоняют её через RAM-мок.

## Файлы

| Файл | Роль |
|---|---|
| `mod.rs` | Корень; enum `FsError` |
| `journal.rs` | WAL-журнал: `Journal`, `Txn`, `JournalArea` — кольцевой лог, атомарные мультиблочные транзакции, CRC-коммиты, crash-consistent `recover()` |
| `ext2/mod.rs` | Ядро драйвера: `Ext2Fs` (format/mount), `Tx` (dirty-набор, аллокаторы, block-map), файловые операции, VfsNode-адаптеры `Ext2Dir`/`Ext2File`, маппинг ошибок `fs_to_vfs` |
| `ext2/structs.rs` | On-disk `#[repr(C)]`-структуры с compile-time фиксацией размеров (`Ext2SuperBlock` 0x88 B, `Ext2GroupDesc` 32 B, `Ext2Inode` 128 B, `JournalSuper` 56 B и др.), магии, unaligned read/write, CRC32 |
| `ext2/alloc.rs` | Битмап-примитивы: `alloc_bit`, `set/clear/test_bit`, `count_set_bits`; `alloc_block`/`free_block`/`alloc_inode`/`free_inode` с синком счётчиков sb/GD |
| `ext2/dir.rs` | Движок каталогов: `iter_entries` (строгая валидация тайлинга `rec_len`), `find`, `init_dot_entries`, `insert_into_block` (расщепление slack донора), `remove_from_block` (слияние `rec_len` с предыдущей), `DirEntry`, `min_rec_len` |
| `ext2/inode.rs` | Чтение block map: `block_for_offset` — 12 direct + single/double/triple indirect (`PTRS_PER_BLOCK=1024`); дыры → `None` |

## Ключевые символы

- `FsError::{BadSuperBlock, BadJournal, OutOfSpace, NotFound, AlreadyExists, IoError, NameTooLong, Corrupt, FileTooBig}`.
- `Ext2Fs`: `format(dev)`, `mount(dev) -> Arc<dyn VfsNode>`, `mount_fs(dev) -> Arc<Ext2Fs>`,
  `has_valid_superblock(dev)`, `read_file/write_file/truncate_file/create/unlink`, `sync()` (no-op),
  `read_fs_block/read_inode/lookup_entry/read_dir_entries`.
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
- Формат многогрупповой: всё устройство минус 65 блоков журнала, группы по 32768 блоков.
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

### Журнал
- On-disk транзакция = `[Descriptor][Data]*N[Commit]`; Descriptor держит до 254 таргетов,
  Commit — magic + seq + CRC32 по всем data-блокам. Коммит-запись — точка атомарности/дюрабильности.
- `commit`: проверка лимитов, reclaim всего лога при нехватке места (безопасно — чекпойнтинг
  синхронный), валидация таргетов. Один запасной слот лога держится, чтобы `head == tail`
  однозначно означало «пусто» (на это опирается recover).
- `recover()` (на mount, до построения корня): пустой лог → 0; иначе скан от `tail` по seq,
  реплей только при валидных descriptor+commit, совпадении seq и CRC32; первая битая/незакоммиченная
  транзакция останавливает реплей с отбрасыванием остатка. Реплей — идемпотентная перезапись блоков.
- Тот же таргет, залогированный дважды: побеждает поздняя запись.

### Монтирование (из `boot.rs::init_fs`)
`has_valid_superblock` → `mount` → валидация (magic, block size, счётчики, `s_inode_size`) →
`Journal::open` + `recover()` → `reconcile_free_counts` → `Arc<Ext2Fs>`. Boot выбирает
virtio-blk, фоллбэк NVMe; форматирование — только genuinely blank диска
(валидный суперблок с убитым WAL = ошибка, а не разрешение стереть данные); маунт в `/mnt`.

## Зависимости

- **От:** `drivers::BlockDevice` (`read_block`/`write_block` по 512-байтным секторам;
  реализуют `virtio::blk`, `nvme` и тестовый мок), `sync::spinlock`, `vfs` (VfsNode/VfsError/FsStat).
  Направление vfs←fs инвертировано намеренно: fs зависит от vfs, а boot подшивает корень fs в vfs-дерево.
- **На неё:** `boot.rs`, shell-команды mount/format, `test.rs` (fs_prop_tests, mock_block).

## Грабли

- `inode_location` отвергает мусорные inode-номера через `Corrupt`, а не клампит — кламп
  алиасил чужой слот inode-таблицы и освобождал произвольные блоки.
- `free_block` игнорирует блоки ниже `s_first_data_block` — иначе заворачивался и освобождал
  мусорный бит.
- `iter_entries` требует точного тайлинга `rec_len` по `[0, BS)` — любое отклонение → `Corrupt`.
- VfsNode-адаптеры держат `cached_size` как фоллбэк для `size()`, если `read_inode` упал
  (report 0 заставлял инсталляторы копировать пустой ld-linux.so).
