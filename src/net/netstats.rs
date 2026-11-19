//! Счётчики канала: байты и пакеты в каждом направлении.
//!
//! [`crate::net::link`] — единственная воронка Ethernet-кадров в обе стороны
//! (`arp`/`ip`/`tcp`/RX-дренаж все ходят через `Link::send`/`Link::recv`),
//! поэтому учёт живёт здесь, а не в драйверах: иначе e1000 и RNDIS пришлось бы
//! учить одинаковой арифметике, а «какой драйвер сейчас активен» протекал бы в
//! счётчики. Эти числа отдают `/proc/net/dev` и `SIOCGIF*` для userspace.
//!
//! Что именно считается: кадр, который драйвер **принял** (`send` вернул
//! ненулевой размер) или **отдал** (`recv` вернул `Some(len)`), а не байты на
//! проводе. Отброшенный драйвером кадр (`send` = 0, пустая очередь RX) сюда не
//! попадает — потери и ошибки приёма остаются драйверскими (`usb_rndis::stats`,
//! у e1000 не считаются вовсе; см. [`crate::net::iface_stats`]).
//!
//! Атомики с `Relaxed`-упорядочиванием намеренно: это диагностика, а не
//! синхронизация. Спинлок здесь был бы ошибкой — `Link::recv` вызывается из
//! [`crate::net::Stack::step`] под локом `NET`, и второй спинлок в этой
//! вложенности ничего не защищает, зато превращает каждый кадр в пару
//! `lock`/`unlock`. Модуль остаётся `core`-only, поэтому `host-tests`
//! подключают ровно тот исходник, что компилирует ядро.

use core::sync::atomic::{AtomicU64, Ordering};

/// Снимок счётчиков канала на момент чтения.
///
/// Четыре независимых поля, а не согласованный «пакет+байты»: пакет и его
/// байты инкрементируются двумя атомиками, поэтому снимок между ними может
/// увидеть байты уже, а пакет ещё нет. Для `/proc/net/dev` это допустимо
/// (Linux сам читает per-CPU счётчики без глобального снимка), и это
/// задокументировано, а не спрятано.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkCounters {
    /// Байты, принятые стеком от драйвера (включая заголовки кадра).
    pub rx_bytes: u64,
    /// Кадры, принятые стеком от драйвера.
    pub rx_packets: u64,
    /// Байты, переданные драйверу (включая заголовки кадра).
    pub tx_bytes: u64,
    /// Кадры, которые драйвер принял к передаче.
    pub tx_packets: u64,
}

/// Набор счётчиков одного канала с атомарным инкрементом.
///
/// `const fn new()` позволяет держать экземпляр в `static` без ленивой
/// инициализации: ядро не имеет `Once`-инфраструктуры для статиков, а
/// `AtomicU64::new` — const.
pub struct CounterSet {
    rx_bytes: AtomicU64,
    rx_packets: AtomicU64,
    tx_bytes: AtomicU64,
    tx_packets: AtomicU64,
}

impl CounterSet {
    /// Нулевые счётчики.
    pub const fn new() -> Self {
        CounterSet {
            rx_bytes: AtomicU64::new(0),
            rx_packets: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            tx_packets: AtomicU64::new(0),
        }
    }

    /// Учесть кадр, отданный драйвером стеку.
    ///
    /// Нулевая длина не считается: `recv` с `Some(0)` не бывает (пустая
    /// очередь — это `None`), а `send` возвращает 0 как «не принят», поэтому
    /// общий для двух направлений страховочный `if` не даёт посчитать кадр,
    /// которого не было.
    pub fn record_rx(&self, len: usize) {
        if len == 0 {
            return;
        }
        self.rx_bytes.fetch_add(len as u64, Ordering::Relaxed);
        self.rx_packets.fetch_add(1, Ordering::Relaxed);
    }

    /// Учесть кадр, принятый драйвером к передаче.
    pub fn record_tx(&self, len: usize) {
        if len == 0 {
            return;
        }
        self.tx_bytes.fetch_add(len as u64, Ordering::Relaxed);
        self.tx_packets.fetch_add(1, Ordering::Relaxed);
    }

    /// Согласованно-поштучно прочитать четыре счётчика.
    pub fn snapshot(&self) -> LinkCounters {
        LinkCounters {
            rx_bytes: self.rx_bytes.load(Ordering::Relaxed),
            rx_packets: self.rx_packets.load(Ordering::Relaxed),
            tx_bytes: self.tx_bytes.load(Ordering::Relaxed),
            tx_packets: self.tx_packets.load(Ordering::Relaxed),
        }
    }
}

impl Default for CounterSet {
    fn default() -> Self {
        Self::new()
    }
}

/// Счётчики единственного канала этой сборки (второго NIC-а нет).
pub static LINK: CounterSet = CounterSet::new();
