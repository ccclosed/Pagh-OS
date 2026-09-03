# `host-tests/` — хост-тесты чистых модулей ядра

Отдельный хост-крейт (`pagh-host-tests`), **исключён из workspace** ядра
(`[workspace].exclude` в корневом `Cargo.toml`): ядро `no_std` bare-metal и не может
хост-компилироваться. Тесты — proptest-свойства P1–P41 над чистой логикой ядра.

## Как включается код ядра

`host-tests/src/lib.rs` включает kernel-модули **напрямую** через
`#[path = "../../src/..."]`: `errno`, `stat`, `validate`, `io`, `abi`, `rand_clock`, `stack`,
`elf_classify`, `http`, `wire`, `dns`, `deb`, `tar`, `install`, `apt_index`, `apt_resolve`,
`mirror`, `diag`, `mem`, `dirent`, `timeconv`, `fd_alloc` — тесты исполняют точный исходник
ядра (нет копий и дрейфа). Плюс no-op `warn!`-шим и `extern crate alloc`.
Эти модули обязаны быть только `core`+`alloc`.

## Структура

| Путь | Роль |
|---|---|
| `Cargo.toml` | Зависимости зеркалят ядро: `miniz_oxide =0.8.9`, `ruzstd =0.8.3` (без default features — иначе тянет `twox-hash`, которого нет в `vendor/`), `xz4rust =0.2.1`; dev-dep `proptest = "1"` |
| `.cargo/config.toml` | Переопределяет `build.target` (хардкод `x86_64-pc-windows-msvc`) и расширяет `build-std` — cargo КОНКАТЕНИРУЕТ наследуемые массивы, очистить `-Zbuild-std` из корневого конфига нельзя |
| `src/lib.rs` | Ядро харнесса: `#[path]`-инклюды, список тест-модулей `p01`–`p41`, `p29_fixtures`, `bigindex` |
| `src/properties/p01.rs` … `p41.rs` | 41 файл свойств: DMA share/unshare, консервация PMM, контекст-фреймы, ext2 sizing/roundtrip, классификация ELF, парсинг HTTP/DNS/deb/tar, apt index/resolve, dirent, конверсия времени, fd-аллокация, xz/zstd (P29) и др. |
| `src/properties/p29_fixtures.rs` | ~226 KB `@generated` const-массивов байтов. НЕ редактировать руками |
| `src/bigindex.rs` | 60k-станзовый apt-index репро (диагностика краша #14) |
| `proptest-regressions/` | Персистентные regression-кейсы proptest (`p15.txt`, `p17.txt`) |

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
