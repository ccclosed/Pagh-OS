// Feature: USB HID — boot/report protocol и разбор конфигурации
// Свойства `usb_hid_report` / обхода дескрипторов: чистая политика HID-драйвера.
//
// Эти тесты гоняют точные модули, которые компилирует ядро:
//   * `usb_hid_report::transfer_len` — размер буфера из `wMaxPacketSize` с
//     инвариантом «не короче одного пакета» (>= одного полного отчёта);
//   * `usb_hid_report::mouse_report` / `keyboard_report` — boot-раскладки и
//     документированные report-protocol fallback'и (7-байтная 16-битная
//     раскладка E-Signal, 8-битные оси), включая границу неоднозначности;
//   * `usb_desc::parse_hid_interfaces` — композитные устройства с несколькими
//     HID-интерфейсами, альтернативные настройки, испорченные/усечённые
//     дескрипторы.

use crate::usb_desc::{parse_hid_interfaces, HidIface};
use crate::usb_hid_report::{
    bindable, clamp_delta, class_of, keyboard_report, minimum_report_len, mouse_report,
    recovery_action, transfer_len, HidClass, RecoveryAction, ReportMode, BOOT_KEYBOARD_REPORT,
    BOOT_MOUSE_REPORT, MAX_RECOVERIES, MAX_REPORT, PROTOCOL_KEYBOARD, PROTOCOL_MOUSE,
    SUBCLASS_BOOT,
};
use proptest::prelude::*;

// ─── размер transfer-буфера ─────────────────────────────────────────────────

proptest! {
    /// Буфер должен быть не меньше одного целого пакета (защита от babble) и
    /// зажиматься до разбираемого драйвером максимума.
    #[test]
    fn usb_transfer_len_never_shorter_than_a_packet(max_packet in any::<u16>()) {
        let len = transfer_len(max_packet);
        // Биты 11-12 — множитель high-speed, а не байты.
        let mps = (max_packet & 0x07FF) as usize;
        prop_assert!((BOOT_KEYBOARD_REPORT..=MAX_REPORT).contains(&len));
        if mps <= MAX_REPORT {
            prop_assert!(len >= mps, "len {} < wMaxPacketSize {}", len, mps);
        } else {
            prop_assert_eq!(len, MAX_REPORT);
        }
    }
}

/// Регрессия: 7-байтный отчёт при `wMaxPacketSize = 4` (прежняя жёсткая длина
/// мыши) должен помещаться.
#[test]
fn usb_transfer_len_fits_the_7_byte_mouse_report() {
    assert_eq!(transfer_len(4), 8);
    assert!(transfer_len(4) >= 7);
    assert_eq!(transfer_len(7), 8);
    assert_eq!(transfer_len(8), 8);
    assert_eq!(transfer_len(16), 16);
    assert_eq!(transfer_len(64), 64);
    // Биты множителя high-speed не раздувают длину в байтах.
    assert_eq!(transfer_len(0x1808), 8);
    // Завышенные объявления зажимаются (и логируются вызывающим).
    assert_eq!(transfer_len(1024), MAX_REPORT);
    assert_eq!(transfer_len(0), BOOT_KEYBOARD_REPORT);
}

// ─── зажим дельт ────────────────────────────────────────────────────────────

proptest! {
    /// 16-битная дельта отчёта зажимается в 8-битный диапазон PS/2-пакета, а не
    /// заворачивается (прежний `as i8` превращал +1000 в -24).
    #[test]
    fn usb_clamp_delta_keeps_range_and_direction(v in any::<i16>()) {
        let c = clamp_delta(v);
        prop_assert!((i8::MIN..=i8::MAX).contains(&c));
        if (i8::MIN as i16..=i8::MAX as i16).contains(&v) {
            prop_assert_eq!(c, v as i8);
        } else if v > 0 {
            prop_assert_eq!(c, i8::MAX);
        } else {
            prop_assert_eq!(c, i8::MIN);
        }
    }
}

