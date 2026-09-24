# Issue #17 — `EXC #14` на parse-стадии apt-индекса: доказательство (закрыт)

**Статус:** верификация/доказательство (задача `t25`, kind=work). Прогон 2026-09-17,
HEAD `cd542bbceeda2a85183e6f5ad290e0fa62ce397f` («Merge pull request #38 …»).
Инструмент — `tools/e2e.py` (Linux-драйвер вместо `tools/e2e_bigindex.ps1`, см.
`tools/README.md`). Рабочее дерево во время прогонов параллельно правили другие
задачи, поэтому у каждого прогона указан sha256 загруженного ELF; **#14-путь
(`src/pkg/apt_index.rs`, `src/memory/{heap,pmm}.rs`, `src/pkg/deb.rs`) этими
правками не затронут** — `git status` по `src/pkg/` показывает только док-рефакторинг
`apt.rs` (issue #19 cleanup), а `src/selftest_lx.rs` диффа по bigindex-секции не имеет.

## Вердикт

**#17 закрыт: крэш `[EXC #14]` не воспроизводится ни на одном протестированном
масштабе и ни на одной из шести сборок.** Harness'ы остаются как постоянные
регресс-репро (`tools/e2e.py bigindex`, `host-tests/src/bigindex.rs`,
`lx_bigindex` + `lx_bigindex_inram`) — они дёшевы и являются единственным
детерминированным детектором этого класса.

## Прогоны

| # | Что | Команда | Индекс | ELF sha256 (нач.) | Результат | Время |
|---|---|---|---|---|---|---|
| 1 | streaming 60k | `python3 tools/e2e.py bigindex --stanzas 60000 --timeout 900 --serial-log serial_bigindex_60k.log` | `Packages` 22 949 891 B (21.89 MiB), `.gz` 1 702 056 B (1.62 MiB), 60 000 станзов | `931fa671250f4c29` | `apt: read package lists - 21.8 MiB / 60000 pkgs` → `apt: index ready - 60000 packages` → `LXSELFTEST bigindex PASS (streaming; 60000 packages)`, **exit 0** | 36.2 s |
| 2 | in-RAM (Test A) | `python3 tools/e2e.py bigindex --stanzas 60000 --in-ram --timeout 900 --serial-log serial_bigindex_60k_inram.log` | **фиксированные 12 000 станзов in-kernel** (см. ограничение §1.2) | `9cd3509ed2bf25cf` | `BIGINDEX variant=IN-RAM … 12000 stanzas IN-KERNEL` → `LXSELFTEST bigindex PASS (in-ram in-kernel; 12000 packages parsed)`, **exit 0** | 42.0 s |
| 3 | streaming 120k (запас по масштабу) | `python3 tools/e2e.py bigindex --stanzas 120000 --timeout 900 --serial-log serial_bigindex_120k.log` | `Packages` 45 929 891 B (43.80 MiB), `.gz` 3 403 681 B (3.25 MiB) | `77cbcf12b10954ab` | `apt: read package lists - 43.8 MiB / 120000 pkgs` → `index ready - 120000 packages` → `PASS (streaming; 120000 packages)`, **exit 0** | 55.2 s |
| 4 | host-репро | `cd host-tests && cargo test --locked --target x86_64-unknown-linux-gnu big_index -- --nocapture` | 60 000 станзов × 3 теста (`big_index_parse_packages_at_scale`, `big_index_through_gzip_decompress_stream`, `big_index_queries_and_resolver_at_scale`) | — | `test result: ok. 3 passed; 0 failed` | 8.9 s |
| 5 | host-property-сюита | `python3 tools/host_tests.py` | — | — | `test result: ok. 212 passed; 0 failed; 0 ignored` | 100.4 s |
| 6 | локальный apt по HTTP (mini_repo) | `python3 tools/e2e.py local-mirror --timeout 420 --serial-log serial_e2e_t25.log` | mini_repo `hello-pagh` | `f894272a6c467c01` | `LXSELFTEST apt_e2e PASS (index 1 pkgs; installed 1; spawned hello-pagh pid=5)` + `hello from apt` + `LXSELFTEST https_get PASS`, **exit 0** | 38.6 s |
| 7 | живой Debian-индекс (доп., прогон `t1`) | `python3 tools/e2e.py live-update --timeout 600 --allow-partial` | реальный `stable/main/amd64`, **68 825 пакетов** | `629cfae07372e0c5` | `LIVE_APT_UPDATE: count=68825`, `Resident_Index_Footprint = 24165148 bytes`; parse-часть чистая (маркеры §«Нулевые маркеры» = 0). Харнесс затем FAIL'ит на шаге запуска busybox — тестовая правка, к #14 не относится (§1.3) | 60.3 s |
| 8 | negative control | `python3 tools/e2e.py bigindex --stanzas 1 --timeout 300 --serial-log serial_bigindex_tiny.log` | 1 станз | `a1e0c1308d1ede66` | `LXSELFTEST bigindex PASS (streaming; 1 packages)`, **exit 0** — гейт не проверяет масштаб (§1.1) | 247.9 s / 38.4 s |

Геометрия масштаба: историческая точка крэша из issue — **~5 459 пакетов / ~4 MiB**
decompressed. Прогон 1 её превышает ×11, прогон 3 — ×22, прогон 7 (живой индекс) — ×12.6.

## Нулевые маркеры (скан всех serial-логов)

`serial_bigindex_60k.log`, `serial_bigindex_60k_inram.log`, `serial_bigindex_120k.log`,
`serial_bigindex.log` (прогон 3000 из `t1`), `serial_bigindex_tiny{,2}.log`,
`serial_e2e_t25.log`, `serial_live.log`:

| Маркер | Вхождений |
|---|---|
| `EXC #14` | **0** |
| `WATCHDOG` | **0** |
| `PAGE FAULT` | **0** |
| `RIP=` | **0** |
| `panic` | **0** |
| `[PMM] DOUBLE FREE` | **0** |

Heap headroom на пике: 60k streaming — used 17 868 KiB / free 506 419 KiB;
120k streaming — used 35 638 KiB / free 488 649 KiB; in-RAM на 12k — used ~9 106 KiB.

## Что доказывает, а что нет

- **Доказывает:** parse-путь (`StanzaParser::push_view` → `PackageIndexBuilder` на
  `good_memory_allocator`) детерминированно проходит 60k и 120k синтетических станзов
  и 68 825 реальных пакетов Debian без #14 PF, без дабл-фри PMM и без паник; запас
  кучи >480 MiB на всех прогонах. Исходная для #14 сборка galloc'а присутствует
  (`Cargo.toml`, `src/memory/heap.rs`), т.е. репро гоняет именно тот аллокатор,
  после которого требовалось подтверждение.
- **Не доказывает:** root cause исторического крэша — он не воспроизведён ни разу
  (это доказательство отсутствия на достигнутом масштабе, а не разбор причины).
- **Про `[WATCHDOG]`:** watchdog ловит только зависшие **Linux-совместимые syscall'ы**
  (`src/arch/x86_64/linux/mod.rs:540` под `compat_exists`-гейтом), а оба bigindex-варианта
  работают на kernel-thread. Отсутствие `[WATCHDOG]` здесь ожидаемо и не является
  доказательством отсутствия зависания: детектор зависания в этих прогонах — таймаут
  E2E-драйвера. Для живого apt-пути (compat-процессы) `[WATCHDOG]` работает штатно.

## Ограничения и рекомендации (не блокируют закрытие)

1. **Гейт слеп к масштабу.** `bigindex_streaming` печатает PASS для любого `Ok(n)`:
   1-станзовый индекс даёт `PASS (streaming; 1 packages)` и exit 0 (проверено дважды,
   прогон 8). Рекомендация: проверять `packages parsed == --stanzas` в
   `tools/e2e.py` (метрика `packages parsed` уже пишется в `.cache/e2e_*_summary.json`)
   либо добавить минимальный порог в харнесс.
2. **`lx_bigindex_inram` — фиксированные 12 000 станзов** (`const N_STANZAS = 12_000`
   в `src/selftest_lx.rs`): это 2.2× исторической точки крэша, но 0.2× масштаба 60k,
   и серв-индекс/`--stanzas` на вариант не влияют. В текстах не называть его «полным
   масштабом»; при желании поднять константу (doc-comment объясняет, почему 60k
   in-kernel генерация под TCG признана нецелесообразной).
3. **live-update** (`t1`) валится на последнем шаге (харнесс ждёт `/mnt/bin/busybox`,
   пакет ставится в `/mnt/usr/bin/busybox`) — тестовая правка, к #14 не относится;
   parse-часть (68 825 пакетов) проходит.

## Воспроизведение

```sh
python3 tools/e2e.py bigindex --stanzas 60000  --timeout 900 --serial-log serial_bigindex_60k.log
python3 tools/e2e.py bigindex --stanzas 60000 --in-ram --timeout 900 --serial-log serial_bigindex_60k_inram.log
python3 tools/e2e.py bigindex --stanzas 120000 --timeout 900 --serial-log serial_bigindex_120k.log
python3 tools/e2e.py bigindex --stanzas 1      --timeout 300 --serial-log serial_bigindex_tiny.log   # negative control
python3 tools/e2e.py local-mirror --timeout 420 --serial-log serial_e2e_t25.log
cd host-tests && cargo test --locked --target x86_64-unknown-linux-gnu big_index -- --nocapture
python3 tools/host_tests.py
```

Сырые логи: `serial_bigindex_{60k,60k_inram,120k,tiny,tiny2}.log`, `serial_e2e_t25.log`,
`serial_live.log` (корень репо, git-ignored) и их копии + JSON-сводки драйвера в
`.cache/t25_evidence/` (там же `t25_host_bigindex.log`, `t25_host_full.log`).

> **Примечание о долговечности ссылок (t26):** драйвер по умолчанию кладёт лог и
> JSON-сводку по имени *режима* (`serial_bigindex.log`,
> `.cache/e2e_bigindex_summary.json`), поэтому более поздние прогоны перезаписывают
> их (сейчас там 1-станзовый контроль и ERROR-запись). Запись о прогоне 3000
> сохранилась только в архивной сводке `.cache/t25_evidence/e2e_bigindex_summary.json`
> (PASS, `streaming; 3000 packages`, 38.1 s). Для новых прогонов задавайте
> `--serial-log`/`--json` явно.

## Независимая верификация (t26, 2026-09-17)

Проверка выполнена заново, без опоры на логи исполнителя: индексы
перегенерированы детерминированно, прогон сделан на текущем (компилируемом)
дереве со своим serial-логом, а размер/хеш отдаваемого зеркала снят
одновременным пробником — файл на диске плюс HTTP-выборка той же URL с хоста.

| Проверка | Результат |
|---|---|
| Индекс 60k перегенерирован | `Packages` 22 949 891 B, `.gz` 1 702 056 B, `grep -c '^Package: '` = **60 000** — совпадает с заявленным байт-в-байт |
| Индекс 120k перегенерирован | 45 929 891 B / 3 403 681 B / **120 000** станзов — совпадает |
| Что реально отдавалось зеркалу | on-disk == HTTP == 1 702 056 B, sha256 `49e5aee5…` == хеш моего регена (`.cache/t26_evidence/`) |
| Мой живой прогон 60k | ELF `2f18fb305f2f0c9f`, 33.2 s, exit 0: `variant=STREAMING` → `apt: fetched 1.6 MiB` → `read package lists - 21.8 MiB / 60000 pkgs` → `index ready - 60000 packages` → `PASS (streaming; 60000 packages)`; heap-строки байт-в-байт как у исполнителя |
| Маркеры в моём логе | `[EXC #14]` 0, `[WATCHDOG]` 0, `PAGE FAULT` 0, `RIP=` 0, `panic` 0, `[PMM] DOUBLE FREE` 0 |
| Мой живой прогон in-RAM | ELF `89fea46666ca9913`, 51.2 s, exit 0, `parsed=12000` |
| Скан логов исполнителя (7 архивных) | все шесть маркеров = 0 — его заявление подтверждается независимо |
| Заявленные размеры/строки/хеши | найдены в его логах и JSON-сводках; хеши ELF в доке совпадают со сводками; `index ready - 60000/120000` на месте (parse-стадия действительно достигнута) |
| Живой Debian-индекс | `LIVE_APT_UPDATE: count=68825` и `Resident_Index_Footprint = 24165148 bytes` присутствуют в архивном `serial_live.log` |
| host-репро в CI-гейте | `python3 tools/host_tests.py` (запускать из корня репо!) → `223 passed; 0 failed`, **в выводе присутствуют все три `bigindex::… ok`**; CI (`ci.yml`) гоняет именно `host_tests.py` → репро исполняется, а не просто существует |
| Фильтр `cargo test big_index` | 3 passed (60k станзов ×3 теста) |

**Вердикт верификации: подтверждаю.** Блокирующих расхождений нет; вывод «#17
закрыт» не опирается на прогон мимо parse-стадии (достигнут полный счёт
60000/120000/12000, размер индекса подтверждён независимо).

### Гейт масштаба (исправлен капитаном после t25) — проверено живьём

- `python3 tools/e2e.py bigindex --stanzas 1` → **exit 2, verdict `REFUSED
  (sub-scale index)`**: срабатывает *pre-flight* порог `BIGINDEX_MIN_STANZAS = 50_000`
  (сообщение объясняет точку крэша ~5459 станзов; QEMU не поднимается). Это
  сильнее, чем «exit 1 с проваленной проверкой», которое ожидалось в задаче
  верификации — фиксируйте фактический код **2** (обход для осознанных
  sub-scale контролей — `--allow-small-index`).
- `python3 tools/e2e.py bigindex --stanzas 60000` → exit 0, среди проверок
  `PASS parsed scale >= 60000 packages (--stanzas 60000): parsed=60000`.
- `python3 tools/e2e.py bigindex --stanzas 60000 --in-ram` → exit 0,
  `PASS parsed scale >= 12000 packages (in-RAM variant (fixed 12000 stanzas))`.
- Остаточная слабость (не блокер): `parsed` считается как `max()` по всем
  `(\d+) packages` во всём serial-логе; надёжнее якорить на терминальный маркер
  `LXSELFTEST bigindex PASS (…; N packages)` / `apt: index ready - N packages`,
  чтобы счёт нельзя было замаскировать строкой другого харнесса.

Артефакты верификации: `serial_t26_60k.log`, `serial_t26_inram.log`,
`serial_t26_tiny.log`, `.cache/t26_*_summary.json`, пробник и сырые выводы —
`.cache/t26_evidence/`.
