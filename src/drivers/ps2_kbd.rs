// drivers/ps2_kbd.rs — драйвер PS/2 клавиатуры
// 64-битное ядро ОС x86_64 на Rust (#![no_std])

use crate::drivers::CharacterDevice;
use crate::sync::spinlock::Spinlock;
use alloc::sync::Arc;

const BUF_SIZE: usize = 128;

struct KeyboardInner {
    buf: [u8; BUF_SIZE],
    head: usize,
    tail: usize,
}

pub struct Ps2Keyboard {
    inner: Spinlock<KeyboardInner>,
}

impl Ps2Keyboard {
    pub fn new() -> Self {
        Ps2Keyboard {
            inner: Spinlock::new(KeyboardInner {
                buf: [0; BUF_SIZE],
                head: 0,
                tail: 0,
            }),
        }
    }

    /// Non-destructive check - is at least one scancode queued?
    pub fn has_scancode(&self) -> bool {
        let inner = self.inner.lock();
        inner.head != inner.tail
    }

    /// Push a scancode byte into the ring buffer (called from IRQ context).
    /// Allocation-free.
    pub fn push_scancode(&self, byte: u8) {
        let mut inner = self.inner.lock();
        let tail = inner.tail;
        let next = (tail + 1) % BUF_SIZE;
        if next != inner.head {
            inner.buf[tail] = byte;
            inner.tail = next;
        }
    }
}

impl CharacterDevice for Ps2Keyboard {
    fn name(&self) -> &str {
        "keyboard"
    }

    fn read_char(&self) -> Option<u8> {
        let mut inner = self.inner.lock();
        if inner.head != inner.tail {
            let byte = inner.buf[inner.head];
            inner.head = (inner.head + 1) % BUF_SIZE;
            Some(byte)
        } else {
            None
        }
    }
}

static KEYBOARD: Spinlock<Option<Arc<Ps2Keyboard>>> = Spinlock::new(None);

/// Кольца ввода терминальных сессий (`[0]` не используется — boot-сессия читает
/// кольцо [`KEYBOARD`]).
///
/// Сессия окна получает **своё** кольцо: физическая клавиатура ей не принадлежит
/// (`/dev/vtinput1` — единственный источник, контракт `vtfb1`/`vtinput1`), поэтому
/// IRQ1/USB никогда в это кольцо не пишут, а `/dev/vtinput1` — пишет. Благодаря
/// отдельному кольцу ввод окна не смешивается с вводом boot-shell'а и наоборот.
static SESSION_RINGS: [Spinlock<KeyboardInner>; crate::drivers::session_route::MAX_SESSIONS] = [const {
    Spinlock::new(KeyboardInner {
        buf: [0; BUF_SIZE],
        head: 0,
        tail: 0,
    })
};
    crate::drivers::session_route::MAX_SESSIONS];

// ─── per-session foreground pid / Ctrl-latches ───────────────────────────────
//
// Каждая консольная сессия имеет СВОЙ foreground-процесс и свои защёлки ^C/^Z:
// два окна (или окно и boot-shell) не должны отбирать друг у друга терминал и
// глотать чужой Ctrl+C. Boot-сессия (0) по-прежнему живёт в публичных статиках
// [`CTRL_C`]/[`CTRL_Z`]/[`FG_PID`] — их читают IRQ1 и старые пути; сессии окон
// (`>= 1`) — в массивах ниже, доступ через [`ctrl_c`]/[`ctrl_z`]/[`fg_pid`].

/// Защёлка Ctrl+C по сессиям (индекс 0 не используется: см. [`CTRL_C`]).
static SESSION_CTRL_C: [core::sync::atomic::AtomicBool;
    crate::drivers::session_route::MAX_SESSIONS] = [const { core::sync::atomic::AtomicBool::new(false) };
    crate::drivers::session_route::MAX_SESSIONS];
