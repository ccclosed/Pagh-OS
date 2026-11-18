//! Сетевой subsystem — СОБСТВЕННЫЙ TCP/IP-стек ядра.
//!
//! Это написанная с нуля замена прежней связки smoltcp + virtio-net. Слои,
//! снизу вверх:
//!
//!   * [`link`] — канальный уровень: e1000 или USB RNDIS (поллинг, без IRQ).
//!   * [`arp`] — решения по Ethernet-фреймингу + ARP-кэш/разрешение.
//!   * [`ip`] — IPv4-маршрутизация/фрагментация/сборка + ICMP echo.
//!   * [`udp`] — UDP-сокеты + DHCP-клиент.
//!   * [`tcp`] — TCP state machine, окна, таймеры, congestion control.
//!   * [`wire`] — чистые wire-форматы и checksum (host-тестируемо).
//!
//! Всё изменяемое сетевое состояние живёт в [`Stack`] под одним спинлоком
//! ([`NET`]). Дисциплина локинга: каждая точка входа выполняется в THREAD-
//! контексте (выделенный net-поток или потоки shell/сисколлов, ведущие bounded
//! locked-step насосы). Ничто никогда не трогает эти структуры из контекста
//! прерываний, так что вложенность локов не может дедлокнуться.
//!
//! Порядок поднятия:
//!   1. [`init`] перечисляет PCI и выбирает канал: e1000, иначе USB RNDIS.
//!      Если xHCI-контроллер есть, а телефон ещё не перечислен, стек строится
//!      ненастроенным и ждёт появления канала в шаге [`Stack::step`].
//!   2. [`net_thread`] многократно вызывает [`poll`]: RX-кадры вычерпываются и
//!      диспетчеризуются, работает DHCP-клиент (с откатом на статический адрес
//!      QEMU user-net `10.0.2.15/24` gw `10.0.2.2` после таймаута), выполняются
//!      шаги TCP-вывода/ретрансмиссии и кормятся demo echo-сервисы.
//!   3. ICMP echo отвечает нативно [`ip`]; `ping` использует его end-to-end.
//!
//! Если ни e1000, ни xHCI нет, [`init`] возвращает `Err(NetError::NoDevice)`;
//! вызывающий пишет предупреждение, и загрузка продолжается (R17.3).

pub mod arp;
pub mod ca_bundle;
pub mod dns;
pub mod hostname;
pub mod http;
pub mod http_fetch;
pub mod ip;
pub mod link;
pub mod progress;
pub mod tcp;
pub mod tls;
pub mod tls_auth;
pub mod tls_chain;
pub mod tls_verify;
pub mod udp;
pub mod wire;
pub mod x509;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub use wire::{IpAddr, IpEndpoint, Ipv4Addr, Ipv4Cidr, Ipv6Addr};

use crate::net::link::Link;
use crate::sync::spinlock::Spinlock;
use crate::task::scheduler;
use crate::{info, warn};

/// Хэндл транспортного сокета (TCP или UDP — обе таблицы индексируются с 0).
pub type SocketHandle = usize;

pub use wire::Mac;

/// Ошибки сетевого subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    /// Пригодный NIC не найден, сеть недоступна.
    NoDevice,
    /// NIC найден, но инициализация упала, или сокетная операция не может
    /// завершиться.
    DeviceInit,
}

/// Статический fallback-адрес, когда DHCP не завершился (R13.3). Для e1000 это
/// общеизвестные значения QEMU user-mode networking.
const FALLBACK_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const FALLBACK_PREFIX: u8 = 24;
const FALLBACK_GW: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

/// Fallback для USB-тетеринга телефона: подсеть, которую раздаёт Android по
/// умолчанию. Используется, только если DHCP не ответил; обычно телефон выдаёт
/// адрес по DHCP и эти значения не нужны.
const RNDIS_FALLBACK_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 42, 21);
const RNDIS_FALLBACK_GW: Ipv4Addr = Ipv4Addr::new(192, 168, 42, 129);

/// QEMU user-mode networking всегда отвечает DNS на этом адресе, так что это
/// резолвер последней надежды (R13.3).
const QEMU_DNS: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);

/// Число подсказок `spin_loop` на шаг между шагами насоса DNS (как у `nc_echo`).
const DNS_STEP_SPIN: u32 = 20_000;
/// Таймаут DNS-разрешения (~700 мс).
const DNS_TIMEOUT_TICKS: u64 = crate::arch::x86_64::apic::ms_to_ticks(700);
/// Интервал ретрансмиссии DNS-запроса: если ответ не пришёл за столько тиков,
/// тот же запрос (тот же id) посылается снова до дедлайна.
const DNS_RESEND_TICKS: u64 = crate::arch::x86_64::apic::ms_to_ticks(100);
/// Жёсткий верхний предел итераций насоса DNS, независимый от часов.
const DNS_MAX_STEPS: u32 = 20_000;
/// Таймаут DHCP (~5 с) до применения статического fallback (R13.3).
const DHCP_TIMEOUT_TICKS: u64 = crate::arch::x86_64::apic::ms_to_ticks(5_000);
/// RNDIS-телефону даётся больше времени: первый bulk-обмен идёт через
/// USB-транзакции, а DHCP-клиент стартует сразу после перечисления. 5 секунд
/// на медленном контроллере/TCG — это ещё не «DHCP не отвечает».
const RNDIS_DHCP_TIMEOUT_TICKS: u64 = crate::arch::x86_64::apic::ms_to_ticks(15_000);

