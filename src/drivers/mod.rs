// drivers/mod.rs — Device Manager и трейты драйверов
// 64-битное ядро ОС x86_64 на Rust (#![no_std])

/// Чистая геометрия текстовой сетки консоли (клетки 8×18 на экране), host-тестируемая.
pub mod console_layout;
pub mod cursor;
pub mod e1000;
pub mod earlyfb;
/// Очередь evdev-событий и дублирование PS/2/USB-ввода в `/dev/input/event0`.
pub mod evdev;
/// Чистый ABI `fbdev` (`struct fb_var/fix_screeninfo`), host-тестируемый.
pub mod fb_abi;
/// Чистая арифметика offscreen-поверхности (геометрия, разбиение по страницам),
/// host-тестируемая.
pub mod fb_surface;
pub mod framebuffer;
/// Единый читатель байтового потока 8042 (IRQ1/IRQ12 + страховочный опрос `inputd`).
pub mod i8042;
/// Чистый кодек `struct input_event` + set-1 → Linux keycodes, host-тестируемый.
pub mod input_event;
/// Чистое кольцо сырого ввода (`inputd`-очередь), host-тестируемое.
pub mod input_ring;
/// `inputd`: kernel-поток диспетчеризации PS/2 + USB-HID в evdev/shell-кольцо.
pub mod inputd;
pub mod memdisk;
pub mod nvme;
pub mod nvme_prp;
/// Пиксельная поверхность offscreen-сессии терминального окна.
pub mod offscreen;
pub mod partition;
pub mod pci;
pub mod ps2_kbd;
pub mod ps2_mouse;
pub mod scrollback;
pub mod serial;
/// Реестр консольных сессий: чья задача владеет каким экземпляром консоли.
pub mod session;
/// Чистая маршрутизация «задача → консольная сессия» (host-тестируемая).
pub mod session_route;
pub mod tty;
pub mod usb_desc;
pub mod usb_hid;
pub mod usb_hid_report;
pub mod usb_rndis;
pub mod usb_rndis_proto;
pub mod virtio;
pub mod vt;
/// Чистый ABI `/dev/vtconsole0` (128-байтный заголовок + 16-байтные клетки).
pub mod vt_abi;
/// Чистый переводчик VT-байтов `/dev/vtinput` в PS/2 set-1.
pub mod vt_keys;
/// Указатель терминальных сессий: `/dev/vtmouse*` → состояние мыши сессии.
pub mod vt_mouse;
/// Чистый ABI `/dev/vtmouse*` (16-байтная запись указателя).
pub mod vt_mouse_abi;
/// Чистый ABI `/dev/vtfb1` (24-байтный `VTFBIOGET_INFO` + план `read`).
pub mod vtfb_abi;
/// `/dev/vtinput`: VT-байты → вход kernel-shell.
pub mod vtinput;
/// Разделяемая VT-поверхность (публикация из shell/VT, mmap/read).
pub mod vtsurface;
pub mod xhci;
mod xhci_hub;

use crate::sync::spinlock::Spinlock;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;

/// Приёмник текстовой консоли (serial, framebuffer, ...). Реализации должны
/// быть безопасны при совместном использовании потоками/IRQ-контекстом (внутренняя изменяемость под локом).
pub trait Console: Send + Sync {
    fn write_str(&self, s: &str);
}

/// Трейт драйверов символьных устройств (клавиатура, serial и т.п.).
pub trait CharacterDevice: Send + Sync {
    fn name(&self) -> &str;
    fn read_char(&self) -> Option<u8>;
}

/// Trait for block-device drivers (disk, etc.).
pub trait BlockDevice: Send + Sync {
    fn name(&self) -> &str;
    fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<usize, ()>;
    fn write_block(&self, block: u64, buf: &[u8]) -> Result<usize, ()>;
    /// Total addressable 512-byte sectors reported by the device.
    ///
    /// Default `0` means "unknown / not reported"; callers that size a layout
    /// from capacity (e.g. `ext2::format`) must treat `0` as "no capacity
    /// information available".
    fn sector_count(&self) -> u64 {
        0
    }

    /// Flush the device's volatile write cache to stable storage (`NVMe FLUSH`,
    /// `VIRTIO_BLK_T_FLUSH`).
    ///
    /// The WAL's crash-consistency argument needs write *ordering* to reach the
    /// medium, not merely to reach the device: a controller with a volatile cache
    /// can reorder or lose everything it has acknowledged when power goes away
    /// (issue #15 — on virtio-blk under QEMU ordering effectively holds because
    /// the "device" is a host file, on real NVMe it does not).
    ///
    /// The default is a no-op, which is correct for devices with no volatile
    /// cache: the RAM-mock `BlockDevice` used by the in-kernel fs tests, and any
    /// driver that has not implemented the command yet.
    fn flush(&self) -> Result<(), ()> {
        Ok(())
    }
}

/// Global device registry.
pub struct DeviceManager {
    chars: BTreeMap<String, Arc<dyn CharacterDevice>>,
    blocks: BTreeMap<String, Arc<dyn BlockDevice>>,
}

