# Верификация issue #16 / t11 — независимый отчёт

**Проверяемая ветка:** `arch/at-random-entropy` = `bd4e06f97b44cff138a089bacbb36906df3a63fc`
(от `main` `cd542bb`), worktree исполнителя `/home/oleg/t11-attrandom`.
**Верификатор:** pasha-vfs (не автор t11). Ветку исполнителя не менял.
**Мои worktree:** `/home/oleg/pagh-wt-atrandom` (деточ at `bd4e06f`, прогоны гейтов и QEMU),
`/home/oleg/pagh-wt-atrandom-probe` (ветка `verify/atrandom-probe`: инструментальная проба,
в мерж не годится).
**База «до»:** `verify/atrandom-baseline` = `d23f735` (`docs/atrandom-baseline.md`).

## Вердикт: PASS, findings — только документация (1 medium + 3 low), блокеров нет

Все функциональные утверждения t11 подтверждены независимо. Ни одно не опирается на
инструменты исполнителя: гейты, три QEMU-прогона и атаки выполнены из моих worktree.

## Артефакты

| Что | Значение |
|---|---|
| `cargo build` | rc=0, **33** предупреждения (инвентарный дифф ниже) |
| `python3 tools/build.py build` | rc=0, linked debug ELF |
| `python3 tools/build.py build --release` | rc=0, linked release ELF |
| `cargo fmt --all -- --check` (только в моём worktree, read-only) | rc=0, пустой вывод |
| `python3 tools/check_safety.py` | `OK (7 critical files)` — 7-й файл `src/security/seed.rs` попал под гейт автоматически: в списке `critical` стоит **каталог** `src/security`, а не перечень файлов. В `seed.rs` `unsafe` нет вообще (0 вхождений), реальные SAFETY-ноты на месте в `entropy.rs` (`_rdtsc`, `rdseed_word`, `rdrand_word`) |
| `python3 tools/host_tests.py` | **222 passed / 0 failed** (база 212 + 10 свойств P51) |
| ELF sha256 (мой путь сборки) | `04736ef8b4aefa938777528079f29439596709917ae5b8d736566631c15df3f3` |
| ELF sha256 (артефакт исполнителя) | `e395e8237f6faa4c7b0843ad5ee71452c061be80ead2b652cf9899c43a61adda` — **совпадает с заявленным**; отличается от моего только потому, что ядро не бит-репродуцируемо между каталогами сборки (debug-info хранит пути). Для masha-qa: сверять sha нужно со своего сборочного каталога, а не с числом из доки |

### In-QEMU, ELF исполнителя (3 прогона)

| Прогон | Маркер | `stage=degraded` | `SELFTEST SUMMARY` |
|---|---|---|---|
| `--cpu qemu64` #1 | `blocks=64 distinct=64 digest=170406b4172ebbb2 seed_fp=f021ef29380bf7ab entropy=none` | 1 | 61 routines, 0 failed |
| `--cpu qemu64` #2 | `blocks=64 distinct=64 digest=a8cf42d6d099a10c seed_fp=be1769e898cee7ac entropy=none` | 1 | 61 routines, 0 failed |
| `--cpu max` | `blocks=64 distinct=64 digest=f1225f4039a60ef1 seed_fp=n/a (hardware entropy in use) entropy=rdseed+rdrand` | **0** | 61 routines, 0 failed |

Логи: `/tmp/atrandom/after-{qemu64-run1,qemu64-run2,max-run1}.log`.

### Проба на моей ветке `verify/atrandom-probe` (6 прогонов)

Инструментирует `collect_boot_seed` (печатает каждый поглощённый терм и seed; арифметику не
меняет) и `mixed_fill` (`[atblk]` — вход→блок). Логи: `/tmp/atrandom/probe-boot{1..5}.log`,
`probe-max.log`.

* `-cpu max`: **0** строк `[atseed]`/`[atblk]` → boot-seed в аппаратном режиме действительно
  не строится (заявление исполнителя подтверждено моим инструментом, а не его).
* `-cpu qemu64` ×5: seed_RUST == seed_МОДЕЛЬ во всех 5 (транскрипт воспроизведён побайтово
  независимой реализацией) и реальный блок `5936c98c65fd9510fd83087f4370eb7e` воспроизведён
  из залогированных входов.

## Инвентарный дифф предупреждений против базы t2

Исчезли ровно 5 предупреждений (38 → 33), состав по инвентарю
(`/home/oleg/Pagh-OS/.cache/t2_baseline/cargo-build.log` → мой `cargo build`):