/// Защёлка Ctrl+Z по сессиям (индекс 0 не используется).
static SESSION_CTRL_Z: [core::sync::atomic::AtomicBool;
    crate::drivers::session_route::MAX_SESSIONS] = [const { core::sync::atomic::AtomicBool::new(false) };
    crate::drivers::session_route::MAX_SESSIONS];
/// PID foreground `Compat_Process` по сессиям (индекс 0 не используется).
static SESSION_FG_PIDS: [core::sync::atomic::AtomicU64;
    crate::drivers::session_route::MAX_SESSIONS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; crate::drivers::session_route::MAX_SESSIONS];

fn session_slot(session: usize) -> Option<usize> {
    if session != crate::drivers::session_route::BOOT_SESSION
        && session < crate::drivers::session_route::MAX_SESSIONS
    {
        Some(session)
    } else {
        None
    }
}

/// Забрать (и сбросить) защёлку Ctrl+C сессии.
pub fn take_ctrl_c(session: usize) -> bool {
    match session_slot(session) {
        Some(s) => SESSION_CTRL_C[s].swap(false, core::sync::atomic::Ordering::Relaxed),
        None => CTRL_C.swap(false, core::sync::atomic::Ordering::Relaxed),
    }
}

/// Поставить защёлку Ctrl+C сессии.
pub fn set_ctrl_c(session: usize, on: bool) {
    match session_slot(session) {
        Some(s) => SESSION_CTRL_C[s].store(on, core::sync::atomic::Ordering::Relaxed),
        None => CTRL_C.store(on, core::sync::atomic::Ordering::Relaxed),
    }
}

/// Забрать (и сбросить) защёлку Ctrl+Z сессии.
pub fn take_ctrl_z(session: usize) -> bool {
    match session_slot(session) {
        Some(s) => SESSION_CTRL_Z[s].swap(false, core::sync::atomic::Ordering::Relaxed),
        None => CTRL_Z.swap(false, core::sync::atomic::Ordering::Relaxed),
    }
}

/// Поставить защёлку Ctrl+Z сессии.
pub fn set_ctrl_z(session: usize, on: bool) {
    match session_slot(session) {
        Some(s) => SESSION_CTRL_Z[s].store(on, core::sync::atomic::Ordering::Relaxed),
        None => CTRL_Z.store(on, core::sync::atomic::Ordering::Relaxed),
    }
}

/// PID foreground-программы сессии (`0` — foreground у shell).
pub fn fg_pid(session: usize) -> u64 {
    match session_slot(session) {
        Some(s) => SESSION_FG_PIDS[s].load(core::sync::atomic::Ordering::Relaxed),
        None => FG_PID.load(core::sync::atomic::Ordering::Relaxed),
    }
}

/// Записать PID foreground-программы сессии (`0` — вернуть терминал shell).
pub fn set_fg_pid(session: usize, pid: u64) {
    match session_slot(session) {
        Some(s) => SESSION_FG_PIDS[s].store(pid, core::sync::atomic::Ordering::Relaxed),
        None => FG_PID.store(pid, core::sync::atomic::Ordering::Relaxed),
    }
}

/// Initialize the PS/2 keyboard driver.
pub fn init() {
    let kbd = Arc::new(Ps2Keyboard::new());
    crate::drivers::register_char(kbd.clone());
    *KEYBOARD.lock() = Some(kbd);
    crate::debug!("[PS2KBD] Keyboard driver initialized (IRQ1 not wired)");
}

/// Whether an 8042 controller answers at all.
///
/// The status port reads back as `0xFF` on machines without one (the bus floats
/// high), which is exactly the machine this kernel's USB path exists for. The
/// boot menu prints this so a screen-only field report can tell "no keyboard
/// because no PS/2" from "PS/2 present but silent".
pub fn controller_present() -> bool {
    use x86_64::instructions::port::Port;
    // SAFETY: port 0x64 is the 8042 status register; a read has no side effects
    // beyond consuming a pending output byte's status.
    unsafe {
        let mut status: Port<u8> = Port::new(0x64);
        status.read() != 0xFF
    }
}

