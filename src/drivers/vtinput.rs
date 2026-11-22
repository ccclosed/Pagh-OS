//! `/dev/vtinput`: VT-байты терминала → вход kernel-shell.
//!
//! Kernel-shell читает ввод только из PS/2-кольца (`ps2_kbd`), поэтому байты,
//! присланные userspace-терминалом, переводятся в set-1 скан-коды чистым
//! [`super::vt_keys::KeyParser`] и кладутся в то же кольцо. Так ввод доходит до
//! того же decoder'а/редактора/хоткеев, что и физическая клавиатура, без
//! второго input-path.
//!
//! # Владелец клавиатуры (дедупликация)
//!
//! Физическая клавиша уже попадает в `/dev/input/event0` (композитор), и
//! терминальное окно возвращает её в `/dev/vtinput`. Если бы IRQ1 продолжал
//! класть тот же байт в shell-кольцо, shell получил бы его дважды — сначала
//! напрямую из IRQ, затем из vtinput. Поэтому успешная запись из
//! userspace-процесса защёлкивает его как **владельца клавиатуры**
//! ([`client_active`]): пока владелец жив, `ps2_kbd` отдаёт физический байт
//! только в evdev (композитору) и **не** кладёт его в shell-кольцо. Единственная
//! доставка в shell идёт через этот модуль — ровно один раз.
//!
//! Защёлка снимается сама: shell раз в четверть секунды зовёт
//! [`reap_dead_client`] и, если процесса-владельца больше нет, возвращает прямой
//! PS/2-путь. Поэтому shell-only загрузка (никто не писал в vtinput) работает
//! как раньше, а после закрытия терминала клавиатура не остаётся «мёртвой».
//! Владелец определяется только для compat-процесса (`current_has_compat`):
//! запись из kernel-контекста (например, selftest) владельцем не становится.
//!
//! Синтетические скан-коды намеренно **не** дублируются в `/dev/input/event0`:
//! desktop читает event0 для своего окна, а `vtinput` пишет уже его собственный
//! терминал — иначе получился бы цикл «терминал → vtinput → event0 → терминал».
//!
//! Кольцо ввода ограничено существующим PS/2-кольцом (128 байт), а `write(2)`
//! не блокируется на медленном shell: лишние скан-коды просто вытесняются
//! кольцом, как при быстром наборе.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::vt_keys::KeyParser;
use crate::sync::spinlock::Spinlock;

/// Декодеры VT-байтов по сессиям: `[0]` — boot-консоль, `[i >= 1]` — окно.
///
/// У каждой сессии свой [`KeyParser`]: ESC-последовательность, начатая в одном
/// окне, не должна достраиваться из байтов другого (и состояние модификаторов
/// тоже сессионное). Чистый декодер тот же самый.
static PARSERS: [Spinlock<KeyParser>; crate::drivers::session_route::MAX_SESSIONS] =
    [const { Spinlock::new(KeyParser::new()) }; crate::drivers::session_route::MAX_SESSIONS];

fn parser_for(session: usize) -> &'static Spinlock<KeyParser> {
    &PARSERS[if session < PARSERS.len() { session } else { 0 }]
}

/// Забыть состояние ввода сессии (выход/переоткрытие окна).
///
/// Имя и смысл совпадают с `reset_session` из #167: недособранная
/// ESC-последовательность не должна переживать генерацию, иначе первые байты
/// нового окна «достраивали» бы последовательность прошлого. Дополнительно
/// чистится кольцо скан-кодов сессии (см. [`crate::drivers::ps2_kbd::reset_session`]):
/// задача прошлой сессии могла выйти посреди пачки.

/// `ioctl(fd, VTINPUT_SET_FOCUS, on)` — окно сообщает ядру, что композитор
/// отдал ему клавиатуру (или забрал).
///
/// Зачем это ядру: физические клавиши приходят в IRQ1/USB и по-прежнему
/// кладутся в boot-кольцо (композитор читает их из `/dev/input/event0`
/// независимо). Пока фокус у терминального окна, клиент возвращает те же
/// клавиши через `/dev/vtinput{N}`, поэтому прямой путь в boot-shell обязан
/// молчать — иначе каждая буква, набранная в терминале, попадала бы и в
/// boot-консоль (двойное эхо/двойная история, «ввод ушёл в boot VT»).
pub const VTINPUT_SET_FOCUS: u64 = 0x4710;

/// Фокус клавиатуры по сессиям: `[0]` не используется.
static FOCUS: [AtomicBool; crate::drivers::session_route::MAX_SESSIONS] =
    [const { AtomicBool::new(false) }; crate::drivers::session_route::MAX_SESSIONS];

