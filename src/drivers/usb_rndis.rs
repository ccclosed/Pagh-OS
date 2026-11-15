//! USB RNDIS link: управляющая последовательность, RX/TX-очереди и состояние.
//!
//! Транспорт (EP0 class-запросы и bulk-кольца) живёт в `drivers::xhci`; здесь —
//! протокол поверх него и граница с сетевым стеком. Разделение намеренное:
//! сетевой поток забирает кадры из очереди и кладёт свои, но никогда не ждёт
//! контроллер USB, а поток `usb` никогда не берёт лок сетевого стека. Очереди
//! ограничены, поэтому медленный потребитель теряет кадры (как переполненное
//! TX-кольцо e1000), а не растит кучу ядра.
//!
//! Устройство поднимается в такой последовательности: INITIALIZE (1.0), QUERY
//! physical medium (необязательно), QUERY MAC, SET packet filter — и только
//! после ненулевого фильтра устройство начинает передавать кадры. Xid
//! уникален для каждого запроса; ответ принимается только с тем же xid и
//! только при успешном статусе.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::drivers::usb_rndis_proto as proto;
use crate::sync::spinlock::Spinlock;

/// Глубина очередей кадров в каждую сторону. 64 кадра — примерно окно
/// TCP-вывода; переполнение означает, что противоположная сторона не успевает,
/// и кадр теряется, как в переполненном TX-кольце NIC.
const QUEUE_DEPTH: usize = 64;

/// `bInterface`-независимый транспорт EP0: послать encapsulated-команду и
/// прочитать encapsulated-ответ. Реализация — в xHCI; ошибка любого шага —
/// `None`.
pub trait RndisTransport {
    fn command(&mut self, request: &[u8], response: &mut [u8]) -> Option<usize>;
}

/// Что устройство сообщило о себе при подъёме.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkInfo {
    pub mac: [u8; 6],
    /// OID_GEN_PHYSICAL_MEDIUM: 0 — 802.3, 1 — wireless WAN и т.д.
    pub medium: u32,
    /// Максимальный размер передачи, который объявило устройство.
    pub max_transfer: u32,
}

/// Счётчики для `usb`/`ifconfig` и диагностики.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub rx_packets: u64,
    pub rx_dropped: u64,
    pub tx_packets: u64,
    pub tx_dropped: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub keepalive_ok: u64,
    pub keepalive_fail: u64,
}

struct LinkState {
    info: LinkInfo,
    stats: Stats,
    /// Сколько poll'ов USB-потока прошло с последнего keepalive.
    keepalive_polls: u32,
}

static STATE: Spinlock<Option<LinkState>> = Spinlock::new(None);
static RX_QUEUE: Spinlock<VecDeque<Vec<u8>>> = Spinlock::new(VecDeque::new());
static TX_QUEUE: Spinlock<VecDeque<Vec<u8>>> = Spinlock::new(VecDeque::new());

/// Сколько poll'ов между keepalive. Поток `usb` спит 1 тик между проходами,
/// поэтому это примерно 5 секунд — требование RNDIS для соединения, которое
/// иначе считается потерянным.
const KEEPALIVE_POLLS: u32 = 5_000;

