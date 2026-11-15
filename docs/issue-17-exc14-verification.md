# Issue #17 — `EXC #14` на parse-стадии apt-индекса: доказательство (закрыт)

## Вердикт

**#17 закрыт: крэш `[EXC #14]` не воспроизводится ни на одном протестированном масштабе и
ни на одной из шести сборок.** Harness'ы остаются как постоянные регресс-репро
(`tools/e2e.py bigindex`, `host-tests/src/bigindex.rs`, `lx_bigindex` +
`lx_bigindex_inram`) — они дёшевы и являются единственным детерминированным детектором
этого класса.

## Прогоны

| # | Что | Команда | Индекс | Результат | Время |
|---|---|---|---|---|---|
| 1 | streaming 60k | `python3 tools/e2e.py bigindex --stanzas 60000 --timeout 900 --serial-log serial_bigindex_60k.log` | `Packages` 22 949 891 B (21.89 MiB), `.gz` 1 702 056 B (1.62 MiB), 60 000 станзов | `apt: read package lists - 21.8 MiB / 60000 pkgs` → `apt: index ready - 60000 packages` → `LXSELFTEST bigindex PASS (streaming; 60000 packages)`, **exit 0** | 36.2 s |
| 2 | in-RAM | `python3 tools/e2e.py bigindex --stanzas 60000 --in-ram --timeout 900 --serial-log serial_bigindex_60k_inram.log` | **фиксированные 12 000 станзов in-kernel** (см. ограничение ниже) | `BIGINDEX variant=IN-RAM … 12000 stanzas IN-KERNEL` → `LXSELFTEST bigindex PASS (in-ram in-kernel; 12000 packages parsed)`, **exit 0** | 42.0 s |
| 3 | streaming 120k (запас по масштабу) | `python3 tools/e2e.py bigindex --stanzas 120000 --timeout 900 --serial-log serial_bigindex_120k.log` | `Packages` 45 929 891 B (43.80 MiB), `.gz` 3 403 681 B (3.25 MiB) | `apt: read package lists - 43.8 MiB / 120000 pkgs` → `index ready - 120000 packages` → `PASS (streaming; 120000 packages)`, **exit 0** | 55.2 s |
| 4 | host-репро | `cd host-tests && cargo test --locked --target x86_64-unknown-linux-gnu big_index -- --nocapture` | 60 000 станзов × 3 теста (`big_index_parse_packages_at_scale`, `big_index_through_gzip_decompress_stream`, `big_index_queries_and_resolver_at_scale`) | `test result: ok. 3 passed; 0 failed` | 8.9 s |
| 5 | host-property-сюита | `python3 tools/host_tests.py` | — | `test result: ok. 212 passed; 0 failed; 0 ignored` | 100.4 s |
| 6 | локальный apt по HTTP (mini_repo) | `python3 tools/e2e.py local-mirror --timeout 420 --serial-log serial_local_mirror.log` | mini_repo `hello-pagh` | `LXSELFTEST apt_e2e PASS (index 1 pkgs; installed 1; spawned hello-pagh pid=5)` + `hello from apt` + `LXSELFTEST https_get PASS`, **exit 0** | 38.6 s |
| 7 | живой Debian-индекс (доп.) | `python3 tools/e2e.py live-update --timeout 600 --allow-partial` | реальный `stable/main/amd64`, **68 825 пакетов** | `LIVE_APT_UPDATE: count=68825`, `Resident_Index_Footprint = 24165148 bytes`; parse-часть чистая (маркеры ниже = 0). Харнесс затем FAIL'ит на шаге запуска busybox — тестовая правка, к #14 не относится | 60.3 s |
| 8 | negative control | `python3 tools/e2e.py bigindex --stanzas 1 --timeout 300 --serial-log serial_bigindex_tiny.log` | 1 станз | **exit 2, verdict `REFUSED (sub-scale index)`**: pre-flight порог `BIGINDEX_MIN_STANZAS = 50_000` (обход для осознанных sub-scale контролей — `--allow-small-index`) | 247.9 s / 38.4 s |