/// Запомнить фокус окна (`on == false` — композитор забрал клавиатуру).
pub fn set_focus(session: usize, on: bool) {
    if session == crate::drivers::session_route::BOOT_SESSION || session >= FOCUS.len() {
        return;
    }
    FOCUS[session].store(on, Ordering::Relaxed);
    // #172 защёлкивает физическую клавиатуру на окне, чтобы IRQ-трекинг
    // `^C`/`^Z` не действовал на boot-задание. Держим защёлку в согласии с
    // фокусом композитора: при потере фокуса (клик по рабочему столу) она
    // снимается и клавиатура возвращается boot-shell'у; при возврате фокуса
    // защёлкивается снова, чтобы первое же нажатие не ушло дважды.
    if on {
        claim_client(session);
    } else {
        release_client(session);
    }
}

/// Защёлкнуть владельцем клавиатуры клиента слота (если он известен и жив).
fn claim_client(session: usize) {
    let pid = crate::drivers::session::open_client(session);
    if pid != 0 && crate::task::compat::compat_exists(pid) {
        CLIENT_PID.store(pid, Ordering::Relaxed);
        CLIENT_ACTIVE.store(true, Ordering::Relaxed);
    }
}

/// Снять защёлку, если она принадлежит клиенту этого слота.
fn release_client(session: usize) {
    let pid = crate::drivers::session::open_client(session);
    if pid != 0 && CLIENT_PID.load(Ordering::Relaxed) == pid {
        CLIENT_ACTIVE.store(false, Ordering::Relaxed);
        CLIENT_PID.store(0, Ordering::Relaxed);
    }
}

/// Есть ли сейчас сфокусированное терминальное окно с живой сессией.
///
/// Проверка дешёвая (атомики) и вызывается из `inputd` на каждый скан-код:
/// фокус умершего клиента не должен навсегда «съесть» клавиатуру boot-shell,
/// поэтому живучесть сессии проверяется здесь же. Выход сессии сбрасывает
/// фокус явно ([`reset_session`]).
pub fn session_focused() -> bool {
    for (session, flag) in FOCUS.iter().enumerate().skip(1) {
        if flag.load(Ordering::Relaxed) && crate::drivers::session::live(session) {
            return true;
        }
    }
    false
}

/// Забыть состояние сессии (выход/переоткрытие окна): фокус и недособранная
/// ESC-последовательность не должны переживать генерацию.
pub fn reset_session(session: usize) {
    if session == crate::drivers::session_route::BOOT_SESSION || session >= PARSERS.len() {
        return;
    }
    parser_for(session).lock().reset();
    crate::drivers::ps2_kbd::reset_session(session);
    FOCUS[session].store(false, Ordering::Relaxed);
    release_client(session);
}

/// PID userspace-владельца клавиатуры (последний писавший в `/dev/vtinput`).
static CLIENT_PID: AtomicU64 = AtomicU64::new(0);
/// Быстрый признак для IRQ1 (без локов и без `compat_exists`): физические
/// скан-коды этого владельца уже придут в shell через `vtinput`.
static CLIENT_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Тик последней проверки живости владельца; проверка идёт из shell, не из IRQ.
static LAST_REAP_TICK: AtomicU64 = AtomicU64::new(0);

/// Период проверки живости владельца в тиках (четверть секунды при 1000 Гц).
const REAP_INTERVAL_TICKS: u64 = crate::arch::x86_64::apic::TICK_HZ / 4;

/// Владеет ли сейчас клавиатурой внешний VT-клиент (окно Terminal).
///
/// Читается из IRQ1/USB-пути: только атомик, без `compat_exists` (тот берёт
/// спинлок реестра compat, который IRQ трогать не должен).
pub fn client_active() -> bool {
    CLIENT_ACTIVE.load(Ordering::Relaxed)
}

/// Заявить физическую клавиатуру за вызывающим compat-процессом.
///
/// Вызывается при `open` устройств терминальной сессии и из записи в
/// `/dev/vtinput1`: владение должно начаться с ПЕРВОГО нажатия, а не после него,
/// иначе самый первый `Ctrl+C`/`Ctrl+Z` из окна успевает отработать в boot-сессии
/// (найдено в live-QEMU: окно исчезало). Не-compat вызывающий (ядро/selftest)
/// владельцем не становится.
pub fn claim_keyboard() {
    if crate::task::compat::current_has_compat() {
        note_client(crate::task::scheduler::current_pid());
    }
}

/// Запомнить писавшего как владельца клавиатуры. Вызывается только из
/// [`claim_keyboard`]/[`write`], когда текущая задача — compat-процесс.
fn note_client(pid: u64) {
    CLIENT_PID.store(pid, Ordering::Relaxed);
    CLIENT_ACTIVE.store(true, Ordering::Relaxed);
}

