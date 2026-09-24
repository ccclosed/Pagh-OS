# Issue #19 — «embedded-tls детерминированно встаёт на больших потоках»: доказательство (закрыт)

**Статус:** верификация/устранение (задача `t18`, kind=work). База — HEAD
`cd542bbceeda2a85183e6f5ad290e0fa62ce397f` («Merge pull request #38 …»).
Рабочее дерево во время прогонов параллельно правили другие задачи, поэтому
авторитетные прогоны сделаны в **отдельном чистом дереве** (`/home/oleg/t18-clean`:
`git clone --local` от HEAD + ТОЛЬКО диффа t18, `git status` — 14 файлов из
§«Изменённые файлы»). У каждого прогона указан sha256 загруженного ELF и
`git_head`.

## Вердикт

**Стойка не воспроизводится.** Заявленный в #19 «детерминированный hang
embedded-tls на ~12 MiB» на текущем стеке отсутствует: полный `Packages.gz`
(`stable/main/amd64`, **13 332 733 B** — ровно тот класс размера, где заявлена
стойка) расшифровывается до конца одним fetch'ем за считанные секунды, а живой
`apt update` проходит по HTTPS целиком и заканчивается `PASS`.

Ключевая находка по истории: утверждение записано **2026-07-22** (`891e2c9`,
ещё «VARIANT-A TLS transport» **на smoltcp**), тогда как собственный TCP-стек
заменил smoltcp **2026-08-24** (`369c408`) и в тот же день лёг фикс «window-update
ACK после чтения приложением» (x40, `0fb66e5`) — то есть ровно тот класс
(окно/ack-фидбек), который #19 называет подозреваемым. Транспорт
(`TlsTransport`/`block_on`) с тех пор не менялся; но живой харнесс всё это время
принудительно ходил cleartext HTTP (`set_mirror("http://…")`), поэтому
утверждение не перепроверялось, а только перепечатывалось доками (синк
2026-09-13, `ff0f5f0`). **Исторический механизм в текущем дереве
реконструировать нельзя** (старого стека нет) — здесь он не угадывается, а
фиксируется как непроверяемое утверждение, опровергнутое измерением.

## Прогоны (байтовое доказательство)

| # | Что | Команда (в `/home/oleg/t18-clean`) | ELF sha256 (нач.) | Результат | Время |
|---|---|---|---|---|---|
| 1 | живой `apt update` по HTTPS (чистое дерево) | `python3 tools/e2e.py live-update --allow-partial --timeout 1200 --serial-log serial_live.log` | `382a5285a9118917` | `stage=done … body=13332733 raw_in=13356522 read_polls=1737 pending_polls=74 bytes_per_s=1837983` → `LIVE_APT_UPDATE: count=68825` → `.deb` по HTTPS (955 316 B) → `/mnt/usr/bin/busybox` запущен → `LXSELFTEST live_update PASS`, **exit 0** | 79.9 s |
| 2 | регресс `lx_tlsbig` (чистое дерево) | `python3 tools/e2e.py shell --features lx_tlsbig --script tools/e2e_tlsbig.script --timeout 1000 --settle 30` | `c6bff40c9c44ec47` | `LXSELFTEST tls_big PASS (TLS 1.3; 13332733 bytes decrypted end to end)`, **exit 0** | 65.3 s |
| 3 | `lx_tlsbig` под CPU-голоданием (8 `yes`, гость TCG замедлен) | `bash /home/oleg/t18-load-test.sh` | — | `stage=done … body=13332733 raw_in=13357293 read_polls=1821 pending_polls=91 bytes_per_s=1255436` → `PASS`, **exit 0** | 54.0 s |
| 4 | живой `apt update` по HTTPS (рабочее дерево, до чистого) | `python3 tools/e2e.py live-update --allow-partial --timeout 1200` | `5747fb5cfdb7ebd1` | `body=13332733 … bytes_per_s=2006430` → `count=68825` → `PASS`, **exit 0** | 119.6 s |
| 5 | `lx_tlsbig` (рабочее дерево, до чистого) | `python3 tools/e2e.py shell --features lx_tlsbig --script tools/e2e_tlsbig.script` | `a71feff33a3a2028` | `stage=done … body=13332733 raw_in=13357139 read_polls=1967 pending_polls=215 bytes_per_s=2134603` → `PASS`, **exit 0** | 62.8 s |
| 6 | гейты AGENTS.md (чистое дерево) | `cargo build`; `python3 tools/build.py build --release`; `cargo fmt --all -- --check`; `python3 tools/check_safety.py`; `python3 tools/host_tests.py` | — | 0 ошибок; fmt пусто; `safety policy: OK`; `test result: ok. 212 passed; 0 failed` | см. `tools/README.md` |

Геометрия: заявленная точка стойки ~12 MiB (12 582 912 B) **меньше** фактически
скачанных 13 332 733 B, т.е. трансфер прошёл её и дошёл до конца; во всех прогонах
`raw_in ≈ body + TLS-заголовки/теги/рукопожатие`, `pending_polls` — единицы процентов
от `read_polls` (это короткие ожидания данных, не стойка).

## Что сделано кодом (по факту, а не «на всякий случай»)

- `src/net/tls.rs`: байт-трейс (`TLS_RAW_IN`/`TLS_READ_POLLS`/`TLS_READ_PENDING`),
  строка `Package_Fetcher(tls): stage=done … bytes_per_s=`, watchdog `stage=stall`
  с flow-control-снапшотом сокета, и сохранение исходной `TlsError` +
  недобора байт при обрыве потока (раньше `Err(_) => break` схлопывал причину в
  голый `Incomplete`);
- `src/net/tcp.rs`: `TcpSock::diag()` — одна строка `state/rx/free/adv_wnd/
  rcv_nxt/wnd_update/ooo/snd_*`, чтобы stall-строка сразу называла слой;
- `src/selftest_lx.rs`: новый харнесс `run_tls_big_check` (feature `lx_tlsbig`) —
  регрессионный пин #19; живой `run_live_update_check` переведён на HTTPS и
  **отказывается стартовать на cleartext-конфиге**; ассерт busybox переведён на
  реальный member `.deb` `/usr/bin/busybox` (в архиве нет ни `/bin/busybox`, ни
  symlink-члена — проверено разбором `busybox-static_1.37.0-6+b9_amd64.deb`,
  member 2 024 544 B); `Cargo.toml`/`src/lib.rs`/`src/boot.rs` — фича `lx_tlsbig`;
- `src/pkg/apt.rs`: `DEFAULT_MIRROR_HOST`/`DEFAULT_MIRROR_BASE` — единственное
  место, где назван живой миррор (было два: дефолт apt и HTTP-зеркало харнесса);
- `vendor/embedded-tls` **не правился**: `git status vendor/` пуст, sha256
  `src/connection.rs` совпадает с `.cargo-checksum.json`
  (`0137af6e3ab8aaaa…`), латчи `certificate_received`/`certificate_verified` на месте.

## Воспроизведение

```sh
# 1. регрессионный пин #19 (один много-МБ HTTPS GET, без parse)
python3 tools/e2e.py shell --features lx_tlsbig --script tools/e2e_tlsbig.script \
    --timeout 1000 --settle 30

# 2. полный живой путь по HTTPS (индекс + install + запуск busybox)
python3 tools/e2e.py live-update --allow-partial --timeout 1200
```

Ожидаемые строки-доказательства: `Package_Fetcher(tls): stage=done … body=13332733 …`
и `LXSELFTEST tls_big PASS` / `LXSELFTEST live_update PASS (index … pkgs …)`.
Если стойка вернётся, `stage=stall` назовёт слой сам (сокет / транспорт /
record-слой) — этого диагностического канала у #19 не было.

## Изменённые файлы

`src/net/tls.rs`, `src/net/tcp.rs`, `src/net/README.md`, `src/selftest_lx.rs`,
`src/pkg/apt.rs`, `src/pkg/README.md`, `src/lib.rs`, `src/boot.rs`, `Cargo.toml`,
`README.md`, `AGENTS.md`, `SECURITY.md`, `tools/README.md`,
`tools/e2e_live_update.ps1`, `tools/e2e_tlsbig.script` (новый),
`src/README.md` (repair по finding t19-doc-1).

## Независимая верификация (t19, 2026-09-17)

Проверено заново своими прогонами и своими сверками тел (свежие ELF, свои
serial-логи; сырые артефакты — `.cache/t19_evidence/`). Дерево в момент прогонов
параллельно правили другие задачи, поэтому у каждого прогона свой sha256 ELF.

| Прогон | Команда | ELF (нач.) | Ключевые строки | Время |
|---|---|---|---|---|
| `tls_big` #1 | `python3 tools/e2e.py shell --features lx_tlsbig --script tools/e2e_tlsbig.script --timeout 1000 --settle 30 --serial-log serial_t19_tlsbig_1.log` | `4611b4dec47010f5` | `stage=done … body=13332733 raw_in=13357100 read_polls=2215 pending_polls=471 bytes_per_s=1577091` → `LXSELFTEST tls_big PASS (TLS 1.3; 13332733 bytes decrypted end to end)`, **exit 0** | 69.7 s |
| `tls_big` #2 (детерминизм) | то же, `--serial-log serial_t19_tlsbig_2.log` | тот же билд | `body=13332733 raw_in=13356809 read_polls=2464 pending_polls=757 bytes_per_s=1277816` → **PASS**, **exit 0** | ~65 s |
| живой `apt update` по HTTPS | `python3 tools/e2e.py live-update --timeout 1800 --serial-log serial_t19_live.log` | `bf73be0951415513` | `apt: Get https://deb.debian.org/…/Packages.gz` → `stage=done … body=13332733` → `LIVE_APT_UPDATE: count=68825` → `.deb` по HTTPS `body=955316` → `compat pid=4 started … from '/mnt/usr/bin/busybox'` → `LXSELFTEST live_update PASS (index 68825 pkgs; installed 1; spawned busybox pid=4)`, **exit 0** | 74.2 s |
| регресс `https_get` | `python3 tools/e2e.py local-mirror --serial-log serial_t19_httpsget.log` | — | `stage=done host=deb.debian.org path=/debian/dists/stable/Release body=138612` → `LXSELFTEST https_get PASS`, `apt_e2e PASS`, **exit 0** | ~40 s |
| НЕГАТИВ (недоверенная цепочка) | локальный self-signed HTTPS-сервер на 10.0.2.2:8443 (SAN `deb.debian.org`, `10.0.2.2`), в госте `apt setmirror https://10.0.2.2:8443 /` + `apt update` | default-билд | `Package_Fetcher(tls): stage=verify cause=InvalidCertificate (chain: NoAnchor)` → `stage=tls:handshake … cause=Tls (handshake/record failure; no application data was exchanged)`; `stage=done host=10.0.2.2` — **0 вхождений** | ~50 s |

Сверка тел (хост-сторона, независимо от гостя):
`https://deb.debian.org/debian/dists/stable/main/binary-amd64/Packages.gz` →
`Content-Length` = **13 332 733** = размер в `stage=done` у обоих прогонов `tls_big`
и у живого прогона; sha256 тела
`42c23aaca08e8225ed0449360724b92531952cbe4ce3bbf189298a7108f40a85`, 57 МБ после
распаковки, **68 825** станзов → ровно столько же распарсил живой прогон
(`count=68825`). `…/dists/stable/Release` → `Content-Length` = **138 612** = размер
в `stage=done` у `https_get`. `.deb` busybox: 955 316 B по HTTPS.

Итог: `LXSELFTEST tls_big PASS` дважды с N = 13 332 733 ≥ `TLS_BIG_MIN_BYTES`
(4 194 304) — точка «~12 MiB» пройдена с запасом; живой путь идёт по HTTPS
(1 × `apt: Get https://…Packages.gz`, 0 × `apt: Get http://…`), `TLS verifier
refusals` = 0, строки `FAIL installed busybox binary not found` нет (ассерт на
`/mnt/usr/bin/busybox`, бинарь реально запущен); негативный кейс с недоверенной
цепочкой отклонён на `stage=verify` без обмена прикладными данными; `https_get`
не деградировал. **Верификация: подтверждаю.**

Замечания верификатора (не блокируют подтверждение, требуют правки до закрытия #19):

1. **finding (doc):** `src/README.md:98` всё ещё утверждает «`run_live_update_check()`
   (`lx_livetest`) — полный live `apt update` (**HTTP, не HTTPS — у embedded-tls
   детерминированный хэнг на больших стримах**)» — ровно тот устаревший тезис,
   который #19 опровергает; там же (строка 91) список feature-гейтов не упоминает
   `lx_tlsbig`. Три файла из задания (root `README.md`, `SECURITY.md`,
   `src/net/README.md`) чисты. Замена: «полный live `apt update` по HTTPS (TLS 1.3;
   cleartext-конфиг — FAIL), assert count ≥ 50 000» + `lx_tlsbig` в списке гейтов.
   **Исправлено** этим (repair) коммитом ветки `net/tls-large-stream`: `src/README.md`
   теперь говорит «полный live `apt update` по HTTPS (TLS 1.3; cleartext-конфиг — FAIL)»,
   `lx_tlsbig` добавлен в ОБА списка feature-гейтов (`lib.rs`-карта, строка 32, и раздел
   `selftest_lx.rs`), а `run_tls_big_check()` описан как регресс-пин #19.
2. **ограничение:** `tls_big` логирует только длину тела — ни хеша, ни явной
   сверки с `Content-Length`. Сейчас целостность подтверждается косвенно: длина ==
   независимо снятому `Content-Length`, а в живом прогоне ещё и распаковкой +
   парсингом (68 825 станзов). Полезно добавить в `stage=done` хеш тела (или явную
   сверку с `Content-Length`), чтобы «докатилось» и «целостно» различались внутри гостя.
3. **ограничение:** `tools/mini_repo.py` не умеет TLS (ни `ssl`, ни сертификата), а
   якоря доверия ядра запинованы на 4 реальных корня — поэтому «локальный
   позитивный multi-MB HTTPS GET» на этом дереве неконструируем; локальный
   HTTPS-сервер может служить только негативным кейсом (что и сделано). Позитивный
   multi-MB кейс идёт против реального зеркала — для #19 это строже тестового CA,
   но требует сети.