/// Поднять RNDIS поверх готового транспорта.
///
/// Возвращает MAC и параметры устройства; `None` — любой обязательный шаг не
/// завершился успехом. Состояние и очереди при этом остаются сброшенными, чтобы
/// следующий повтор начинался с чистого листа.
pub fn bring_up(t: &mut impl RndisTransport) -> Option<LinkInfo> {
    reset();

    let mut xid = 1u32;
    let mut next_xid = || {
        let x = xid;
        xid = xid.wrapping_add(1);
        if xid == 0 {
            xid = 1;
        }
        x
    };

    let mut resp = [0u8; 1024];

    // 1. INITIALIZE 1.0. Объявленный максимум — наш transfer-буфер.
    let init_xid = next_xid();
    let msg = proto::encode_initialize(init_xid, proto::MAX_TRANSFER as u32);
    let mut init = None;
    for _ in 0..3 {
        let Some(n) = t.command(&msg, &mut resp) else {
            continue;
        };
        if let Some(i) = proto::parse_initialize_complete(&resp[..n]) {
            if proto::parse_header(&resp[..n]).map(|h| h.request_id) == Some(init_xid) {
                init = Some(i);
                break;
            }
        }
    }
    let init = init?;
    if init.status != proto::STATUS_SUCCESS {
        return None;
    }

    // 2. PHYSICAL MEDIUM необязателен: отсутствие OID не мешает 802.3-кадру.
    let mut medium = 0u32;
    let query_xid = next_xid();
    let msg = proto::encode_query(query_xid, proto::OID_GEN_PHYSICAL_MEDIUM, 4);
    for _ in 0..2 {
        let Some(n) = t.command(&msg, &mut resp) else {
            continue;
        };
        if let Some(q) = proto::parse_query_complete(&resp[..n]) {
            if q.status == proto::STATUS_SUCCESS && q.info.len() >= 4 {
                medium = u32::from_le_bytes([q.info[0], q.info[1], q.info[2], q.info[3]]);
                break;
            }
        }
    }

    // 3. MAC. Permanent address — то, что нужно DHCP; current address — запасной
    // вариант для устройств, которые не отвечают на первый OID.
    let mac = query_mac(t, &mut next_xid, proto::OID_802_3_PERMANENT_ADDRESS)
        .or_else(|| query_mac(t, &mut next_xid, proto::OID_802_3_CURRENT_ADDRESS))?;
    let mac = normalize_mac(mac);

    // 4. Ненулевой packet filter переводит устройство в data-initialized.
    let set_xid = next_xid();
    let msg = proto::encode_set(
        set_xid,
        proto::OID_GEN_CURRENT_PACKET_FILTER,
        proto::PACKET_FILTER_DEFAULT,
    );
    let mut filter_ok = false;
    for _ in 0..3 {
        let Some(n) = t.command(&msg, &mut resp) else {
            continue;
        };
        if let Some(h) = proto::parse_simple_complete(&resp[..n], proto::CMPLT_SET) {
            if h.request_id == set_xid && h.status == proto::STATUS_SUCCESS {
                filter_ok = true;
                break;
            }
        }
    }
    if !filter_ok {
        return None;
    }

    let info = LinkInfo {
        mac,
        medium,
        max_transfer: init.max_transfer_size,
    };
    let mut g = STATE.lock();
    *g = Some(LinkState {
        info,
        stats: Stats::default(),
        keepalive_polls: 0,
    });
    Some(info)
}

fn query_mac(
    t: &mut impl RndisTransport,
    next_xid: &mut impl FnMut() -> u32,
    oid: u32,
) -> Option<[u8; 6]> {
    // Информационный буфер 48 байт — требование RNDIS к запросу MAC; ответ
    // обязан быть ровно 6 байт, иначе это не Ethernet-адрес.
    let xid = next_xid();
    let msg = proto::encode_query(xid, oid, 48);
    let mut resp = [0u8; 256];
    for _ in 0..3 {
        let Some(n) = t.command(&msg, &mut resp) else {
            continue;
        };
        let Some(q) = proto::parse_query_complete(&resp[..n]) else {
            continue;
        };
        if q.status != proto::STATUS_SUCCESS || q.info.len() != 6 {
            continue;
        }
        let mut mac = [0u8; 6];
        mac.copy_from_slice(q.info);
        return Some(mac);
    }
    None
}

/// Привести MAC к адресу, пригодному для DHCP: нулевой и multicast-адрес
/// ломают ARP-ответы. Нулевому даётся постоянный локальный адрес, у multicast
/// очищается младший бит первого октета.
fn normalize_mac(mut mac: [u8; 6]) -> [u8; 6] {
    if mac == [0u8; 6] {
        return [0x02, 0x52, 0x4E, 0x44, 0x49, 0x53]; // 02:52:4e:44:49:53
    }
    mac[0] &= !0x01;
    mac
}

/// Сбросить состояние и обе очереди. Вызывается при подъёме и при потере
/// устройства/перезапуске контроллера.
pub fn reset() {
    *STATE.lock() = None;
    RX_QUEUE.lock().clear();
    TX_QUEUE.lock().clear();
}

/// Готов ли канал к передаче кадров.
pub fn is_ready() -> bool {
    STATE.lock().as_ref().is_some()
}

/// MAC поднятого устройства.
pub fn mac() -> Option<[u8; 6]> {
    STATE.lock().as_ref().map(|s| s.info.mac)
}

/// Снимок информации о канале.
pub fn link_info() -> Option<LinkInfo> {
    STATE.lock().as_ref().map(|s| s.info)
}

/// Счётчики канала.
pub fn stats() -> Stats {
    STATE.lock().as_ref().map(|s| s.stats).unwrap_or_default()
}

// ─── RX: кадры от USB-потока к сетевому стеку ───────────────────────────────