1–4. `unnecessary unsafe block` ×4, `src/security/entropy.rs` — законно: в этом тулчейне
`__cpuid`/`__cpuid_count` **safe** (проверено и капитаном, и мной), а `_rdseed64_step`/
`_rdrand64_step` вызываются внутри `#[target_feature(enable = …)]`-функций, где их
предусловие уже выполнено (из обычного контекста тот же вызов — E0133, что и подтверждает
осмысленность оставшихся `unsafe fn`).
5. **`function stats is never used`, `src/memory/heap.rs`** — исчезло потому, что новый
`collect_boot_seed` теперь вызывает `heap::stats()`.

В базе **не было** предупреждения об неиспользуемом импорте `Spinlock` в `misc.rs`
(в старом коде `Spinlock` использовался fallback-состоянием). То есть удаление импорта —
профилактика, а не исчезнувшее предупреждение (см. finding F2).

## Проверка свойств P51 на тавтологичность (мои мутанты, не контрольные функции исполнителя)

Мутировал `src/security/seed.rs` в СВОЁМ worktree и запускал `cargo test p51`:

| Мутация | Поймали свойства |
|---|---|
| секрет выброшен из `derive32` (`let _ = seed`) | `p51_shipped_separates_secrets`, `..._resists_the_public_input_attack`, `..._avalanches` — FAILED |
| вход `ticks` выброшен | `p51_shipped_depends_on_every_single_input`, `..._avalanches` — FAILED |
| (после теста мутации откатил: `git diff` по `seed.rs` пуст) | 7/10 и 8/10 соответственно — остальные проходят по документации: `is_deterministic` и `never_repeats` и не должны ловить эти классы |

Вывод: свойства **исполняемы и кусаются** независимо от того, что отрицательные контроли
написаны тем же автором. «Длина 16 / не все нули / два вызова различаются» как единственные
свойства не используются — проверено чтением `p51.rs` (394 строки).

## Ответы на критические углы капитана

### 1. Попытка ПРЕДСКАЗАТЬ новый блок (а не только опровергнуть старый)

Сценарий атакующего: известны все публичные входы (`ticks` ±5, `rtc` ±1 c, `pid`), известны
даже фиксированные в этой конфигурации термы (`cr3`, `addr.pool`, `pid.next`, PMM/heap —
именно они в моих 5 прогонах **не менялись**: 0 бит), неизвестны тайминговые
(`tsc.start`, `tsc.jitter`, `tsc.iters`, 4 латентности чтения RTC).

* Перебор 4 752 структурированных кандидатов (iters × латентности × ticks × rtc × pid,
  включая подстановку джиттера другой загрузки) — **ни одного совпадения**.
* Чувствительность: сдвиг на 1 любого таймингового терма (или даже константы `cr3`)
  полностью меняет seed и блок — значит предсказание требует бит-точного знания джиттера.
* Что именно ломает модель: `tsc.jitter` — XOR-свёртка ~2 000–2 300 подряд идущих RDTSC-дельт
  внутри ядра. Под QEMU/TCG это хостовое планировочное дрожание; гость его не наблюдает
  (даже co-located процесс видит только границы окна, но не дельты внутри), и без него
  транскрипт не сходится.
* **Оценка энтропии (честная граница):** по 5 прогонам различаются `tsc.start` (5/5),
  `jitter` (5/5), `iters` (1965…2330, разброс 365 ≈ **8.5 бит**), латентности RTC
  (~39–59 тыс. циклов, 4 сэмпла ≈ десятки бит суммарно); шесть термов были **константны**
  во всех 5 прогонах (0 бит: `cr3`, `addr.pool`, `pid.next`, `pmm.free=0`, `pmm.total`,
  heap-тройка). Итого: непредсказуемость опирается только на тайминги, наблюдаемая
  вариативность ≈ десятки бит, но **доказать** её 5 прогонами нельзя — ровно как написано
  у исполнителя («энтропия заявлена, а не доказана»). Это не ложное утверждение, а честная
  граница, и она подтверждена моим измерением, а не пересказана.

### 2. Про `seed_fp` — что он доказывает, а что нет

Код (`boot_seed_fingerprint`) читал: `SHA-256(DOMAIN ‖ "fingerprint" ‖ seed)[:8]`, ни счётчик,
ни наблюдаемые в него не входят — **структурно верно**, и мой пересчёт отпечатка из
залогированного seed совпал (`b8e976fb…`). Но:

* `seed_fp` доказывает, что **seed'ы двух загрузок разные** — это необходимое условие, а не
  достаточное: если бы seed был чистой функцией `ticks`/`rtc`/`pid` (как в старом коде),
  `seed_fp` тоже различался бы между загрузками, потому что эти входы различаются.
  Достаточность даёт другой аргумент — состав транскрипта (тайминговые термы, недоступные
  наблюдателю) и неудача перебора по публичным входам (мой §«Попытка предсказать»).
  → в доке исполнителя это место сформулировано сильнее, чем позволяет доказательство (F1).
