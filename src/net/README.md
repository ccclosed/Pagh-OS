# `src/net/` — собственный TCP/IP-стек

Полностью своя реализация (без сетевых крейтов): IPv4/IPv6, ARP/NDP, ICMP/ICMPv6, DHCP,
TCP с window scaling/SACK/Reno, UDP, DNS, HTTP(S). Всё изменяемое — в одном `Stack` под
одним спинлоком (`static NET: Spinlock<Option<Stack>>`).

Дисциплина локинга: **каждая точка входа исполняется в thread-контексте** (выделенный
net-поток или bounded locked-step помпы); из IRQ-контекста стек не трогается — вложенность
локов не может дедлокнуться. NIC (e1000) поллится, IRQ-обработчика нет.

## Файлы

| Файл | Роль |
|---|---|
| `mod.rs` | Корень: `Stack`, лок `NET`, init, poll-цикл, `net_thread`, публичный TCP/UDP API, DNS `resolve`, `ping`, `nc_echo`, demo echo-сервисы |
| `wire.rs` | Чистые wire-форматы + encode/decode: Ethernet, ARP, IPv4/IPv6 (+ext headers), ICMP/ICMPv6/NDP/RA, UDP, TCP (MSS/WS/SACK-опции), internet checksum. Юнит-тесты инлайн |
| `arp.rs` | Link-слой: `ArpTable` (IPv4, RFC 826), `NdpTable` (IPv6, RFC 4861); парковка кадров + rate-limited запросы |
| `ip.rs` | IPv4/IPv6: next-hop роутинг, TX-фрагментация v4, RX-реассемблинг обоих семейств, ICMP echo-ответы, SLAAC (`ra_apply`), разрешение ping-waiter'ов |
| `udp.rs` | Таблица UDP-сокетов + DHCP-клиент (`DhcpClient`, RFC 2131) |
| `tcp.rs` | Полный автомат TCP: `TcpSock`/`TcpTable`, sliding window со scaling, SACK, Reno, RTO-оценка, zero-window probing |
| `dns.rs` | Чистый билдер DNS-запросов + парсер A/AAAA (hardened, panic-free) |
| `http.rs` | Чистый билдер HTTP/1.1 GET + парсер головы ответа (`HeadParse`) |
| `http_fetch.rs` | Эффектный HTTP GET поверх стека для пакетного фетчера (`http_get`, `fetch_deb`) |
| `tls.rs` | HTTPS через `embedded-tls` (TLS 1.3) поверх стека: `TlsTransport`, мини-`block_on`, `KernelRng`. **VARIANT A: без проверки сертификатов** |
| `x509.rs` | Чистый строгий DER-ридер + минимальный X.509-парсер для верификатора: `parse_certificate` (TBS целиком как вход подписи, SPKI, SAN, basicConstraints, validity в **i64**), байты `signatureValue` + enforce «внешний AlgorithmIdentifier == TBS» (RFC 5280 §4.1.1.2) |
| `hostname.rs` | RFC 6125 hostname/SAN-матчинг: wildcard только целым левым лейблом SAN (ровно один лейбл хоста), host-side `*` — всегда отказ, IP-литералы — по октетам |
| `tls_verify.rs` | Диспетчер подписей сертификатов: RSASSA-PKCS1-v1_5 (SHA-256/384/512, мин. модуль 2048 бит), ECDSA P-256/P-384, Ed25519; RSA-PSS явно отклоняется. Крейты `rsa`/`p384`/`ed25519-dalek` (pure-Rust, vendored) |
| `tls_chain.rs` | Trust anchors (raw DER Name + ключ) + построитель цепочки: leaf → intermediates → anchor по **байт-равенству** issuer/subject; подпись каждого звена через `tls_verify`; `cA=TRUE` обязателен у всех эмитентов (отсутствие basicConstraints — отказ); validity всех сертификатов; **clock gate**: `now < 2025-01-01` → `ChainError::Clock` (незаданный RTC = «не знаю который час» = отказ). Неудавшийся одноимённый intermediate не затеняет якоря (откат к anchor set всегда); при полном отказе возвращается **первая** ошибка name-match по порядку сообщения. `ChainError::{NoAnchor, IncompleteChain, Expired, NotYetValid, Clock, NotCa, Verify}` |
| `ca_bundle.rs` | **Сгенерированный** (tools/gen_ca_bundle.py) trust-anchor бандл: метка + raw DER самоподписанных корней (ISRG Root X1/X2 — цепочки Let's Encrypt, которыми подписан deb.debian.org; GTS R1/R4 — CDN). Обновлять только перегенерацией, файл коммитится; P48 доказывает валидность каждого якоря kernel-парсером и диспетчером подписей |
| `tls_auth.rs` | Однократное server-authentication-решение (чистый слой, ещё не подключён к handshake): `authenticate_server(entries, host, anchors, now)` = chain verify (через `tls_chain`) + hostname-авторизация **только по SAN** (dNSName для DNS-цели, iPAddress по октетам для IP-литерала; CN-fallback отсутствует, отсутствие/битость SAN — отказ) + экспорт leaf-ключа (`LeafKey`, owned) для шага CertificateVerify. Плюс точное сообщение RFC 8446 §4.4.3 (`certificate_verify_message`) и диспетчер `Tls13Scheme` (ECDSA P-256/P-384, Ed25519, RSA-PSS — PSS в TLS 1.3 для RSA-ключей **обязателен** и параметров не имеет; это осознанно НЕ то же решение, что отказ от PSS-OID в X.509-цепочках). Хост не задан → `NoHostname`; ключ вне supported surface → `UnsupportedLeafKey` |
| `progress.rs` | Однострочный `\r`-прогресс загрузок |

## Ключевые символы

- `init() -> Result<(), NetError>` — PCI enumerate + attach e1000 + неконфигурированный `Stack`;
  без NIC — `Err(NoDevice)`, boot продолжается.
- `poll()`, `net_thread()` (спавнится из `boot.rs`), `Stack::step()`, `Stack::input_frame()`.
- TCP: `tcp_connect / tcp_connect_buffered / tcp_established / tcp_send_chunk / tcp_recv_chunk /
  tcp_rx_at_eof / tcp_close`.
- UDP: `udp_open / udp_sendto / udp_recvfrom / udp_remove`; эфемерные порты 49152..=65535.
- `resolve(hostname) -> Option<IpAddr>` — литералы сразу; затем **A первым, AAAA фоллбэк**
  (slirp отвечает AAAA, но не роутит v6 — регрессионный фикс); ретрансмит 100 мс, дедлайн ~700 мс.
- `ping(addr) -> Option<u64>`, `nc_echo(remote, payload)`, `udp_echo_enable(port)`,
  `tcp_echo_listen(port)`, `ip_config()`, `ip6_config()`, `dns_server()`.
- `wire`: `Mac`, `Ipv4Addr/Ipv4Cidr`, `Ipv6Addr/Ipv6Cidr` (RFC 5952 Display, v4-mapped,
  solicited-node, EUI-64), `IpAddr`, `IpEndpoint`; парсеры литералов; `checksum`;
  `tcp_parse` (опции 2/3/4/5, до 4 SACK-блоков), `tcp_build` (pseudo-header).
- `tcp::State` — полный набор из 11 состояний; `LISTEN_BACKLOG=16` half-open (SYN-flood cap).
- `http_fetch::FetchError::{NoNetwork, ConnectTimeout, Status, UnknownLength, Incomplete, ReadTimeout, Tls}`;
  `MAX_TOTAL=32 MiB` — потолок любой загрузки.

## Как работает

### RX-поток
`net_thread` → `poll()` → `NET.lock()` → `Stack::step()`:
1. Дренаж RX-кадров e1000 в стековый буфер 2048; `input_frame` парсит Ethernet, фильтрует
   адресата (свой unicast / broadcast / 33:33 multicast; NIC в promiscuous), диспетчер по ethertype.
2. Старение соседей/фрагментов; разовый IPv6 Router Solicitation.
3. `DhcpClient::drive` (лизина применяется по ACK).
4. Таймаут DHCP (~5 c) → статический QEMU-фоллбэк `10.0.2.15/24`, gw `10.0.2.2` — один раз.
5. `TcpTable::poll_all` (TX/ретрансмиты) + demo echo-сервисы (порты UDP/TCP 7).

### TX-поток
Приложение → tcp/udp API → `ip::output` → выбор next-hop (broadcast/in-subnet/gateway;
v6: multicast MAC, link-local/on-link, RA-роутер; v6 на TX не фрагментируется) →
ARP/NDP lookup; не резолвится → парковка кадра + rate-limited запрос → `e1000::send`.

### IPv4/IPv6
- IPv4: приём только своих/broadcast адресов; реассемблер ≤4 буферов, выравнивание 8 байт;
  диспетчер по proto на ICMP/UDP/TCP.
- IPv6: NDP/RA требуют **hop limit 255** и link-local источника (анти-спуфинг);
  SLAAC: глобальный адрес = prefix ‖ EUI-64 (prefix ≤ /64), + unsolicited NA.

### TCP
- Window scaling (RFC 7323, cap 14, только если оба SYN несут опцию), MSS-согласование
  (`MSS=1460`, peer-дефолт 536), SACK-Permitted + отправка/приём SACK-блоков.
- Reno: slow start + congestion avoidance, IW = 4·MSS, fast retransmit на 3 dup-ACK.
- RTO: Jacobson/Karels, Karn (ретранcмит отменяет RTT-сэмпл), backoff 300 мс → 30 c;
  `CONNECT_MAX_RETRIES=5`, `DATA_MAX_RETRIES=15`.
- Delayed ACK: каждые 2 сегмента + 16-тиковый tail guard; window-update ACK при освобождении
  ≥ rx_cap/4.
- Zero-window: persist-зонд **байта на snd_una** (зонд snd_nxt бы корраптил поток).
- RST-валидация (RFC 5961 упрощённо): слепые RST дропаются; out-of-order очередь `OOO_CAP=16`
  с генерацией dup-ACK+SACK; TIME-WAIT укорочен до 2 c.

### Сокеты для Linux-compat
`src/arch/x86_64/linux/inet_sock.rs`: `InetTcp` (ленивый слот до connect) и `InetUdp`
(`peer` выставляется connect(2) — так glibc resolver шлёт DNS). `parse_sockaddr_ep`
принимает `sockaddr_in`/`sockaddr_in6`, сворачивая `::ffff:a.b.c.d`. Блокирующие вызовы —
через `yield_current()`.

## Зависимости

- **От:** `drivers::pci`, `drivers::e1000`, `sync::spinlock`, `task::scheduler`,
  `arch::apic::TICK_HZ`, `security::entropy::secure_u64`; крейты `embedded-tls`,
  `embedded-io(-async)`, `rand_core` (только tls.rs).
- **На неё:** `inet_sock.rs` (AF_INET syscall'ы), `pkg::apt` + `pkg::install_fs`,
  `shell::commands.rs` (ping/nc/pkg/ifconfig), `selftest_lx.rs`, `boot.rs`.

## Грабли и безопасность

- **TLS — VARIANT A**: TLS 1.3 БЕЗ проверки цепочки/hostname/expiry (`UnsecureProvider`).
  Шифровано, но MITM-аемо. Огромный баннер в доке модуля + разовый runtime `warn!`.
  RNG (`KernelRng` над RDSEED/RDRAND) fail-closed.
- ARP-обучение ограничено: unsolicited-ответы и чужие запросы кэш не наполняют.
- DNS transaction ID из `random_u32` (не из тиков — тиковые коллидировались), ответ обязан
  побайтово эхоить вопрос. DHCP xid регенерируется случайно.
- `udp.demux` дропает при полной очереди (ICMP port-unreachable нет — задокументированный gap);
  датаграммы режутся до 2048.
- Парсер v6 ext-заголовков bounds-check'ит (крафтеные пакеты раньше читали OOB).
- QEMU-допущения захардкожены: `10.0.2.15/2.2/2.3`.
