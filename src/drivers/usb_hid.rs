//! USB HID boot protocol → существующий путь ввода ядра.
//!
//! Транслятор, а не второй стек ввода. Shell, редактор строки, защёлки ^C/^Z,
//! stdin `lxrun` и все compat-процессы читают **PS/2 set-1 скан-коды** из одного
//! ring buffer (`drivers::ps2_kbd`). USB-клавиатура присылает HID usage codes в
//! 8-байтном boot-отчёте. Вместо обучения каждого потребителя второму источнику
//! модуль переводит отчёт в ровно тот байтовый поток, который дал бы 8042, —
//! включая префикс `0xE0` для extended-клавиш и break-коды со старшим битом — и
//! кладёт его в то же кольцо. Ниже по потоку разницы не видно, поэтому ^C с
//! USB-клавиатуры останавливает `lxrun`-программу, как и раньше.
//!
//! Boot protocol в первую очередь, report protocol — только документированный
//! fallback. Boot-клавиатура всегда шлёт фиксированный 8-байтный отчёт
//! `[modifiers, reserved, key0..key5]`, boot-мышь — 3-байтный
//! `[buttons, dx, dy]`, поэтому нормальному пути разбор report descriptor не
//! нужен (и не реализован). xHCI-драйвер отправляет SET_PROTOCOL(0) — boot —
//! каждому boot-интерфейсу.
//!
//! FALLBACK REPORT-PROTOCOL. Устройство может отказать SET_PROTOCOL(0) (STALL) и
//! слать раскладку своего report descriptor, которую драйвер без разбора не
//! знает. В этом случае boot-формат слепо не парсится: для клавиатуры
//! принимается 8-байтная boot-подобная раскладка (большинство report-клавиатур её
//! и используют), а мышь разбирает
//! [`crate::drivers::usb_hid_report::mouse_report`], который знает 7-байтную
//! 16-битную раскладку E-Signal/Holtek и иначе считает оси 8-битными.
//! Wheel/pan декодируются, но не доставляются (в PS/2-путь, который они кормят,
//! нет wheel-байта); это документированное ограничение, а не молчаливое.
//!
//! Решения о буфере и формате живут в `usb_hid_report` — чистом модуле, который
//! включают host-тесты, поэтому правила раскладки проверяются без USB-устройства.
//!
//! СОСТОЯНИЕ. Boot-отчёт — снимок зажатых клавиш, а не поток событий: транслятор
//! хранит предыдущее множество, чтобы вывести make/break-коды, и так же сравнивает
//! байт модификаторов.

use crate::drivers::usb_hid_report::{self, HidClass, ReportMode};
use crate::drivers::{ps2_kbd, ps2_mouse};
use crate::sync::spinlock::Spinlock;
use core::sync::atomic::{AtomicU64, Ordering};

/// USB HID usage id → PS/2 set-1 скан-коды (без префикса `0xE0`).
///
/// Разреженная таблица по usage id; `0` означает «нет отображения» (медиа-клавиши,
/// питание и vendor-диапазоны, которые boot-клавиатура и не должна слать).
struct KeyMap {
    usage: u8,
    code: u8,
}

/// Записи с префиксом несут байт расширения `0xE0`, как в PS/2-потоке.
const fn k(usage: u8, code: u8) -> KeyMap {
    KeyMap { usage, code }
}

