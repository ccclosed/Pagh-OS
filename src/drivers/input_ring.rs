//! Чистое кольцо сырого ввода — очередь между производителями (`IRQ1`/`IRQ12`,
//! USB-HID) и потоком `inputd`.
//!
//! ПОЧЕМУ ОТДЕЛЬНЫЙ МОДУЛЬ. Правило «что делать при переполнении» — это
//! решение, а не деталь: PS/2-кольцо сканкодов ядра (`ps2_kbd`) при заполнении
//! **отвергает новый** байт, а не перезаписывает самый старый. Иначе быстрое
//! нажатие вытеснило бы уже принятый make-код, и следующий break пришёл бы без
//! пары — модификатор «залип» бы, а shell получил бы чужой символ. Очередь
//! `inputd` держит ту же политику, поэтому здесь она и закреплена: `push`
//! возвращает `false` и вызывающий считает потерю, вместо того чтобы молча
//! затереть историю.
//!
//! `core`-only, без зависимостей от ядра: host-свойство `p66` исполняет ровно
//! этот исходник, который компилирует ядро (`host-tests` подключает его через
//! `#[path]`).

// Полный API кольца (`capacity`/`is_empty`/`is_full`/`clear`) используют
// host-свойства и inline-тесты; ядро берёт только `push`/`pop`/`len`. Молчать об
// этом `dead_code`-предупреждением в каждой сборке ядра не нужно.
#![allow(dead_code)]

/// FIFO фиксированной ёмкости `N` без аллокаций.
///
/// `N` должно быть больше нуля; при `N == 0` кольцо всегда полно и `push`
/// отвергает любой элемент (деления на ноль не происходит — проверка ёмкости
/// стоит до вычисления индекса).
pub struct InputRing<T: Copy, const N: usize> {
    slots: [Option<T>; N],
    /// Индекс самого старого элемента (голова очереди).
    head: usize,
    /// Сколько слотов занято; хвост всегда `(head + len) % N`.
    len: usize,
}

impl<T: Copy, const N: usize> InputRing<T, N> {
    pub const fn new() -> Self {
        InputRing {
            slots: [None; N],
            head: 0,
            len: 0,
        }
    }

    /// Ёмкость (число элементов, которые кольцо держит без потерь).
    pub const fn capacity(&self) -> usize {
        N
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_full(&self) -> bool {
        self.len == N
    }

    /// Положить элемент в хвост. `false` — кольцо полно, элемент НЕ принят.
    ///
    /// Решение «отвергнуть новый, сохранив старые» — намеренное: см. шапку
    /// модуля. Вызывающий обязан посчитать отказ (диагностика/`inputd`), а не
    /// игнорировать его.
    pub fn push(&mut self, item: T) -> bool {
        if self.len == N {
            return false;
        }
        let idx = (self.head + self.len) % N;
        self.slots[idx] = Some(item);
        self.len += 1;
        true
    }

    /// Забрать самый старый элемент (FIFO). `None` — пусто.
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let item = self.slots[self.head].take();
        self.head = (self.head + 1) % N;
        self.len -= 1;
        item
    }

    /// Опустошить кольцо (selftest/аварийный сброс).
    pub fn clear(&mut self) {
        for slot in self.slots.iter_mut() {
            *slot = None;
        }
        self.head = 0;
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::InputRing;

    #[test]
    fn fifo_preserves_order() {
        let mut r: InputRing<u8, 4> = InputRing::new();
        assert!(r.push(1) && r.push(2) && r.push(3));
        assert_eq!((r.len(), r.capacity()), (3, 4));
        assert_eq!(r.pop(), Some(1));
        assert_eq!(r.pop(), Some(2));
        assert_eq!(r.pop(), Some(3));
        assert_eq!(r.pop(), None);
        assert!(r.is_empty());
    }

    #[test]
    fn full_ring_rejects_the_new_item_and_keeps_the_prefix() {
        let mut r: InputRing<u8, 3> = InputRing::new();
        assert!(r.push(1) && r.push(2) && r.push(3));
        assert!(!r.push(4), "overflow must reject the newest element");
        assert_eq!(r.len(), 3);
        assert_eq!([r.pop(), r.pop(), r.pop()], [Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn wraps_after_partial_drain() {
        let mut r: InputRing<u8, 4> = InputRing::new();
        assert!(r.push(1) && r.push(2) && r.push(3) && r.push(4));
        assert_eq!(r.pop(), Some(1));
        assert_eq!(r.pop(), Some(2));
        assert!(r.push(5) && r.push(6), "two freed slots accept two items");
        assert_eq!(
            [r.pop(), r.pop(), r.pop(), r.pop()],
            [Some(3), Some(4), Some(5), Some(6)]
        );
    }

    #[test]
    fn zero_capacity_never_accepts() {
        let mut r: InputRing<u8, 0> = InputRing::new();
        assert!(!r.push(1));
        assert_eq!(r.pop(), None);
    }

    #[test]
    fn clear_forgets_pending_items() {
        let mut r: InputRing<u8, 4> = InputRing::new();
        assert!(r.push(9));
        r.clear();
        assert!(r.is_empty());
        assert_eq!(r.pop(), None);
    }
}