/// Положить принятый Ethernet-кадр в очередь для сетевого стека.
///
/// Вызывается из потока `usb` после разбора конверта. Очередь ограничена:
/// переполнение считается потерей, а не поводом ждать сетевой стек.
pub fn push_rx_frame(frame: &[u8]) {
    if frame.len() < 14 {
        if let Some(s) = STATE.lock().as_mut() {
            s.stats.rx_errors = s.stats.rx_errors.saturating_add(1);
        }
        return;
    }
    let mut q = RX_QUEUE.lock();
    if q.len() >= QUEUE_DEPTH {
        drop(q);
        if let Some(s) = STATE.lock().as_mut() {
            s.stats.rx_dropped = s.stats.rx_dropped.saturating_add(1);
        }
        return;
    }
    q.push_back(frame.to_vec());
    drop(q);
    if let Some(s) = STATE.lock().as_mut() {
        s.stats.rx_packets = s.stats.rx_packets.saturating_add(1);
    }
}

/// Забрать один принятый кадр (для сетевого стека).
pub fn recv(dst: &mut [u8]) -> Option<usize> {
    let frame = RX_QUEUE.lock().pop_front()?;
    if frame.len() > dst.len() {
        if let Some(s) = STATE.lock().as_mut() {
            s.stats.rx_dropped = s.stats.rx_dropped.saturating_add(1);
        }
        return None;
    }
    dst[..frame.len()].copy_from_slice(&frame);
    Some(frame.len())
}

/// Отметить ошибку разбора входа (битый конверт, ошибка transfer).
pub fn note_rx_error() {
    if let Some(s) = STATE.lock().as_mut() {
        s.stats.rx_errors = s.stats.rx_errors.saturating_add(1);
    }
}

// ─── TX: кадры от сетевого стека к USB-потоку ───────────────────────────────

/// Поставить Ethernet-кадр в очередь передачи. Возвращает длину кадра или 0,
/// если канал не поднят либо очередь полна (сетевой стек повторит отправку).
pub fn send(frame: &[u8]) -> usize {
    if frame.len() < 14 || frame.len() > 1518 {
        return 0;
    }
    if !is_ready() {
        return 0;
    }
    let mut q = TX_QUEUE.lock();
    if q.len() >= QUEUE_DEPTH {
        drop(q);
        if let Some(s) = STATE.lock().as_mut() {
            s.stats.tx_dropped = s.stats.tx_dropped.saturating_add(1);
        }
        return 0;
    }
    q.push_back(frame.to_vec());
    drop(q);
    frame.len()
}

/// Забрать кадр для отправки в bulk-OUT (вызывает поток `usb`).
pub fn take_tx() -> Option<Vec<u8>> {
    TX_QUEUE.lock().pop_front()
}

/// Учесть завершённую передачу.
pub fn note_tx_done() {
    if let Some(s) = STATE.lock().as_mut() {
        s.stats.tx_packets = s.stats.tx_packets.saturating_add(1);
    }
}

pub fn note_tx_error() {
    if let Some(s) = STATE.lock().as_mut() {
        s.stats.tx_errors = s.stats.tx_errors.saturating_add(1);
    }
}

// ─── Keepalive ──────────────────────────────────────────────────────────────

/// Пора ли послать keepalive. Считает poll'ы, поэтому не зависит от тиков
/// планировщика, которые в boot-меню могут стоять.
pub fn keepalive_due() -> bool {
    let due = match STATE.lock().as_mut() {
        Some(s) => {
            s.keepalive_polls = s.keepalive_polls.saturating_add(1);
            if s.keepalive_polls >= KEEPALIVE_POLLS {
                s.keepalive_polls = 0;
                true
            } else {
                false
            }
        }
        None => false,
    };
    due
}

pub fn note_keepalive(ok: bool) {
    if let Some(s) = STATE.lock().as_mut() {
        if ok {
            s.stats.keepalive_ok = s.stats.keepalive_ok.saturating_add(1);
        } else {
            s.stats.keepalive_fail = s.stats.keepalive_fail.saturating_add(1);
        }
    }
}

/// Послать keepalive и проверить ответ. Вызывается из потока `usb`; при
/// неудаче канал не сбрасывается — следующая попытка придёт через интервал, а
/// данные продолжат идти, если устройство живо.
pub fn keepalive(t: &mut impl RndisTransport, xid: u32) -> bool {
    let msg = proto::encode_keepalive(xid);
    let mut resp = [0u8; 64];
    for _ in 0..2 {
        let Some(n) = t.command(&msg, &mut resp) else {
            continue;
        };
        if let Some(h) = proto::parse_simple_complete(&resp[..n], proto::CMPLT_KEEPALIVE) {
            if h.status == proto::STATUS_SUCCESS {
                return true;
            }
        }
    }
    false
}