/// Текущая IP-конфигурация, показываемая `ifconfig` (см. [`ip_config`]).
#[derive(Debug, Clone, Copy)]
pub struct IpConfig {
    /// Назначенный адрес + префикс (например `10.0.2.15/24`).
    pub addr: Ipv4Cidr,
    /// Шлюз по умолчанию.
    pub gateway: Ipv4Addr,
    /// Аппаратный (MAC) адрес NIC.
    pub mac: Mac,
}

/// Результат одноразового клиентского обмена [`nc_echo`].
#[derive(Debug, Clone)]
pub enum NcResult {
    /// Соединение не установлено (refused / unreachable / таймаут до состояния
    /// `Established`).
    Failed,
    /// Соединение установлено; `bytes` содержит всё, что пришло в ответ
    /// (для echo-сервера это отражённый отправленный payload).
    Echoed(Vec<u8>),
}

/// Всё изменяемое сетевое состояние под одним локом.
///
/// Подмодули обращаются к этим полям напрямую (`pub(crate)`), что держит слои
/// плоскими: каждый слой — простая функция над `&mut Stack`.
pub struct Stack {
    // Интерфейс
    pub(crate) mac: Mac,
    /// Активный канал; `None` — стек построен, но RNDIS ещё не перечислен.
    pub(crate) link: Option<Link>,

    // Конфигурация адреса — IPv4 (DHCP или статический fallback)
    pub(crate) cidr: Option<Ipv4Cidr>,
    pub(crate) gateway: Option<Ipv4Addr>,
    pub(crate) dns_servers: Vec<Ipv4Addr>,
    pub(crate) configured: bool,
    pub(crate) deadline_tick: Option<u64>,
    pub(crate) dhcp_fallback_used: bool,

    // Конфигурация адреса — IPv6 (link-local всегда; глобальный через SLAAC)
    pub(crate) ll_addr6: Ipv6Addr,
    pub(crate) cidr6: Option<wire::Ipv6Cidr>,
    pub(crate) v6_gateway: Option<Ipv6Addr>,
    /// Ставится, когда Router Solicitation уже ушёл в этой загрузке.
    pub(crate) rs_sent: bool,

    // Выделение эфемерных портов
    pub(crate) next_eph: u16,

    // Слои
    pub(crate) arp: arp::ArpTable,
    pub(crate) nd: arp::NdpTable,
    pub(crate) reasm: ip::Reassembler,
    pub(crate) reasm6: ip::Reassembler,
    pub(crate) udp: udp::UdpTable,
    pub(crate) tcp: tcp::TcpTable,
    pub(crate) dhcp: udp::DhcpClient,
    pub(crate) pings: Vec<ip::PingWaiter>,

    // Demo-сервисы
    pub(crate) udp_echo_handle: Option<SocketHandle>,
    pub(crate) udp_echo_port: u16,
    pub(crate) tcp_echo_port: Option<u16>,

    // Счётчик IP identification для фрагментации
    pub(crate) next_ip_ident: u16,
}

static NET: Spinlock<Option<Stack>> = Spinlock::new(None);

/// Свежий псевдослучайный u32 для ISS/xid. Аппаратная энтропия, когда есть,
/// иначе tick-mixed xorshift (здесь никогда не security-critical).
pub(crate) fn random_u32() -> u32 {
    static STATE: Spinlock<u32> = Spinlock::new(0x9E37_79B9);
    let mut g = STATE.lock();
    let mut x = *g ^ crate::security::entropy::secure_u64().unwrap_or(scheduler::ticks()) as u32;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *g = x;
    x
}

/// Выделить эфемерный локальный порт, не занятый сейчас ни одним TCP- или
/// UDP-сокетом. ДОЛЖНО вызываться с уже взятым `NET` (см.
/// [`ephemeral_port_locked`]); оставлено как документация дисциплины.
fn _ephemeral_port_requires_held_lock() {}

// ─── Init ────────────────────────────────────────────────────────────────────

/// Построить ненастроенный стек над выбранным каналом (или без него).
///
/// `mac_bytes` — нули, пока RNDIS не перечислился: адрес интерфейса появится в
/// [`Stack::step`] вместе с каналом, и до этого ни один кадр не уйдёт.
fn build_stack(link: Option<Link>, mac_bytes: [u8; 6]) -> Stack {
    let mac = Mac(mac_bytes);
    Stack {
        mac,
        link,
        cidr: None,
        gateway: None,
        dns_servers: Vec::new(),
        configured: false,
        deadline_tick: None,
        dhcp_fallback_used: false,
        ll_addr6: wire::Ipv6Addr::link_local_from_mac(mac),
        cidr6: None,
        v6_gateway: None,
        rs_sent: false,
        next_eph: 49152,
        arp: arp::ArpTable::new(),
        nd: arp::NdpTable::new(),
        reasm: ip::Reassembler::new(),
        reasm6: ip::Reassembler::new(),
        udp: udp::UdpTable::new(),
        tcp: tcp::TcpTable::new(),
        dhcp: udp::DhcpClient::new(),
        pings: Vec::new(),
        udp_echo_handle: None,
        udp_echo_port: 0,
        tcp_echo_port: None,
        next_ip_ident: 1,
    }
}

