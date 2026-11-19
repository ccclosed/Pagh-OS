// Feature: счётчики канала для userspace (`/proc/net/dev`, `SIOCGIF*`).
//
// `src/net/link.rs` — единственная воронка Ethernet-кадров в обе стороны,
// поэтому учёт байтов/пакетов живёт в чистом `src/net/netstats.rs`, а не в
// драйверах: e1000 и RNDIS считаются одинаково, а `host-tests` включают ровно
// тот исходник, что компилирует ядро.
//
// Свойства закрепляют ровно те инварианты, на которые опирается `/proc/net/dev`:
//   * свежий набор — нули (иначе первый же снимок соврёт о трафике);
//   * каждый принятый/переданный кадр добавляет свои байты и ровно один пакет;
//   * «не принят драйвером» (`send` = 0) не считается переданным кадром —
//     счётчик TX обязан совпадать с числом РЕАЛЬНО ушедших кадров;
//   * счётчики только растут (снимок — это монотонная история, а не окно);
//   * параллельный инкремент из нескольких потоков не теряет обновлений
//     (`Relaxed`-атомики, без спинлока под `NET`).

use crate::netstats::{CounterSet, LinkCounters};
use proptest::prelude::*;

#[test]
fn fresh_counter_set_is_all_zero() {
    let set = CounterSet::new();
    assert_eq!(
        set.snapshot(),
        LinkCounters {
            rx_bytes: 0,
            rx_packets: 0,
            tx_bytes: 0,
            tx_packets: 0,
        }
    );
}

#[test]
fn default_is_zero_too() {
    assert_eq!(CounterSet::default().snapshot(), CounterSet::new().snapshot());
}

proptest! {
    /// Точная арифметика: байты — сумма длин, пакеты — число кадров. Смешение
    /// направлений не допускается (RX не влияет на TX и наоборот).
    #[test]
    fn record_rx_and_tx_keep_exact_totals(
        rx in proptest::collection::vec(1usize..=1514, 0..64),
        tx in proptest::collection::vec(1usize..=1514, 0..64),
    ) {
        let set = CounterSet::new();
        for len in &rx {
            set.record_rx(*len);
        }
        for len in &tx {
            set.record_tx(*len);
        }
        let snap = set.snapshot();
        prop_assert_eq!(snap.rx_bytes, rx.iter().map(|n| *n as u64).sum::<u64>());
        prop_assert_eq!(snap.rx_packets, rx.len() as u64);
        prop_assert_eq!(snap.tx_bytes, tx.iter().map(|n| *n as u64).sum::<u64>());
        prop_assert_eq!(snap.tx_packets, tx.len() as u64);
    }

    /// Счётчики монотонны: любой снимок не меньше предыдущего, даже если между
    /// ними пришёл «пустой» кадр нулевой длины.
    #[test]
    fn counters_are_monotonic(
        lens in proptest::collection::vec(0usize..=2048, 0..128),
    ) {
        let set = CounterSet::new();
        let mut prev = set.snapshot();
        for len in lens {
            set.record_rx(len);
            let cur = set.snapshot();
            prop_assert!(cur.rx_bytes >= prev.rx_bytes);
            prop_assert!(cur.rx_packets >= prev.rx_packets);
            prop_assert_eq!(cur.tx_bytes, prev.tx_bytes);
            prop_assert_eq!(cur.tx_packets, prev.tx_packets);
            prev = cur;
        }
    }
}

#[test]
fn zero_length_is_not_a_packet() {
    // `send` возвращает 0 как «драйвер не принял кадр», и это не должно
    // выглядеть переданным пакетом в `/proc/net/dev`.
    let set = CounterSet::new();
    set.record_tx(0);
    set.record_rx(0);
    assert_eq!(set.snapshot(), CounterSet::new().snapshot());
}

#[test]
fn concurrent_increments_are_not_lost() {
    // `Link::recv` вызывается и из `Stack::step` под `NET`, и из насосов
    // `http_fetch`/`tls`; счётчик обязан быть атомарным, а не «примерно
    // атомарным». 4 потока × 10_000 кадров по 100 байт.
    use std::sync::Arc;
    let set = Arc::new(CounterSet::new());
    let mut handles = Vec::new();
    for _ in 0..4 {
        let set = Arc::clone(&set);
        handles.push(std::thread::spawn(move || {
            for _ in 0..10_000 {
                set.record_rx(100);
                set.record_tx(100);
            }
        }));
    }
    for h in handles {
        h.join().expect("worker thread panicked");
    }
    let snap = set.snapshot();
    assert_eq!(snap.rx_packets, 40_000);
    assert_eq!(snap.tx_packets, 40_000);
    assert_eq!(snap.rx_bytes, 4_000_000);
    assert_eq!(snap.tx_bytes, 4_000_000);
}

#[test]
fn snapshot_copy_is_independent() {
    // Снимок — значение, а не живое окно: последующий учёт его не меняет,
    // иначе /proc/net/dev, отрендеренный из снимка, «дрожал» бы под читателем.
    let set = CounterSet::new();
    set.record_rx(60);
    let before = set.snapshot();
    set.record_rx(60);
    assert_eq!(before.rx_packets, 1);
    assert_eq!(before.rx_bytes, 60);
    assert_eq!(set.snapshot().rx_packets, 2);
}