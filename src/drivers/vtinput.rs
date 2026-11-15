//! `/dev/vtinput`: VT-байты терминала → вход kernel-shell.
//!
//! Kernel-shell читает ввод только из PS/2-кольца (`ps2_kbd`), поэтому байты,
//! присланные userspace-терминалом, переводятся в set-1 скан-коды чистым
//! [`super::vt_keys::KeyParser`] и кладутся в то же кольцо. Так ввод доходит до
//! того же decoder'а/редактора/хоткеев, что и физическая клавиатура, без
//! второго input-path.
//!
//! Синтетические скан-коды намеренно **не** дублируются в `/dev/input/event0`:
//! desktop читает event0 для своего окна, а `vtinput` пишет уже его собственный
//! терминал — иначе получился бы цикл «терминал → vtinput → event0 → терминал».
//!
//! Кольцо ввода ограничено существующим PS/2-кольцом (128 байт), а `write(2)`
//! не блокируется на медленном shell: лишние скан-коды просто вытесняются
//! кольцом, как при быстром наборе.

use super::vt_keys::KeyParser;
use crate::sync::spinlock::Spinlock;

static PARSER: Spinlock<KeyParser> = Spinlock::new(KeyParser::new());

/// Перевести VT-байты в скан-коды и разбудить shell. Возвращает `len` — запись
/// всегда принимается целиком (как `/dev/uinput`-подобный sink).
pub fn write(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
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
        let mut parser = PARSER.lock();
        parser.feed(bytes, &mut |scancode| {
            crate::drivers::ps2_kbd::inject_scancode_from_vt(scancode);
        });
    }
    // `inject_scancode_from_vt` выставляет `has_scancode()`; idle-shell просыпается
    // на ближайшем тике и обрабатывает ввод обычным путём.
    bytes.len()
}

/// Сбросить недособранную ESC-последовательность (selftest/переоткрытие).
pub fn reset() {
    PARSER.lock().reset();
}