/// Снять защёлку владельца, если его процесс уже завершился.
///
/// Зовётся из shell-цикла (обычный контекст, прерывания включены) и
/// троттлится: `compat_exists` берёт спинлок реестра, а цикл shell крутится на
/// каждом тике. Пока владелец жив, ничего не меняется; после его смерти прямой
/// PS/2-путь в shell возвращается сам — иначе закрытый терминал оставил бы
/// клавиатуру без ввода.
pub fn reap_dead_client() {
    if !client_active() {
        return;
    }
    let now = crate::task::scheduler::ticks();
    let last = LAST_REAP_TICK.load(Ordering::Relaxed);
    if now.wrapping_sub(last) < REAP_INTERVAL_TICKS {
        return;
    }
    LAST_REAP_TICK.store(now, Ordering::Relaxed);
    let pid = CLIENT_PID.load(Ordering::Relaxed);
    if pid == 0 || !crate::task::compat::compat_exists(pid) {
        CLIENT_ACTIVE.store(false, Ordering::Relaxed);
        CLIENT_PID.store(0, Ordering::Relaxed);
    }
}

/// Отдать VT-байты foreground-программе сессии, применив `ISIG`.
///
/// Пока foreground — Linux-программа (`lxrun`), её stdin читается из очереди
/// сессии в io_sys; байты идут туда дословно (UTF-8/escape сохраняются). Но
/// управляющий терминал обязан превращать `^C`/`^Z`/`^\\` в сигналы, пока
/// программа не сняла `ISIG` (raw-режим): иначе вызов `read(2)` в cooked-режиме
/// получил бы байт `0x03` вместо `SIGINT`, и `Ctrl+C` в окне не прерывал бы
/// задание. Сигнальные байты не доставляются (Linux ISIG их не отдаёт).
///
/// `true` — байты обработаны как ввод программы; `false` — foreground'а нет, и
/// вызывающий переводит их в скан-коды shell'а.
fn deliver_to_job(session: usize, fg: u64, bytes: &[u8]) -> bool {
    use crate::arch::x86_64::linux::signal_frame::{SIGINT, SIGQUIT, SIGTSTP};
    if fg == 0 || !crate::task::compat::compat_exists(fg) {
        return false;
    }
    // Raw-режим (nvim/less) сам разбирает управляющие байты: ISIG снят вместе
    // с ICANON, поэтому байт доставляется как есть в stdin-очередь.
    if crate::task::compat::compat_is_raw(fg) {
        crate::arch::x86_64::linux::io_sys::push_stdin_bytes_for(session, bytes);
        return true;
    }
    // Cooked-режим: сигнальные байты — в сигналы (ISIG), остальное — в
    // scancode-кольцо ЭТОЙ сессии, чтобы `read_stdin_line` применил ту же
    // cooked-дисциплину (эхо, Backspace, Enter), что и к физической клавиатуре.
    // Отдавать сырые байты в очередь здесь нельзя: они вернулись бы из `read`
    // до Enter, и построчное чтение перестало бы быть построчным.
    let mut kept = [0u8; 64];
    let mut kept_len = 0usize;
    let mut flush = |bytes: &[u8], session: usize| {
        let mut parser = parser_for(session).lock();
        if session == crate::drivers::session_route::BOOT_SESSION {
            parser.feed(bytes, &mut |scancode| {
                crate::drivers::ps2_kbd::inject_scancode_from_vt(scancode);
            });
        } else {
            parser.feed(bytes, &mut |scancode| {
                crate::drivers::ps2_kbd::push_session_scancode(session, scancode);
            });
        }
    };
    for &byte in bytes {
        match byte {
            0x03 | 0x1A | 0x1C => {
                let sig = if byte == 0x03 {
                    SIGINT
                } else if byte == 0x1A {
                    SIGTSTP
                } else {
                    SIGQUIT
                };
                // Защёлка — чтобы foreground_wait shell'а увидел нажатие и
                // вернул промпт/напечатал `^C`/`^Z` даже когда сигнал доставили
                // мы, а не декодер скан-кодов.
                if sig == SIGINT {
                    crate::drivers::ps2_kbd::set_ctrl_c(session, true);
                } else if sig == SIGTSTP {
                    crate::drivers::ps2_kbd::set_ctrl_z(session, true);
                }
                if kept_len > 0 {
                    flush(&kept[..kept_len], session);
                    kept_len = 0;
                }
                let pgid = crate::drivers::tty::foreground_pgid_for(session);
                let _ = crate::arch::x86_64::linux::signal::send_signal_to_pgid_all(pgid, sig);
            }
            _ => {
                kept[kept_len] = byte;
                kept_len += 1;
                if kept_len == kept.len() {
                    flush(&kept[..kept_len], session);
                    kept_len = 0;
                }
            }
        }
    }
    if kept_len > 0 {
        flush(&kept[..kept_len], session);
    }
    true
}

