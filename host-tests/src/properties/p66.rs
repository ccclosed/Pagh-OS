// Feature: `inputd` — диспетчер физического ввода, свойство 66.
//
// Очередь `inputd` (`src/drivers/input_ring.rs`) стоит между производителями
// (PS/2 IRQ1/IRQ12, USB-HID) и единственным потребителем (поток `inputd`).
// Ошибка в ней — это не «редкий сбой», а залипший модификатор или потерянный
// break-код: shell получает чужой символ, а `^C` не срабатывает. Поэтому
// контракт доказывается против независимой модели `VecDeque` на произвольных
// последовательностях push/pop и при переполнении:
//
//   * FIFO: последовательность `pop` равна последовательности принятых `push`;
//   * переполнение отвергает НОВЫЙ элемент и не портит уже принятый префикс
//     (иначе быстрый ввод вытеснил бы make-код и следующий break пришёл бы без
//     пары);
//   * ни один элемент не доставляется дважды и не исчезает внутри ёмкости:
//     `принято == доставлено + осталось`, `отвергнуто == попыток - принято`;
//   * `clear` забывает всё и возвращает кольцо в состояние `new()`.

use crate::input_ring::InputRing;
use alloc::collections::VecDeque;
use proptest::prelude::*;

/// Одна операция над очередью — модель и реальность получают одну и ту же.
#[derive(Clone, Debug)]
enum Op {
    Push(u8),
    Pop,
    Clear,
}

/// Независимая модель: `VecDeque` с явным бюджетом ёмкости.
fn model(ops: &[Op], cap: usize) -> (alloc::vec::Vec<u8>, VecDeque<u8>, u64) {
    let mut delivered = alloc::vec::Vec::new();
    let mut pending: VecDeque<u8> = VecDeque::new();
    let mut rejected = 0u64;
    for op in ops {
        match op {
            Op::Push(v) => {
                if pending.len() == cap {
                    // Переполнение: новый отвергнут, старые целы.
                    rejected += 1;
                } else {
                    pending.push_back(*v);
                }
            }
            Op::Pop => {
                if let Some(v) = pending.pop_front() {
                    delivered.push(v);
                }
            }
            Op::Clear => pending.clear(),
        }
    }
    (delivered, pending, rejected)
}

fn ops_strategy(max_ops: usize) -> impl Strategy<Value = alloc::vec::Vec<Op>> {
    let op = prop_oneof![
        3 => any::<u8>().prop_map(Op::Push),
        2 => Just(Op::Pop),
        1 => Just(Op::Clear),
    ];
    proptest::collection::vec(op, 0..=max_ops)
}

/// Только push/pop: на них формулируется закон сохранения (clear выбрасывает
/// невыданное, и `принято == доставлено + осталось` для него не выполняется).
fn push_pop_strategy(max_ops: usize) -> impl Strategy<Value = alloc::vec::Vec<Op>> {
    let op = prop_oneof![
        3 => any::<u8>().prop_map(Op::Push),
        2 => Just(Op::Pop),
    ];
    proptest::collection::vec(op, 0..=max_ops)
}

proptest! {
    /// Порядок и полнота доставки: ровно модель, для любой последовательности
    /// push/pop/clear.
    ///
    /// Ёмкость 7 — достаточно большая, чтобы кольцо пережило несколько оборотов
    /// (`head` уезжает за конец массива), и достаточно малая, чтобы переполнение
    /// случалось на коротких генерациях.
    #[test]
    fn ring_matches_fifo_model(ops in ops_strategy(64)) {
        const CAP: usize = 7;
        let mut ring: InputRing<u8, CAP> = InputRing::new();
        let mut delivered = alloc::vec::Vec::new();
        let mut rejected = 0u64;

        for op in &ops {
            match op {
                Op::Push(v) => {
                    let accepted = ring.push(*v);
                    if !accepted {
                        rejected += 1;
                        prop_assert!(ring.is_full(), "only a full ring may reject a push");
                    }
                }
                Op::Pop => {
                    if let Some(v) = ring.pop() {
                        delivered.push(v);
                    }
                }
                Op::Clear => ring.clear(),
            }
        }

        let (want_delivered, want_pending, want_rejected) = model(&ops, CAP);
        prop_assert_eq!(delivered, want_delivered);
        prop_assert_eq!(rejected, want_rejected);
        let left: alloc::vec::Vec<u8> = {
            let mut v = alloc::vec::Vec::new();
            while let Some(b) = ring.pop() {
                v.push(b);
            }
            v
        };
        prop_assert_eq!(left, want_pending.into_iter().collect::<alloc::vec::Vec<u8>>());
        prop_assert!(ring.is_empty(), "draining leaves the ring empty");
    }

    /// Каждый принятый элемент доставляется ровно один раз, каждый отвергнутый
    /// не появляется вовсе — учёт сходится при любой длине серии.
    #[test]
    fn accounting_is_conserved(ops in push_pop_strategy(96)) {
        const CAP: usize = 16;
        let mut ring: InputRing<u8, CAP> = InputRing::new();
        let mut attempted = 0u64;
        let mut accepted = 0u64;
        let mut delivered = 0u64;
        let mut rejected = 0u64;
        for op in &ops {
            match op {
                Op::Push(v) => {
                    attempted += 1;
                    if ring.push(*v) {
                        accepted += 1;
                    } else {
                        rejected += 1;
                    }
                }
                Op::Pop => {
                    if ring.pop().is_some() {
                        delivered += 1;
                    }
                }
                Op::Clear => unreachable!("push_pop_strategy never yields Clear"),
            }
            prop_assert!(ring.len() <= CAP, "len never exceeds capacity");
            prop_assert_eq!(accepted + rejected, attempted);
        }
        prop_assert_eq!(accepted, delivered + ring.len() as u64,
            "accepted == delivered + left inside the ring");
        prop_assert!(ring.len() <= CAP);
    }

    /// `clear` возвращает кольцо в состояние нового: пусто, и следующий push
    /// кладёт элемент в начало, а не в «дыру» после старой головы.
    #[test]
    fn clear_resets_to_new(prefix in proptest::collection::vec(any::<u8>(), 0..=8)) {
        let mut ring: InputRing<u8, 4> = InputRing::new();
        for b in &prefix {
            let _ = ring.push(*b);
        }
        ring.clear();
        prop_assert!(ring.is_empty());
        prop_assert_eq!(ring.pop(), None);
        prop_assert!(ring.push(0x42));
        prop_assert_eq!(ring.pop(), Some(0x42));
        prop_assert!(ring.is_empty());
    }
}