Геометрия масштаба: историческая точка крэша из issue — **~5 459 пакетов / ~4 MiB**
decompressed. Прогон 1 её превышает ×11, прогон 3 — ×22, прогон 7 (живой индекс) — ×12.6.

## Нулевые маркеры (скан всех serial-логов)

Просканированы `serial_bigindex_60k.log`, `serial_bigindex_60k_inram.log`,
`serial_bigindex_120k.log`, `serial_bigindex.log` (прогон 3000), `serial_bigindex_tiny{,2}.log`,
`serial_local_mirror.log`, `serial_live.log`:

| Маркер | Вхождений |
|---|---|
| `EXC #14` | **0** |
| `WATCHDOG` | **0** |
| `PAGE FAULT` | **0** |
| `RIP=` | **0** |
| `panic` | **0** |
| `[PMM] DOUBLE FREE` | **0** |

Heap headroom на пике: 60k streaming — used 17 868 KiB / free 506 419 KiB; 120k streaming —
used 35 638 KiB / free 488 649 KiB; in-RAM на 12k — used ~9 106 KiB.

## Что доказывает, а что нет

- **Доказывает:** parse-путь (`StanzaParser::push_view` → `PackageIndexBuilder` на
  `good_memory_allocator`) детерминированно проходит 60k и 120k синтетических станзов и
  68 825 реальных пакетов Debian без #14 PF, без дабл-фри PMM и без паник; запас кучи
  >480 MiB на всех прогонах. Исходная для #14 сборка galloc'а присутствует (`Cargo.toml`,
  `src/memory/heap.rs`), т.е. репро гоняет именно тот аллокатор.
- **Не доказывает:** root cause исторического крэша — он не воспроизведён ни разу (это
  доказательство отсутствия на достигнутом масштабе, а не разбор причины).
- **Про `[WATCHDOG]`:** watchdog ловит только зависшие **Linux-совместимые syscall'ы**
  (`src/arch/x86_64/linux/mod.rs:540` под `compat_exists`-гейтом), а оба bigindex-варианта
  работают на kernel-thread. Отсутствие `[WATCHDOG]` здесь ожидаемо; детектор зависания в
  этих прогонах — таймаут E2E-драйвера. Для живого apt-пути (compat-процессы) `[WATCHDOG]`
  работает штатно.

## Ограничения

1. **In-RAM-вариант — фиксированные 12 000 станзов** (`const N_STANZAS = 12_000` в
   `src/selftest_lx.rs`): это 2.2× исторической точки крэша, но 0.2× масштаба 60k;
   сервер-индекс/`--stanzas` на вариант не влияют. Не называть его «полным масштабом».
2. **`live-update`** валится на последнем шаге (харнесс ждёт `/mnt/bin/busybox`, пакет
   ставится в `/mnt/usr/bin/busybox`) — тестовая правка, к #14 не относится; parse-часть
   (68 825 пакетов) проходит.
3. **Долговечность логов:** драйвер по умолчанию кладёт лог и JSON-сводку по имени *режима*
   (`serial_bigindex.log`, `.cache/e2e_bigindex_summary.json`), поэтому более поздние прогоны
   перезаписывают их. Для новых прогонов задавайте `--serial-log`/`--json` явно.

## Воспроизведение

```sh
python3 tools/e2e.py bigindex --stanzas 60000  --timeout 900 --serial-log serial_bigindex_60k.log
python3 tools/e2e.py bigindex --stanzas 60000 --in-ram --timeout 900 --serial-log serial_bigindex_60k_inram.log
python3 tools/e2e.py bigindex --stanzas 120000 --timeout 900 --serial-log serial_bigindex_120k.log
python3 tools/e2e.py bigindex --stanzas 1      --timeout 300 --serial-log serial_bigindex_tiny.log   # negative control
python3 tools/e2e.py local-mirror --timeout 420 --serial-log serial_local_mirror.log
cd host-tests && cargo test --locked --target x86_64-unknown-linux-gnu big_index -- --nocapture
python3 tools/host_tests.py
```

Ожидаемые строки-доказательства: `LXSELFTEST bigindex PASS (streaming; N packages)`,
`apt: index ready - N packages`; negative control обязан отказаться с кодом 2, а не пройти
на одном станзе.