/// Take one scancode out of the ring, if any is queued.
///
/// The shell reads the ring through its own decoder; this is the same operation
/// for any other kernel consumer (the in-QEMU selftest drives the HID translator
/// and reads back what it produced, which is the only way to test that path
/// without a human at the keyboard).
///
/// Роутинг по задаче: boot-сессия читает кольцо физической клавиатуры, сессия
/// окна — своё кольцо `/dev/vtinput1`. Поэтому `nano`, `paint` и `read_secret`
/// работают в окне, не зная о существовании двух источников.
pub fn take_scancode() -> Option<u8> {
    take_scancode_for(crate::drivers::session::current())
}

/// Взять скан-код из кольца указанной сессии (`0` — физическая клавиатура).
pub fn take_scancode_for(session: usize) -> Option<u8> {
    if session == crate::drivers::session_route::BOOT_SESSION {
        return KEYBOARD.lock().as_ref().and_then(|kbd| kbd.read_char());
    }
    let Some(ring) = SESSION_RINGS.get(session) else {
        return None;
    };
    let mut inner = ring.lock();
    if inner.head == inner.tail {
        return None;
    }
    let byte = inner.buf[inner.head];
    inner.head = (inner.head + 1) % BUF_SIZE;
    Some(byte)
}

/// Есть ли непрочитанный скан-код в кольце указанной сессии.
pub fn has_scancode_for(session: usize) -> bool {
    if session == crate::drivers::session_route::BOOT_SESSION {
        // Именно `has_scancode_raw`, а не `has_scancode`: последний сам
        // маршрутизируется через эту функцию, и вызов дал бы бесконечную
        // рекурсию (переполнение стека ядра на первом же тике после промпта —
        // в живом прогоне это выглядело как «shell перестал принимать ввод»).
        return has_scancode_raw();
    }
    let Some(ring) = SESSION_RINGS.get(session) else {
        return false;
    };
    let inner = ring.lock();
    inner.head != inner.tail
}

/// Сбросить кольцо ввода сессии (новая генерация окна).
///
/// Новая генерация не должна наследовать недоставленные скан-коды прошлой:
/// задача прошлой сессии могла выйти посреди пачки (например, на Enter
/// команды `exit`), и хвост пачки остался в кольце. Новый shell прочитал бы
/// его как «уже набранное» — а break-коды и обрывки make-кодов могли бы
/// съесть первую клавишу пользователя. Сброс делает старт окна детерминированным.
pub fn reset_session(session: usize) {
    if session == crate::drivers::session_route::BOOT_SESSION {
        return;
    }
    let Some(ring) = SESSION_RINGS.get(session) else {
        return;
    };
    let mut inner = ring.lock();
    inner.head = 0;
    inner.tail = 0;
}

/// Положить скан-код в кольцо сессии (`/dev/vtinput1`; см. [`SESSION_RINGS`]).
///
/// Вытеснение при переполнении — то же, что у физического кольца: ввод окна не
/// должно блокировать запись клиента.
pub fn push_session_scancode(session: usize, byte: u8) {
    if session == crate::drivers::session_route::BOOT_SESSION {
        push_to_ring(byte);
        return;
    }
    let Some(ring) = SESSION_RINGS.get(session) else {
        return;
    };
    let mut inner = ring.lock();
    let tail = inner.tail;
    let next = (tail + 1) % BUF_SIZE;
    if next != inner.head {
        inner.buf[tail] = byte;
        inner.tail = next;
    }
}

/// Inject a scancode from another keyboard source (USB HID), exactly as if the
/// 8042 had delivered it.
///
/// The byte enters the *producer* side of the input path — the `inputd` queue —
/// and `inputd` routes it exactly like an IRQ1 byte. Every consumer (the shell's
/// decoder, the `^C`/`^Z` latches, a compat process's stdin) still reads the same
/// PS/2 ring, so a USB keyboard needs no changes downstream. The modifier
/// tracking the driver does for `^C` is deliberately NOT duplicated here: it runs
/// in `irq_handler`, and duplicating it would double-count a Ctrl press that
/// arrived on both paths (firmware PS/2 translation plus a real USB device) and
/// latch `^C` without a keypress.
pub fn inject_scancode(byte: u8) {
    // Производитель ничего не доставляет сам: байт встаёт в очередь `inputd`,
    // и маршрутизацию `deliver_physical` выполняет ОДИН поток для обоих
    // источников — IRQ1 и USB HID. Дедупликация `vtinput` поэтому существует в
    // одном месте, а не повторяется в USB-трансляторе.
    crate::drivers::inputd::push_scancode(byte);
}