/// Инициализировать сетевой subsystem (R13.1): выбрать канал и построить
/// (пока ненастроенный) стек. Получение адреса происходит в [`poll`], когда
/// заработает net-поток.
///
/// Порядок выбора: e1000, иначе USB RNDIS, иначе — если есть xHCI-контроллер —
/// стек без канала: телефон может быть подключён уже после этой точки (поток
/// `usb` перечисляет его позже), и `Stack::step` подхватит канал, как только он
/// появится. `Err(NetError::NoDevice)` остаётся для машины без обоих драйверов;
/// вызывающий логирует и продолжает загрузку (R17.3).
pub fn init() -> Result<(), NetError> {
    let devices = crate::drivers::pci::enumerate();
    let attached = match crate::drivers::e1000::attach(&devices) {
        Ok(mac_bytes) => Some((Link::E1000, mac_bytes)),
        Err(_) => link::attach(),
    };
    if let Some((kind, mac_bytes)) = attached {
        link::set(kind);
        *NET.lock() = Some(build_stack(Some(kind), mac_bytes));
        info!(
            "net: stack built over {} (awaiting DHCP, static fallback after timeout)",
            kind.name()
        );
        return Ok(());
    }
    if crate::drivers::xhci::present() {
        *NET.lock() = Some(build_stack(None, [0u8; 6]));
        info!("net: no link yet; awaiting usb-rndis enumeration");
        return Ok(());
    }
    Err(NetError::NoDevice)
}

/// Поздняя попытка построить стек, если при загрузке канала не было вовсе.
///
/// Нужна драйверу, который появился после `net::init` (RNDIS на уже
/// перечисленном xHCI): поток сети крутится всегда и обязан подхватить канал,
/// а не остаться навсегда с пустым `NET`.
fn ensure_stack() {
    if NET.lock().is_some() {
        return;
    }
    if let Some((kind, mac_bytes)) = link::attach() {
        link::set(kind);
        *NET.lock() = Some(build_stack(Some(kind), mac_bytes));
        info!("net: late stack built over {}", kind.name());
    }
}

// ─── Цикл poll ───────────────────────────────────────────────────────────────

impl Stack {
    /// Продвинуть весь стек на один шаг. Вызывается [`poll`] и (внутри крейта)
    /// bounded locked-step насосами в `http_fetch`/`tls`.
    pub(crate) fn step(&mut self) {
        // 0. Канал мог появиться после init: RNDIS перечисляется потоком `usb`
        //    уже после первого шага сети. До появления канала шаг не делает
        //    ничего: DHCP без MAC и без передачи бессмыслен.
        if self.link.is_none() {
            let Some((kind, mac_bytes)) = link::attach() else {
                return;
            };
            link::set(kind);
            self.link = Some(kind);
            self.mac = Mac(mac_bytes);
            self.ll_addr6 = wire::Ipv6Addr::link_local_from_mac(self.mac);
            self.cidr = None;
            self.gateway = None;
            self.configured = false;
            self.deadline_tick = None;
            self.dhcp_fallback_used = false;
            self.dhcp = udp::DhcpClient::new();
            info!(
                "net: {} attached, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}; starting DHCP",
                kind.name(),
                self.mac.0[0],
                self.mac.0[1],
                self.mac.0[2],
                self.mac.0[3],
                self.mac.0[4],
                self.mac.0[5]
            );
        }
        let now = scheduler::ticks();

        // 1. RX: вычерпать каждый завершённый кадр из активного канала.
        let mut frame = [0u8; 2048];
        while let Some(n) = link::recv(&mut frame) {
            self.input_frame(&frame[..n], now);
        }

        // 2. Учёт соседей/фрагментов для обоих семейств + RS один раз.
        self.arp.on_tick(now);
        self.nd.on_tick(now);
        self.reasm.on_tick(now);
        self.reasm6.on_tick(now);
        if !self.rs_sent {
            self.rs_sent = true;
            let icmp = wire::router_solicit(self.ll_addr6, self.mac);
            let pkt = wire::ipv6_build(
                self.ll_addr6,
                wire::Ipv6Addr::ALL_ROUTERS,
                wire::PROTO_ICMPV6,
                255,
                &icmp,
            );
            let frame = wire::eth_frame(
                wire::ipv6_multicast_mac(wire::Ipv6Addr::ALL_ROUTERS),
                self.mac,
                wire::ETHERTYPE_IPV6,
                &pkt,
            );
            link::send(&frame);
        }

        // 3. DHCP: discovery, пока не настроено; renew после привязки.
        //     (временно вынесено, чтобы занять остальной стек)
        {
            let mut client = core::mem::take(&mut self.dhcp);
            let lease = client.drive(self, now);
            self.dhcp = client;
            if let Some(lease) = lease {
                self.cidr = Some(Ipv4Cidr::new(lease.addr, lease.prefix));
                self.gateway = lease.router;
                self.dns_servers = lease.dns_servers;
                self.configured = true;
                info!(
                    "net: DHCP lease acquired: {} gw {}",
                    self.cidr.unwrap(),
                    lease
                        .router
                        .map(|r| r.to_string())
                        .unwrap_or_else(|| String::from("-"))
                );
            }
        }

        // 4. Таймаут DHCP -> статический fallback, применяется ровно один раз (R13.3).
        if !self.configured && !self.dhcp_fallback_used {
            if self.deadline_tick.is_none() {
                let timeout = if self.link == Some(Link::UsbRndis) {
                    RNDIS_DHCP_TIMEOUT_TICKS
                } else {
                    DHCP_TIMEOUT_TICKS
                };
                self.deadline_tick = Some(scheduler::ticks() + timeout);
            }
            if let Some(deadline) = self.deadline_tick {
                if scheduler::ticks() >= deadline {
                    // У телефона своя подсеть: QEMU-адреса здесь не пригодны ни
                    // для ping, ни для HTTP. Обычно до fallback не доходит — DHCP
                    // телефона отвечает за доли секунды.
                    let (fallback_ip, fallback_gw) = if self.link == Some(Link::UsbRndis) {
                        (RNDIS_FALLBACK_IP, RNDIS_FALLBACK_GW)
                    } else {
                        (FALLBACK_IP, FALLBACK_GW)
                    };
                    self.cidr = Some(Ipv4Cidr::new(fallback_ip, FALLBACK_PREFIX));
                    self.gateway = Some(fallback_gw);
                    self.configured = true;
                    self.dhcp_fallback_used = true;
                    info!(
                        "net: DHCP timed out, static fallback {} gw {}",
                        fallback_ip, fallback_gw
                    );
                }
            }
        }

        // 5. Проход TCP-передачи/ретрансмиссии.
        tcp::TcpTable::poll_all(self, now);

        // 6. Demo-сервисы.
        self.service_udp_echo();
        self.service_tcp_echo();

        // 7. Ожидающие ping удаляет сам `net::ping` после разрешения.
    }

