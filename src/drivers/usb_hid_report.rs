//! Чистая политика USB HID отчётов: размер буфера, раскладки, граница
//! восстановления.
//!
//! Решения о том, каким должен быть transfer-буфер, какая у отчёта раскладка и
//! сколько раз можно сбрасывать остановленный эндпоинт, не зависят от железа и
//! живут здесь, чтобы host-тесты исполняли тот же исходник, что компилирует ядро.

/// Значение `bInterfaceSubClass`, означающее «boot-интерфейс» (HID 1.11 §4.2).
pub const SUBCLASS_BOOT: u8 = 1;
/// `bInterfaceProtocol` 1 — клавиатура (HID 1.11 §4.3).
pub const PROTOCOL_KEYBOARD: u8 = 1;
/// `bInterfaceProtocol` 2 — мышь (HID 1.11 §4.3).
pub const PROTOCOL_MOUSE: u8 = 2;

/// Фиксированный размер boot-отчёта клавиатуры (HID 1.11, приложение B.1).
pub const BOOT_KEYBOARD_REPORT: usize = 8;
/// Фиксированный размер boot-отчёта мыши (HID 1.11, приложение B.2).
pub const BOOT_MOUSE_REPORT: usize = 3;

/// Наибольшая interrupt-IN передача, которую вооружает драйвер.
///
/// Full-speed HID-эндпоинт может заявить до 64 байт. Report-protocol HID-
/// устройства могут заявить больше, но такой отчёт клавиатуры/мыши драйвер
/// разобрать не может; буфер зажимается, а зажим логируется, чтобы необычно
/// большой эндпоинт был виден, а не давал молчаливый цикл babble.
pub const MAX_REPORT: usize = 64;

/// Сколько попыток Reset Endpoint получает один HID-эндпоинт до отказа драйвера
/// от него. Выбрано, чтобы пережить разовый babble/data-buffer-error от
/// report-protocol устройства, чей отчёт длиннее буфера, но не устроить бесконечный
/// шторм reset на действительно мёртвом устройстве.
pub const MAX_RECOVERIES: u8 = 3;

/// Какой разборщик нужен интерфейсу — по байтам subclass/protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HidClass {
    Keyboard,
    Mouse,
    /// Vendor/consumer-коллекции и интерфейсы с `protocol = 0`: это не boot-
    /// клавиатура и не мышь, и раскладки для них у драйвера нет.
    Unsupported,
}

/// Какую раскладку шлёт устройство — решил результат SET_PROTOCOL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportMode {
    /// SET_PROTOCOL(0) прошёл: фиксированные 8 байт клавиатуры / 3 байта мыши.
    Boot,
    /// Устройство отказало SET_PROTOCOL(0) и шлёт раскладку своего report
    /// descriptor. Разбор best effort, описан в `usb_hid.rs`.
    Report,
}

impl ReportMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            ReportMode::Boot => "boot",
            ReportMode::Report => "report",
        }
    }
}

/// Является ли интерфейс boot-клавиатурой/мышью, которую драйвер умеет разбирать.
///
/// Boot *class* обязателен даже для устройства, которое в итоге работает в report
/// protocol: у vendor-коллекции с protocol 0 нет семантики клавиатуры/мыши, и
/// привязка её эндпоинта сожгла бы DMA-буфер и кольцо ради отчётов, которые
/// `usb_hid` всё равно обязан игнорировать.
pub fn bindable(subclass: u8, protocol: u8) -> bool {
    class_of(subclass, protocol) != HidClass::Unsupported
}

/// Классифицировать интерфейс по `bInterfaceSubClass`/`bInterfaceProtocol`.
pub fn class_of(subclass: u8, protocol: u8) -> HidClass {
    if subclass != SUBCLASS_BOOT {
        return HidClass::Unsupported;
    }
    match protocol {
        PROTOCOL_KEYBOARD => HidClass::Keyboard,
        PROTOCOL_MOUSE => HidClass::Mouse,
        _ => HidClass::Unsupported,
    }
}

/// Размер буфера interrupt-IN передачи для эндпоинта.
///
/// ИНВАРИАНТ: запрошенная длина должна быть не меньше `wMaxPacketSize` эндпоинта,
/// потому что одна interrupt-передача может законно доставить полный пакет.
/// Устройство, шлющее больше запрошенной длины, заставляет контроллер сообщить
/// babble (CC 3) и остановить эндпоинт. Нижняя граница 8 сохраняет boot-отчёт
/// клавиатуры целым, даже если сломанный дескриптор заявил меньший размер пакета.
pub fn transfer_len(max_packet: u16) -> usize {
    // Биты 11-12 wMaxPacketSize — множитель high-speed, а не байты.
    let mps = (max_packet & 0x07FF) as usize;
    mps.max(BOOT_KEYBOARD_REPORT).min(MAX_REPORT)
}

