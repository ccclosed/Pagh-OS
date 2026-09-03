# `tools/` — хост-тулинг: сборка, тесты, E2E

Кроссплатформенный билд-драйвер, хост-тесты, CI-гейты и E2E-харнессы.

## Файлы

| Файл | Роль |
|---|---|
| `build.py` | Кроссплатформенный build/link/stage/run драйвер (бэкенд Makefile) |
| `host_tests.py` | Обёртка: определяет host triple через `rustc -vV`, запускает `cargo test --locked --target <host>` в `host-tests/` |
| `check_safety.py` | CI-гейт unsafe-политики: сканирует `src/security/`, `src/arch/x86_64/linux/mod.rs`, `src/memory/vmm.rs`, `src/net/tls.rs`, `src/pkg/apt.rs` — каждый `unsafe {` обязан иметь `SAFETY:`-коммент в предыдущих 6 строках, иначе exit 1 |
| `mini_repo.py` | Мини Debian-зеркало в `tools/mini_repo/` для apt-E2E |
| `build-rust-app.sh` | Сборка userland-приложений (`rust-apps/`) под `x86_64-unknown-linux-musl` |
| `e2e_local_mirror.ps1` | Детерминированный apt E2E: release-сборка c `--features lx_selftest`, stage, serve mini_repo, QEMU, assert serial-маркеров |
| `e2e_live_update.ps1` | Live `apt update` против `deb.debian.org` (`--features lx_livetest`); assert `LIVE_APT_UPDATE: count=N`, N ≥ 50000; тайминги soft |
| `e2e_bigindex.ps1` | Репро parse-краша #14 (`--features lx_bigindex`, `-InRam` добавляет `lx_bigindex_inram`); скан serial на `[EXC #14]` |
| `smoke_assertions.ps1` | Проверка smoke-критериев R4.1/R4.2/R7.4 по захваченным serial-логам (промпт достигнут, debug-link работает, HTTPS-INSECURE предупреждение ровно один раз) |
| `mini_repo/` | Сгенерированное дерево зеркала (gitignored) |

## build.py — как собирается ядро

- `build`: `cargo build --locked [--release] [--features ...]`, затем линковка `libpagh.a`
  (`pagh.lib`) в `pagh.elf` через rust-lld:
  `rust-lld -flavor gnu -T linker.ld -nostdlib -static --whole-archive <archive> --no-whole-archive -o pagh.elf`.
- `stage`: чистка + пересборка `iso_root/` — `pagh.elf` в корень, `EFI/BOOT/BOOTX64.EFI`
  из `limine-12.3.1/`, `boot/limine.conf` записывается в двух местах (корень ISO + `EFI/BOOT/`).
- `run`: stage + QEMU (`-bios OVMF.fd`, `fat:rw:iso_root`, virtio-blk `disk.img`, e1000 NIC
  с hostfwd `tcp/udp 5555->7`, `-m 512M`, `-serial stdio`, debug-трейс в `qemu_debug.log`).
- Env-оверрайды: `LIMINE_DIR` (дефолт `limine-12.3.1`), `OVMF` (дефолт `OVMF.fd`),
  `PAGH_DISK` (дефолт `disk.img`).

## mini_repo.py

Моды: `build` (собирает `dists/stable/main/binary-amd64/Packages[.gz]` + pool с
`hello-pagh_1.0_amd64.deb` — hand-assembled static ELF, печатающий `hello from apt`),
`serve [port]` (bind 0.0.0.0; из гостя виден как `10.0.2.2:8000`),
`bigindex [N] [port]` — синтетический 60000-станзовый `Packages.gz` для репро-харнесса.

## E2E-скрипты (PowerShell)

- Общий паттерн: сборка feature-ELF → перезапись `iso_root/pagh.elf` → serve → QEMU →
  assert по serial → восстановление дефолтного ELF. `-KeepArtifacts` сохраняет артефакты.
- Параметры: `-Port` (8000), `-TimeoutSec` (120 / 1200 / 240), `-KeepArtifacts`,
  `-InRam` (только bigindex), `-SkipDebugBuild` (smoke).
- Замечание: `mini_repo.py serve` слушает 0.0.0.0; скрипты ждут раскладки
  `limine-12.3.1/BOOTX64.EFI` + `OVMF.fd`.

## Грабли

- E2E-скрипты перезаписывают `iso_root/pagh.elf` и восстанавливают его после — не убить
  дефолтный артефакт посреди прогона.
- `build.py` даёт диск 64 MiB и `-m 512M`, а `run.sh` — 1 GiB диск и `-m 1024M` (куче нужно
  ≥1 GiB RAM; см. `src/memory/README.md`).
- NIC расходится: e1000 (`build.py`, `run.sh`) против virtio-net-pci (`run.cmd`, bg-скрипты).