// ─── раскладки отчёта мыши ──────────────────────────────────────────────────

proptest! {
    /// Boot-мышь: ровно `[buttons, dx, dy]`, дельты — i8.
    #[test]
    fn usb_mouse_boot_layout(b0 in any::<u8>(), b1 in any::<u8>(), b2 in any::<u8>()) {
        let r = mouse_report(&[b0, b1, b2], ReportMode::Boot).expect("3-byte boot report");
        prop_assert_eq!(r.buttons, b0 & 0x07);
        prop_assert_eq!(r.dx, b1 as i8 as i16);
        prop_assert_eq!(r.dy, b2 as i8 as i16);
        prop_assert_eq!(r.wheel, 0);
        prop_assert_eq!(r.pan, 0);
    }

    /// 7-байтный report protocol: 16-битные little-endian оси — раскладка
    /// E-Signal/Holtek.
    #[test]
    fn usb_mouse_report_protocol_16bit(
        b0 in any::<u8>(), xlo in any::<u8>(), xhi in any::<u8>(),
        ylo in any::<u8>(), yhi in any::<u8>(), w in any::<u8>(), p in any::<u8>(),
    ) {
        let report = [b0, xlo, xhi, ylo, yhi, w, p];
        let r = mouse_report(&report, ReportMode::Report).expect("7-byte report");
        prop_assert_eq!(r.buttons, b0 & 0x07);
        prop_assert_eq!(r.dx, i16::from_le_bytes([xlo, xhi]));
        prop_assert_eq!(r.dy, i16::from_le_bytes([ylo, yhi]));
        prop_assert_eq!(r.wheel, w as i8);
        prop_assert_eq!(r.pan, p as i8);
        // Байты 16-битного X — НЕ boot-байты dx/dy: слепой boot-разбор прочитал бы xlo
        // как dx, а xhi как dy.
        let boot = mouse_report(&report, ReportMode::Boot).expect("boot parse still possible");
        prop_assert_eq!(boot.dx, xlo as i8 as i16);
        prop_assert_eq!(boot.dy, xhi as i8 as i16);
    }

    /// 3-6-байтные report-protocol отчёты декодируются как 8-битные оси; 4-й байт,
    /// если есть, — колесо. Это документированная граница best effort.
    #[test]
    fn usb_mouse_report_protocol_short(
        b0 in any::<u8>(), b1 in any::<u8>(), b2 in any::<u8>(), b3 in any::<u8>(),
    ) {
        for len in 3..=4usize {
            let report = [b0, b1, b2, b3];
            let r = mouse_report(&report[..len], ReportMode::Report)
                .expect("3-4 byte report protocol report");
            prop_assert_eq!(r.buttons, b0 & 0x07);
            prop_assert_eq!(r.dx, b1 as i8 as i16);
            prop_assert_eq!(r.dy, b2 as i8 as i16);
            prop_assert_eq!(r.wheel, if len >= 4 { b3 as i8 } else { 0 });
            prop_assert_eq!(r.pan, 0);
        }
    }
}

/// Ни одна раскладка мыши не декодируется из меньше чем 3 байт, и это единственный
/// отказ: 3-байтный отчёт валиден в обоих режимах.
#[test]
fn usb_mouse_short_reports_are_rejected() {
    assert!(mouse_report(&[], ReportMode::Boot).is_none());
    assert!(mouse_report(&[0], ReportMode::Boot).is_none());
    assert!(mouse_report(&[0, 1], ReportMode::Report).is_none());
    assert!(mouse_report(&[0, 1, 2], ReportMode::Boot).is_some());
    assert!(mouse_report(&[0, 1, 2], ReportMode::Report).is_some());
}

// ─── отчёт клавиатуры ───────────────────────────────────────────────────────

