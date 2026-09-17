# Issue #16 — предсказуемый `AT_RANDOM`-фоллбэк (xorshift) устранён: доказательство

**Статус:** верификация/устранение (задача `t11`, kind=work). База — HEAD
`cd542bbceeda2a85183e6f5ad290e0fa62ce397f` («Merge pull request #38 …»), ветка
`arch/at-random-entropy`, изолированный worktree `/home/oleg/t11-attrandom`
(собран из main, чужих правок внутри нет). У каждого прогона — свой sha256
загруженного ELF.

## Вердикт

**Предсказуемость устранена, деградация объявлена, `-cpu max` не изменился.**

1. Фоллбэк больше не является линейным потоком по наблюдаемым величинам: блок
   выводится из **смешанного boot-seed** (SHA-256-транскрипт независимых
   источников + счётчик + наблюдаемые входы). Проверяется host-свойствами P51 —
   в том числе **исполняемыми отрицательными контролями**: те же свойства обязаны
   (и это проверено тестом) отвергнуть старый xorshift, константный микшер и
   микшер с выброшенным входом.
2. Деградация **явная**: при отсутствии RDSEED/RDRAND загрузка печатает
   `entropy: stage=degraded cause=NoHardwareEntropy cpu=…`, а in-QEMU проверка
   печатает `SELFTEST at_random: … distinct=64 … seed_fp=… entropy=none …`.
3. `-cpu max` (RDSEED/RDRAND) идёт прежним путём: предупреждения нет
   (0 вхождений в логе), `entropy=rdseed+rdrand`, `seed_fp=n/a (hardware entropy
   in use)` — boot-seed в этом режиме вообще не строится.
4. Паники/отказа стартовать процесс нет ни в одном режиме: `LXSELFTEST`-харнессы
   и `selftest` проходят на обоих CPU.

## Что было не так

`misc::random_bytes_16()` (ELF `AT_RANDOM` — ключи stack-canary и pointer-mangling
для glibc) при отсутствии аппаратной энтропии отдавал 64-битный xorshift:

```rust
x = FIXED ^ ticks*K ^ rtc.rotate_left(32) ^ (pid << 48) ^ secure_u64().unwrap_or(0)
x ^= x << 13; x ^= x >> 7; x ^= x << 17;  second = x * K2
```

Все слагаемые — публичные (время загрузки, тик-часы, pid) либо константа:
пятое слагаемое на этом пути **всегда** ноль (аппаратной энтропии нет по условию —
именно поэтому мы в фоллбэке). Это не только комментарий, но и измерение:
независимая модель pasha-vfs (`verify/atrandom-baseline`, `d23f735`) побайтово
воспроизвела все 8 блоков, а перебор по `ticks` давал seed за 1 попытку при окне
±0, за 3 при ±1 и за 11 при ±5 (точность `/proc/uptime`); состояние глобальное и
персистентное, так что одного наблюдения хватало на всю дальнейшую
последовательность. На `-cpu max` фоллбэк не срабатывает (hw=rdseed+rdrand), и
модель блоки не воспроизводит — дефект живёт ровно на конфигурациях без
RDRAND/RDSEED (в частности QEMU `-cpu qemu64`).

Заодно исправлен дефект документации, найденный pasha-vfs: доккомментарий
утверждал «…the current pid, **and a free-running process-lifetime counter**» и
«so the block NEVER degrades to all-zero bytes». Никакого счётчика жизни процесса
в коде не было, а ненулевизна не гарантировалась (ноль достижим при конкретных
тиках; наблюдаемое значение — `9539086926640840705`). Текст переписан по факту.

## Что сделано

| Файл | Что |
|---|---|
| `src/security/seed.rs` (новый) | **Чистый** микшер (core + `sha2`, без I/O и глобалов): `SeedPool` (SHA-256-транскрипт с метками и длинами), `Observables`, `derive_bytes` — тот же исходник гоняют host-свойства P51 |
| `src/security/entropy.rs` | `collect_boot_seed()` (джиттер TSC, гонки/латентность чтений CMOS RTC, счётчики ядра: тики, current/next pid, кадры PMM, куча; layout: CR3, адреса объектов), `mixed_fill()` (boot-seed + монотонный счётчик + наблюдаемые), `report_capabilities()` (boot-probe), `boot_seed_fingerprint()` (односторонний отпечаток seed для доказательств), предупреждение `stage=degraded` ровно один раз. Плюс убраны 4 лишних `unsafe`-блока (интринсики на текущем тулчейне safe) |
| `src/arch/x86_64/linux/misc.rs` | `random_bytes_16()`: HW-путь без изменений; фоллбэк — `mixed_fill`; доккомментарий переписан по факту (см. выше) |
| `src/boot.rs` | boot-probe `report_capabilities()` в начале `kernel_main` — деградация видна ДО старта первого процесса; при наличии HW-энтропии — no-op |
| `src/test.rs` | новая in-QEMU проверка `at_random`: 64 блока, не повторы/не вырожденные, печатает `digest` и `seed_fp` |
| `host-tests/src/lib.rs`, `properties/p51.rs` (новый) | 10 host-свойств: 6 на поставляемый микшер + 3 отрицательных контроля + контракт транскрипта |
| `src/security/README.md`, `SECURITY.md`, `HARDENING.md`, `README.md`, `tools/README.md` | документация: механизм, честная граница (best-effort, НЕ CSPRNG), маркеры, как гонять `-cpu qemu64` |

## Ответы на критерии верификатора (по каждому свойству: a/b/c/d/e)

(a) что варьируется на входе; (b) падает ли на вырождении в константу; (c) падает
ли при выброшенном входе (например ticks); (d) связь вход→выход или форма;
(e) отрицательный контроль на СТАРОМ микшере. Все (b)/(c)/(e) — **исполняемые
тесты**, а не утверждения.

| Свойство (P51) | (a) вход | (b) константа | (c) выброшенный вход | (d) связь | (e) старый микшер |
|---|---|---|---|---|---|
| `..._is_deterministic` | ничего (два одинаковых вызова) | **проходит** (осознанно: это проверка «это функция», не security-свойство) | проходит | да (равенство двух прогонов) | проходит (старый тоже функция) — задокументировано |
| `..._avalanches` | 1 бит секрета/счётчика/каждого наблюдаемого (512 флипов на кейс), порог: каждый флип ∈ [24,104] из 128 бит, среднее ∈ [58,70] | **падает** (расстояние 0) | проходит (выброшенный вход ловит следующее свойство) | да, вход-бит → выход-биты | **падает**: линейная схема даёт 1–2 бита на флип |
| `..._separates_secrets` | секрет при ФИКСИРОВАННЫХ публичных входах (64 секрета) | **падает** (все выходы равны) | проходит | да, секрет → различные блоки | **падает**: секрета нет, выходы схлопываются |
| `..._depends_on_every_single_input` | ТОЛЬКО ticks (256 значений), затем ТОЛЬКО pid | **падает** (один и тот же блок) | **падает** — проверено двумя сборками микшера в тесте (`ticks_ignored`, `pid_ignored`) | да | проходит (старый зависит от ticks/pid) — поэтому его ловят 2/3/5, а не это |
| `..._resists_the_public_input_attack` | модель атаки: все публичные входы известны точно, перебираются секреты (4 догадки) | **падает** (догадка воспроизводит константу) | проходит | да, модель «атака по публичным входам» | **падает**: первая же догадка воспроизводит блок; тест дополнительно доказывает `real == model` для старой схемы |
| `..._never_repeats_a_block` | счётчик (1024 значения) при одинаковых наблюдаемых — модель «два процесса с одинаковыми публичными входами» | **падает** | — | да | **падает** (в удалённом коде счётчика не было) |
| `p51_seed_pool_...` | транскрипт: порядок и метки | — | — | да (одинаковый транскрипт → одинаковый seed; изменённый порядок/метка → другой) | — |

Чего в P51 **нет** осознанно: проверок формы («длина 16», «не все нули»,
«два вызова различаются» как единственные). Ненулевизна старого кода была удачей,
а не инвариантом, и в новой схеме она тоже не гарантируется хешем — поэтому она
не объявлена свойством.

## Доказательства прогонов

### Гейты (ветка `arch/at-random-entropy`)

| # | Команда | Результат |
|---|---|---|
| 1 | `cargo build` | rc=0; `pagh (lib)` **33** предупреждения (база 38: −4 лишних `unsafe` в `src/security/entropy.rs`, −1 неиспользуемый импорт в `misc.rs`), 0 ошибок |
| 2 | `python3 tools/build.py build --release` | rc=0, rust-lld линкует `pagh.elf` |
| 3 | `cargo fmt --all -- --check` (только в своём worktree, read-only) | пустой вывод, rc=0 |
| 4 | `python3 tools/check_safety.py` | `safety policy: OK (**7** critical files)` — седьмой файл это новый `src/security/seed.rs` |
| 5 | `python3 tools/host_tests.py` | `test result: ok. **222** passed; 0 failed` (база 212 + 10 свойств P51), 162 s |

Конкретизация (b)/(c)/(e) в выводе сьюта:
`p51_negative_control_old_xorshift_is_rejected`, `p51_negative_control_constant_mixer_is_rejected`,
`p51_negative_control_dropped_input_is_rejected` — все `ok`.

### In-QEMU (три прогона одного и того же ELF, отличается только `-cpu`)

ELF sha256 **`e395e8237f6faa4c7b0843ad5ee71452c061be80ead2b652cf9899c43a61adda`** во всех трёх.

| Прогон | Команда | Ключевые строки | Время |
|---|---|---|---|
| `-cpu qemu64` #1 | `python3 tools/e2e.py selftest --cpu qemu64 --timeout 600 --serial-log serial_t11_qemu64_run1.log` | `[WARN] entropy: stage=degraded cause=NoHardwareEntropy cpu=none (boot QEMU with -cpu max …)` (строка 34, ДО селфтеста) → `SELFTEST at_random: blocks=64 distinct=64 digest=9551a44126ca508c seed_fp=a5ce4f8e9bbe0e20 entropy=none …` → `SELFTEST SUMMARY: 61 routines, 0 failed checks`, exit 0 | 42.7 s |
| `-cpu qemu64` #2 | то же, `--serial-log serial_t11_qemu64_run2.log` | то же предупреждение → `… distinct=64 digest=f04be4b3397bc647 **seed_fp=a77af24ff20cdab4** …`, `61 routines, 0 failed`, exit 0 | 28.2 s |
| `-cpu max` | `python3 tools/e2e.py selftest --cpu max --serial-log serial_t11_qemu_max.log` | `stage=degraded` — **0 вхождений**; `… distinct=64 digest=e3b6084327aac537 seed_fp=n/a (hardware entropy in use) entropy=rdseed+rdrand`, `61 routines, 0 failed`, exit 0 | 25.2 s |
| база (до правки) | `python3 tools/e2e.py selftest --cpu qemu64` на чистом HEAD | ELF `059f718eaadba097…`, `SELFTEST SUMMARY: 60 routines, 0 failed checks`, exit 0 — регрессии по числу/зелёности рутин нет | 86.2 s |

Что именно доказывают эти три строки:

* **`seed_fp` различается у двух qemu64-прогонов** (`a5ce4f8e…` vs `a77af24f…`).
  Отпечаток зависит ТОЛЬКО от boot-seed (ни счётчик, ни наблюдаемые в него не
  входят), значит на одной и той же VM-конфигурации seed РАЗНЫЙ — фоллбэк не
  сводится к функции конфигурации/времени загрузки. Именно этого не мог показать
  `digest` (он подмешивает тики, которые и так различаются между прогонами).
* **`digest` различается** — блоки зависят от наблюдаемых и счётчика.
* **на `-cpu max` предупреждения нет** и `entropy=rdseed+rdrand`: аппаратный путь
  не тронут, boot-seed не строится (`seed_fp=n/a`).
* 64 блока, все различные, на обоих CPU: вырождение в константу/повтор ловится
  in-QEMU проверкой, а не только host-свойствами.

**Честные границы** (повторены в `SECURITY.md`): это best-effort, НЕ CSPRNG.
Секретность даёт boot-seed (тайминговый джиттер + состояние машины); его энтропия
*заявлена*, а не доказана. На идеально детерминированном replay seed
воспроизводим — поэтому деградация объявляется в логе, `sys_getrandom` остаётся
fail-closed (`EAGAIN`) и никогда не отдаёт эти байты, а `-cpu max` остаётся
рекомендуемой конфигурацией.

## Воспроизведение

```sh
# host-свойства микшера (+ отрицательные контроли)
python3 tools/host_tests.py                       # или: cd host-tests && cargo test p51

# путь БЕЗ аппаратной энтропии (там жил дефект)
python3 tools/e2e.py selftest --cpu qemu64 --timeout 600 --serial-log serial_t11_qemu64_run1.log

# аппаратный путь: предупреждения быть не должно
python3 tools/e2e.py selftest --cpu max --timeout 600 --serial-log serial_t11_qemu_max.log
```

Ожидаемые маркеры: `entropy: stage=degraded cause=NoHardwareEntropy cpu=none …`,
`SELFTEST at_random: blocks=64 distinct=64 digest=… seed_fp=… entropy=…`,
`SELFTEST SUMMARY: 61 routines, 0 failed checks`.

## Изменённые файлы

`src/security/seed.rs` (новый), `src/security/entropy.rs`, `src/security/mod.rs`,
`src/security/README.md`, `src/arch/x86_64/linux/misc.rs`, `src/boot.rs`,
`src/test.rs`, `host-tests/src/lib.rs`, `host-tests/src/properties/p51.rs` (новый),
`SECURITY.md`, `HARDENING.md`, `README.md`, `tools/README.md`.
