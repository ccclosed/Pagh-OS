# AGENTS.md — руководство для AI-агентов (и людей), работающих над pagh

Небольшое 64-битное ядро ОС на Rust (`#![no_std]`), загружаемое через Limine на UEFI и
работающее под QEMU/OVMF. Написано AI под надзором человека — прочитайте этот файл до того,
как трогать код: здесь перечислены команды, которые обязаны проходить, и инварианты, которые
легко сломать молча.

## Авторитетная документация

Читайте в этом порядке; не полагайтесь на память вместо этих файлов:

| Файл | Содержимое |
|---|---|
| `README.md` | Архитектура, возможности, сборка/запуск, честные ограничения |
| `src/README.md` | Карта корня крейта, точная последовательность загрузки (порядок фаз важен) |
| `src/<subsystem>/README.md` | Design-доки подсистем — **each folder documents itself**; обновляйте README при изменении дизайна |
| `CONTRIBUTING.md` | Руководство для контрибьюторов (RU) |
| `tools/README.md` | Справочник по сборке/тестам/E2E-инструментам |

## Commands (all six must be green before you push)

```sh
cargo fmt --all -- --check                  # formatting gate
python3 tools/build.py build                # debug kernel + link (CI parity)
python3 tools/build.py build --release      # release kernel + link
python3 tools/host_tests.py                 # host property tests (or: cd host-tests && cargo test)
python3 tools/check_safety.py               # unsafe-policy gate
python3 tools/check_agents_md.py            # the claims in this file vs the tree
```

CI (`.github/workflows/ci.yml`) запускает ровно эти шесть, по одному job на каждую. Арбитр —
CI: локальный зелёный необходим, но недостаточен. Блок — это *множество*, а не пожелание:
`tools/check_agents_md.py` сравнивает его с шагами `run:` workflow в обе стороны, поэтому
гейт, который CI запускает, а блок не упоминает (или наоборот), валит проверку. Для более
быстрого внутреннего цикла `cargo build` собирает только debug-библиотеку;
`tools/build.py build` дополнительно линкует, а `tools/build.py stage` пишет загрузочный ESP
для QEMU. Чтобы увидеть, как claim-гейт доказывает сам себя, запустите его с `--probe`: он
ломает каждый класс утверждений в scratch-копии и требует находку на каждую поломку.

Ядру нужен запиненный nightly (`rust-toolchain.toml`) с `rust-src` (build-std) и
`rust-lld`; линковка идёт через `linker.ld` в `pagh.elf`. На Windows используйте
`run.cmd build|run`; на Linux — `./build.sh` / `./run.sh`. `OVMF.fd` и `disk.img` локальны и
git-ignored — никогда их не коммитьте. Загрузчик Limine версионно-агностичен: любое дерево
`limine*/` git-ignored; `tools/limine.py` находит его (или автоматически скачивает
последний бинарный релиз в `limine/`) — не хардкодьте версии Limine в скриптах.

## Canonical entry points

Одна роль — один актуальный путь, который должны использовать CI, доки и свежий checkout.
Компаньон `-legacy` сохраняется для паритета на другой ОС и **не** запускается здесь. Какая
строка актуальна — утверждение о репозитории, а не то, что гейт может вывести из файловой
системы, поэтому это записано, а `tools/check_agents_md.py` проверяет таблицу: каждый путь
существует, у `-legacy`-строки есть канонический аналог, она называет другой файл и говорит,
почему она legacy.

```text
kernel-build      -> tools/build.py          # builder: cargo + linker; `stage` writes the ESP
kernel-run        -> run.sh                  # Linux/QEMU launcher (build.sh = toolchain guard)
kernel-run-legacy -> run.cmd                 # Windows-only legacy launcher (run.cmd build|run)
kernel-e2e        -> tools/e2e.py            # in-guest E2E driver (Linux/CI, no pwsh)
kernel-e2e-legacy -> tools/e2e_*.ps1         # Windows-only legacy harnesses (need pwsh), same scenarios
```

## Раскладка репозитория