proptest! {
    /// Usage 0 (нет клавиши) и 1 (ErrorRollOver) не являются нажатиями; всё остальное
    /// передаётся дословно, а отчёты короче boot отклоняются вместо чтения за границей.
    #[test]
    fn usb_keyboard_report_layout(
        mods in any::<u8>(),
        bytes in proptest::collection::vec(any::<u8>(), 6..=12),
    ) {
        let mut report = std::vec![mods, 0u8];
        report.extend_from_slice(&bytes);
        let (m, keys) = keyboard_report(&report).expect(">= 8 bytes");
        prop_assert_eq!(m, mods);
        for i in 0..6 {
            let raw = report[2 + i];
            prop_assert_eq!(keys[i], if raw <= 1 { 0 } else { raw });
        }
        prop_assert!(keyboard_report(&report[..7]).is_none());
    }
}

/// Минимальная длина отчёта по интерфейсу/режиму: boot-клавиатура 8, boot-мышь 3,
/// report-protocol мышь 3 (более короткие раскладки не декодируются), а
/// неподдерживаемый интерфейс не может дать пригодный ввод.
#[test]
fn usb_minimum_report_len_matrix() {
    assert_eq!(
        minimum_report_len(SUBCLASS_BOOT, PROTOCOL_KEYBOARD, ReportMode::Boot),
        BOOT_KEYBOARD_REPORT
    );
    assert_eq!(
        minimum_report_len(SUBCLASS_BOOT, PROTOCOL_KEYBOARD, ReportMode::Report),
        BOOT_KEYBOARD_REPORT
    );
    assert_eq!(
        minimum_report_len(SUBCLASS_BOOT, PROTOCOL_MOUSE, ReportMode::Boot),
        BOOT_MOUSE_REPORT
    );
    assert_eq!(
        minimum_report_len(SUBCLASS_BOOT, PROTOCOL_MOUSE, ReportMode::Report),
        3
    );
    assert_eq!(
        minimum_report_len(0, PROTOCOL_MOUSE, ReportMode::Report),
        usize::MAX
    );
}

// ─── политика класса + политика восстановления ──────────────────────────────

