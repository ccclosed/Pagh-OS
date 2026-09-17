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
| `tar.rs` | Чистый POSIX/ustar/GNU/pax tar reader/writer, zero-copy, валидация checksum: `TarType::{Regular,Directory,Symlink,Hardlink,Other}`, GNU `'L'`/`'K'`, ustar `prefix`, pax `path=`/`linkpath=` |
| `install.rs` | Чистая нормализация путей и модель инсталлятора |
| `install_fs.rs` | Kernel-only ext2-инсталлятор (`install_data_tar`) через `VfsNode`: реальные симлинки и хардлинки (issue #18), создание недостающих родителей |
| `mirror.rs` | Чистый парсер аргумента `apt setmirror` |
| `openpgp.rs` | Чистая политика OpenPGP-верификации (issue #32): выбор доверенного ключа, subkey binding, expiry/revocation/key-flags, точки входа `verify_detached` (`Release.gpg`) и `verify_clearsigned` (`InRelease`), `check_pinned_key` |
| `openpgp_packet.rs` | Чистый слой пакетов: armor (CRC24), framing (old/new, definite lengths), public key/subkey, signature packet, keyring-блок, clearsign-разбор и канонизация |
| `openpgp_crypto.rs` | Чистая криптография верификатора: правило v4-хеша, RSA PKCS#1 v1.5 / EdDSA / ECDSA по дайджесту, локальная SHA-1 (только для v4-фингерпринтов) |
| `openpgp_keys.rs` | **Сгенерированный** (`tools/gen_debian_keyring.py`) пиннутый Debian-keyring: байтовые блоки ключей + фингерпринты, алгоритм, размер, окно валидности, подключи |

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
- `install`: `plan_install(entries) -> InstallPlan` — чистый планировщик (файлы, симлинки, порядок хардлинков, `deferred`/`unresolved`, счётчики пропусков).
- `tar`: ещё `write_tar_members(members, TarFormat)` + `TarMember` (фикстуры со ссылками и длинными именами), `effective_path(entry)` (склейка ustar `prefix`).

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

### Инсталляция в ext2 (issue #18)
Порядок и выбор членов архива решает **чистый** `install::plan_install` (host-свойства
`tar_links`), а `install_data_tar` его исполняет:
- регулярные файлы — как раньше: нормализация пути (`..`-выход за корень → `SkipUnsafe`),
  создание недостающих родителей (резолв **со следованием по ссылкам**, поэтому `lib64 → usr/lib64`
  кладёт файл внутрь цели), удаление+пересоздание существующей записи (ext2 `write_file` только
  растит `i_size`);
- **симлинки создаются как симлинки** (`VfsNode::create_symlink`), цель хранится дословно;
  висячая цель — норма (альтернативы/`ld-linux`);
- **хардлинки — как хардлинки** (`VfsNode::link`, общий inode, `st_nlink == 2`), цель берётся из
  ФС без следования по конечной ссылке; цели, которых ещё нет, повторяются фикспойнтом, а
  неразрешимые пропускаются с одним warn — **копией не становится ничто**;
- существующий каталог на пути члена — пропуск с warn (рекурсивного удаления нет);
- `VfsError::IoError` от ext2 = out-of-space → частичный файл удаляется, `InstallError::NoSpace`.

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

- HTTPS аутентифицирует зеркало fail-closed: цепочка до committed CA-бандла (`ca_bundle.rs`),
  SAN-авторизация хоста, validity + clock gate (незаданный RTC — отказ) и подпись TLS 1.3
  `CertificateVerify`; любой отказ обрывает handshake, а не деградирует до HTTP. Рукопожатие
  без `Certificate`/`CertificateVerify` тоже отказ: вендорный патч `embedded-tls` не принимает
  server `Finished` без них, плюс независимый гейт `net::tls::VERIFIED_HANDSHAKES`. Доверие
  ограничено четырьмя пиннутыми корнями (ISRG Root X1/X2, GTS R1/R4), поэтому HTTPS-зеркало вне
  этой выдачи отвергается (`ChainError::NoAnchor`) — лечится только осознанной перегенерацией
  бандла `tools/gen_ca_bundle.py`. Не проверяются: отзыв (CRL/OCSP), подписи метаданных Debian
  (OpenPGP) и полнота digest'ов пакетов; HTTP-зеркала (`apt setmirror http://…`)
  неаутентифицированы по построению. Fail-closed сборка: `cargo build --no-default-features` —
  тогда `apt update/install` возвращают `NetworkDisabled`.
- Индекс RAM-only: полный Debian ≈ 150 MiB декомпрессированного — потолок по памяти,
  при превышении чистый отказ.
- **OpenPGP-верификатор (issue #32, первый шаг серии).** Реализованы и покрыты host-свойствами
  P51–P53 (`host-tests/src/properties/p5{1,2,3}.rs`) чистые модули `openpgp{,_packet,_crypto}.rs`:
  разбор armor/пакетов, проверка `Release.gpg` (detached) и `InRelease` (clearsigned) по
  пиннутому keyring'у, обработка subkey/expiry/revocation/key-flags. Доверие — только к трём
  ключам, закреплённым по v4-фингерпринту в `src/pkg/openpgp_keys.rs` (генерируется
  `tools/gen_debian_keyring.py` из зафиксированного по sha256 `debian-archive-keyring`; рантайм
  ключи не качает). P53 проверяет РЕАЛЬНЫЕ подписи GnuPG внутри закоммиченных блоков (RSA-4096
  SHA-512 self-sig + subkey binding, Ed25519 SHA-256 с легаси OID 1.3.6.1.4.1.11591.15.1).
  Два осознанных отличия от `gpgv`: подпись просроченного ключа отвергается (gpgv её принимает),
  и частичные длины пакетов не собираются, а отвергаются. Пока НЕ подключено к `apt.rs`:
  привязка `InRelease` → SHA-256 `Packages` → SHA-256 `.deb` и негативный e2e — следующая задача
  серии, поэтому строка «подписи метаданных не проверяются» выше остаётся верной до неё.
