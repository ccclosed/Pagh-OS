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
`tools/e2e_live_update.ps1`, `tools/e2e_tlsbig.script` (новый).
