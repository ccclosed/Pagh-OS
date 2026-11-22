//! Указатель терминальных сессий: `/dev/vtmouse*` → состояние мыши сессии.
//!
//! Почему это отдельный модуль, а не поле в [`super::ps2_mouse`]: физическая
//! мышь у машины одна, и её пакеты приходят в IRQ12. Сессия окна не владеет
//! PS/2 — композитор получает мышь через `/dev/input/event0`, а координаты
//! **внутри окна** не имеют смысла для сканоут-консоли. Поэтому:
//!
//! * [`super::ps2_mouse::poll`] остаётся физическим состоянием (boot-сессия);
//! * `poll()` с точки зрения shell маршрутизируется по задаче: boot-сессия
//!   читает PS/2, сессия окна — сюда (`session::current()`);
//! * клиент окна пишет в `/dev/vtmouse{N}` записи [`super::vt_mouse_abi`].
//!
//! Раньше (этап 1) окно получало **глобальную** мышь: координаты были
//! экранными, а колесо/выделение в окне не работали вовсе. Возврат указателя
//! сессии убирает и то, и другое: каждая сессия помнит свою позицию, кнопки,
//! накопленное колесо и `seq` — ровно те поля, которые читает shell.
//!
//! # Поток записей
//!
//! `write(2)` в `/dev/vtmouse{N}` принимается целиком даже при разрыве записи:
//! хвост (0..15 байт) остаётся в буфере сессии и достраивается следующим
//! `write`'ом. Это тот же контракт, что у `/dev/vtinput*` («запись всегда
//! принимается целиком»), и он избавляет клиента от обязанности писать записи
//! одной операцией — `write` на границе кадра может вернуть короткий счёт.
//!
//! # Изоляция
//!
//! Состояние лежит в массиве по номеру сессии, `[0]` не используется. Сессия
//! читает только свой слот, поэтому указатель в одном окне не двигает
//! выделение в другом и не сбрасывает scrollback соседа. [`reset`] вызывается
//! при выходе сессии, чтобы новое окно в том же слоте не унаследовало кнопку,
//! зажатую в закрытом.

use super::ps2_mouse::MouseState;
use super::session_route::MAX_SESSIONS;
use super::vt_mouse_abi::{clamp_coord, EventStream, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT};
use crate::sync::spinlock::Spinlock;

/// Состояние указателя одной сессии.
struct MouseInner {
    x: i32,
    y: i32,
    left: bool,
    right: bool,
    middle: bool,
    /// Накопленное колесо в щелчках (как у PS/2): потребитель вычитает прошлое
    /// значение, поэтому счётчик оборачивается, а не сбрасывается при чтении.
    wheel: i32,
    /// Растёт на каждую применённую запись — shell просыпается по изменению.
    seq: u64,
    /// Сборщик записей: хвост неполной записи переживает границу `write`.
    stream: EventStream,
}

impl MouseInner {
    const fn new() -> MouseInner {
        MouseInner {
            x: 0,
            y: 0,
            left: false,
            right: false,
            middle: false,
            wheel: 0,
            seq: 0,
            stream: EventStream::new(),
        }
    }
}

/// Состояние указателя по сессиям; `[0]` не используется (boot-сессия читает
/// PS/2 напрямую).
static MOUSE: [Spinlock<MouseInner>; MAX_SESSIONS] =
    [const { Spinlock::new(MouseInner::new()) }; MAX_SESSIONS];

fn slot(session: usize) -> Option<&'static Spinlock<MouseInner>> {
    if session == 0 || session >= MAX_SESSIONS {
        return None;
    }
    Some(&MOUSE[session])
}

/// Снимок указателя сессии для shell/`nano`/`paint`.
///
/// Пустая/неизвестная сессия отвечает нулями: рисовать курсор по «мыши
/// несуществующего окна» негде.
pub fn poll(session: usize) -> MouseState {
    let Some(lock) = slot(session) else {
        return MouseState::default();
    };
    let inner = lock.lock();
    MouseState {
        x: inner.x.max(0) as usize,
        y: inner.y.max(0) as usize,
        left: inner.left,
        right: inner.right,
        middle: inner.middle,
        wheel: inner.wheel,
        seq: inner.seq,
    }
}

/// Принять поток записей указателя от клиента сессии. Возвращает `len`:
/// запись принимается целиком (хвост достраивается в следующий раз).
pub fn write_session(session: usize, bytes: &[u8]) -> usize {
    let Some(lock) = slot(session) else {
        return bytes.len();
    };
    let (width, height) = crate::drivers::framebuffer::dimensions_for(session);
    let mut inner = lock.lock();
    // Замыкание держит `&mut inner`, а `EventStream` — поле `inner`: разложить
    // их одновременно borrow-checker не даст, поэтому сборщик временно
    // извлекается (он `Copy`) и возвращается на место.
    let mut stream = inner.stream;
    stream.push(bytes, &mut |event| {
        inner.x = clamp_coord(event.x, width);
        inner.y = clamp_coord(event.y, height);
        inner.left = event.buttons & BTN_LEFT != 0;
        inner.right = event.buttons & BTN_RIGHT != 0;
        inner.middle = event.buttons & BTN_MIDDLE != 0;
        inner.wheel = inner.wheel.wrapping_add(event.wheel);
        inner.seq = inner.seq.wrapping_add(1);
    });
    inner.stream = stream;
    bytes.len()
}

/// Сбросить состояние сессии (выход/переоткрытие окна).
///
/// Зажатая кнопка не должна переживать окно: новый клиент в том же слоте иначе
/// начал бы с `left = true`, и первое движение «тащило» бы выделение, которого
/// никто не начинал.
pub fn reset(session: usize) {
    if let Some(lock) = slot(session) {
        let mut inner = lock.lock();
        *inner = MouseInner::new();
    }
}

/// Сколько раз состояние сессии менялось (диагностика/тесты).
pub fn seq(session: usize) -> u64 {
    slot(session).map(|lock| lock.lock().seq).unwrap_or(0)
}

/// Признак «сессия вообще получала указатель» (для тестов/логов).
pub fn has_events(session: usize) -> bool {
    seq(session) != 0
}

/// Пустая точка входа для будущих потребителей, которым нужен явный сброс без
/// знания номера сессии: сбрасывает все окна (selftest/reboot-подобные пути).
pub fn reset_all() {
    for session in 1..MAX_SESSIONS {
        reset(session);
    }
}
