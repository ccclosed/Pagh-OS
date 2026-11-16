# Issue #19 — «embedded-tls детерминированно встаёт на больших потоках»: доказательство (закрыт)

## Вердикт

**Стойка не воспроизводится.** Заявленный в #19 «детерминированный hang embedded-tls на
~12 MiB» на текущем стеке отсутствует: полный `Packages.gz` (`stable/main/amd64`,
**13 332 733 B** — ровно тот класс размера, где заявлена стойка) расшифровывается до конца
одним fetch'ем за считанные секунды, а живой `ppkg update` проходит по HTTPS целиком и
заканчивается `PASS`.

Исторический контекст: утверждение записано против ещё smoltcp-транспорта, тогда как
собственный TCP-стек заменил smoltcp и в тот же день получил фикс «window-update ACK после
чтения приложением» — ровно тот класс (окно/ack-фидбек), который #19 называет
подозреваемым. Транспорт (`TlsTransport`/`block_on`) с тех пор не менялся, но живой харнесс
всё это время принудительно ходил cleartext HTTP, поэтому утверждение не перепроверялось, а
только перепечатывалось доками. **Исторический механизм в текущем дереве реконструировать
нельзя** (старого стека нет) — он фиксируется как непроверяемое утверждение, опровергнутое
измерением.

## Прогоны (байтовое доказательство)

| # | Что | Команда | Результат | Время |
|---|---|---|---|---|
| 1 | живой `ppkg update` по HTTPS | `python3 tools/e2e.py live-update --allow-partial --timeout 1200 --serial-log serial_live.log` | `stage=done … body=13332733 raw_in=13356522 read_polls=1737 pending_polls=74 bytes_per_s=1837983` → `LIVE_APT_UPDATE: count=68825` → `.deb` по HTTPS (955 316 B) → `/mnt/usr/bin/busybox` запущен → `LXSELFTEST live_update PASS`, **exit 0** | 79.9 s |
| 2 | регресс `lx_tlsbig` | `python3 tools/e2e.py shell --features lx_tlsbig --script tools/e2e_tlsbig.script --timeout 1000 --settle 30` | `LXSELFTEST tls_big PASS (TLS 1.3; 13332733 bytes decrypted end to end)`, **exit 0** | 65.3 s |
| 3 | `lx_tlsbig` под CPU-голоданием (8 `yes`, гость TCG замедлен) | (8 × `yes` на хосте; гость TCG замедлен) | `stage=done … body=13332733 raw_in=13357293 read_polls=1821 pending_polls=91 bytes_per_s=1255436` → `PASS`, **exit 0** | 54.0 s |
| 4 | `lx_tlsbig` #2 (детерминизм) | то же, `--serial-log serial_tlsbig_2.log` | `body=13332733 raw_in=13356809 read_polls=2464 pending_polls=757 bytes_per_s=1277816` → **PASS**, **exit 0** | ~65 s |
| 5 | регресс `https_get` | `python3 tools/e2e.py local-mirror --serial-log serial_httpsget.log` | `stage=done host=deb.debian.org path=/debian/dists/stable/Release body=138612` → `LXSELFTEST https_get PASS`, `apt_e2e PASS`, **exit 0** | ~40 s |
| 6 | НЕГАТИВ (недоверенная цепочка) | локальный self-signed HTTPS на 10.0.2.2:8443 (SAN `deb.debian.org`, `10.0.2.2`), в госте `ppkg setmirror https://10.0.2.2:8443 /` + `ppkg update` | `Package_Fetcher(tls): stage=verify cause=InvalidCertificate (chain: NoAnchor)` → `stage=tls:handshake … cause=Tls (handshake/record failure; no application data was exchanged)`; `stage=done host=10.0.2.2` — **0 вхождений** | ~50 s |

Геометрия: заявленная точка стойки ~12 MiB (12 582 912 B) **меньше** фактически скачанных
13 332 733 B, т.е. трансфер прошёл её и дошёл до конца; во всех прогонах
`raw_in ≈ body + TLS-заголовки/теги/рукопожатие`, `pending_polls` — единицы процентов от
`read_polls` (короткие ожидания данных, не стойка).