/// Доставить физический скан-код (IRQ1 или USB HID) обоим потребителям.
///
/// `/dev/input/event0` получает байт **всегда**: композитор — независимый
/// читатель, и чтение event0 не отнимает скан-коды у shell. А вот в shell-кольцо
/// байт кладётся только пока клавиатурой не владеет внешний VT-клиент
/// ([`crate::drivers::vtinput::client_active`]): этот же байт вернётся в shell
/// через `/dev/vtinput`, и вторая копия в том же кольце дала бы двойной ввод
/// (двойное эхо, два элемента истории, повтор команды). Владельца, который уже
/// завершился, снимает shell-цикл (`vtinput::reap_dead_client`), поэтому
/// shell-only загрузка и закрытый терминал ввод не теряют.
///
/// Единственный вызывающий — диспетчер `inputd::drain`: производитель (IRQ1,
/// USB HID) только кладёт байт в очередь.
pub(crate) fn deliver_physical(byte: u8) {
    crate::drivers::evdev::feed_scancode(byte);
    if !crate::drivers::vtinput::client_active() {
        push_to_ring(byte);
    }
}

/// Положить байт в PS/2-кольцо, из которого читает shell.
fn push_to_ring(byte: u8) {
    if let Some(kbd) = KEYBOARD.lock().as_ref() {
        kbd.push_scancode(byte);
    }
}

/// Положить скан-код в shell-кольцо, НЕ дублируя его в evdev.
///
/// Используется `/dev/vtinput`: терминал уже получает ввод из evdev-очереди
/// desktop'а, и эхо его же синтетических клавиш обратно в event0 дало бы цикл.
/// Шеллу нужен именно scancode-ring, поэтому evdev-путь здесь пропускается.
/// Пока владелец клавиатуры — VT-клиент, это единственная доставка ввода в shell
/// (прямой путь IRQ1/USB байт в кольцо не кладёт).
pub fn inject_scancode_from_vt(byte: u8) {
    push_to_ring(byte);
}

/// Whether at least one scancode is currently queued (non-destructive).
///
/// The shell's REPL wakes on every timer tick; this is how it tells a real
/// keypress from an idle wake-up, so it can skip the overlay/repaint cycle (and
/// the frame flush it ends with) when there is nothing to do.
pub fn has_scancode() -> bool {
    has_scancode_for(crate::drivers::session::current())
}

/// Кольцо физической клавиатуры безусловно (без роутинга по задаче).
///
/// Нужно самому [`has_scancode_for`] и путям, которые обязаны смотреть именно
/// физическое кольцо (роутинг сессии зовёт эту функцию после своей проверки —
/// рекурсия исключена порядком веток).
fn has_scancode_raw() -> bool {
    KEYBOARD
        .lock()
        .as_ref()
        .map(|kbd| kbd.has_scancode())
        .unwrap_or(false)
}

/// IRQ1 handler — reads scancode from port 0x60.
/// Set when the driver-level tracker sees Ctrl+C make, regardless of which
/// consumer later reads the scancode (shell editor OR a compat process's
/// stdin). The `lxrun` foreground wait polls this to terminate the child.
pub static CTRL_C: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Set when the driver sees Ctrl+Z make; the `lxrun` foreground wait turns it
/// into SIGTSTP for the foreground process group.
pub static CTRL_Z: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// PID of the current foreground Compat_Process (set by `cmd_lxrun`).
/// When the driver detects ^C, it directly terminates this pid — the shell
/// may itself be blocked in read() and unable to poll a latch.
pub static FG_PID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

