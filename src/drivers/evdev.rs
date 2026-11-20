//! Очередь evdev-событий и привязка к драйверам ввода.
//!
//! `/dev/input/event0` — не байтовый файл: читатель получает поток 24-байтных
//! `struct input_event` (см. [`super::input_event`]), сформированный из того же
//! ввода, что видит шелл. События **дублируются**, а не вычитываются из
//! PS/2-кольца: открытие/чтение `event0` не отнимает скан-коды у `/dev/stdin`
//! и shell-редактора. Поэтому путь shell не меняется, даже когда композитор
//! держит устройство открытым.
//!
//! Три источника:
//!   * PS/2 IRQ1 и USB HID (через `ps2_kbd::inject_scancode` → очередь
//!     `inputd`) — байты set-1 проходят через общий
//!     [`super::input_event::ScancodeDecoder`] в диспетчере `inputd::drain`;
//!   * PS/2 IRQ12 и USB HID mouse (через `ps2_mouse::inject_packet`) — после
//!     сборки пакета эмитятся `BTN_*`/`REL_X`/`REL_Y`/`REL_WHEEL`;
//!   * каждый пакет закрывается `EV_SYN`/`SYN_REPORT`.
//!
//! Очередь кольцевая, фиксированного размера (без аллокаций в IRQ): при
//! переполнении выбрасывается самое старое событие — так же предпочитает
//! поступать Linux evdev, и свежий ввод важнее истории. `read(2)` на пустой
//! очереди блокируется (или `EAGAIN` для `O_NONBLOCK`), `poll(2)`/`select(2)`
//! сообщают `POLLIN` ровно когда событие есть.

use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use super::input_event::{
    encode_input_event, Event, ScancodeDecoder, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EV_KEY, EV_REL,
    EV_SYN, INPUT_EVENT_SIZE, REL_WHEEL, REL_X, REL_Y, SYN_REPORT,
};
use crate::sync::spinlock::Spinlock;

/// Сколько событий держит кольцо. Композитор вычитывает пачками после
/// `poll`, поэтому 256 событий (6 КиБ) — секунды быстрого ввода; переполнение
/// не теряет свежий хвост.
const QUEUE_CAP: usize = 256;

#[derive(Clone, Copy)]
struct QueuedEvent {
    ev: Event,
    /// Тик ядра в момент события: evdev-таймштамп фиксируется при появлении, а
    /// не при чтении (читатель может прийти позже).
    ticks: u64,
}

struct Queue {
    events: [QueuedEvent; QUEUE_CAP],
    head: usize,
    len: usize,
}

impl Queue {
    const fn new() -> Self {
        Queue {
            events: [QueuedEvent {
                ev: Event {
                    type_: 0,
                    code: 0,
                    value: 0,
                },
                ticks: 0,
            }; QUEUE_CAP],
            head: 0,
            len: 0,
        }
    }