- `src/` — ядро (крейт `pagh`, `staticlib`, target `x86_64-unknown-none`). Подсистемы:
  `arch` (CPU + слой Linux-compat в `arch/x86_64/linux/`), `drivers`, `fs` (ext2 + WAL),
  `memory` (PMM/VMM/heap), `net` (собственный стек TCP/IP, без smoltcp), `pkg` (apt),
  `shell`, `task`, `vfs`, `sync`, `security`, `debug`, плюс корневые модули (`boot.rs`,
  `log.rs`, `provision.rs`, `test.rs`, `selftest_lx.rs`).
- `host-tests/` — proptest-крейт, **excluded from the kernel workspace** (bare-metal vs host
  targets). Чистые исходники ядра подключаются напрямую через `#[path]` — тесты исполняют
  те же файлы, которые компилирует ядро.
- `vendor/` — все зависимости vendored; `third_party/x86_64` подключён через
  `[patch.crates-io]`. Крейты сжатия/TLS запинены точно (`miniz_oxide`, `ruzstd`,
  `xz4rust`, `embedded-tls`) с `default-features = false` — не «обновляйте» и не включайте
  фичи обратно; no_std-сборка сломается. `vendor/**` — это `-text` в `.gitattributes`
  (контрольные суммы чувствительны к байтам); никогда не пересохраняйте vendored-файлы
  (`never re-save vendored files`) с конвертированными окончаниями строк.
  - **Один намеренный локальный патч (`One deliberate local patch`):
    `vendor/embedded-tls/src/connection.rs`.** В upstream крейт доходил до `ApplicationData`, даже когда сервер не присылал
    `Certificate`/`CertificateVerify`, поэтому пир (или активный MITM, сам завершающий
    обмен ключами) мог полностью пропустить верификатор. `Handshake` теперь несёт
    `certificate_received`/`certificate_verified`, и `Finished` отвергается, если оба не
    выставлены (PSK-рукопожатия — исключение). Если когда-либо обновите этот крейт,
    повторно примените патч и обновите запись для `src/connection.rs` в
    `vendor/embedded-tls/.cargo-checksum.json` — иначе cargo уронит сборку.
    `src/net/tls.rs::VERIFIED_HANDSHAKES` — ядерная страховка к этим подтяжкам.

## Жёсткие инварианты (их нарушение выглядит как «аппаратный» баг)

1. **Контракты asm-ABI несущие.** `SavedRegs` (`arch/x86_64/linux/regs.rs`) повторяет
   порядок push в `int80_stub`/`syscall_entry` (`syscall.rs`): 15 GPR, `rax` по смещению
   112 и per-task **слот user-RSP по `+120`** (читается/пишется `execve`, `clone`,
   доставкой сигналов). У `task/switch.rs::irq32_stub` своя раскладка кадра, побайтово
   проверяемая в `src/test.rs`. Никогда не переставляйте и не добавляйте Rust-смещение без
   обновления asm.
2. **Compat ⇒ вход через `syscall`.** Linux Compat_Processes входят в сисколлы только через
   `syscall_entry`; на этом пути сохранённые `rcx` = user RIP, `r11` = user RFLAGS, а слот
   `+120` = user RSP. Код, полагающийся на это (execve, clone, сигналы), корректен *именно
   поэтому*; compat-процесс, входящий через `int 0x80`, это сломает — не добавляйте такой.
3. **Один CPU, спинлоки маскируют IF.** Спинлок ядра выключает прерывания, пока удерживается,
   поэтому обработчик IRQ, берущий тот же лок, не может задеадлочить — но не-IRQ-контекст
   никогда не должен брать лок дважды (нереентерабельность). Реестр compat дополнительно
   охраняется счётчиком глубины реентерабельности (`compat_lock_held`); путь page fault его
   проверяет.
4. **Никогда не держите `COMPAT_STATES` (реестр compat) через блокирующую работу** — извлеките
   нужное под локом, отпустите, сделайте блокирующий I/O, перехватите лок для коммита
   (паттерн — в `io_sys.rs`).
5. **`ticks()` продвигается только с включёнными прерываниями.** Настоящий сисколл входит с
   замаскированным IF (SFMASK / interrupt gate), и `linux_dispatch` размаскирует его на время
   обработчика — но только когда вызывающий передаёт ненулевой `reentry_allowed`, что делают
   две входные заглушки и не должен делать boot-time selftest (прямой вызов в
   не-планируемом boot-потоке). `hlt`-сон с замаскированным IF спит вечно.
6. **`create_user_process`/пути spawn идут под `without_interrupts`** — тик таймера,
   наблюдающий недостроенный CR3, портит планирование.
