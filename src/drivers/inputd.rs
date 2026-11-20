//! `inputd` — kernel-поток, который диспетчеризует физический ввод.
//!
//! ПОЧЕМУ ОТДЕЛЬНЫЙ ПОТОК. Производители ввода живут в разных контекстах:
//! PS/2 приходит из IRQ1/IRQ12 (прерывания замаскированы, никакого I/O и
//! аллокаций), USB HID — из потока `usb` (опрашиваемый xHCI), а boot-меню
//! вообще работает до планировщика с выключенными прерываниями. Решение «куда
//! именно доставить байт» (evdev или shell-кольцо, с дедупликацией `vtinput`)
//! не должно быть скопировано в каждый из них: раньше оно и было размазано
//! (`ps2_kbd::deliver_physical`, `ps2_mouse::decode_packet`, `usb_hid`), и любой
//! новый источник обязан был повторить правило дедупликации, чтобы не получить
//! двойной ввод.
//!
//! Теперь производитель только **кладёт** элемент в это кольцо (аллокаций нет,
//! один короткий спинлок), а `inputd` — единственный, кто **дренирует** его в
//! потребителей. Правило дедупликации осталось ровно одно и осталось в
//! `ps2_kbd::deliver_physical`; здесь его не дублируют.
//!
//! ПОЧЕМУ БЕЗ ПОТЕРИ И БЕЗ ДУБЛЕЙ. Дренаж держит [`DISPATCH`] на всё время
//! «взять из кольца → доставить»: спинлок маскирует `IF`, поэтому на этом CPU
//! ни IRQ, ни другой поток не могут вклиниться между `pop` и доставкой. Порядок
//! элементов — глобальный порядок попадания в кольцо, каждый элемент
//! доставляется ровно один раз. Именно на это опираются in-QEMU-тесты: они
//! сами зовут [`drain`] и получают детерминированный результат, не останавливая
//! поток.
//!
//! ПЕРЕПОЛНЕНИЕ. Ёмкость [`QUEUE_CAP`] и политика — из [`super::input_ring`]:
//! при заполнении отвергается новый элемент, счётчик [`dropped`] растёт. На
//! нормальной скорости (дренаж раз в тик, 1000 Гц, 256 элементов) очередь пуста
//! практически всегда; счётчик существует, чтобы потеря была наблюдаемой, а не
//! молчаливой.
//!
//! BOOT-МЕНЮ. Оно работает до спавна потоков и с выключенными прерываниями,
//! поэтому само зовёт [`drain`] после `xhci::poll()`: USB-отчёты, которые
//! `usb_hid` перевёл в скан-коды, попадают в shell-кольцо тем же кодом, что и в
//! работающей системе.
//!
//! SMP НЕ ЗАЯВЛЯЕТСЯ: один CPU, один `inputd`. Никаких per-CPU очередей здесь
//! нет и не подразумевается.

use crate::sync::spinlock::Spinlock;

use super::input_ring::InputRing;

/// Ёмкость очереди сырого ввода.
///
/// Диспетчер обслуживает очередь каждый тик (1 мс при `TICK_HZ = 1000`), поэтому
/// 256 элементов — это несколько секунд обычной печати и сотни миллисекунд даже
/// непрерывного шторма с USB-клавиатуры (один HID-отчёт разворачивается максимум
/// в ~16 байт). Больше не нужно; при переполнении включается [`dropped`].
pub const QUEUE_CAP: usize = 256;

/// Один элемент сырого ввода — ровно то, что производитель знает о событии.
///
/// Варианты не взаимозаменяемы: `Aux` — байт потока 8042 (собирается в пакет
/// внутри `ps2_mouse`), `MousePacket` — уже собранный USB-пакет; смешивать их
/// нельзя, иначе сборщик PS/2-пакета дочитал бы чужой байт. `Scancode` — байт
/// set-1 из IRQ1 или из USB HID-транслятора.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Raw {
    Scancode(u8),
    Aux(u8),
    MousePacket([u8; 3]),
}

struct State {
    ring: InputRing<Raw, QUEUE_CAP>,
    pushed: u64,
    dropped: u64,
    drained: u64,
}