static MAP: [KeyMap; 102] = [
    k(0x04, 0x1E), // a
    k(0x05, 0x30), // b
    k(0x06, 0x2E), // c
    k(0x07, 0x20), // d
    k(0x08, 0x12), // e
    k(0x09, 0x21), // f
    k(0x0A, 0x22), // g
    k(0x0B, 0x23), // h
    k(0x0C, 0x17), // i
    k(0x0D, 0x24), // j
    k(0x0E, 0x25), // k
    k(0x0F, 0x26), // l
    k(0x10, 0x32), // m
    k(0x11, 0x31), // n
    k(0x12, 0x18), // o
    k(0x13, 0x19), // p
    k(0x14, 0x10), // q
    k(0x15, 0x13), // r
    k(0x16, 0x1F), // s
    k(0x17, 0x14), // t
    k(0x18, 0x16), // u
    k(0x19, 0x2F), // v
    k(0x1A, 0x11), // w
    k(0x1B, 0x2D), // x
    k(0x1C, 0x15), // y
    k(0x1D, 0x2C), // z
    k(0x1E, 0x02), // 1
    k(0x1F, 0x03), // 2
    k(0x20, 0x04), // 3
    k(0x21, 0x05), // 4
    k(0x22, 0x06), // 5
    k(0x23, 0x07), // 6
    k(0x24, 0x08), // 7
    k(0x25, 0x09), // 8
    k(0x26, 0x0A), // 9
    k(0x27, 0x0B), // 0
    k(0x28, 0x1C), // Enter
    k(0x29, 0x01), // Escape
    k(0x2A, 0x0E), // Backspace
    k(0x2B, 0x0F), // Tab
    k(0x2C, 0x39), // Space
    k(0x2D, 0x0C), // -
    k(0x2E, 0x0D), // =
    k(0x2F, 0x1A), // [
    k(0x30, 0x1B), // ]
    k(0x31, 0x2B), // backslash
    k(0x32, 0x2B), // (non-US # — same physical key)
    k(0x33, 0x27), // ;
    k(0x34, 0x28), // '
    k(0x35, 0x29), // `
    k(0x36, 0x33), // ,
    k(0x37, 0x34), // .
    k(0x38, 0x35), // /
    k(0x39, 0x3A), // CapsLock
    k(0x3A, 0x3B), // F1
    k(0x3B, 0x3C),
    k(0x3C, 0x3D),
    k(0x3D, 0x3E),
    k(0x3E, 0x3F),
    k(0x3F, 0x40),
    k(0x40, 0x41),
    k(0x41, 0x42),
    k(0x42, 0x43),
    k(0x43, 0x44),
    k(0x44, 0x57), // F11
    k(0x45, 0x58), // F12
    k(0x49, 0x52), // Insert      (E0)
    k(0x4A, 0x47), // Home        (E0)
    k(0x4B, 0x49), // PageUp      (E0)
    k(0x4C, 0x53), // Delete      (E0)
    k(0x4D, 0x4F), // End         (E0)
    k(0x4E, 0x51), // PageDown    (E0)
    k(0x4F, 0x4D), // Right       (E0)
    k(0x50, 0x4B), // Left        (E0)
    k(0x51, 0x50), // Down        (E0)
    k(0x52, 0x48), // Up          (E0)
    k(0x53, 0x45), // NumLock
    k(0x54, 0x35), // KP /
    k(0x55, 0x37), // KP *
    k(0x56, 0x4A), // KP -
    k(0x57, 0x4E), // KP +
    k(0x58, 0x1C), // KP Enter
    k(0x59, 0x4F), // KP 1
    k(0x5A, 0x50),
    k(0x5B, 0x51),
    k(0x5C, 0x4B),
    k(0x5D, 0x4C),
    k(0x5E, 0x4D),
    k(0x5F, 0x47),
    k(0x60, 0x48),
    k(0x61, 0x49),
    k(0x62, 0x52),
    k(0x63, 0x53), // KP .
    k(0x64, 0x56), // 102nd key (<> on ISO layouts)
    k(0x68, 0x3F), // F13 (uncommon)
    k(0x69, 0x40),
    k(0x6A, 0x41),
    k(0x6B, 0x42),
    k(0x6C, 0x43),
    k(0x6D, 0x44),
    k(0x6E, 0x57),
    k(0x6F, 0x58),
];

/// Usage, которые в PS/2-потоке обязаны идти с префиксом `0xE0`.
///
/// Настоящий 8042 выдаёт префикс для «расширенной» половины клавиатуры: стрелки,
/// навигационный блок, Enter и слэш цифрового блока и правые модификаторы. Ошибка
/// здесь видна: ^C с правого Ctrl или стрелка пришли бы посторонним символом.
fn is_extended(usage: u8) -> bool {
    matches!(usage, 0x49..=0x52 | 0x54 | 0x55 | 0x57 | 0x58 | 0x5E..=0x63 | 0x64)
}

fn lookup(usage: u8) -> Option<u8> {
    MAP.iter().find(|m| m.usage == usage).map(|m| m.code)
}

/// Состояние клавиатуры: клавиши и модификаторы, зажатые в предыдущем отчёте.
struct KeyboardState {
    /// Usage id, которые сейчас считаются зажатыми (0 — ни одной).
    keys: [u8; 6],
    modifiers: u8,
    /// Отчётов переведено — для диагностики.
    reports: u64,
    /// Клавиши, которые не удалось отобразить (чтобы оператор видел, почему клавиша мертва).
    unmapped: u64,
}

static MOUSE_REPORTS: AtomicU64 = AtomicU64::new(0);

static KEYBOARD: Spinlock<KeyboardState> = Spinlock::new(KeyboardState {
    keys: [0; 6],
    modifiers: 0,
    reports: 0,
    unmapped: 0,
});

/// Биты модификаторов в байте 0 boot-отчёта клавиатуры и скан-код каждого.
/// `ext` отмечает правые варианты (префикс `0xE0`).
const MODIFIERS: [(u8, u8, bool); 8] = [
    (1 << 0, 0x1D, false), // Left Ctrl
    (1 << 1, 0x2A, false), // Left Shift
    (1 << 2, 0x38, false), // Left Alt
    (1 << 3, 0x5B, true),  // Left GUI
    (1 << 4, 0x1D, true),  // Right Ctrl
    (1 << 5, 0x36, false), // Right Shift
    (1 << 6, 0x38, true),  // Right Alt
    (1 << 7, 0x5C, true),  // Right GUI
];