Сверка тел (хост-сторона, независимо от гостя):
`https://deb.debian.org/debian/dists/stable/main/binary-amd64/Packages.gz` →
`Content-Length` = **13 332 733** = размер в `stage=done` у обоих прогонов; sha256 тела
`42c23aaca08e8225ed0449360724b92531952cbe4ce3bbf189298a7108f40a85`, 57 МБ после
распаковки, **68 825** станзов → ровно столько же распарсил живой прогон (`count=68825`).
`…/dists/stable/Release` → `Content-Length` = **138 612** = размер в `stage=done` у
`https_get`. `.deb` busybox: 955 316 B по HTTPS.

Итог: `LXSELFTEST tls_big PASS` дважды с N = 13 332 733 ≥ `TLS_BIG_MIN_BYTES`
(4 194 304); живой путь идёт по HTTPS (1 × `ppkg: Get https://…Packages.gz`, 0 ×
`ppkg: Get http://…`), `TLS verifier refusals` = 0; негативный кейс с недоверенной цепочкой
отклонён на `stage=verify` без обмена прикладными данными; `https_get` не деградировал.

## Что сделано кодом

- `src/net/tls.rs`: байт-трейс (`TLS_RAW_IN`/`TLS_READ_POLLS`/`TLS_READ_PENDING`), строка
  `Package_Fetcher(tls): stage=done … bytes_per_s=`, watchdog `stage=stall` с
  flow-control-снапшотом сокета, сохранение исходной `TlsError` и недобора байт при обрыве
  потока (раньше `Err(_) => break` схлопывал причину в голый `Incomplete`);
- `src/net/tcp.rs`: `TcpSock::diag()` — одна строка
  `state/rx/free/adv_wnd/rcv_nxt/wnd_update/ooo/snd_*`, чтобы stall-строка сразу называла
  слой;
- `src/selftest_lx.rs`: новый харнесс `run_tls_big_check` (feature `lx_tlsbig`) —
  регрессионный пин #19; живой `run_live_update_check` переведён на HTTPS и **отказывается
  стартовать на cleartext-конфиге**; ассерт busybox переведён на реальный member `.deb`
  `/usr/bin/busybox` (в архиве нет ни `/bin/busybox`, ни symlink-члена — проверено разбором
  `busybox-static_1.37.0-6+b9_amd64.deb`, member 2 024 544 B);
  `Cargo.toml`/`src/lib.rs`/`src/boot.rs` — фича `lx_tlsbig`;
- `src/pkg/apt.rs`: `DEFAULT_MIRROR_HOST`/`DEFAULT_MIRROR_BASE` — единственное место, где
  назван живой миррор (было два: дефолт apt и HTTP-зеркало харнесса);
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
и `LXSELFTEST tls_big PASS` / `LXSELFTEST live_update PASS (index … pkgs …)`. Если стойка
вернётся, `stage=stall` назовёт слой сам (сокет / транспорт / record-слой) — этого
диагностического канала у #19 не было.

## Ограничения

1. `tls_big` логирует только длину тела — ни хеша, ни явной сверки с `Content-Length`.
   Целостность подтверждается косвенно: длина == независимо снятому `Content-Length`, а в
   живом прогоне ещё и распаковкой + парсингом (68 825 станзов). Полезно добавить в
   `stage=done` хеш тела, чтобы «докатилось» и «целостно» различались внутри гостя.
2. `tools/mini_repo.py` не умеет TLS (ни `ssl`, ни сертификата), а якоря доверия ядра
   запинованы на 4 реальных корня — поэтому «локальный позитивный multi-MB HTTPS GET» на этом
   дереве неконструируем; локальный HTTPS-сервер может служить только негативным кейсом.
   Позитивный multi-MB кейс идёт против реального зеркала и требует сети.

## Изменённые файлы

`src/net/tls.rs`, `src/net/tcp.rs`, `src/net/README.md`, `src/selftest_lx.rs`,
`src/pkg/apt.rs`, `src/pkg/README.md`, `src/lib.rs`, `src/boot.rs`, `Cargo.toml`,
`README.md`, `AGENTS.md`, `SECURITY.md`, `tools/README.md`, `tools/e2e_live_update.ps1`,
`tools/e2e_tlsbig.script` (новый), `src/README.md`.