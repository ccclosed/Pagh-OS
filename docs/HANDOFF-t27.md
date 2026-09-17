# HANDOFF — t27: сборка main, версия, PR, закрытие issues #11/#12/#16/#17/#18/#19/#32

**Статус на момент остановки (бюджет):** реализация всех семи issues готова и лежит в ветках;
не сделано: `t24` (верификация #32 на собранном main), `t27` (слияние + версия + PR + тексты
закрытия), `t17` (верификация ext2-линков — см. §8), `t28` (доки), `t29` (не нужна вовсе).

**Готовый артефакт резолва (главное):** ветка **`hand6` = `5a65ead`** в worktree
`/home/oleg/pagh-wt-rehearsal2`. Это траектория шагов 1–6 (tools, elf-load-rollback, procfs,
tls, at-random, pmm-hygiene) **с уже выполненным ручным резолвом шага 6** и E0252-фиксом.

Проверено на `hand6` (не пересказ, а прогоны):

| проверка | результат |
|---|---|
| `cargo build` | ok, **33 варнинга** (= база) |
| `cargo build --features lx_selftest` | ok, 33 варнинга |
| `cargo fmt --all -- --check` | чисто |
| `python3 tools/e2e.py selftest --features lx_selftest` | **`SELFTEST SUMMARY: 65 routines, 0 failed checks, 0 skipped`** |
| `LXSELFTEST` строки | единственная красная — предсуществующий `getcwd FAIL`; он лечится шагом 12 (`verify/cwd-namespace`) |

Как переиспользовать: `git checkout -B t27int hand6` и продолжать шаги 7–12 **поверх** —
резолв шага 6 и E0252-фикс наследуются. Готовый патч резолва: `docs/handoff-t27-step6-resolve.patch`
(`git diff cc290c5 5a65ead -- src/test.rs src/vfs/elf.rs`, 481 строка).

---

## 1. Карта веток (имя → SHA → от чего растёт)

**Мерджатся в main:**