    fn push(&mut self, ev: Event, ticks: u64) {
        if self.len == QUEUE_CAP {
            // Самое старое событие вылетает: догоняющий читатель не должен
            // заклинить ввод, пока композитор занят кадром.
            self.head = (self.head + 1) % QUEUE_CAP;
            self.len -= 1;
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
        let idx = (self.head + self.len) % QUEUE_CAP;
        self.events[idx] = QueuedEvent { ev, ticks };
        self.len += 1;
    }

    fn pop(&mut self) -> Option<QueuedEvent> {
        if self.len == 0 {
            return None;
        }
        let ev = self.events[self.head];
        self.head = (self.head + 1) % QUEUE_CAP;
        self.len -= 1;
        Some(ev)
    }
}

static QUEUE: Spinlock<Queue> = Spinlock::new(Queue::new());
static DECODER: Spinlock<ScancodeDecoder> = Spinlock::new(ScancodeDecoder::new());
/// Кнопки в предыдущем пакете мыши: EV_KEY эмитится только на изменение.
static LAST_BUTTONS: AtomicU8 = AtomicU8::new(0);
/// Счётчик выброшенных при переполнении событий (диагностика/selftest).
static DROPPED: AtomicU64 = AtomicU64::new(0);

fn push(ev: Event) {
    let ticks = crate::task::scheduler::ticks();
    QUEUE.lock().push(ev, ticks);
}

/// Скормить один байт PS/2 set-1 (IRQ1 или USB HID). Декодер stateful, поэтому
/// префикс `E0`, пришедший отдельным вызовом, доживает до следующего байта.
pub fn feed_scancode(byte: u8) {
    let decoded = DECODER.lock().feed(byte);
    if let Some(ev) = decoded {
        push(ev);
        push(Event::new(EV_SYN, SYN_REPORT, 0));
    }
}

/// Один пакет относительного указателя: кнопки (биты 0/1/2), дельты в
/// screen-координатах (dy вниз) и щелчки колеса (вверх положительно).
/// `EV_SYN` закрывает пакет, если в нём было хотя бы одно событие.
pub fn feed_mouse(dx: i32, dy: i32, wheel: i32, buttons: u8) {
    let mut any = false;
    let prev = LAST_BUTTONS.swap(buttons & 0x07, Ordering::Relaxed);
    for (bit, code) in [(0x01u8, BTN_LEFT), (0x02, BTN_RIGHT), (0x04, BTN_MIDDLE)] {
        let was = prev & bit != 0;
        let now = buttons & bit != 0;
        if was != now {
            push(Event::new(EV_KEY, code, if now { 1 } else { 0 }));
            any = true;
        }
    }
    if dx != 0 {
        push(Event::new(EV_REL, REL_X, dx));
        any = true;
    }
    if dy != 0 {
        push(Event::new(EV_REL, REL_Y, dy));
        any = true;
    }
    if wheel != 0 {
        push(Event::new(EV_REL, REL_WHEEL, wheel));
        any = true;
    }
    if any {
        push(Event::new(EV_SYN, SYN_REPORT, 0));
    }
}

/// Есть ли непрочитанное событие (для `poll`/`select`).
pub fn has_events() -> bool {
    QUEUE.lock().len != 0
}

/// Сколько событий ждёт читателя (диагностика/selftest).
pub fn pending() -> usize {
    QUEUE.lock().len
}

/// Забрать одно событие в 24-байтный буфер. `false` — очередь пуста.
pub fn read_event(dst: &mut [u8; INPUT_EVENT_SIZE]) -> bool {
    let Some(q) = QUEUE.lock().pop() else {
        return false;
    };
    let hz = crate::arch::x86_64::apic::TICK_HZ.max(1);
    let sec = q.ticks / hz;
    let usec = (q.ticks % hz) * 1_000_000 / hz;
    *dst = encode_input_event(sec, usec, q.ev);
    true
}

/// Сбросить очередь, декодер и состояние кнопок.
///
/// Нужен selftest'у, чтобы прошлый тест не оставил зажатый `E0`/кнопку и не
/// сделал следующий прогон зависящим от порядка. Рабочий композитор этого не
/// вызывает.
pub fn reset() {
    *QUEUE.lock() = Queue::new();
    DECODER.lock().reset();
    LAST_BUTTONS.store(0, Ordering::Relaxed);
    DROPPED.store(0, Ordering::Relaxed);
}

/// Число событий, потерянных при переполнении (диагностика/selftest).
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// Сколько байт занимает одно событие в ABI (assert хоста/ядра).
pub const EVENT_SIZE: usize = INPUT_EVENT_SIZE;

/// Прямая инъекция одного события (selftest/будущие устройства, у которых нет
/// PS/2-представления). Не добавляет `EV_SYN` — пакет собирает вызывающий.
pub fn inject(ev: Event) {
    push(ev);
}

/// Прямая инъекция `EV_SYN` (selftest).
pub fn sync_now() {
    push(Event::new(EV_SYN, SYN_REPORT, 0));
}
