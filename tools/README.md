# `tools/` — хост-тулинг: сборка, тесты, E2E

Кроссплатформенный билд-драйвер, хост-тесты, CI-гейты и E2E-харнессы.

## Файлы

| Файл | Роль |
|---|---|
| `build.py` | Кроссплатформенный build/link/stage/run драйвер (бэкенд Makefile) |
| `limine.py` | Версионно-независимый локатор/установщик `BOOTX64.EFI`: ищет любой локальный `limine*/` (или `LIMINE_EFI`/`LIMINE_DIR`), иначе качает последний бинарный релиз Limine (`limine-binary.zip`) в `limine/`; путь на stdout, прогресс на stderr |
| `host_tests.py` | Обёртка: определяет host triple через `rustc -vV`, запускает `cargo test --locked --target <host>` в `host-tests/` |
| `toolchain.sh` | Guard для `build.sh`/`run.sh`: отказывается собирать ненайтовым (stable/дистрибутивным) `cargo` — тот падает с `-Zjson-target-spec` и не называет настоящую причину. `tools/build.py` его не требует |
| `check_safety.py` | CI-гейт unsafe-политики: сканирует `src/security/`, `src/arch/x86_64/linux/mod.rs`, `src/memory/vmm.rs`, `src/net/tls.rs`, `src/pkg/apt.rs` — каждый `unsafe {` обязан иметь `SAFETY:`-коммент в предыдущих 6 строках, иначе exit 1 |
| `gen_ca_bundle.py` | Генератор trust-anchor бандла TLS-верификатора: скачивает curl/Mozilla CA extract, выбирает корни по subject CN (ISRG Root X1/X2, GTS R1/R4), пишет детерминированный `src/net/ca_bundle.rs` (DER-массивы + метки); сгенерированный файл коммитится, перегенерация — только осознанно |
| `fetch_p44_fixtures.py` | Генератор фикстур host-свойства P44: скачивает реальные ISRG Root X1 и лист `deb.debian.org`, пишет `host-tests/src/properties/p44_fixture_{root,leaf}.rs` (закоммичены; перегенерация — осознанно) |
| `gen_debian_keyring.py` | Генератор пиннутого Debian-keyring'а OpenPGP-верификатора (issue #32): качает зафиксированный по sha256 `debian-archive-keyring_*.deb` (или берёт `--deb FILE`), выбирает ключи **по v4-фингерпринту**, сверяет UID/алгоритм/размер/создание/истечение/подключи, пишет детерминированный `src/pkg/openpgp_keys.rs` (блоки байт + таблица пинов); `--check` — diff без записи, `--print-keyring` — тот же набор как бинарный keyring для `gpg --list-packets`. Перегенерация — только осознанно |
| `gen_openpgp_fixtures.py` | Генератор OpenPGP-фикстур для host-свойств P51–P53: настоящие подписи GnuPG 2.4 (`--faked-system-time`, Ed25519 + RSA-2048 c signing-подключом, просроченный и непроверенный ключи), пишет `host-tests/src/properties/openpgp_fixtures.rs` (закоммичены; генерация ключей случайна, поэтому перегенерация — осознанный акт) |
| `pgp_packets.py` | Мини-читалка OpenPGP-пакетов (реестр блоков, MPI, subpacket'ы, v4-фингерпринты) для обоих генераторов выше; только стандартная библиотека |
| `mini_repo.py` | Мини Debian-зеркало в `tools/mini_repo/` для apt-E2E: **пять suite'ов** — `stable` (корректный и **подписанный**), `tampered-index` (подпись валидна, но отдаваемый `Packages` не тот, что описан в подписанном `Release`; длина совпадает — ловится только SHA-256), `tampered-deb` (метаданные корректны, а `.deb` не тот; тоже равной длины), `unsigned` (подписей нет) и `untrusted` (подписан непроверенным ключом). Подпись — `tools/openpgp_sign.py` по детерминированному тестовому seed'у, без секретов в репозитории |
| `openpgp_sign.py` | Детерминированная OpenPGP-подпись для фикстур (issue #32): RFC 8032 Ed25519 от фиксированного seed + v4-пакеты (public key с легаси-OID, UID, сертификация 0x13, detached/clearsign) + armor с CRC24. Только stdlib, векторы RFC 8032 проверяются перед каждой генерацией; применим только для тестовых фикстур |
| `gen_openpgp_testkey.py` | Генератор E2E-**тестового** trust-anchor'а `src/pkg/openpgp_test_keys.rs` (компилируется только под `lx_selftest`/`lx_bigindex`): детерминированный Ed25519-ключ + три пиннутых ключа Debian в одной таблице. `--print-keyring DIR` выкладывает бинарные блоки для `gpg --list-packets` |
| `build-rust-app.sh` | Сборка userland-приложений (`rust-apps/`) под `x86_64-unknown-linux-musl` |
| `e2e_local_mirror.ps1` | Детерминированный apt E2E: release-сборка c `--features lx_selftest`, stage, serve mini_repo, QEMU, assert serial-маркеров |
| `e2e_live_update.ps1` | Live `apt update` против `deb.debian.org` (`--features lx_livetest`) по HTTP (embedded-tls висит на ~12 MiB, issue #19); assert `LIVE_APT_UPDATE: count=N`, N ≥ 50000; тайминги soft |
| `e2e_bigindex.ps1` | Репро parse-краша #14 (`--features lx_bigindex`, `-InRam` добавляет `lx_bigindex_inram`); скан serial на `[EXC #14]` |
| `smoke_assertions.ps1` | Проверка smoke-критериев R4.1/R4.2/R7.4 по захваченным serial-логам (промпт достигнут, debug-link работает, аутентификация HTTPS-сервера подтверждена: `LXSELFTEST https_get PASS` либо отказ верификатора `Package_Fetcher(tls): stage=verify cause=…`) |
| `mini_repo/` | Сгенерированное дерево зеркала (**закоммичено**, чтобы e2e не требовал сборки) |

## build.py — как собирается ядро

- `build`: `cargo build --locked [--release] [--features ...]`, затем линковка `libpagh.a`
  (`pagh.lib`) в `pagh.elf` через rust-lld:
  `rust-lld -flavor gnu -T linker.ld -nostdlib -static --whole-archive <archive> --no-whole-archive -o pagh.elf`.
- `stage`: чистка + пересборка `iso_root/` — `pagh.elf` в корень, `EFI/BOOT/BOOTX64.EFI`
  через `limine.py` (любая локальная `limine*/`-дерево, иначе автоскачивание),
  `boot/limine.conf` записывается в двух местах (корень ISO + `EFI/BOOT/`).
- `run`: stage + QEMU (`-cpu max` по умолчанию, `-bios <OVMF>`, `fat:rw:iso_root`, virtio-blk `disk.img`, e1000 NIC
  с hostfwd `tcp/udp 5555->7`, `-m 1024M`, `-serial stdio`, debug-трейс в `qemu_debug.log`).
  По умолчанию `-cpu max`: TLS-путь требует RDSEED/RDRAND, которых у дефолтного `qemu64`
  нет, поэтому живой HTTPS-чек падает с `stage=entropy` (и ничего не объясняет).
- Env-оверрады: `LIMINE_DIR`/`LIMINE_EFI` (иначе автопоиск/автоскачивание), `OVMF`/`--ovmf`
  (если заданы и файл существует — берутся они; иначе `OVMF.fd` из корня репо; иначе системный
  `OVMF_CODE.fd`, напр. `/usr/share/edk2/ovmf/`), `PAGH_DISK` (дефолт `disk.img`),
  `PAGH_QEMU_CPU` (дефолт `max` — **только для `build.py`**: три `e2e_*.ps1` жёстко передают
  `-cpu max`, `run.sh` читает другое имя `PAGH_CPU`, а `run.cmd` `-cpu` не передаёт вообще,
  поэтому на Windows-пути живой HTTPS-чек упадёт в `stage=entropy`).

## mini_repo.py

Моды: `build` (собирает `dists/stable/main/binary-amd64/Packages[.gz]` + pool с
`hello-pagh_1.0_amd64.deb` — hand-assembled static ELF, печатающий `hello from apt`),
`serve [port]` (bind 0.0.0.0; из гостя виден как `10.0.2.2:8000`),
`bigindex [N] [port]` — синтетический 60000-станзовый `Packages.gz` для репро-харнесса.

## E2E-скрипты (PowerShell)

- Общий паттерн: сборка feature-ELF → перезапись `iso_root/pagh.elf` → serve → QEMU →
  assert по serial → восстановление дефолтного ELF. `-KeepArtifacts` сохраняет артефакты.
- Параметры: `-Port` (8000; `e2e_local_mirror`/`e2e_bigindex`), `-Stanzas` (60000; только
  `e2e_bigindex`), `-TimeoutSec` (120 / 1200 / 240), `-KeepArtifacts`, `-InRam` (только
  bigindex), `-SkipDebugBuild` (smoke). У `e2e_live_update.ps1` параметра `-Port` нет.
- Замечание: `mini_repo.py serve` слушает 0.0.0.0; скрипты ждут раскладки
  `BOOTX64.EFI` (через `limine.py`) + `OVMF.fd`.

## Грабли

- E2E-скрипты перезаписывают `iso_root/pagh.elf` и восстанавливают его после — не убить
  дефолтный артефакт посреди прогона.
- `build.py` даёт диск 64 MiB и `-m 1024M`, а `run.sh` — 1 GiB диск и `-m 1024M` (куче нужно
  ≥1 GiB RAM; см. `src/memory/README.md`; на машинах с меньшей RAM ядро само капит кучу
  с `[WARN]` вместо паники).
- NIC расходится: e1000 (`build.py`, `run.sh`) против virtio-net-pci (`run.cmd`, bg-скрипты).

## Грабли: «не тот cargo»

Если `cargo` в `PATH` — не rustup-шим, а дистрибутивный бинарь, сборка падает с

```
error: `.json` target specs require -Zjson-target-spec to be added to the cargo invocation
```

Причина не в этой опции: у ядра кастомный target `x86_64-unknown-none.json`, а
`json-target-spec` — unstable-опция, поэтому stable-cargo до реальной проблемы просто не
доходит. Нужен toolchain из `rust-toolchain.toml` (nightly + `rust-src` + `rust-lld`).

Лечится добавлением rustup в `PATH` — `. "$HOME/.cargo/env"` (и эту же строку в `~/.bashrc`).
`build.sh`/`run.sh` теперь проверяют это сами (`tools/toolchain.sh`) и печатают объяснение
вместо ошибки про `-Zjson-target-spec`.