// Driver-level modifier state (set-1): LCtrl 0x1D/0x9D, RCtrl E0 1D/E0 9D,
// C = 0x2E. Tracked here so ^C detection cannot be starved by a program
// that never decodes modifiers itself.
static DRV_CTRL: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static DRV_EXTENDED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn irq_handler() {
    // SAFETY: Reading from port 0x60 is standard PS/2 data port access.
    let scancode: u8 = unsafe {
        let mut port = x86_64::instructions::port::Port::new(0x60);
        port.read()
    };

    // ── ^C tracking (make/break, E0-aware) ──
    use core::sync::atomic::Ordering;
    if scancode == 0xE0 {
        DRV_EXTENDED.store(true, Ordering::Relaxed);
    } else {
        let extended = DRV_EXTENDED.swap(false, Ordering::Relaxed);
        let make = scancode & 0x80 == 0;
        match (extended, scancode) {
            (false, 0x1D) => DRV_CTRL.store(make, Ordering::Relaxed),
            (true, 0x1D) => DRV_CTRL.store(make, Ordering::Relaxed),
            (false, 0x2C) if make && DRV_CTRL.load(Ordering::Relaxed) => {
                CTRL_Z.store(true, Ordering::Relaxed);
            }
            (false, 0x2E) if make && DRV_CTRL.load(Ordering::Relaxed) => {
                CTRL_C.store(true, Ordering::Relaxed);
                let fg = FG_PID.load(Ordering::Relaxed);
                // Клавиатурой владеет клиент окна, а foreground-группа этой
                // ветки — он сам (клиент запущен из boot-shell): ^C, набранный
                // В окне, пришёл сюда как физический scancode, но адресован он
                // session-shell'у окна (его байты уже идут через
                // `/dev/vtinput1`). Убить здесь клиента — значит закрыть всё
                // окно по ^C, набранному внутри него. Программа, запущенная
                // `lxrun` В окне, привязана к сессии 1 и защиту не получает:
                // её ^C по-прежнему доставляется ниже, foreground-группе.
                let window_owns_keys = fg != 0
                    && crate::drivers::vtinput::client_active()
                    && crate::drivers::session::bound_session(fg)
                        == crate::drivers::session_route::BOOT_SESSION;
                if fg != 0
                    && crate::task::compat::compat_exists(fg)
                    && !crate::task::compat::compat_is_raw(fg)
                    && !window_owns_keys
                {
                    // Phase-2 signals: ^C delivers a real SIGINT to the
                    // terminal's FOREGROUND PROCESS GROUP — a job that forked
                    // has several thread groups and every one of them must see
                    // the interrupt, not only the pid the shell records. A
                    // program with a handler gets it (the blocking-wait EINTR
                    // checks wake it); without one the default-terminate action
                    // kills it at the next syscall return — the classic
                    // driver-level bypass remains only as the fallback for a
                    // foreground group that has no live member (e.g. ^C racing
                    // the handoff in `cmd_lxrun`).
                    let pgid = crate::drivers::tty::foreground_pgid();
                    if crate::arch::x86_64::linux::signal::send_signal_to_pgid_all(
                        pgid,
                        crate::arch::x86_64::linux::signal_frame::SIGINT,
                    )
                    .is_err()
                    {
                        crate::task::scheduler::request_exit(fg);
                    }
                }
            }
            _ => {}
        }
    }

    // IRQ только КЛАДЁТ байт в очередь. Доставку (evdev + shell-кольцо с
    // дедупликацией `vtinput`) выполняет поток `inputd`: одно правило на IRQ1 и
    // USB HID, и никакой работы с потребителями в IRQ-контексте.
    crate::drivers::inputd::push_scancode(scancode);
}

/// True when the ring buffer holds an unread scancode. Lets
/// poll/epoll report stdin readable only when a read can make progress.
pub fn has_pending() -> bool {
    KEYBOARD
        .lock()
        .as_ref()
        .map(|k| k.has_scancode())
        .unwrap_or(false)
}
