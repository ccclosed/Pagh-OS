//! Планирование файловых I/O syscall'ов и эффектные обработчики.
//!
//! Здесь только **чистая** логика планирования `read` и `lseek`: функции без
//! аллокаций, только `core`, вычисляющие результат без обращения к VFS,
//! per-process `FdTable`, железу или глобальному изменяемому состоянию (R11.6).
//! Эффектные обработчики, связывающие эти планы с реальными дескрипторами,
//! живут в `io_sys`.
//!
//! Тип `Errno` берётся из соседнего модуля `errno` через `super::errno`, чтобы
//! один и тот же исходник компилировался и в ядре
//! (`crate::arch::x86_64::linux::errno`), и будучи включённым в крейт
//! `host-tests` как соседний модуль корня (R11.6).
#![allow(dead_code)]

use super::errno::Errno;

/// `whence` для `lseek`: поставить смещение `delta` от начала файла.
pub const SEEK_SET: u32 = 0;
/// `whence` для `lseek`: поставить смещение `delta` от текущего смещения.
pub const SEEK_CUR: u32 = 1;
/// `whence` для `lseek`: поставить смещение `delta` от конца файла.
pub const SEEK_END: u32 = 2;

/// Первые компоненты пути, которые сохраняют корень VFS своей базой.
///
/// `/proc` — причина, по которой этот предикат существует как именованная,
/// тестируемая единица: `resolve_path` НЕ должен переписывать `/proc/...` в
/// `/mnt/proc/...`, иначе синтетическое дерево станет недостижимо для
/// Linux-программ. Сравнение идёт по **целым компонентам**: путь `/process` —
/// гостевой путь под `/mnt`, а не путь `/proc` (`docs/procfs.md` §1.3).
pub const ROOT_KEEPING_COMPONENTS: [&str; 5] = ["mnt", "dev", "proc", "sys", "tmp"];

/// USB-тома монтируются в корень (`/usb0p1`), а не под `/mnt`: `mount(2)`
/// Phase 1 принимает только `"/" + source`, и Files открывает точку из события
/// как есть. Точное `"usb"` в списке не сработало бы — компонента называется
/// `usb0p1`, поэтому проверяется префикс `usb`; `/usbfoo` тоже сохраняет корень
/// (простое совпадение префикса, как и задокументировано в спеке).
fn is_usb_root(component: &str) -> bool {
    component.starts_with("usb")
}

/// Разрешается ли `abs` (абсолютный нормализованный гостевой путь) от корня
/// VFS, а не переносится под `/mnt`?
pub fn guest_path_keeps_root(abs: &str) -> bool {
    let first = abs.trim_start_matches('/').split('/').next().unwrap_or("");
    ROOT_KEEPING_COMPONENTS.contains(&first) || is_usb_root(first)
}

/// Спланировать `read` `count` байт со смещения `off` файла длины `size`.
///
/// Возвращает `(copied, new_off)`: число байт урезано до остатка до EOF, а
/// смещение дескриптора сдвигается ровно на это число (R2.3, Property 5):
///
/// * `copied = min(count, size.saturating_sub(off))`
/// * `new_off = off + copied`
///
/// `saturating_sub` делает смещение на EOF или за ним дающим `copied == 0` (и
/// значит `new_off == off`) вместо underflow. `off + copied` переполниться не
/// может, потому что при `off <= size` выполнено `copied <= size - off`, иначе
/// `copied == 0`.
pub fn plan_read(size: u64, off: u64, count: u64) -> (u64, u64) {
    let copied = core::cmp::min(count, size.saturating_sub(off));
    let new_off = off + copied;
    (copied, new_off)
}

/// Спланировать `lseek` к новому абсолютному смещению.
///
/// `whence` выбирает базу, к которой применяется знаковый `delta`:
///
/// * [`SEEK_SET`] → база `0`
/// * [`SEEK_CUR`] → база `cur` (текущее смещение дескриптора)
/// * [`SEEK_END`] → база `size` (длина файла)
///
/// Абсолютное смещение считается в `i128`, чтобы не переполниться на границе
/// `u64`, затем проверяется: неотрицательный результат, влезающий в `u64`,
/// возвращается как `Ok`; отрицательный результат, результат за `u64::MAX` или
/// неизвестный `whence` дают `Err(Errno::EINVAL)`, оставляя смещение вызывающего
/// неизменным (R2.7, R2.15, Property 6).
pub fn plan_lseek(whence: u32, cur: u64, size: u64, delta: i64) -> Result<u64, Errno> {
    let base: u64 = match whence {
        SEEK_SET => 0,
        SEEK_CUR => cur,
        SEEK_END => size,
        _ => return Err(Errno::EINVAL),
    };

    let absolute = base as i128 + delta as i128;
    if absolute < 0 || absolute > u64::MAX as i128 {
        return Err(Errno::EINVAL);
    }
    Ok(absolute as u64)
}