7. **Порядок загрузки в `boot.rs` хрупок**: `enable_sse()` первым (x86-interrupt prologues
   испускают `movaps`; Limine передаёт управление с OSFXSR=0), ext2 монтируется до включения
   прерываний, перечислению virtio нужна куча.
8. **`panic = "abort"`**: panic где угодно убивает машину. In-QEMU selftests (`src/test.rs`)
   используют `assert_kernel!` (печать + продолжение) и обязаны восстанавливать всё
   состояние, которое трогают (PMM, кучу, IF, VFS). Проверки `selftest_lx` печатают
   `LXSELFTEST <name> PASS/FAIL` и возвращаются.
9. **Фича-гейты**: `default = ["network_packages"]` включает apt; harness'ы `lx_selftest` /
   `lx_livetest` / `lx_bigindex` / `lx_tlsbig` должны оставаться скомпилированными-out (и не
   менять загрузку), когда не включены. TLS fail-closed: рукопожатие прерывается, если
   цепочка не валидируется против закоммиченного CA-бандла, SAN не авторизует хост, clock
   gate не проходит или подпись `CertificateVerify` не сходится. `apt` также проверяет то,
   что ставит: `InRelease`/`Release.gpg` против запиненного keyring'а Debian, затем дайджест
   **и** размер `Packages` из подписанного `Release`, затем дайджест **и** размер каждого
   `.deb` из подписанного индекса; что остаётся непроверенным (распространение отзыва,
   replay целой старой тройки на suite'ах без `Valid-Until`, команда `pkg` без индекса),
   названо в `SECURITY.md`.
10. **Unsafe policy**: every `unsafe {` in `src/security/`,
    `arch/x86_64/linux/mod.rs`, `memory/vmm.rs`, `net/tls.rs`, `pkg/apt.rs`
    needs a `SAFETY:` comment within the previous 6 lines
    (`tools/check_safety.py` enforces; the rest of the kernel follows the
    same style voluntarily — keep it).
11. **Учётные данные — состояние слоя сисколлов, никогда не политика VFS.**
    `CompatState::creds` (`src/task/compat.rs`) — единственный владелец шести ids процесса;
    стартует как `Creds::ROOT`, наследуется `fork`/`clone` и сохраняется `execve`
    (setuid-бинарники не моделируются). Чистые решения живут в
    `src/arch/x86_64/linux/cred.rs` (host-свойство P56). Инварианты:
    * `VfsNode` несёт только метаданные (`mode`/`owner`/`set_mode`/`set_owner`). Никогда не
      добавляйте проверку учётных данных внутрь реализации `VfsNode` — boot provisioning,
      установщик пакетов и shell вызывают VFS напрямую и должны работать без идентичности.
    * root обходит **чтение и запись**, но никогда **исполнение**: режим без бита `0o111` не
      исполняем даже для root. Не «исправляйте» исполняемый `0644`, спецкейсом для root.
    * новый сисколл следует трёхправному контракту `abi.rs` (константа nr, ветка dispatch,
      `SUPPORTED_SYSCALLS` + независимый литерал `EXPECTED`). Путевые сисколлы используют
      `check_node_access`/`check_dir_mutation`; изменения ids идут через метод перехода
      `Creds`, никогда не по полям в обработчике.
    Контракт и точный список пробелов: `docs/security-model.md`.

## Философия тестирования

- **Чистая логика → host property tests.** Модуль должен оставаться `core`(+`alloc`)-only,
  чтобы его можно было `#[path]`-подключить в `host-tests/src/lib.rs` и покрыть файлом
  свойств `properties/pNN.rs`. Если изменение трогает такой модуль (`abi`, `errno`,
  `validate`, `io`, `wire`, `deb`, `tar`, `apt_index`, `signal_frame`, `cred`, …),
  расширьте/поправьте свойство; список `supported_set_is_exact` в `#[cfg(test)]` внутри
  `abi.rs` должен совпадать с `is_supported`.
- **Состояние ядра → in-QEMU selftests** (`src/test.rs`, запуск через shell-команду
  `selftest`; неразрушающие, детерминированные XorShift-сиды). `selftest 2` прогоняет набор
  дважды за загрузку и печатает вердикт `SELFTEST IDEMPOTENCY: …` плюс ledger каждого прохода
  `[selftest] PMM hygiene: free frames … -> …`: рутина, удерживающая PMM-кадры (или любое
  другое состояние ядра), делает проходы расходящимися — так нашли ELF-fuzz-рутину,
  вычерпывавшую весь пул.
- **Linux-compat end-to-end → `selftest_lx`** (фича-гейтед harness'ы) плюс гостевой драйвер
  `tools/e2e.py` (режимы `selftest`, `local-mirror`, `live-update`, `bigindex`, `shell`):
  локальный mini-repo, живой apt update, bigindex-репро, многомегабайтный TLS-поток через
  `lx_tlsbig`. Harness'ы `tools/e2e_*.ps1` — Windows-only legacy-эквиваленты тех же
  сценариев, см. *Canonical entry points*; новая работа идёт в `tools/e2e.py` (Linux и CI
  могут его запускать).
- Регрессионный фикс без теста не сделан. Новый чистый модуль без свойства — подозрителен.

## Соглашения

- Doc-комментарии несут дизайн: ABI-контракты, теги требований (R-номера), SAFETY-заметки.
  Сохраняйте плотность соседнего кода; меняя поведение, меняйте комментарий в том же коммите.
- Обновляйте README подсистемы (и списки ограничений корневого `README.md`), когда меняется
  поведение — устаревшие доки здесь считаются багами.
- Коммиты: `area: imperative summary` (`kernel:`, `linux:`, `net:`, `fs:`, `fix:`, `docs:`,
  `ci:`), тело объясняет *почему* и *что сломается без этого*.
- Работайте в feature-ветках → PR; мержите только с зелёным CI. Stacked PR допустимы
  (ответвляйтесь от более ранней feature-ветки, затем ретаргетьте, когда она смержится).
- **Сразу после слияния PR удаляйте его ветку (remote и local) и ретаргетьте любые stacked
  PR на `main`.** Оставшаяся смерженная ветка — ловушка: следующий stacked PR всё ещё
  по умолчанию берёт её как base, и слияние уходит в ветку вместо `main` — main молча
  теряет работу, пока ветка выглядит готовой (именно так PR #27 изначально обошёл `main`).
- Поверхность shell/регрессий — serial-лог: диагностика использует маркеры `[WATCHDOG]`,
  `[DIAG]`, `LXSELFTEST`, `[EXC #N]`, `[BOOTID]` — E2E-ассерты грепают их; не переименовывайте
  их необдуманно. `[BOOTID]` — доказательство boot identity (`tools/e2e.py` патчит его
  placeholder внутри staged ELF и требует тег прогона обратно на serial — см.
  `tools/e2e_boot_integrity.md`); удаление или переформатирование молча делает каждый
  E2E-вердикт неопровержимым.

## Версионирование

Semver `MAJOR.MINOR.PATCH` — именно в этом стандартном порядке (major, *затем* minor,
*затем* patch). Единственный источник — `Cargo.toml [package].version`; она попадает в
`/mnt/etc/pagh-release` и motd через `env!("CARGO_PKG_VERSION")` (`provision.rs`), поэтому
устаревшая версия видна пользователю при загрузке.

**Ветки и релизные каналы.** `main` — стабильная ветка; `dev` — экспериментальная
интеграционная и несёт версию *следующего* релиза. Feature- и fix-PR целятся в `dev` (CI
зелёный до слияния); стабилизация релиза сливает `dev` в `main` через `--no-ff`, без
изменений кода помимо релизных метаданных. Release candidates приходят только из `dev` и
только как `vX.Y.Z-rc.N` (канала `-pre.N` нет); стабильные релизы тегируются `vX.Y.Z` из
`main`, а старые голые числовые теги репозитория остаются историей. Hotfix ограничен
critical/high verified security finding или регрессией, блокирующей релиз, в выпущенной
версии: ответвляется от `main`, тегируется сразу в stable **без** RC и сливается обратно в
`dev` через `--no-ff`. Строка версии `main` ниже описывает только stable-релиз — `dev` её
никогда не переписывает (dev-онли bump заявляет свою версию в `currently reads` ниже).
`.github/workflows/release.yml` отвергает stable-тег, недостижимый из `main`; тег `-rc.N`,
недостижимый из `dev` или уже находящийся на `main`; любой другой суффикс; и тег, чья
базовая версия расходится с `Cargo.toml`. К релизу прикладываются релизное ядро
(`pagh-vTAG.elf`, с `.sha256`) и архив исходников с контрольной суммой. Полная модель — в
`BRANCHING.md`, операционный чек-лист — в `RELEASING.md`.

- **MAJOR** — образные поломки: изменения on-disk/boot-формата, удалённые семейства
  сисколлов, несовместимые ожидания userland. Редко; последним был 2.0.0 (собственный стек
  TCP/IP + e1000 вместо smoltcp + virtio-net).
- **MINOR** — новые видимые пользователю фичи или изменения поведения, даже полностью
  обратно совместимые. Прецедент: `release 1.1.0: the tick-rate change is a feature
  (behavior change), so minor bump, not patch`. Bump идёт в том же PR (или финальном
  коммите stacked-серии), который выпускает фичу, — не когда-нибудь потом. (tags
  2.3.0 → 2.4.0 → 2.4.1 → 2.4.2 → 2.5.0, each with a GitHub release); `Cargo.toml` —
  источник истины, и фича в полёте несёт свой MINOR bump в PR, который её выпускает —
  2.4.0 пришёл с format-guard PR, а ещё не смерженная ветка не должна здесь отражаться.
- **PATCH** — фиксы, диагностика, доки, тулинг, обновления vendored-зависимостей без
  изменения поведения. Docs-only коммиты (этот файл, README'шки) версию не бампают.

**Текущие версии и недавняя история.** **`main` reads 2.22.7** — стабильный
релиз: USB-клавиатура и мышь работают на реальном xHCI (boot-протокол, neutral-записи
PORTSC, hub-тайминги, исправленный wrap кольца передач), плюс закрытые на `dev` три
x86-находки раунда 3; до него 2.22.6 закрыл класс machine-kill на CPU-исключениях ring 3,
неограниченную аллокацию `fcntl(F_DUPFD)` и самодеадлок epoll/signalfd, плюс обход
installer spelling-split;
до него раунд 2: 2.22.4 закрыл use-after-free владения fork-COW кадрами,
2.22.5 — обход confinement установщика через hardlink-алиас; ещё раньше
2026-11-13 audit-пакеты: 2.22.0 закрыл пути ring3-to-ring0, 2.22.1 ограничил установщик
пакетов, 2.22.2 ужесточил VFS/ext2, 2.22.3 закрыл класс user-buffer TOCTOU.

**`dev` currently reads 2.22.7** — та же версия, что `main` (релиз 2.22.7 уже включает
USB-фиксы); три x86-находки раунда 3, недостаточно серьёзные для hotfix 2.22.6, вошли в
этот релиз (подробности и статус — в `SECURITY.md`). (1) `mprotect`-защиты были совещательными на fork-shared
страницах и частично покрытых `VmRegion`: новый программный бит листа (`RO_BIT`) фиксирует,
что маппинг запрещает запись, поэтому `cow_copy_page` копирует только при записываемом
маппинге, а запись в `mprotect(PROT_READ)` даёт SIGSEGV; `update_region_flags` делит регион,
который span покрывает лишь частично, поэтому `PROT_NONE`-страницы в частично покрытом
маппинге не переподкрепляются лениво на следующем касании. PoC аудита `prot_bypass` умирает
с SIGSEGV, shell жив; обычный COW fork/write и динамически слинкованный glibc-бинарник
(`fastfetch`) работают. (2) crafted ext2-superblock с
`s_inodes_count = s_inodes_per_group = 0xFFFFFFFF` заставлял каждый mount сканировать
`2^32` бит bitmap (28 с boot CPU) и писать фиктивный `s_free_inodes_count`;
`ext2::bounds::validate_group_counts` теперь отвергает невозможные per-group счётчики
(`FsError::Corrupt`, чистый отказ mount) до сканирования, а span сканирования дополнительно
клампится до одного блока bitmap (`clamp_group_span`) с сатурирующими итогами. (3) `execve`
освобождал заменяемое адресное пространство, пока sibling `CLONE_VM`-задача, не находившаяся
в ready queue (припаркованная SIGSTOP в `STOPPED_TASKS`), всё ещё на нём работала — UAF
таблиц страниц в ожидании `SIGCONT`; `scheduler::cr3_referenced_by` теперь смотрит во все три
реестра живых задач (ready, stopped, pending reap) и для `drop_exclusive_user_space`, и для
exit reaper, с чистой host-тестируемой политикой (`src/task/as_refs.rs`), in-QEMU selftest
планировщика и строкой лога `freed=false`, называющей реестр, удержавший пространство.
Сырой `clone`-репро аудита так и не создал второй поток, поэтому импакт находки остаётся
статическим (UAF не продемонстрирован); фикс — инвариант учёта, а не PoC-driven краш.
(4) В той же версии `dev` — исправления USB HID по реальным устройствам (R82HE keyboard
3151:502d, E-Signal LUOM G10 mouse 04d9:a09f): SET_PROTOCOL отправлялся с `wValue=1`, то
есть *report* protocol, хотя комментарий и доки говорили boot (HID 1.11 §7.2.6: boot = 0);
interrupt-IN буфер был жёстко 8/4 байта, поэтому 7-байтный report-protocol отчёт мыши
вызывал babble и эндпоинт отключался до перезагрузки. Теперь SET_PROTOCOL(0) уходит на
каждый boot-интерфейс с проверкой результата (`mode=boot|report`), буфер равен
`wMaxPacketSize` (clamp 8..=64), ошибка передачи восстанавливается xHCI Reset Endpoint с
переармированием (до трёх попыток), а композитное устройство разбирается целиком: все
HID-интерфейсы, привязка всех boot-клавиатур/мышей (`Slot.hid` — `Vec`, SET_PROTOCOL/SET_IDLE
на интерфейс, одна Configure Endpoint со всеми DCI). Ограничения названы: хабы не
поддерживаются, `protocol = 0`/vendor-коллекции логируются и не привязываются, устройства
только-report-protocol — best effort (клавиатура — boot-подобные 8 байт; мышь — 7-байтная
16-битная E-Signal или 8-битные оси, дельты клампятся в диапазон PS/2-пакета, wheel/pan
декодируются, но не доставляются), LED output report не реализован. (5) Затем тот же
аппарат показал более глубокую ошибку порта xHCI ниже HID-исправления, в двух итерациях:
reset каждые ~124 мс убран; PP power-cycle после 16 неудач оставлял порты `pp=0 ccs=0`; в
записи PORTSC драйвер возвращал `PED` (бит 1, write-1-to-disable) и change-биты, поэтому
подтверждение PRC/CSC отключало только что включённый порт (`post=0x6e1`). Теперь каждая
запись PORTSC идёт через neutral-маску, reset — `neutral | PR` (USB2) или `neutral | WPR`
(USB3), а политика следует `hub.c`/`xhci-hub.c` (debounce 25 мс/100 мс, бюджет 2 с;
100 мс power settle; ожидание reset 10/200/800 мс до пяти записей; 200 мс стабильного
PED+U0 до адресации; пауза ≥1 с после потерянного PED; PP только поднимается с
readback/повтором). Один порт продвигается за `poll()`, поэтому boot-меню продолжает
считать. Затем третий прогон показал отказ ввода после минуты: замыкающий Link TRB
писался со *следующим* циклом производителя и без `TRB_TOGGLE`, поэтому контроллер не шёл
по ссылке; теперь Link TRB несёт текущий цикл и toggle-бит, а `tools/e2e.py usb` печатает
через замыкание (241 отчёт) как регрессию. Версия 2.22.7 — `dev` PATCH bump; `main`
остаётся 2.22.6.

Сжатая история предыдущих релизов (подробности — `CHANGELOG.md`): 2.22.6 — четыре high
раунда 3 (ring-3 исключения, `F_DUPFD` cap, epoll/signalfd deadlock, installer spelling);
2.22.5 — hardlink-алиас установщика; 2.22.4 — владение fork-COW кадрами; 2.22.3 —
user-buffer TOCTOU и cap payload'а `sendto`/`recvfrom`; 2.22.2 — crafted ext2 и права
обхода путей; 2.22.1 — confinement установщика `.deb`; 2.22.0 — изоляция ring 3;
2.21.0 — console scrollback; 2.20.0 — `/proc/<pid>/maps` с настоящими file identity;
2.19.0 — девять shell-команд и per-thread procfs; 2.18.13 — поля Supported Protocol;
2.18.12 — остановленный xHCI (DMA-владение, bus mastering, TSC-дедлайны); 2.18.11 —
один порт за `poll()`; 2.18.10 — extended capabilities до 64 KiB; 2.18.9 — retry с
Disable Slot; 2.18.8 — Supported Protocol и adopt включённых портов; 2.18.7 — стабильный
PED+U0; 2.18.6 — диагностика reset; 2.18.5 — ожидание PED; 2.18.4 — EP0 max packet,
warm reset USB3; 2.18.3 — поиск контроллера и debounce; 2.18.2 — четыре дефекта
энумерации; 2.18.1 — disk-меню и правило владения; 2.18.0 — измеряемый LAPIC, GPT,
logfile; 2.17.2 — huge page в обходчике; 2.17.1 — отказ `map_mmio`; 2.17.0 — batch
реального железа; 2.16.1 — fastfetch/persistent-диск; 2.16.0 — signalfd, process groups,
job control, procfs slice 5, NVMe; 2.15.0 — Linux-compat batch; 2.14.0 — shell `df`/`ps`;
2.13.0 — `/proc/*/fd` и настоящий loadavg; 2.12.0 — `/proc/mounts`; 2.11.0 — procfs
slice 4; 2.10.0 — procfs slice 3; 2.9.x — правда о памяти и `/proc/stat`; 2.8.0 — модель
безопасности; 2.7.x — antialiased-шрифт и QA-фиксы; 2.6.0 — Fira Code и программный
курсор; 2.5.0 — procfs, сигналы, apt trust chain; 2.3.0 — fail-closed TLS; 2.2.0 — cap
кучи и версионно-агностичный Limine; 2.1.0 — COW fork/demand paging и POSIX-сигналы.

- **Каденция релизов (запрос человека, 2.15.0)**: не тегируйте каждый коммит. Накапливайте
  **15 коммитов на релиз**; первый коммит пакета несёт bump версии, последний — тег и GitHub
  release. Версию пакета выбирайте по изменению с наибольшим влиянием: любой образный/ABI
  break → MAJOR, любая видимая фича → MINOR, иначе PATCH. Низковлиятельные
  коммиты только с документацией и тестами едут в следующий MINOR/PATCH вместо отдельного релиза. CI всё равно
  работает на каждом коммите.
- **Теги**: тегируйте релизный коммит голой строкой версии (`2.1.0`, в существующем стиле
  тегов) и пушьте тег. Теги никогда не должны опережать `Cargo.toml` — старый тег `2.0.4`
  стоял на коммите, где версия крейта ещё была 2.0.3; именно этот дрейф раздел и
  предотвращает.
- **Bump обязан включать запись `pagh` в `Cargo.lock`.** CI собирает через
  `cargo build --locked`, поэтому bump без обновления lock-файла валит kernel job примерно
  за 20 с. Локальный `cargo build` обновляет lock — коммитьте его в ТОМ ЖЕ коммите, что и
  `Cargo.toml`.
- CI пока не форсит bump; это обязанность ревьюеров и агентов.

## Известные открытые пробелы (good first issues, всё задокументировано в коде)

- procfs покрывает срезы 1–5 (issue #11, контракт `docs/procfs.md`):
  `/proc/{cpuinfo,meminfo,uptime,stat,version,filesystems,loadavg,mounts}`,
  `/proc/<pid>/{cmdline,stat,status,maps,cgroup,mountinfo,limits}`,
  `/proc/self/{exe,cmdline,status,maps,stat,statm,mounts,cgroup,mountinfo,limits}` и
  read-only `/proc/sys/kernel/{ostype,osrelease,hostname}` рендерятся из живого состояния
  ядра (CPUID, счётчики PMM, тиковые часы, учёт user/system/idle тиков планировщика и живые
  счётчики задач, снимок `CompatState` целевого процесса). `/proc/self/fd` и
  `/proc/<pid>/fd` выставляют таблицу fd как числовые symlink'и; `/proc/<pid>/fdinfo/<n>`
  сообщает реальные `pos`/`flags`/`mnt_id`/`ino` открытого дескриптора (0 для объектов без
  VFS mount/inode), а `/proc/<pid>/limits` рендерит 16 строк Linux rlimit из таблицы
  override цели и значений по умолчанию ядра. Отложено: `/proc/<pid>/task/*`,
  необязательные поля mountinfo и метаданные statfs.
- Сигналы (issue #12): `kill(2)` (nr 62), адресация process group (`kill(0|-1|-pgid)`),
  планирование `SIGSTOP`/`SIGCONT`, `wait4(WUNTRACED|WCONTINUED)` и доставка по тику
  таймера задаче без сисколлов реализованы. SIGCHLD ставится родителю в очередь на
  exit/stop/continue ребёнка (2.15.0). Настоящие process groups теперь есть в `CompatState`
  (`pgid`, `setpgid`/`getpgid`/`getpgrp`/`setsid`, `kill(0)`/`kill(-pgid)` адресуют группу)
  с 2.16.0. Terminal job control пришёл вместе с ними: консоль — управляющий терминал с
  одной foreground process group (`drivers::tty`, чистые правила в
  `arch/x86_64/linux/tty.rs`, host-свойство `tty_job`), `TIOCGPGRP`/`TIOCSPGRP` отвечают
  как `tcgetpgrp`/`tcsetpgrp`, фоновое чтение получает `SIGTTIN`, а (при `TOSTOP`) фоновая
  запись — `SIGTTOU`, и `^Z` останавливает foreground GROUP. Остающиеся отклонения названы
  в модуле, а не подразумеваются: нет сессий (правило сессии `tcsetpgrp`
  аппроксимируется), фоновый доступ возвращает `EINTR` вместо перезапуска после `SIGCONT`,
  а `TOSTOP` по умолчанию включён там, где Linux выключает.
- Устройства ввода: PS/2-клавиатура/мышь (IRQ1/IRQ12) и USB HID через xHCI-контроллер,
  которым ядро управляет само (`drivers::xhci`, `drivers::usb_hid`, опрос из потока `usb` и
  напрямую из boot-меню). Весь путь проверен в QEMU с эмулированными контроллером и
  клавиатурой: детект портов, Enable Slot, Address Device, дескрипторы, SET_CONFIGURATION,
  boot protocol, interrupt-IN отчёты и печатаемый текст, доходящий и до shell, и до
  boot-меню. Четыре реальных бага стояли между «код написан» и «код работает», все
  исправлены в 2.18.2. **Не проверено: реальное железо** (следующий полевой тест его
  прогонит); нигде не реализованы: хабы (только корневые порты), изохронные передачи,
  teardown слота при отключении и USB mass storage — поэтому disk image загрузочного
  носителя монтируется как RAM-диск, а не читается со флешки.
- Драйвер NVMe опрашиваемый (без IRQ), с многостраничными PRP-списками и четырьмя
  командами в полёте; FLUSH (opcode 0x00) выдаётся на границах WAL-транзакций. Пока не
  хватает: драйвер подключает только первый активный namespace (разделы нарезаются
  `drivers::partition` в `nvme0pN`), одна пара I/O-очередей (admin-команды
  сериализуются), нет timeout recovery (таймаут завершения помечает устройство failed до
  перезагрузки), нет SMART/self-test и shutdown notification, а невыровненные записи —
  read-modify-write на LBA, а не батчем.
  (Бывший пробел «embedded-tls встаёт на больших потоках» закрыт: многомегабайтная
  TLS-загрузка проходит end to end — `lx_tlsbig`, issue #19, — а живой `apt update` идёт
  по HTTPS.)
- Учётные данные процессов и права файловой системы реализованы (контракт
  `docs/security-model.md`): per-process real/effective/saved uid+gid в `CompatState`,
  семейство `set*id`, проверки owner/group/other на `open`, `execve`, мутациях каталогов,
  `chmod`/`chown` и `kill`, плюс write-back ext2 `i_mode`/`i_uid`/`i_gid`. Чего всё ещё
  нет и что названо в `SECURITY.md`: логина/аккаунтов (shell и все порождённые ядром
  процессы стартуют root), повышения через setuid-бинарники на `execve`, дополнительных
  групп/ACL/`fsuid`, наборов capabilities/seccomp/LSM/audit, проверок sticky-бита и
  владельца каталога; синтетические узлы (`/dev`, `/proc`, ramfs) не могут сохранить
  владельца и держат исторический per-process override для `stat`; SMEP — единственная
  аппаратная митигация (SMAP/KPTI отсутствуют). `access(2)` проверяет real ids; `stat`
  сообщает реальные метаданные inode.