#[test]
fn usb_class_policy_matrix() {
    assert_eq!(class_of(SUBCLASS_BOOT, PROTOCOL_KEYBOARD), HidClass::Keyboard);
    assert_eq!(class_of(SUBCLASS_BOOT, PROTOCOL_MOUSE), HidClass::Mouse);
    // protocol 0 — «нет протокола» (consumer/system/vendor-коллекции).
    assert_eq!(class_of(SUBCLASS_BOOT, 0), HidClass::Unsupported);
    // subclass 0 — не boot-интерфейс.
    assert_eq!(class_of(0, PROTOCOL_KEYBOARD), HidClass::Unsupported);
    assert!(bindable(SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
    assert!(bindable(SUBCLASS_BOOT, PROTOCOL_MOUSE));
    assert!(!bindable(SUBCLASS_BOOT, 0));
    assert!(!bindable(2, PROTOCOL_MOUSE));
}

proptest! {
    /// Граница повторов точна: первые `MAX_RECOVERIES` ошибок сбрасывают эндпоинт, а
    /// следующая отключает его (без бесконечного шторма reset).
    #[test]
    fn usb_recovery_action_bound(previous in any::<u8>()) {
        let expected = if previous < MAX_RECOVERIES {
            RecoveryAction::ResetEndpoint
        } else {
            RecoveryAction::Disable
        };
        prop_assert_eq!(recovery_action(previous), expected);
    }
}

// ─── обход configuration descriptor ─────────────────────────────────────────

fn iface_desc(number: u8, alt: u8, class: u8, subclass: u8, protocol: u8) -> std::vec::Vec<u8> {
    std::vec![9, 4, number, alt, 0, class, subclass, protocol, 0]
}

fn ep_desc(addr: u8, attrs: u8, max_packet: u16, interval: u8) -> std::vec::Vec<u8> {
    let [lo, hi] = max_packet.to_le_bytes();
    std::vec![7, 5, addr, attrs, lo, hi, interval]
}

fn config_desc(total: u16) -> std::vec::Vec<u8> {
    let [lo, hi] = total.to_le_bytes();
    std::vec![9, 2, lo, hi, 0, 7, 0x80, 0x32, 1]
}

/// Композитное устройство с тремя интерфейсами: if0 boot-клавиатура (8-байтные
/// отчёты), if1 HID consumer control (protocol 0), if2 boot-мышь (7-байтный отчёт).
/// Все три разбираются; привязываются только две.
#[test]
fn usb_desc_parses_composite_hid() {
    let mut body = std::vec::Vec::new();
    body.extend(iface_desc(0, 0, 3, SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
    body.extend(ep_desc(0x81, 0x03, 8, 10));
    body.extend(iface_desc(1, 0, 3, SUBCLASS_BOOT, 0));
    body.extend(ep_desc(0x82, 0x03, 8, 10));
    body.extend(iface_desc(2, 0, 3, SUBCLASS_BOOT, PROTOCOL_MOUSE));
    body.extend(ep_desc(0x83, 0x03, 7, 4));
    let mut conf = config_desc(9 + body.len() as u16);
    conf.extend(body);
    conf[5] = 7; // bConfigurationValue

    let ifaces = parse_hid_interfaces(&conf);
    assert_eq!(ifaces.len(), 3, "every HID interface must be found");
    assert_eq!(
        ifaces[0],
        HidIface {
            config_value: 7,
            interface: 0,
            subclass: SUBCLASS_BOOT,
            protocol: PROTOCOL_KEYBOARD,
            ep_addr: 0x81,
            max_packet: 8,
            interval: 10,
        }
    );
    assert_eq!(ifaces[1].interface, 1);
    assert_eq!(ifaces[1].protocol, 0);
    assert_eq!(ifaces[2].ep_addr, 0x83);
    assert_eq!(ifaces[2].max_packet, 7);

    let bound: std::vec::Vec<_> = ifaces
        .iter()
        .filter(|i| bindable(i.subclass, i.protocol))
        .collect();
    assert_eq!(bound.len(), 2, "consumer control is not a boot keyboard/mouse");
    assert_eq!(bound[0].interface, 0);
    assert_eq!(bound[1].interface, 2);
}

/// Альтернативные настройки принадлежат тому же номеру интерфейса и не должны давать
/// второй эндпоинт: драйвер никогда не выбирает альтернативную настройку.
#[test]
fn usb_desc_ignores_alternate_settings_and_duplicate_endpoints() {
    let mut body = std::vec::Vec::new();
    body.extend(iface_desc(0, 0, 3, SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
    // Два interrupt-IN эндпоинта в настройке по умолчанию: используется только первый.
    body.extend(ep_desc(0x81, 0x03, 8, 10));
    body.extend(ep_desc(0x82, 0x03, 8, 10));
    // Альтернативная настройка 1 того же интерфейса: игнорируется целиком.
    body.extend(iface_desc(0, 1, 3, SUBCLASS_BOOT, PROTOCOL_MOUSE));
    body.extend(ep_desc(0x84, 0x03, 8, 10));
    let mut conf = config_desc(9 + body.len() as u16);
    conf.extend(body);

    let ifaces = parse_hid_interfaces(&conf);
    assert_eq!(ifaces.len(), 1);
    assert_eq!(ifaces[0].interface, 0);
    assert_eq!(ifaces[0].protocol, PROTOCOL_KEYBOARD);
    assert_eq!(ifaces[0].ep_addr, 0x81);
}

/// Не-HID интерфейсы, HID-интерфейсы без interrupt-IN эндпоинтов и bulk-эндпоинты
/// пропускаются; обход остаётся выровненным через неизвестные class-дескрипторы
/// (например, HID report descriptor типа 0x22).
#[test]
fn usb_desc_skips_non_input_interfaces() {
    let mut body = std::vec::Vec::new();
    // Не-HID интерфейс (mass storage); эндпоинт 0x01 interrupt, но class != 3.
    body.extend(iface_desc(0, 0, 8, 6, 0x50));
    body.extend(ep_desc(0x01, 0x03, 8, 10));
    // HID-интерфейс, единственный эндпоинт которого — interrupt OUT.
    body.extend(iface_desc(1, 0, 3, SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
    body.extend(ep_desc(0x02, 0x03, 8, 10));
    // HID-интерфейс, единственный эндпоинт которого — bulk IN.
    body.extend(iface_desc(2, 0, 3, SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
    body.extend(ep_desc(0x83, 0x02, 8, 10));
    // HID-интерфейс с class-дескриптором (report descriptor типа 0x22), затем
    // interrupt-IN эндпоинт: оба должны быть пройдены корректно.
    body.extend(iface_desc(3, 0, 3, SUBCLASS_BOOT, PROTOCOL_MOUSE));
    body.extend(std::vec![9, 0x21, 0x11, 0x01, 0, 1, 0x22, 0x40, 0]);
    body.extend(ep_desc(0x84, 0x03, 8, 10));
    let mut conf = config_desc(9 + body.len() as u16);
    conf.extend(body);

    let ifaces = parse_hid_interfaces(&conf);
    assert_eq!(ifaces.len(), 1);
    assert_eq!(ifaces[0].interface, 3);
    assert_eq!(ifaces[0].ep_addr, 0x84);
}

proptest! {
    /// Произвольные байты — «дескриптор лжеца»: обход не должен паниковать, обязан
    /// завершиться, а каждый возвращённый интерфейс — быть HID-интерфейсом с
    /// эндпоинтом направления IN.
    #[test]
    fn usb_desc_is_total_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let ifaces = parse_hid_interfaces(&bytes);
        for iface in &ifaces {
            prop_assert_eq!(iface.ep_addr & 0x80, 0x80, "only IN endpoints are bound");
        }
    }

    /// Усечение валидного дескриптора не паникует и не изобретает интерфейс, которого не
    /// было в полном дескрипторе (свойство префикса: результат — префикс полного разбора).
    #[test]
    fn usb_desc_truncation_is_a_prefix(
        n_interfaces in 1usize..=4,
        max_packet in 1u16..=64,
        cut in 0usize..512,
    ) {
        let mut body = std::vec::Vec::new();
        for n in 0..n_interfaces {
            body.extend(iface_desc(n as u8, 0, 3, SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
            body.extend(ep_desc(0x81 + (n as u8 & 0x0F), 0x03, max_packet, 10));
        }
        let mut conf = config_desc(9 + body.len() as u16);
        conf.extend(body);
        let full = parse_hid_interfaces(&conf);
        let cut = cut.min(conf.len());
        let partial = parse_hid_interfaces(&conf[..cut]);
        prop_assert!(partial.len() <= full.len());
        for (i, iface) in partial.iter().enumerate() {
            prop_assert_eq!(iface, &full[i], "truncated parse diverged at {}", i);
        }
    }
}

/// Поля длины, по которым нельзя пройти (ноль, один или за концом), останавливают
/// обход вместо цикла или чтения за границей.
#[test]
fn usb_desc_rejects_unwalkable_lengths() {
    assert!(parse_hid_interfaces(&[]).is_empty());
    assert!(parse_hid_interfaces(&[0, 4, 0]).is_empty());
    assert!(parse_hid_interfaces(&[1, 4, 0]).is_empty());
    // Дескриптор заявляет 200 байт внутри 9-байтного буфера.
    assert!(parse_hid_interfaces(&[200, 4, 0, 0, 0, 3, 1, 1, 0]).is_empty());
}