/// Минимальная длина отчёта, которую можно декодировать для этого интерфейса.
///
/// Boot-отчёты клавиатуры фиксированы на 8 байтах, boot-отчёт мыши — 3. В report
/// protocol раскладка неизвестна, но мышиная не декодируется из меньше чем 3 байт,
/// а пути клавиатуры нужна 8-байтная boot-подобная форма (best effort, см.
/// `usb_hid.rs`).
pub fn minimum_report_len(subclass: u8, protocol: u8, mode: ReportMode) -> usize {
    match (class_of(subclass, protocol), mode) {
        (HidClass::Keyboard, _) => BOOT_KEYBOARD_REPORT,
        (HidClass::Mouse, ReportMode::Boot) => BOOT_MOUSE_REPORT,
        (HidClass::Mouse, ReportMode::Report) => 3,
        (HidClass::Unsupported, _) => usize::MAX,
    }
}

/// Один декодированный отчёт относительного указателя.
///
/// `dx`/`dy` в HID-соглашении знаков: положительный `dy` направлен вниз по экрану.
/// Транслятор переворачивает Y для PS/2-пути; хранение сырого значения здесь делает
/// тесты раскладок независимыми от этого соглашения потребителя.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseReport {
    pub buttons: u8,
    pub dx: i16,
    pub dy: i16,
    /// Вертикальные щелчки колеса (только report protocol; в PS/2-пути здесь нет
    /// wheel-байта, поэтому пока только диагностика).
    pub wheel: i8,
    /// Горизонтальный pan (только report protocol, только диагностика).
    pub pan: i8,
}

/// Декодировать один отчёт мыши согласно режиму интерфейса.
///
/// Boot-режим — фиксированные 3 байта `[buttons, dx, dy]`. В report-режиме точная
/// раскладка зависит от устройства, а разбор report descriptor вне области; драйвер
/// поддерживает две формы:
///
/// * **7 байт и больше** — отчёт в стиле E-Signal/Holtek
///   `[buttons, x_lo, x_hi, y_lo, y_hi, wheel, pan]` с 16-битными little-endian
///   X/Y;
/// * **3-6 байт** — 8-битные оси: `[buttons, dx, dy, (wheel), ...]`. 5/6-байтную
///   16-битную раскладку без report descriptor не отличить, и она документирована
///   как неподдерживаемая, а не угадывается.
pub fn mouse_report(report: &[u8], mode: ReportMode) -> Option<MouseReport> {
    if report.len() < 3 {
        return None;
    }
    let buttons = report[0] & 0x07;
    match mode {
        ReportMode::Boot => Some(MouseReport {
            buttons,
            dx: report[1] as i8 as i16,
            dy: report[2] as i8 as i16,
            wheel: 0,
            pan: 0,
        }),
        ReportMode::Report => {
            if report.len() >= 7 {
                Some(MouseReport {
                    buttons,
                    dx: i16::from_le_bytes([report[1], report[2]]),
                    dy: i16::from_le_bytes([report[3], report[4]]),
                    wheel: report[5] as i8,
                    pan: report[6] as i8,
                })
            } else {
                Some(MouseReport {
                    buttons,
                    dx: report[1] as i8 as i16,
                    dy: report[2] as i8 as i16,
                    wheel: if report.len() >= 4 {
                        report[3] as i8
                    } else {
                        0
                    },
                    pan: 0,
                })
            }
        }
    }
}

/// Декодировать один boot-отчёт клавиатуры: `(модификаторы, шесть зажатых usage)`.
///
/// Байт 1 boot-отчёта зарезервирован, байты 2-7 — usage; `0` (нет клавиши) и `1`
/// (ErrorRollOver) не являются нажатиями. Report-клавиатуры разбираются по той же
/// форме (best effort, документировано); интерфейс, чей отчёт не дотягивает даже до
/// 8 байт, здесь отклоняется, а не читается за границей.
pub fn keyboard_report(report: &[u8]) -> Option<(u8, [u8; 6])> {
    if report.len() < BOOT_KEYBOARD_REPORT {
        return None;
    }
    let mut held = [0u8; 6];
    for (i, slot) in held.iter_mut().enumerate() {
        let usage = report[2 + i];
        *slot = if usage <= 1 { 0 } else { usage };
    }
    Some((report[0], held))
}

/// Зажать относительную дельту в 8-битный диапазон, который несёт PS/2-пакет.
///
/// 16-битный отчёт может попросить +1000 за один poll; PS/2-пакет 9-битный, и
/// сужение через `as i8` завернуло бы +1000 в -24 (курсор прыгнул бы назад).
/// Зажим сохраняет направление и максимальную скорость.
pub fn clamp_delta(v: i16) -> i8 {
    v.clamp(i8::MIN as i16, i8::MAX as i16) as i8
}

/// Что драйвер должен сделать после ошибки передачи на эндпоинте.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Выдать xHCI Reset Endpoint и переармировать передачу на следующем poll.
    ResetEndpoint,
    /// Бюджет повторов исчерпан: перестать опрашивать этот эндпоинт.
    Disable,
}

/// Политика восстановления одного эндпоинта по числу уже сделанных reset.
pub fn recovery_action(previous_attempts: u8) -> RecoveryAction {
    if previous_attempts < MAX_RECOVERIES {
        RecoveryAction::ResetEndpoint
    } else {
        RecoveryAction::Disable
    }
}