    /// Диспетчеризовать один принятый Ethernet-кадр вверх по слоям.
    fn input_frame(&mut self, frame: &[u8], now: u64) {
        let Some((dst, _src, ethertype, payload)) = wire::eth_parse(frame) else {
            return;
        };
        // Принимаем: наш unicast, broadcast и IPv6 multicast (33:33:*).
        // e1000 работает в promiscuous; всё остальное дропается здесь.
        let ipv6_mcast = dst.0[0] == 0x33 && dst.0[1] == 0x33;
        if dst != self.mac && dst != wire::Mac::BROADCAST && !ipv6_mcast {
            return;
        }
        match ethertype {
            wire::ETHERTYPE_ARP => {
                self.arp
                    .input(self.mac, self.cidr.map(|c| c.addr), payload, now)
            }
            // ip::input диспетчеризует по нибблу версии (v4 и v6); NDP нужен
            // настоящий MAC отправителя на Ethernet-уровне.
            wire::ETHERTYPE_IPV4 | wire::ETHERTYPE_IPV6 => ip::input(self, payload, _src, now),
            _ => {}
        }
    }

    fn service_udp_echo(&mut self) {
        let Some(h) = self.udp_echo_handle else {
            return;
        };
        let mut buf = [0u8; 2048];
        while let Some((n, ep)) = self.udp.recv(h, &mut buf) {
            let _ = udp::send_from(self, self.udp_echo_port, &buf[..n], ep);
        }
    }