static DEVICE_MANAGER: Spinlock<DeviceManager> = Spinlock::new(DeviceManager {
    chars: BTreeMap::new(),
    blocks: BTreeMap::new(),
});

impl DeviceManager {
    pub fn register_char(&mut self, dev: Arc<dyn CharacterDevice>) {
        let name = String::from(dev.name());
        crate::debug!("[DEVMGR] Register char device: {}", name);
        self.chars.insert(name, dev);
    }

    pub fn register_block(&mut self, dev: Arc<dyn BlockDevice>) {
        let name = String::from(dev.name());
        crate::debug!("[DEVMGR] Register block device: {}", name);
        self.blocks.insert(name, dev);
    }
}

/// Called during boot to initialize built-in devices.
pub fn init() {
    crate::debug!("Initializing device manager...");
    ps2_kbd::init();
    framebuffer::init();
    // Shared VT surface для desktop-терминала: страницы аллоцируются один раз
    // на boot (геометрия берётся из уже поднятого framebuffer'а), поэтому
    // открытие /dev/vtconsole0 не аллоцирует PMM и selftest остаётся
    // детерминированным.
    vtsurface::init();
    // The mouse needs the framebuffer dimensions for cursor clamping, so it is
    // brought up after the framebuffer. Failure is non-fatal (no cursor).
    let (w, h) = framebuffer::dimensions();
    if w != 0 {
        ps2_mouse::init(w, h);
    }
    crate::info!("Device manager ready");
}

/// One line describing every input device the kernel can see.
///
/// WHY ON ONE LINE, ON SCREEN. pagh is tested on machines with no serial port:
/// when a keyboard does not type, the only evidence available is what the screen
/// shows. This compresses the whole answer — is there an xHCI controller, does any
/// port have a device, did the device reach a configured HID interface, are
/// reports arriving, is there a PS/2 controller — into one line the boot menu and
/// the shell banner can print. It separates the three failures that look identical
/// from the outside ("no controller", "controller but nothing attached", "attached
/// but silent").
pub fn input_summary() -> alloc::string::String {
    use alloc::format;
    use alloc::string::String;
    let usb = xhci::summary();
    let (kbd_reports, mouse_reports, unmapped) = usb_hid::stats();
    let ports = if usb.ccs_mask == 0 {
        String::from("none")
    } else {
        let mut text = String::new();
        for port in 1..=usb.ports {
            if usb.ccs_mask & (1 << (port - 1)) == 0 {
                continue;
            }
            if !text.is_empty() {
                text.push(',');
            }
            text.push_str(&format!("{}", port));
        }
        text
    };
    // `inputd` печатает своё состояние рядом с оборудованием: `pending` должен
    // быть 0 после синхронного дренажа boot-меню, а `dropped` — 0 на исправной
    // машине. Ненулевой `dropped` здесь означает, что ввод терялся ещё до shell.
    let inputd = inputd::stats();
    format!(
        "usb: {} ctrl, {} ports, device on [{}], {} HID, kbd {} / mouse {} reports{} | ps2: {} | inputd: {} pending, {} dropped",
        usb.controllers,
        usb.ports,
        ports,
        usb.devices,
        kbd_reports,
        mouse_reports,
        if unmapped > 0 {
            format!(", {} unmapped", unmapped)
        } else {
            String::new()
        },
        if ps2_kbd::controller_present() {
            "yes"
        } else {
            "no"
        },
        inputd.pending,
        inputd.dropped
    )
}

/// Whether anything that can produce a keystroke exists: a USB HID device, or an
/// 8042 controller (whose keyboard may simply be quiet). Used by the boot menu to
/// say out loud when it is waiting for a keyboard that the machine does not have.
pub fn any_keyboard() -> bool {
    xhci::summary().devices > 0 || ps2_kbd::controller_present()
}

/// Register a character device in the global manager.
pub fn register_char(dev: Arc<dyn CharacterDevice>) {
    DEVICE_MANAGER.lock().register_char(dev);
}

/// Register a block device in the global manager.
pub fn register_block(dev: Arc<dyn BlockDevice>) {
    DEVICE_MANAGER.lock().register_block(dev);
}

/// Every registered block device as `(name, sector_count)`, name-sorted.
///
/// The registry is a `BTreeMap`, so the order is stable across boots — which is
/// what lets the shell's `disk` menu number devices and a script refer to the
/// same number in the same run.
pub fn list_blocks() -> alloc::vec::Vec<(alloc::string::String, u64)> {
    DEVICE_MANAGER
        .lock()
        .blocks
        .iter()
        .map(|(name, dev)| (name.clone(), dev.sector_count()))
        .collect()
}

/// Get a character device by name
pub fn get_char(name: &str) -> Option<Arc<dyn CharacterDevice>> {
    DEVICE_MANAGER.lock().chars.get(name).map(Arc::clone)
}

/// Get a block device by name (mirrors [`get_char`]).
pub fn get_block(name: &str) -> Option<Arc<dyn BlockDevice>> {
    DEVICE_MANAGER.lock().blocks.get(name).map(Arc::clone)
}
