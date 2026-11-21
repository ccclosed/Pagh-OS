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

/// Запомнить писавшего как владельца клавиатуры. Вызывается только из
/// [`write`] и только когда текущая задача — compat-процесс.
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

/// Перевести VT-байты в скан-коды и разбудить shell. Возвращает `len` — запись
/// всегда принимается целиком (как `/dev/uinput`-подобный sink).
pub fn write(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    // Запись от userspace-процесса означает, что клавиатурой владеет VT-клиент:
    // физические байты теперь доставляются в shell только через этот путь.
    // kernel-контекст (selftest) владельцем не становится.
    if crate::task::compat::current_has_compat() {
        note_client(crate::task::scheduler::current_pid());
    }
    // Пока foreground — Linux-программа (`lxrun`), её stdin читается из
    // `STDIN_PENDING` в io_sys; отдаём байты туда дословно (UTF-8/escape
    // сохраняются). Иначе ввод адресован shell-редактору — переводим в set-1.
    let fg = crate::drivers::ps2_kbd::FG_PID.load(core::sync::atomic::Ordering::Relaxed);
    if fg != 0 && crate::task::compat::compat_exists(fg) {
        crate::arch::x86_64::linux::io_sys::push_stdin_bytes(bytes);
        return bytes.len();
    }
    {
        let mut parser = parser_for(crate::drivers::session::current()).lock();
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
/// 1. **Защёлкивает владельца клавиатуры, как и boot-путь.** Писавший
///    compat-процесс (терминальное окно) становится владельцем: `deliver_physical`
///    перестаёт класть IRQ1/USB-байт в boot-кольцо, и физическая клавиша
///    доставляется в окно ровно один раз — через `/dev/vtinput1`. Без этой
///    защёлки набранное в окне исполнялось ДВАЖДЫ: в окне (через композитор) и в
///    boot-shell (прямой путь IRQ1), то есть команда в window-сессии дублировалась
///    в фоновую boot-консоль. Это не «окно съело клавиатуру»: desktop с
///    композитором — это и есть режим, в котором физический ввод адресован окну;
///    владелец снимается сам, когда процесс-клиент завершается
///    ([`reap_dead_client`] из boot-shell'а), и прямой путь возвращается.
/// 2. **Пишет в кольцо сессии** (`ps2_kbd::push_session_scancode`), а не в
///    boot-кольцо: иначе ввод окна попадал бы в boot-shell.
///
/// Как и boot-путь, отдаёт байты дословно в `STDIN_PENDING`, пока в системе есть
/// foreground-программа (`lxrun` в окне): её stdin — это байты терминала, а не
/// скан-коды shell-редактора. Ограничение (одна глобальная foreground-группа)
/// названо в `docs/terminal-session.md`.
pub fn write_session(session: usize, bytes: &[u8]) -> usize {
    if bytes.is_empty() || session == crate::drivers::session_route::BOOT_SESSION {
        return write(bytes);
    }
    // Та же защёлка владельца, что у boot-пути: окно водит клавиатуру, пока в
    // него пишет живой клиент. kernel-контекст (selftest) владельцем не
    // становится — иначе тест отобрал бы ввод у настоящей консоли.
    if crate::task::compat::current_has_compat() {
        note_client(crate::task::scheduler::current_pid());
    }
    let fg = crate::drivers::ps2_kbd::FG_PID.load(Ordering::Relaxed);
    if fg != 0 && crate::task::compat::compat_exists(fg) {
        crate::arch::x86_64::linux::io_sys::push_stdin_bytes(bytes);
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