fn emit(usage: u8, make: bool) {
    let Some(code) = lookup(usage) else {
        return;
    };
    let ext = is_extended(usage);
    if ext {
        ps2_kbd::inject_scancode(0xE0);
    }
    ps2_kbd::inject_scancode(if make { code } else { code | 0x80 });
}

/// Перевести один отчёт клавиатуры (8 байт, boot или boot-подобный) в PS/2
/// скан-коды.
fn feed_keyboard(report: &[u8]) {
    let Some((mods, held)) = usb_hid_report::keyboard_report(report) else {
        return;
    };

    let mut state = KEYBOARD.lock();
    state.reports += 1;

    // Сначала модификаторы, чтобы аккорд вроде Ctrl+C доставлялся по порядку.
    for (bit, code, ext) in MODIFIERS {
        let was = state.modifiers & bit != 0;
        let now = mods & bit != 0;
        if was == now {
            continue;
        }
        if ext {
            ps2_kbd::inject_scancode(0xE0);
        }
        ps2_kbd::inject_scancode(if now { code } else { code | 0x80 });
    }
    state.modifiers = mods;

    // Break-коды для клавиш, которые были зажаты и больше нет.
    for i in 0..6 {
        let old = state.keys[i];
        if old == 0 || held.contains(&old) {
            continue;
        }
        if lookup(old).is_none() {
            continue;
        }
        emit(old, false);
    }
    // Make-коды для только что зажатых клавиш.
    for i in 0..6 {
        let new = held[i];
        if new == 0 || state.keys.contains(&new) {
            continue;
        }
        if lookup(new).is_none() {
            state.unmapped += 1;
            continue;
        }
        emit(new, true);
    }
    state.keys = held;
}

/// Перевести один отчёт мыши в PS/2-пакет, который собирает драйвер мыши, чтобы
/// курсор двигался тем же путём.
///
/// Boot-отчёты имеют фиксированную 3-байтную раскладку; report-protocol отчёты
/// разбирает чистая политика раскладок (7 байт с 16-битными осями или 8-битные
/// оси), а дельты зажимаются в диапазон PS/2-пакета вместо заворачивания.
fn feed_mouse(report: &[u8], mode: ReportMode) {
    let Some(decoded) = usb_hid_report::mouse_report(report, mode) else {
        return;
    };
    MOUSE_REPORTS.fetch_add(1, Ordering::Relaxed);
    let buttons = decoded.buttons;
    let dx = usb_hid_report::clamp_delta(decoded.dx);
    // HID boot-mouse отчёты имеют Y-вниз (положительный dy — к низу экрана), а
    // PS/2-соглашение, которое реализует декодер ниже, — Y-вверх (драйвер считает
    // `y - dy`). Перевернуть знак, чтобы USB-мышь двигала курсор как PS/2.
    // `saturating_neg` не даёт i16::MIN из report-протокола переполниться.
    let dy = usb_hid_report::clamp_delta(decoded.dy.saturating_neg());
    // PS/2-пакет: flags (бит 3 всегда 1, биты 0-2 — кнопки, биты 4/5 — знак dx/dy),
    // затем две дельты как 9-битные байты в дополнительном коде.
    let flags = 0x08 | buttons | if dx < 0 { 0x10 } else { 0 } | if dy < 0 { 0x20 } else { 0 };
    ps2_mouse::inject_packet([flags, dx as u8, dy as u8]);
}

/// Подать один HID-отчёт с интерфейса, чьи `subclass`/`protocol` определяют его
/// как boot-клавиатуру или мышь, в той раскладке, которую интерфейс сейчас шлёт.
/// Интерфейсы, не являющиеся boot-клавиатурой/мышью, игнорируются.
pub fn feed_iface(report: &[u8], subclass: u8, protocol: u8, mode: ReportMode) {
    match usb_hid_report::class_of(subclass, protocol) {
        HidClass::Keyboard => feed_keyboard(report),
        HidClass::Mouse => feed_mouse(report, mode),
        HidClass::Unsupported => {}
    }
}

/// Подать один boot-отчёт (точка входа selftest внутри QEMU).
pub fn feed(report: &[u8], protocol: u8) {
    feed_iface(
        report,
        usb_hid_report::SUBCLASS_BOOT,
        protocol,
        ReportMode::Boot,
    );
}

/// `(отчёты клавиатуры, отчёты мыши, неотображённые клавиши)` для диагностики.
pub fn stats() -> (u64, u64, u64) {
    let k = KEYBOARD.lock();
    (k.reports, MOUSE_REPORTS.load(Ordering::Relaxed), k.unmapped)
}