* `digest` смешивает 64 блока, то есть наблюдаемые и счётчик; его изменение не отличает
  «другой seed» от «другие тики» — исполнитель это сам оговаривает, и это верно.

### 3. Диагностический `seed_fp` в serial

* Он печатается **только** из selftest-рутины (`src/test.rs:4216` — единственный вызов
  `boot_seed_fingerprint()`), в обычной загрузке его нет: 8 байт SHA-256 не дают seed
  (preimage), а сам отпечаток не попадает в нормальный лог. Для внешнего наблюдателя это
  не утечка seed.
* Но это **оракул подтверждения**: если энтропия seed'а мала, перебор кандидатов можно
  проверять по напечатанному отпечатку, не имея ни одного AT_RANDOM-блока. Риск отпечатка
  ровно равен энтропии seed'а; поскольку он печатается только в selftest (уже доверенный
  канал) и fallback задокументирован как best-effort, считаю приемлемым. Рекомендация —
  одна строка в доке о том, что `seed_fp` — оракул, а не «безопасный» маркер.

## Findings

| id | severity | файл:строка | проблема | requiredFix |
|---|---|---|---|---|
| F1 | medium | `docs/issue-16-at-random-entropy.md:118-122` | Буллет «seed_fp различается → фоллбэк не сводится к функции конфигурации/времени загрузки» делает вывод сильнее посылки: seed_fp доказывает лишь различие seed'ов (необходимое условие); при seed = f(ticks, rtc, pid) отпечаток тоже различался бы | Переформулировать: «разные seed'ы на одной конфигурации (необходимое условие); невыводимость из публичных входов доказывает состав транскрипта + неудача перебора, см. §…» |
| F2 | low | `docs/issue-16-at-random-entropy.md:95` (и тело коммита) | Пятое исчезнувшее предупреждение приписано «−1 неиспользуемый импорт в misc.rs»: инвентарь базы t2 такого предупреждения не содержит (в старом коде `Spinlock` был нужен). Реально исчезло `function stats is never used` (`src/memory/heap.rs`) — его теперь вызывает `collect_boot_seed`; удаление импорта лишь предотвратило новое предупреждение | Исправить атрибуцию в доке: 4 × unnecessary unsafe + `heap::stats` стал используемым; про импорт — «убран, чтобы не появилось новое» |
| F3 | medium | `SECURITY.md` (новый абзац про `AT_RANDOM`) | «…SHA-256 over TSC timing jitter, CMOS RTC port-read races, kernel counters and address-space layout) plus a per-process counter, **none of which is derivable from the observable inputs** (tick clock, RTC wall clock, pid)» — для «kernel counters» это неверно: `ticks` и есть тот самый tick clock, pid наблюдаем, PMM/heap выводимы/ограничены. Секретность даёт только тайминговая часть (и это же верно сформулировано в `src/security/README.md`) | Сузить фразу до тайминговых источников: «the timing sources are not derivable from …; the counters are mixed in for uniqueness on a deterministic platform» |
| F4 | low | `docs/issue-16-at-random-entropy.md:60`, `src/security/README.md` (раздел про источники) | Layout/счётчики перечислены как источники энтропии без оговорки: измерено, что `cr3`, `addr_pool`, `pid.next`, PMM и heap **не менялись ни в одной из 5 загрузок** (0 бит в этой конфигурации) | Добавить оговорку: «поглощаются, но в детерминированной конфигурации (QEMU) константны и энтропии не дают; непредсказуемость несут тайминговые термы» |

Блокеров нет: ни одно функциональное утверждение не опровергнуто, `sys_getrandom`/TLS-RNG
не тронуты (`fill`/`secure_u64` без изменений, `net/tls.rs` не в коммите), деградация
объявлена ровно одним WARN, аппаратный путь не изменён.

## Воспроизведение (мои команды)

```sh
git worktree add --detach /home/oleg/pagh-wt-atrandom bd4e06f
cd /home/oleg/pagh-wt-atrandom        # гейты + QEMU на ELF исполнителя
cargo build; python3 tools/build.py build; python3 tools/build.py build --release
cargo fmt --all -- --check; python3 tools/check_safety.py; python3 tools/host_tests.py
python3 tools/e2e.py selftest --cpu qemu64 --timeout 300   # ×2
python3 tools/e2e.py selftest --cpu max    --timeout 300

# моя проба (ветка verify/atrandom-probe) — измерение источников и атаки
git worktree add --detach /home/oleg/pagh-wt-atrandom-probe bd4e06f
cd /home/oleg/pagh-wt-atrandom-probe && git switch -c verify/atrandom-probe
# + инструментирование collect_boot_seed/mixed_fill, затем 5 × qemu64 и 1 × max
python3 /tmp/atrandom/model.py --transcript-and-attack   # (см. артефакты в /tmp/atrandom)
```
