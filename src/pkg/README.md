# `src/pkg/` — apt-style пакетный менеджер

Получение и установка Debian `.deb`-пакетов по HTTP/HTTPS: apt-фронтенд (update/install/show/
list/setmirror), парсер индекса `Packages`, резолвер зависимостей, парсер `ar`/`.deb`
с gzip/xz/zstd декомпрессией, tar reader/writer, эффектный ext2-инсталлятор.

Разделение: чистые модули (`core`+`alloc`, хост-тестируемые через `#[path]` в `host-tests`)
и kernel-only (`apt.rs`, `install_fs.rs`).

## Файлы

| Файл | Роль |
|---|---|
| `mod.rs` | Корень; документирует split pure/kernel |
| `apt.rs` | Kernel-only apt-фронтенд: `update`/`install`/`show`/`list`/`setmirror` |
| `apt_index.rs` | Чистый парсер Debian-индекса (RFC822-станзы) + компактный arena-backed `PackageIndex` |
| `apt_resolve.rs` | Чистый резолвер зависимостей → план установки dependency-first |
| `deb.rs` | Чистый парсер `ar`/`.deb`, классификация сжатия, gzip/xz/zstd декомпрессоры (буферные и streaming) |
| `tar.rs` | Чистый POSIX/ustar tar reader/writer, zero-copy, валидация checksum |
| `install.rs` | Чистая нормализация путей и модель инсталлятора |
| `install_fs.rs` | Kernel-only ext2-инсталлятор (`install_data_tar`) через `VfsNode`, включая материализацию симлинков |
| `mirror.rs` | Чистый парсер аргумента `apt setmirror` |

## Ключевые символы

- `apt`: `AptConfig { host, base, suite, component, arch, port, tls }` (дефолт: `deb.debian.org`,
  `/debian`, `stable`, `main`, `amd64`, порт 443, tls), `set_mirror()`, `update() -> usize`,
  `install(name) -> Vec<String>`, `show()`, `list()`, `has_index()`, `index_footprint()`;
  `AptOpError::{NetworkDisabled, NoNetwork, NoIndex, NotFound, Download, Parse, IndexTooLarge, Install}`.
- `apt_index`: `parse_packages(&[u8])`, `parse_depends()`, `StanzaParser` (инкрементальный `push`),
  `PackageIndexBuilder`, `PackageIndex::{get, get_provider, contains, names, footprint}`;
  читаются 8 ключей станзы (Package, Version, Architecture, Filename, Depends, Pre-Depends,
  Provides, Size).
- `apt_resolve`: `resolve_install(index, target, already_installed) -> Vec<String>` —
  итеративный worklist post-order DFS (рекурсивная версия переполняла kernel-стек).
- `deb`: `parse_ar`, `locate_members`, `compression_of`, `decompress_data`,
  `decompress_bytes_capped`, `decompress_stream(data, c, max, sink)`.
- `tar`: `read_tar(buf) -> Vec<TarEntry>`, `write_tar(entries)`; `TarType::{Regular, Directory, Symlink, Other}`.
- `install_fs`: `install_data_tar(entries, root) -> usize`; `InstallError::{NoSpace, Vfs}`.

## Как работает

### Состояние apt (`apt.rs`)
Три глобала под спинлоками: `CONFIG` (зеркало), `INDEX` (распарсенный `PackageIndex`,
**RAM-only, не персистится** — диск мал; пересобирается каждым `apt update`),
`INSTALLED` (`BTreeSet<String>` — сессионный список, не настоящий dpkg db).
Сетевой I/O никогда не держит лок индекса.

### apt update
`{base}/dists/{suite}/{component}/binary-{arch}/Packages.gz` → фоллбэк `.xz` → несжатый
`Packages`. Тело декомпрессится **инкрементально** (`decompress_stream`, чанки 8 KiB),
каждый чанк — в `StanzaParser::push_view` → `PackageIndexBuilder` (arena-интернинг, без
owned `PkgRecord`). Поток ограничен `MAX_INDEX_STREAM_BYTES` (512 MiB) → чистый
`IndexTooLarge` вместо OOM-аборта.

### apt install
`resolve_install` → на каждый пакет: fetch `{base}/{filename}` → `parse_ar` →
`locate_members` → `decompress_data` (data.tar целиком, cap 64 MiB) → `read_tar` →
`install_data_tar(entries, "/mnt")` → `sync()` vfs-ноды → запись в `INSTALLED`.
Упрощения резолвера (задокументированы): версии игнорируются, Pre-Depends слиты с Depends,
отсутствующие транзитивные депы молча пропускаются, первый годный альтернативный вариант,
виртуалы через Provides.

### Инсталляция в ext2
Пропуск non-regular записей; нормализация пути (`..`-выход за корень → `SkipUnsafe`);
создание недостающих родителей; удаление+пересоздание существующего файла (ext2 `write_file`
только растит `i_size`); `VfsError::IoError` от ext2 = out-of-space → частичный файл удаляется,
`InstallError::NoSpace`. Симлинки материализуются копиями (до 4 проходов по цепочкам) —
в ext2-драйвере симлинков нет.

### Декомпрессия
gzip — RFC 1952 вручную + `miniz_oxide`; xz — `xz4rust` (словарь cap 64 MiB); zstd —
`ruzstd::StreamingDecoder`. Все декодеры абортятся на non-progress и превышении cap.

## Константы

| Константа | Значение |
|---|---|
| `MAX_DECOMPRESSED` | 64 MiB (на member) |
| `MAX_INDEX_STREAM_BYTES` | 512 MiB |
| `STREAM_CHUNK` | 8 KiB (небольшой — живёт на kernel-thread стеке) |
| Полный Debian-индекс | ~150 MiB декомпрессированного, десятки минут под QEMU |

## Зависимости

- **От:** `net::tls::https_get`, `net::http_fetch::{http_get, fetch_deb}`, `vfs`,
  `sync::spinlock`, крейты `miniz_oxide`, `xz4rust`, `ruzstd`.
- **На неё:** `shell/commands.rs` (`cmd_apt_*`), `provision.rs`, `selftest_lx.rs`.

## Безопасность

- HTTPS использует `embedded-tls` c `UnsecureProvider`: **нет проверки цепочки/hostname/expiry**,
  тривиально MITM-аемо. Нет CA/InRelease/пакетных hash-верификаций. Fail-closed сборка:
  `cargo build --no-default-features` — тогда `apt update/install` возвращают `NetworkDisabled`.
- Индекс RAM-only: полный Debian ≈ 150 MiB декомпрессированного — потолок по памяти,
  при превышении чистый отказ.