| ветка | SHA | база / примечание |
|---|---|---|
| `tools/e2e-verify-integrity` | `f2bda5d` | от cd542bb; только `tools/` + `AGENTS.md`; **мерджить первым** |
| `security/elf-load-rollback` | `22393b5` | от cd542bb; **источник E0252** (§4) |
| `vfs/procfs` | `ac339c8` | от cd542bb (issue #11) |
| `net/tls-large-stream` | `e560455` | от cd542bb (issue #19); здесь busybox-контроль зеленеет |
| `arch/at-random-entropy` | `4dedc22` | от cd542bb (issue #16); p51 переименован в `at_random.rs` |
| `kernel/selftest-pmm-hygiene` | `2fb311c` | от cd542bb; **семантический конфликт с шагом 2** (§5) |
| `fs/ext2-links` | `a08b0dc` | от cd542bb (t14/t15, issue #18) |
| `pkg/apt-verify-e2e` | `1cc0cc8` | **стек**: содержит `pkg/openpgp-verify` `4f375c0` и `apt-trust-chain`; база — `cd08b8b` (procfs) |
| `arch/signals-kill` | `9aa47f6` | **стек**: содержит `pkg/openpgp-verify`, но НЕ tip `1cc0cc8` (issue #12) |
| `pkg/tar-links` | `271a106` | от `fs/ext2-links` `a08b0dc`; t16 |
| `docs/known-gaps-refresh` | `df92c71` | docs |
| `verify/cwd-namespace` | `d2a0488` | от cd542bb; **мерджится** (лечит red `LXSELFTEST getcwd`); трогает `src/selftest_lx.rs` + строку дока в `src/task/compat.rs` |
| `ci/agents-md-gate` | `f7ccca5` | **последним**, только после фикса блокера F1 в absence-проверке |

**НЕ мерджить:**

| ветка | SHA | почему |
|---|---|---|
| `verify/openpgp-fixtures` | `d16c142` | **ассет t24** (15 кейсов + manifest + gpgv-кросс-проверка), в main не нужен |
| `verify/atrandom-baseline` / `verify/atrandom-probe` | `d23f735` / `054d0ab` | верификационные ветки t12 |
| `rehearsal/t27` / `rehearsal/t27b` / `hand6` | `1d401bb` / `0264eda` / `5a65ead` | репетиция мерджа (`hand6` — рабочий результат, не для main) |

## 2. Первое действие — обязательно

```sh
git fetch origin
git checkout main && git merge --ff-only origin/main
```

Локальный `main` был `cd542bb`, а `origin/main` = **`e5f653e`** (PR #37: `tools/qemu_shot.py` +
секция в `tools/README.md`): все ветки росли от `cd542bb` и отстают на 3 коммита. Без
`ff-only` push упрётся, а конфликты в `tools/README.md` будут выглядеть необъяснимо (§6).

## 3. Порядок мерджа (13 шагов) и почему именно такой

1. `tools/e2e-verify-integrity` `f2bda5d` — только tools; от него зависят канон `python3` и
   `--rtc/--require-boot-proof`.
2. `security/elf-load-rollback` `22393b5` — **после него `cargo build`** (§4).
3. `vfs/procfs` `ac339c8` — **здесь сборка падает E0252** (встреча 2×3); это ожидаемо, фикс в §4.
4. `net/tls-large-stream` `e560455` — docs-конфликт `tools/README.md`; **busybox-контроль
   становится зелёным**: `usr/bin/busybox` 0→5, `"/mnt/bin/busybox"` 1→0.
5. `arch/at-random-entropy` `4dedc22` — чисто (коллизия p51 разведена).
6. `kernel/selftest-pmm-hygiene` `2fb311c` — **единственный семантический конфликт** → §5
   (резолв уже сделан в `hand6`).
7. `fs/ext2-links` `a08b0dc` — конфликты `README.md`, `host-tests/src/lib.rs` (вставочные).
8. `pkg/apt-verify-e2e` `1cc0cc8` — конфликты `README.md`, `SECURITY.md`,
   `host-tests/src/lib.rs`, `tools/README.md`.
9. `arch/signals-kill` `9aa47f6` — **именно после pkg**: ветка уже содержит `pkg/openpgp-verify`,
   поэтому в конфликте остаётся только `host-tests/src/lib.rs` (замерено; при мердже до pkg
   конфликт раздувается на весь OpenPGP-блок).
10. `pkg/tar-links` `271a106` — после `fs/ext2-links` мерджится **чисто** (замерено на `8a6b6f0`).
11. `docs/known-gaps-refresh` `df92c71` — конфликт `AGENTS.md`.
12. `verify/cwd-namespace` `d2a0488` — чисто; **после него LXSELFTEST-набор зелёный**
    (лечит red `getcwd`, см. §5.4 в отчёте f2).
13. `ci/agents-md-gate` `f7ccca5` — последним, после фикса F1.

## 4. Правило: `cargo build` после КАЖДОГО шага

Шаги 2 и 3 по отдельности чистые, а **вместе** дают:

```
error[E0252]: the name `Vec` is defined multiple times
 --> src/vfs/elf.rs:7:5
4 | use alloc::vec::Vec;      <- предыдущий импорт
7 | use alloc::vec::Vec;      <- повторный импорт
```

Причина: `security/elf-load-rollback` добавил `use alloc::vec::Vec;` **после**
`use crate::memory::vmm;`, `vfs/procfs` — тот же импорт **перед** ним; хунки не пересекаются,
git даёт clean-мердж. **Фикс: удалить один из двух `use alloc::vec::Vec;`** (оставить один).
Патч: `docs/handoff-t27-step6-resolve.patch` (там же, в `src/vfs/elf.rs`).

Дополнительно: после шагов 4 и 6 прогонять `cargo build --features lx_selftest` — часть рутин
компилируется только под фичей. Per-branch гейт этот класс не ловит в принципе: дефект
существует только в комбинации.

## 5. Ручной резолв шага 6 (готов и проверен)

Замеренные факты (по деревьям, до сборки):
* `--theirs` (взять `src/test.rs` из pmm-hygiene): **0 вхождений всех трёх рутин**
  elf-load-rollback (`rejected_images_never_touch_the_pmm`,
  `oversized_segment_is_refused_before_allocating`, `mapped_frames_guard_rolls_back` — у
  elf-rollback по 2: определение + регистрация) и теряется его док-блок про PMM-инвариант;
* `--ours` (версия elf-rollback): **0 вхождений skip-API** pmm-hygiene
  (`reset_skip_totals` / `record_skip` / `skipped_checks`, у неё 7);
* `--union`: дублирование кода → `error: this file contains an unclosed delimiter` — НЕ применять;
* pmm-hygiene действительно переносит fuzz в конец: `fuzz header no panic` — индекс **59 из 60**
  у неё, **27 из 63** у elf-rollback (после него 35 записей).

Спецификация резолва (реализована в `hand6`, 4 региона `src/test.rs`):
1. **Док-блок**: сохранить оба текста — HEAD объясняет, *почему* рутина существует (rejected
   load не трогает PMM), theirs — *почему у неё бюджет кадров*.
2. **Счётчики и цикл**: `ok_loads` + `released` + `leaked` + `for i in 0..64` (`accepted` →
   `ok_loads` и в пост-цикле).
3. **Тело итерации**: `before/after = free_frames()`; на `Err` —
   `assert_eq_kernel!(after, before, "fuzz: a rejected load must not consume a single PMM frame")`
   (контракт rollback, ради него elf-rollback и существует); на `Ok(proc)` — `ok_loads += 1` и
   `drop_exclusive_user_space(proc.pml4_phys)` (`released += 1`, иначе `warn!` «not exclusively
   owned»); после итерации — леджер и early-stop по бюджету `floor = start_free / 4`.
4. **Регистрация в `all_tests()`**: три рутины elf-rollback остаются на месте, запись
   `"elf::fuzz header no panic (Property 8)"` уезжает в **самый конец** вектора вместе с
   комментарием pmm-hygiene «LAST on purpose: … a starved routine reports a skip, not a pass».

**Важно после шагов 7–12:** `fuzz_header_no_panic` обязан остаться ПОСЛЕДНИМ. Более поздние
ветки дописывают свои записи в конец `all_tests()` (напр. `entropy::AT_RANDOM …` из шага 5 уже
стоит после него) — их надо поднять выше, а fuzz оставить в конце, иначе возвращается ровно тот
молчаливый skip (рутина, идущая после выжирателя PMM, рапортует skip, а не pass), который
pmm-hygiene и лечила.

Арифметика: base 60 + 3 (elf-rollback) = **63** — это требование к самому резолву. В собранной
траектории шага 6 рутин **65** (procfs +1 и at-random +1 уже в дереве); прогон подтверждает:
`SELFTEST SUMMARY: 65 routines, 0 failed checks, 0 skipped`.

## 6. Union: когда можно и когда нельзя

* `git merge-file --union` — **только для ВСТАВОЧНЫХ конфликтов** (списки `mod`/`#[path]`,
  доки). На **переписывающих** конфликтах union дублирует код и ломает синтаксис — правило
  получено измерением, не догадкой.
* Вставочные (union достаточно): `host-tests/src/lib.rs` (шаги 7/8/9), `README.md`,
  `SECURITY.md`, `AGENTS.md`.
* **Обязательные ручные union в доках:**
  * `tools/README.md` — **тройная** коллизия: PR #37 (строка `qemu_shot.py` + отдельная секция),
    pkg (4 новых строки инструментов + переписанная строка `mini_repo.py`), tls (строка
    `e2e_live_update.ps1` HTTP→HTTPS + TLS-секция). Сохранить все три стороны — иначе в main
    пропадёт описание инструмента или секции, а на `tools/README.md` завязан гейт
    `check_agents_md.py`;
  * `README.md` (tls/pkg/signals), `SECURITY.md` (pkg/tls), `AGENTS.md` (signals/docs),
    `host-tests/src/lib.rs` (номера модулей P51–P55 у pkg свободны: AT_RANDOM переименован в
    `at_random.rs`; descriptиве имена `kill_target`, `procfs_*` — тоже не конфликтуют).
* `verify/openpgp-fixtures` (`d16c142`) — ассет t24, **не мерджить**.

## 7. Финальные шаги t27

1. **Версия одним коммитом**: `Cargo.toml [package].version` + строка в `AGENTS.md` +
   `Cargo.lock` (CI собирает `--locked`; бамп без lockfile валит job).
2. **Седьмой коммит гейта**: снять две записи `ALLOWED_MISSING` (ветка `ci/agents-md-gate`),
   затем `python3 tools/check_agents_md.py` → **exit 0**.
3. **Тексты закрытия** из `.cache/issue-closures/` — все семь написаны (#11, #12, #16, #17,
   #18, #19, #32) → в issues и в PR.
4. **`t24` — верификация #32 на собранном main:**
   * `python3 tools/openpgp_attack_fixtures.py build --suite case` (ассет: worktree
     `/home/oleg/pagh-wt-openpgp-fx`, ветка `verify/openpgp-fixtures` `d16c142`) и
     `… check` (gpgv-кросс-проверка фикстур — часть доказательств);
   * прогнать 15 кейсов через apt в госте и сверить фактические маркеры с `manifest.json`:
     `a01/a02` — **accept** (`apt: verify OK release=InRelease` / `Release.gpg`), `a03/a03b/a04/a05`
     — отказ по `stage=index|deb cause=HashMismatch|SizeMismatch`, `a06` — `signature/BadSignature`,
     `a07` — `metadata/Unsigned`, `a08` — **ожидаемый accept** (replay старого `stable` не
     детектится, это записано в `SECURITY.md`), `b01` — `signature/NoTrustedSignature`,
     `b02` — `signature/NoSignature`, `b03` — `armor/CrcMismatch`, `b04` —
     `clearsign/Malformed`, **`b04b` — `armor/MalformedArmor`** (проверять по фактическому
     поведению: удаление только `END`-строки ловится armor-слоем), `b05` — `NoTrustedSignature`;
   * подпись живого `stable` — подключом `4CB50190…`, он **запиннен** (субключ pinned-примари
     `B8B80B5B…`); пометка «NOT PINNED» в manifest — ошибка генератора, не дефект ядра;
   * clock-кейс: `--rtc base=2020-01-01`;
   * `bigindex --stanzas 60000` на merged main — обязан **дойти до parse-стадии**:
     `apt: index ready - 60000 packages` и `LXSELFTEST bigindex PASS (streaming; 60000 packages)`,
     ноль `[EXC #14]`/`[WATCHDOG]`;
   * остаточные риски (в текст закрытия #32): old valid `Release` принимается; expiry/revocation/
     not-yet-valid/`FutureSignature`/`FutureDate`/`ValidUntilExpired`/`NoIndexEntry` — только
     host-свойства, не E2E; ECDSA-путь — синтетические ключи.

## 8. t17 (верификация ext2-линков) — НЕ закрыт

Закрыто статически:
* `materialize_symlinks` / `read_regular_file` в дереве (`pkg/tar-links` `271a106`) отсутствуют —
  `git grep` пуст, то есть «копиями» действительно больше ничто не становится;
* README не заявляет «materialized as copies»: `src/pkg/README.md:81` — «копией не становится
  ничто»; старая формулировка осталась только как историческая ссылка в `EXT2-LINKS.md:36`.

Не выполнено (остановлено по бюджету): in-guest прогон `local-mirror` на `pkg/tar-links`
(apt install `links-pagh` → `lstat` = S_IFLNK, `readlink` дословно, `open(link)` = содержимое
цели, общий `st_ino` хардлинка и `st_nlink == 2`, член с GNU `'L'` под полным именем >100 Б и
отсутствие усечённого, удаление одного имени не ломает другое, повторный `install/remove` без
мусора), host-side инспекция ext2 через `debugfs`, WAL/reboot-интеграция.

Команда для продолжения:

```sh
cd /home/oleg/pagh-wt-t16
cp /home/oleg/pagh-wt-integrity/tools/e2e.py tools/e2e.py   # канонический драйвер
python3 tools/e2e.py local-mirror --limine-dir /home/oleg/pagh-wt-integrity/limine \
    --serial-log /tmp/t17_lm.log --json /tmp/t17_lm.json \
    --evidence-regex 'LXSELFTEST|links-pagh|apt:|WATCHDOG'
```

Ожидаемые маркеры: `LXSELFTEST tar_links PASS`, `LXSELFTEST ext2_links PASS`,
`LXSELFTEST apt_e2e: installed link fixture …`. **Следить, чтобы НЕ появился**
`LXSELFTEST apt_e2e: link fixture not installed (…); skipping the link assertions` — это
молчаливый skip, из-за которого «зелёный» прогон ничего не проверил. Фикстура: пакет
`links-pagh` в `tools/mini_repo.py` (real / rel / abs / hard / член >100 Б).

## 9. Операционные заметки

* Драйвер: канон — ветка `tools/e2e-verify-integrity` (`f2bda5d`); в worktree лежит untracked
  копия; `--serial-log`/`--json` задавать явно (дефолты перезаписываются).
* `/tmp` — tmpfs 7.8 ГБ; на момент остановки был забит на 100% (в основном чужие
  `/tmp/gate-*`, `/tmp/gv2-*`, 3.9 ГБ + 1.8 ГБ). При полном `/tmp` падает даже захват вывода
  `bash` (ENOSPC) — сначала освободить место, потом работать.
* `/tmp/pagh-docs` — чужой worktree (`docs/known-gaps-refresh`), не удалять.
* `--disk <file>` переиспользует образ (для reboot-проверки WAL); без него гость пишет в
  `.cache/e2e_disk.img` копии.