/// Перевести VT-байты в скан-коды и разбудить shell. Возвращает `len` — запись
/// всегда принимается целиком (как `/dev/uinput`-подобный sink).
pub fn write(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let session = crate::drivers::session::current();
    // Запись от userspace-процесса означает, что клавиатурой владеет VT-клиент:
    // физические байты теперь доставляются в shell только через этот путь.
    // kernel-контекст (selftest) владельцем не становится.
    if crate::task::compat::current_has_compat() {
        note_client(crate::task::scheduler::current_pid());
    }
    // Пока foreground — Linux-программа (`lxrun`), её stdin читается из
    // очереди сессии; иначе ввод адресован shell-редактору — переводим в set-1.
    let session = crate::drivers::session::current();
    let fg = crate::drivers::ps2_kbd::fg_pid(session);
    if deliver_to_job(session, fg, bytes) {
        return bytes.len();
    }
    {
        let mut parser = parser_for(session).lock();
        parser.feed(bytes, &mut |scancode| {
            crate::drivers::ps2_kbd::inject_scancode_from_vt(scancode);
        });
    }
    // `inject_scancode_from_vt` выставляет `has_scancode()`; idle-shell просыпается
    // на ближайшем тике и обрабатывает ввод обычным путём.
    bytes.len()
}

/// Сбросить недособранную ESC-последовательность и защёлку владельца
/// (selftest/переоткрытие).
pub fn reset() {
    for parser in PARSERS.iter() {
        parser.lock().reset();
    }
    CLIENT_ACTIVE.store(false, Ordering::Relaxed);
    CLIENT_PID.store(0, Ordering::Relaxed);
}

/// `/dev/vtinput1`: VT-байты **терминальной сессии** — в её кольцо ввода.
///
/// Отличия от boot-пути [`write`]:
///
/// 1. **Защёлкивает владельца физической клавиатуры** (`client_active`), как и
///    boot-путь: пока kernel-session окно живо, физические скан-коды не
///    дублируются в boot-кольцо, а `Ctrl+C`/`Ctrl+Z` из IRQ-трекинга не
///    убивают/не паркуют boot-задание. Без защёлки одно нажатие доходило до
///    двух разных терминалов (live-QEMU: `Ctrl+C` в окне убивал композитора, а
///    `Ctrl+Z` парковал его), то есть «per-session» на бумаге было общим на
///    деле. `reap_dead_client` возвращает клавиатуру boot-shell'у после выхода
///    окна.
/// 2. **Пишет в кольцо сессии** (`ps2_kbd::push_session_scancode`), а не в
///    boot-кольцо: иначе ввод окна попадал бы в boot-shell.
///
/// 3. **Ввод адресуется foreground-программе ЭТОЙ сессии** (`ps2_kbd::fg_pid` —
///    массив по сессиям): raw-режим получает байты дословно в stdin-очередь
///    сессии, cooked — `ISIG` (сигналы её foreground-группе) плюс scancode-кольцо
///    сессии, чтобы `read_stdin_line` применял ту же cooked-дисциплину. Ввод окна
///    не уходит в stdin программы другого окна или boot-shell'а.
pub fn write_session(session: usize, bytes: &[u8]) -> usize {
    if bytes.is_empty() || session == crate::drivers::session_route::BOOT_SESSION {
        return write(bytes);
    }
    // Запись из окна забирает физическую клавиатуру у boot-консоли. Это
    // ОБЯЗАТЕЛЬНО для kernel-session окна: QEMU/железо доставляет нажатие и в
    // evdev (композитору), и в IRQ1 (shell-кольцо boot), поэтому без защёлки
    // `Ctrl+C`/`Ctrl+Z`, набранный в окне, ДОПОЛНИТЕЛЬНО убивал/парковал
    // boot-задание (терминального клиента) из IRQ-трекинга, и окно исчезало.
    // Теперь физические скан-коды boot-shell'а не пополняются, пока окно живо
    // (`reap_dead_client` возвращает клавиатуру после его выхода), а сигналы
    // обрабатывает сессия окна (`ISIG` ниже).
    if crate::task::compat::current_has_compat() {
        note_client(crate::task::scheduler::current_pid());
    }
    // Foreground — программа ИМЕННО этой сессии: ввод окна не должен уходить
    // в stdin программы другого окна/ boot-shell'а, а `ISIG` (`^C`/`^Z`) обязан
    // слать сигнал foreground-группе этой сессии.
    let fg = crate::drivers::ps2_kbd::fg_pid(session);
    if deliver_to_job(session, fg, bytes) {
        return bytes.len();
    }
    {
        let mut parser = parser_for(session).lock();
        parser.feed(bytes, &mut |scancode| {
            crate::drivers::ps2_kbd::push_session_scancode(session, scancode);
        });
    }
    bytes.len()
}