    fn service_tcp_echo(&mut self) {
        let Some(port) = self.tcp_echo_port else {
            return;
        };
        for h in self.tcp.handles() {
            let matches_port = self
                .tcp
                .get(h)
                .map(|s| s.local_port == port)
                .unwrap_or(false);
            if !matches_port {
                continue;
            }
            let state = self.tcp.get(h).map(|s| s.state()).unwrap();
            match state {
                tcp::State::Established | tcp::State::CloseWait => {
                    // Отразить буферизованные байты.
                    let mut buf = [0u8; 1024];
                    loop {
                        let n = self
                            .tcp
                            .get_mut(h)
                            .map(|s| s.recv_slice(&mut buf))
                            .unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        let data = buf[..n].to_vec();
                        let _ = self.tcp.get_mut(h).map(|s| s.send_slice(&data));
                    }
                    // Peer закрыл и мы всё отразили: закрыть нашу половину.
                    if state == tcp::State::CloseWait {
                        let empty = self.tcp.get(h).map(|s| !s.can_recv()).unwrap_or(true);
                        if empty {
                            if let Some(sock) = self.tcp.get_mut(h) {
                                sock.close();
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Продвинуть сетевой стек на один шаг (вызывается [`net_thread`] и bounded
/// locked-step насосами). Никогда не выполняется в IRQ-контексте.
pub fn poll() {
    ensure_stack();
    let mut guard = NET.lock();
    if let Some(s) = guard.as_mut() {
        s.step();
    }
}

/// Точка входа сетевого потока ядра (R13.4). Бесконечный цикл: продвинуть
/// стек, затем кооперативно поспать до следующего тика таймера.
pub fn net_thread() {
    info!("net: poll thread started");
    let mut polls: u64 = 0;
    loop {
        poll();
        polls += 1;
        // Heartbeat: если эта строка перестала появляться при зависании системы,
        // net-поток (или планировщик, кормящий его тиками) мёртв.
        if polls.is_multiple_of(crate::arch::x86_64::apic::TICK_HZ * 5) {}
        scheduler::sleep_ticks(1);
    }
}

// ─── Публичный TCP API (неизменная поверхность со времён smoltcp) ───────────

/// Открыть исходящее TCP-соединение к `remote` и вернуть хэндл сокета.
pub fn tcp_connect(remote: IpEndpoint) -> Result<SocketHandle, NetError> {
    tcp_connect_buffered(remote, 4096, 4096)
}

/// Открыть исходящее TCP-соединение как [`tcp_connect`], с явно заданными
/// размерами rx/tx-буферов (TLS нужны записи в multi-KiB; fetch'ам — большие окна).
pub fn tcp_connect_buffered(
    remote: IpEndpoint,
    rx_bytes: usize,
    tx_bytes: usize,
) -> Result<SocketHandle, NetError> {
    let mut guard = NET.lock();
    let state = guard.as_mut().ok_or(NetError::NoDevice)?;
    let port = ephemeral_port_locked(state);
    let r = state
        .tcp
        .connect(remote, port, rx_bytes, tx_bytes)
        .map_err(|_| NetError::DeviceInit);
    crate::debug!(
        "[TCP] connect remote={}:{} local_port={} -> {:?}",
        remote.addr,
        remote.port,
        port,
        r
    );
    r
}

/// Завершилось ли TCP-рукопожатие?
///
/// Здесь намеренно спрашивается «доходило ли соединение когда-либо до
/// ESTABLISHED», а не «в ESTABLISHED ли оно сейчас». Peer, посылающий свои данные
/// и FIN одним burst'ом (одноразовый сервер, отвечающий и закрывающийся — ровно
/// то, что делают clipboard bridge и любой peer в стиле `curl` request/response),
/// проводит сокет из SYN-SENT через ESTABLISHED в CLOSE-WAIT за один проход poll.
/// Сравнение `state == Established` полностью упускало это окно: `connect(2)`
/// затем выкручивал весь таймаут и падал с ETIMEDOUT, хотя peer уже ответил —
/// данные в приёмном буфере выбрасывались вместе с сокетом.
pub fn tcp_established(handle: SocketHandle) -> bool {
    let mut g = NET.lock();
    let Some(s) = g.as_mut() else { return false };
    s.step();
    s.tcp
        .get(handle)
        .map(|k| k.ever_established())
        .unwrap_or(false)
}

/// Соединение упало до установки (refused/unreachable)?
pub fn tcp_dead_before_established(handle: SocketHandle) -> bool {
    let g = NET.lock();
    match g.as_ref().and_then(|s| s.tcp.get(handle)) {
        None => true, // gone => dead
        Some(k) => k.refused_flag() || (!k.ever_established() && k.state() == tcp::State::Closed),
    }
}

/// Один раз опросить и поставить в очередь столько `data`, сколько влезает в
/// окно peer. Возвращает число байт, принятых в передающий буфер.
pub fn tcp_send_chunk(handle: SocketHandle, data: &[u8]) -> usize {
    let mut g = NET.lock();
    let Some(s) = g.as_mut() else { return 0 };
    s.step();
    s.tcp
        .get_mut(handle)
        .map(|k| k.send_slice(data))
        .unwrap_or(0)
}

/// Один раз опросить и слить принятые байты в `dst`. Возвращает число скопированных байт.
pub fn tcp_recv_chunk(handle: SocketHandle, dst: &mut [u8]) -> usize {
    let mut g = NET.lock();
    let Some(s) = g.as_mut() else { return 0 };
    s.step();
    s.tcp
        .get_mut(handle)
        .map(|k| k.recv_slice(dst))
        .unwrap_or(0)
}

/// Удалённая половина закрыта и читать нечего: read(2) должен вернуть 0.
pub fn tcp_rx_at_eof(handle: SocketHandle) -> bool {
    let mut g = NET.lock();
    let Some(s) = g.as_mut() else { return true };
    s.step();
    s.tcp.get(handle).map(|k| k.eof_visible()).unwrap_or(true)
}

/// Половинное закрытие TX для `shutdown(SHUT_WR)` (nr 48): FIN заказывается и
/// стек получает несколько poll'ов, чтобы его отправить, но СЛОТ ОСТАЁТСЯ —
/// соединение продолжает принимать данные и живёт до `close`.
///
/// Отличие от [`tcp_close`] только в `remove`: там слот снимается сразу после
/// close-рукопожатия, здесь сокет доигрывает FIN-WAIT сам.
pub fn tcp_shutdown_wr(handle: SocketHandle) {
    let mut g = NET.lock();
    let Some(s) = g.as_mut() else { return };
    if let Some(k) = s.tcp.get_mut(handle) {
        k.close();
    }
    for _ in 0..4 {
        s.step();
    }
}

/// Полузакрыть наш TX, дать close-рукопожатию несколько poll'ов и вернуть слот
/// сокета.
pub fn tcp_close(handle: SocketHandle) {
    let mut g = NET.lock();
    let Some(s) = g.as_mut() else { return };
    if let Some(k) = s.tcp.get_mut(handle) {
        k.close();
    }
    for _ in 0..4 {
        s.step();
    }
    s.tcp.remove(handle);
}

// ─── Публичный UDP API ──────────────────────────────────────────────────────

/// Открыть UDP-сокет, привязанный к эфемерному локальному порту на 0.0.0.0.
pub fn udp_open() -> Result<SocketHandle, NetError> {
    let mut guard = NET.lock();
    let state = guard.as_mut().ok_or(NetError::NoDevice)?;
    let port = ephemeral_port_locked(state);
    Ok(state.udp.open(port))
}

/// Выделить эфемерный локальный порт (49152..=65535), не занятый сейчас ни одним
/// TCP- или UDP-сокетом. Вызывающий ДОЛЖЕН держать `NET`.
fn ephemeral_port_locked(s: &mut Stack) -> u16 {
    loop {
        let p = s.next_eph;
        s.next_eph = if s.next_eph == u16::MAX {
            49152
        } else {
            s.next_eph + 1
        };
        if !s.udp.port_in_use(p) {
            return p;
        }
    }
}

/// Послать одну датаграмму на `remote`.
pub fn udp_sendto(handle: SocketHandle, data: &[u8], remote: IpEndpoint) -> Result<(), NetError> {
    let mut guard = NET.lock();
    let state = guard.as_mut().ok_or(NetError::NoDevice)?;
    let port = state.udp.port_of(handle).ok_or(NetError::DeviceInit)?;
    udp::send_from(state, port, data, remote).map_err(|_| NetError::DeviceInit)
}

/// Убрать UDP-сокет из набора (вернуть буферы). Идемпотентно.
#[allow(dead_code)]
pub fn udp_remove(handle: SocketHandle) {
    let mut g = NET.lock();
    if let Some(s) = g.as_mut() {
        s.udp.close(handle);
    }
}

/// Принять одну датаграмму, если есть (вызывающий гоняет polling между вызовами).
pub fn udp_recvfrom(handle: SocketHandle, dst: &mut [u8]) -> Option<(usize, IpEndpoint)> {
    let mut g = NET.lock();
    let s = g.as_mut()?;
    s.step();
    s.udp.recv(handle, dst)
}

// ─── Сервисы / диагностика ──────────────────────────────────────────────────

/// Включить UDP echo-сервис на `port` (R14.2).
pub fn udp_echo_enable(port: u16) {
    let mut guard = NET.lock();
    let Some(state) = guard.as_mut() else {
        warn!("net: udp_echo_enable: no interface");
        return;
    };
    let h = state.udp.open(port);
    state.udp_echo_handle = Some(h);
    state.udp_echo_port = port;
    info!("net: UDP echo enabled on port {}", port);
}

/// Запустить TCP echo-листенер на `port` (R14.3). Каждое принятое соединение
/// становится отдельным сокетом; листенер сразу принимает следующих клиентов.
pub fn tcp_echo_listen(port: u16) {
    let mut guard = NET.lock();
    let Some(state) = guard.as_mut() else {
        warn!("net: tcp_echo_listen: no interface");
        return;
    };
    match state.tcp.listen(port) {
        Ok(_) => {
            state.tcp_echo_port = Some(port);
            info!("net: TCP echo listening on port {}", port);
        }
        Err(_) => warn!("net: tcp_echo_listen: listen {} failed", port),
    }
}

/// Текущая IP-конфигурация для `ifconfig` или `None`, если NIC нет или адрес ещё
/// не назначен.
pub fn ip_config() -> Option<IpConfig> {
    let guard = NET.lock();
    let state = guard.as_ref()?;
    let cidr = state.cidr?;
    Some(IpConfig {
        addr: cidr,
        gateway: state.gateway.unwrap_or(Ipv4Addr::UNSPECIFIED),
        mac: state.mac,
    })
}

/// Текущая IPv6-конфигурация для `ifconfig` или `None` до SLAAC.
#[derive(Debug, Clone, Copy)]
pub struct Ip6Config {
    pub addr: wire::Ipv6Cidr,
}

/// Глобальный адрес, настроенный SLAAC, если есть.
pub fn ip6_config() -> Option<Ip6Config> {
    let guard = NET.lock();
    let state = guard.as_ref()?;
    Some(Ip6Config { addr: state.cidr6? })
}

/// Изученный из RA маршрутизатор по умолчанию (всегда link-local), если есть.
#[allow(dead_code)]
pub fn ip6_gateway() -> Option<Ipv6Addr> {
    let guard = NET.lock();
    guard.as_ref()?.v6_gateway
}

/// Первый пригодный DNS-сервер или `None`, пока адрес не настроен.
pub fn dns_server() -> Option<Ipv4Addr> {
    let guard = NET.lock();
    let state = guard.as_ref()?;
    if !state.configured {
        return None;
    }
    if let Some(server) = state.dns_servers.first() {
        return Some(*server);
    }
    // QEMU-резолвер существует только в user-mode networking. На USB-тетеринге
    // без DHCP-lease честнее не иметь DNS, чем ходить на чужой адрес: HTTP по
    // IP-литералу продолжает работать.
    match state.link {
        Some(Link::UsbRndis) => None,
        _ => Some(QEMU_DNS),
    }
}

// ─── DNS resolve (A + AAAA; предпочтение семейства следует конфигурации) ───

/// Разрешить `hostname` в [`IpAddr`] через DNS-запрос.
///
/// IPv4/IPv6-литералы возвращаются сразу. Иначе запросы шлются одиночными
/// UDP-датаграммами на [`dns_server`]:53 через временные сокеты, ведомые bounded
/// locked steps: AAAA первым, когда у интерфейса есть IPv6-адрес, затем A;
/// другое семейство — fallback, когда у предпочтительного нет ответа.
pub fn resolve(hostname: &str) -> Option<IpAddr> {
    // Сначала литералы (v4 dotted-quad или любая текстовая форма v6).
    if hostname.contains(':') {
        return wire::parse_ipv6_literal(hostname).map(IpAddr::V6);
    }
    if let Some(o) = dns::parse_ipv4_literal(hostname) {
        return Some(IpAddr::V4(Ipv4Addr::new(o[0], o[1], o[2], o[3])));
    }

    let server = dns_server()?;

    // IPv4 ПЕРВЫМ. QEMU user-mode networking (slirp) анонсирует RA и отвечает
    // на AAAA-запросы, но НЕ маршрутизирует исходящий IPv6, так что AAAA-first
    // отправлял apt/TLS в чёрную дыру (регрессия, привнесённая dual-stack).
    // AAAA остаётся fallback, когда у имени нет A-записи, а явные IPv6-endpoint'ы
    // подключаются нормально.
    let order: [(u16, bool); 2] = [(dns::QTYPE_A, false), (dns::QTYPE_AAAA, true)];

    for (qtype, _) in order {
        if let Some(found) = dns_query_once(server, hostname, qtype) {
            return Some(found);
        }
    }
    None
}

/// Один DNS-обмен `qtype` на временном сокете; возвращает первый подходящий адрес.
fn dns_query_once(server: Ipv4Addr, hostname: &str, qtype: u16) -> Option<IpAddr> {
    // Случайный transaction id: id, выведенный из тика, предсказуем и
    // сталкивается между подряд идущими lookup'ами, позволяя off-path ответу
    // сойти за ответ.
    let id: u16 = random_u32() as u16;
    let mut query: Vec<u8> = Vec::new();
    if !dns::build_dns_query_typed(id, hostname, qtype, &mut query) {
        return None;
    }

    let handle = {
        let mut guard = NET.lock();
        let state = guard.as_mut()?;
        let port = ephemeral_port_locked(state);
        state.udp.open(port)
    };

    let server_ep = IpEndpoint::new(IpAddr::V4(server), 53);
    let now = scheduler::ticks();
    let deadline = now + DNS_TIMEOUT_TICKS;
    let mut next_send = now;
    let mut result: Option<IpAddr> = None;

    for _ in 0..DNS_MAX_STEPS {
        {
            let mut guard = NET.lock();
            let state = match guard.as_mut() {
                Some(s) => s,
                None => break,
            };
            // (Пере)посылать запрос каждые DNS_RESEND_TICKS, пока не придёт ответ
            // или не истечёт дедлайн; одна потерянная датаграмма больше не сжигает
            // весь бюджет на spin.
            if scheduler::ticks() >= next_send {
                let port = state.udp.port_of(handle).unwrap_or(0);
                if udp::send_from(state, port, &query, server_ep).is_ok() {
                    next_send = scheduler::ticks() + DNS_RESEND_TICKS;
                }
            }
            state.step();

            let mut rbuf = [0u8; 1500];
            if let Some((n, _ep)) = state.udp.recv(handle, &mut rbuf) {
                result = match qtype {
                    dns::QTYPE_AAAA => dns::parse_dns_aaaa_response(&rbuf[..n], id, hostname)
                        .map(|o| IpAddr::V6(wire::Ipv6Addr(o))),
                    _ => dns::parse_dns_a_response(&rbuf[..n], id, hostname)
                        .map(|o| IpAddr::V4(Ipv4Addr::new(o[0], o[1], o[2], o[3]))),
                };
                if result.is_some() {
                    break;
                }
            }
        }

        if scheduler::ticks() >= deadline {
            break;
        }
        for _ in 0..DNS_STEP_SPIN {
            core::hint::spin_loop();
        }
    }

    if let Some(state) = NET.lock().as_mut() {
        state.udp.close(handle);
    }

    result
}

/// Провести короткий самодостаточный TCP-клиентский обмен для команды shell
/// `nc` (R15.2, R15.3): connect, отправить после установки, собрать отражённые
/// байты в bounded окне poll, close, teardown.
pub fn nc_echo(remote: IpEndpoint, payload: &[u8]) -> NcResult {
    let handle = match tcp_connect(remote) {
        Ok(h) => h,
        Err(_) => return NcResult::Failed,
    };

    let mut established = false;
    let mut sent = false;
    let mut reply: Vec<u8> = Vec::new();
    let mut done = false;

    for _ in 0..4000 {
        {
            let mut guard = NET.lock();
            let Some(s) = guard.as_mut() else {
                return NcResult::Failed;
            };
            s.step();

            match s.tcp.get(handle) {
                None => {
                    if !established {
                        s.tcp.remove(handle);
                        return NcResult::Failed; // refused / reaped before establish
                    }
                }
                Some(sock) => {
                    // `ever_established`, а не `state == Established`: peer, который
                    // отвечает и закрывается одним burst'ом, проводит сокет через
                    // ESTABLISHED в CLOSE-WAIT за один проход poll, и проверка на
                    // равенство тогда не срабатывает никогда — payload не отправлен,
                    // а `nc` сообщает о сбое серверу, который уже ответил. Та же
                    // ловушка, что в `tcp_established`.
                    if sock.ever_established() {
                        established = true;
                    }
                }
            }

            if established && !sent {
                let n = s
                    .tcp
                    .get_mut(handle)
                    .map(|k| k.send_slice(payload))
                    .unwrap_or(0);
                if n == payload.len() {
                    sent = true;
                }
            }

            // Вычерпать любые отражённые байты.
            loop {
                let mut buf = [0u8; 1024];
                let n = s
                    .tcp
                    .get_mut(handle)
                    .map(|k| k.recv_slice(&mut buf))
                    .unwrap_or(0);
                if n == 0 {
                    break;
                }
                reply.extend_from_slice(&buf[..n]);
            }

            if sent && reply.len() >= payload.len() {
                if let Some(k) = s.tcp.get_mut(handle) {
                    k.close();
                }
                done = true;
            }
        }

        for _ in 0..20_000 {
            core::hint::spin_loop();
        }
        if done {
            break;
        }
    }

    // Дать close-рукопожатию несколько poll'ов, затем вернуть слот.
    {
        let mut guard = NET.lock();
        if let Some(s) = guard.as_mut() {
            for _ in 0..4 {
                s.step();
            }
            s.tcp.remove(handle);
        }
    }

    if established {
        NcResult::Echoed(reply)
    } else {
        NcResult::Failed
    }
}

// ─── ICMP ping ───────────────────────────────────────────────────────────────

/// Послать один ICMP echo-запрос на `addr` (v4 или v6) и ждать до ~1 с ответа.
/// Возвращает измеренное round-trip время в тиках (мс при TICK_HZ).
pub fn ping(addr: IpAddr) -> Option<u64> {
    const PING_TIMEOUT_TICKS: u64 = crate::arch::x86_64::apic::ms_to_ticks(1000);

    let ident = random_u32() as u16;
    let seq = 1u16;
    let started = scheduler::ticks();
    let payload = b"pagh-ping";

    // Зарегистрировать ожидающего ДО отправки, чтобы быстрый ответ не потерялся.
    {
        let mut guard = NET.lock();
        let state = guard.as_mut()?;
        if state.pings.len() >= 8 {
            state.pings.remove(0);
        }
        state.pings.push(ip::PingWaiter {
            ident,
            seq,
            started,
            seq_any: true,
            done: false,
            rtt: None,
        });

        let now = scheduler::ticks();
        match addr {
            IpAddr::V4(v4) => {
                let msg = wire::icmp_echo_build(wire::ICMP_ECHO_REQUEST, ident, seq, payload);
                ip::output(state, None, IpAddr::V4(v4), wire::PROTO_ICMP, &msg, now);
            }
            IpAddr::V6(v6) => {
                let mut body = alloc::vec::Vec::with_capacity(4 + payload.len());
                body.extend_from_slice(&ident.to_be_bytes());
                body.extend_from_slice(&seq.to_be_bytes());
                body.extend_from_slice(payload);
                let src = if v6.is_link_local() {
                    state.ll_addr6
                } else {
                    state.cidr6.map(|c| c.addr).unwrap_or(state.ll_addr6)
                };
                let msg = wire::icmpv6_build(src, v6, wire::ICMPV6_ECHO_REQUEST, 0, &body);
                ip::output(state, None, IpAddr::V6(v6), wire::PROTO_ICMPV6, &msg, now);
            }
        }
    }

    loop {
        {
            let mut guard = NET.lock();
            if let Some(state) = guard.as_mut() {
                if let Some(p) = state.pings.iter().find(|p| p.ident == ident && p.done) {
                    let rtt = p.rtt;
                    state.pings.retain(|p| p.ident != ident);
                    return rtt;
                }
            }
        }
        if scheduler::ticks() >= started + PING_TIMEOUT_TICKS {
            // Разрегистрировать.
            let mut guard = NET.lock();
            if let Some(state) = guard.as_mut() {
                state.pings.retain(|p| p.ident != ident);
            }
            return None;
        }
        poll();
        scheduler::sleep_ticks(1);
    }
}