static QUEUE: Spinlock<State> = Spinlock::new(State {
    ring: InputRing::new(),
    pushed: 0,
    dropped: 0,
    drained: 0,
});

/// Сериализует весь дренаж и маскирует прерывания на его время.
///
/// Это не «ещё один лок защиты данных» (данные защищает [`QUEUE`]): он
/// превращает «взять элемент → доставить» в неделимую операцию, поэтому два
/// одновременных дренажа не могут доставить элементы в обратном порядке.
static DISPATCH: Spinlock<()> = Spinlock::new(());

/// Положить элемент в очередь. Вызывается и из IRQ-контекста: только спинлок и
/// запись в массив, без аллокаций и без доставки.
fn push(item: Raw) {
    let mut q = QUEUE.lock();
    q.pushed = q.pushed.wrapping_add(1);
    if !q.ring.push(item) {
        q.dropped = q.dropped.wrapping_add(1);
    }
}

/// Байт set-1 от PS/2 IRQ1 или USB HID. Доставит `inputd` (или [`drain`]).
pub fn push_scancode(byte: u8) {
    push(Raw::Scancode(byte));
}

/// Байт aux-потока PS/2 (IRQ12). Сборка пакета — в `ps2_mouse`.
pub fn push_aux(byte: u8) {
    push(Raw::Aux(byte));
}

/// Готовый пакет USB HID мыши.
pub fn push_mouse_packet(packet: [u8; 3]) {
    push(Raw::MousePacket(packet));
}

/// Доставить один элемент в потребителя. Вызывается только под [`DISPATCH`].
fn dispatch(item: Raw) {
    match item {
        // PS/2 IRQ1 и USB HID сходятся здесь: одно правило маршрутизации
        // (`vtinput`-дедупликация) на оба источника.
        Raw::Scancode(byte) => super::ps2_kbd::deliver_physical(byte),
        Raw::Aux(byte) => super::ps2_mouse::feed_aux_byte(byte),
        Raw::MousePacket(packet) => super::ps2_mouse::commit_packet(packet),
    }
}

/// Доставить всё, что накопилось, и вернуть число доставленных элементов.
///
/// Вызывается потоком `inputd` раз в тик, boot-меню (после `xhci::poll()`) и
/// in-QEMU-тестами. НЕ вызывать из IRQ-контекста: доставка берёт другие локы и
/// зовёт `evdev`/`ps2_mouse`, которые для IRQ не предназначены.
pub fn drain() -> usize {
    let _serial = DISPATCH.lock();
    let mut delivered = 0usize;
    loop {
        let item = {
            let mut q = QUEUE.lock();
            match q.ring.pop() {
                Some(item) => {
                    q.drained = q.drained.wrapping_add(1);
                    item
                }
                None => break,
            }
        };
        dispatch(item);
        delivered += 1;
    }
    delivered
}

/// Тело kernel-потока `inputd`: дренаж раз в тик.
///
/// Спавнится из `boot::kernel_main` безусловно: без оборудования очередь пуста,
/// а `sleep_ticks(1)` отдаёт CPU. Поток не рендерит курсор и не интерпретирует
/// клики — это userspace-композитор (`/dev/input/event0`).
pub fn inputd_thread() {
    crate::info!("inputd: dispatch thread started");
    loop {
        drain();
        crate::task::scheduler::sleep_ticks(1);
    }
}

/// Снимок счётчиков `inputd` (диагностика/`input_summary`/selftest).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    pub pending: usize,
    pub pushed: u64,
    pub drained: u64,
    pub dropped: u64,
}

pub fn stats() -> Stats {
    let q = QUEUE.lock();
    Stats {
        pending: q.ring.len(),
        pushed: q.pushed,
        drained: q.drained,
        dropped: q.dropped,
    }
}

/// Сколько элементов ждёт дренажа.
pub fn pending() -> usize {
    QUEUE.lock().ring.len()
}

/// Сколько элементов отвергнуто переполнением с момента загрузки.
pub fn dropped() -> u64 {
    QUEUE.lock().dropped
}
