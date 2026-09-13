# `host-tests/` — хост-тесты чистых модулей ядра

Отдельный хост-крейт (`pagh-host-tests`), **исключён из workspace** ядра
(`[workspace].exclude` в корневом `Cargo.toml`): ядро `no_std` bare-metal и не может
хост-компилироваться. Тесты — proptest-свойства P1–P49 над чистой логикой ядра.

## Как включается код ядра

`host-tests/src/lib.rs` включает kernel-модули **напрямую** через
`#[path = "../../src/..."]`: `errno`, `stat`, `validate`, `io`, `abi`, `rand_clock`, `stack`,
`elf_classify`, `http`, `wire`, `dns`, `deb`, `tar`, `install`, `apt_index`, `apt_resolve`,
`mirror`, `diag`, `mem`, `dirent`, `timeconv`, `fd_alloc`, `signal_frame`, `x509`, `hostname`,
`tls_verify`, `tls_chain`, `ca_bundle`, `tls_auth` — тесты исполняют точный исходник
ядра (нет копий и дрейфа). Плюс no-op `warn!`-шим и `extern crate alloc`.
Эти модули обязаны быть только `core`+`alloc`.

## Структура

| Путь | Роль |
|---|---|
| `Cargo.toml` | Зависимости зеркалят ядро: `miniz_oxide =0.8.9`, `ruzstd =0.8.3` (без default features — иначе тянет `twox-hash`, которого нет в `vendor/`), `xz4rust =0.2.1`; dev-dep `proptest = "1"` |
| `.cargo/config.toml` | Переопределяет `build.target` (хардкод `x86_64-pc-windows-msvc`) и расширяет `build-std` — cargo КОНКАТЕНИРУЕТ наследуемые массивы, очистить `-Zbuild-std` из корневого конфига нельзя |
| `src/lib.rs` | Ядро харнесса: `#[path]`-инклюды, список тест-модулей `p01`–`p49`, `det_rng`, `chain_der`, `p29_fixtures`, `bigindex` |
| `src/properties/p01.rs` … `p49.rs` | 49 файлов свойств: DMA share/unshare, консервация PMM, контекст-фреймы, ext2 sizing/roundtrip, классификация ELF, парсинг HTTP/DNS/deb/tar, apt index/resolve, dirent, конверсия времени, fd-аллокация, xz/zstd (P29), сигнальный ABI (P42), DER-ридер + X.509-время (P43), парсер сертификатов (P44), hostname/SAN (P45), подписи сертификатов (P46), цепочка + clock gate (P47), CA-бандл (P48), server-auth + `CertificateVerify` (P49) и др. |
| `src/properties/det_rng.rs`, `chain_der.rs` | Общие хелперы: детерминированный RNG (вынесен из P46) и сборка синтетических DER-цепочек (вынесена из P47) |
| `src/properties/p44_fixture_root.rs`, `p44_fixture_leaf.rs` | `@generated` фикстуры сертификатов для P44 |
| `src/properties/p29_fixtures.rs` | ~226 KB `@generated` const-массивов байтов. НЕ редактировать руками |
| `src/bigindex.rs` | 60k-станзовый apt-index репро (диагностика краша #14) |
| `proptest-regressions/` | Персистентные regression-кейсы proptest (`p15`, `p17`, `p44`, `p47`, `p49`) |

## Запуск

```sh
make test                    # = python3 tools/host_tests.py
cd host-tests && cargo test  # напрямую
```

CI-джоба `host-tests` гоняет на `nightly-2026-07-12`.

## Грабли

- `host-tests/.cargo/config.toml` хардкодит `x86_64-pc-windows-msvc`; на Linux/WSL явный
  `--target <host>` от `tools/host_tests.py` его перекрывает — «голый» `cargo test` в чекауте
  непортабелен.
- `p29_fixtures.rs` регенерируется `gen_fixtures.py` (нужен Python 3.14 — `compression.zstd`);
  генератор авторитетен, т.к. компрессоры питона не совпадают с декомпрессорами под тестом.
- `#[path]`-инклюды: рефактор ядра может сломать хост-сборку (обратно — безопасно).
- `ruzstd` держать с `default-features = false`.
