//! Планирование syscall'ов `getrandom` и `clock_gettime`.
//!
//! Здесь только **чистая** логика планирования: функции без аллокаций, только
//! `core`, вычисляющие результат без обращения к железу, источнику RNG, тиковым
//! часам ядра или глобальному изменяемому состоянию (R11.6). Эффектные
//! обработчики, связывающие эти планы с реальным RNG и часами, передают
//! фактическую частоту тиков в [`ticks_to_timespec`].
//!
//! Тип `Errno` берётся из соседнего `errno` через `super::errno`, чтобы один и
//! тот же исходник компилировался и в ядре
//! (`crate::arch::x86_64::linux::errno`), и будучи включённым в крейт
//! `host-tests` как соседний модуль корня (R11.6).
#![allow(dead_code)]

use super::errno::Errno;

/// Наносекунд в секунде.
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Id часов POSIX `clock_gettime` для часов реального времени (стенных).
pub const CLOCK_REALTIME: u32 = 0;
/// Id часов POSIX `clock_gettime` для монотонных часов.
pub const CLOCK_MONOTONIC: u32 = 1;

/// Linux `struct timespec` в раскладке ABI x86_64.
///
/// Оба поля знаковые 64-битные. Нормализованный timespec держит
/// `tv_nsec ∈ [0, 1_000_000_000)` (R2.13).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timespec {
    /// Целые секунды.
    pub tv_sec: i64,
    /// Дробный остаток в наносекундах, всегда в `[0, 1_000_000_000)`.
    pub tv_nsec: i64,
}

/// Спланировать заполнение `n` байт `getrandom` в буфер длины `buflen`.
///
/// Возвращает `Ok(n)` — число байт, которое обработчик обязан заполнить, — когда
/// запрос помещается в буфер (`n <= buflen`), иначе `Err(Errno::EINVAL)`, и тогда
/// обработчик оставляет буфер пользователя неизменным (R2.12, R2.16,
/// Property 9).
pub fn getrandom_plan(buflen: u64, n: u64) -> Result<u64, Errno> {
    if n <= buflen {
        Ok(n)
    } else {
        Err(Errno::EINVAL)
    }
}

/// Преобразовать число тиков ядра в нормализованный [`Timespec`] для
/// `clock_gettime`.
///
/// `clock_id` должен быть одной из поддерживаемых POSIX-часов,
/// [`CLOCK_REALTIME`] или [`CLOCK_MONOTONIC`]; любой другой id даёт
/// `Err(Errno::EINVAL)`, и обработчик оставляет буфер пользователя неизменным
/// (R2.13, R2.16, Property 10).
///
/// `tick_hz` — частота тиков (тиков в секунду), которую передаёт эффектный
/// вызывающий; параметр делает функцию чистой и host-тестируемой. Суммарные
/// наносекунды считаются в 128-битной арифметике, чтобы не переполниться до
/// разделения на секунды и остаток:
///
/// * `total_ns = ticks * 1_000_000_000 / tick_hz`
/// * `tv_sec   = total_ns / 1_000_000_000`
/// * `tv_nsec  = total_ns % 1_000_000_000`  (гарантированно в `[0, 1e9)`)
///
/// `tick_hz == 0` отвергается с `Err(Errno::EINVAL)` вместо деления на ноль,
/// поэтому функция тотальна для любого входа.
pub fn ticks_to_timespec(ticks: u64, clock_id: u32, tick_hz: u64) -> Result<Timespec, Errno> {
    // Принимать любой стандартный id часов Linux (0..=7). CPUTIME/RAW/COARSE/
    // BOOTTIME все обслуживаются одним монотонным источником на тиках; их отказ
    // заставлял OpenSSL/libcurl заливать EINVAL через
    // clock_gettime(CLOCK_MONOTONIC_RAW).
    if clock_id > 7 {
        return Err(Errno::EINVAL);
    }

    if tick_hz == 0 {
        return Err(Errno::EINVAL);
    }

    let total_ns: u128 = (ticks as u128 * NANOS_PER_SEC as u128) / tick_hz as u128;
    let tv_sec = (total_ns / NANOS_PER_SEC as u128) as i64;
    let tv_nsec = (total_ns % NANOS_PER_SEC as u128) as i64;

    Ok(Timespec { tv_sec, tv_nsec })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn getrandom_accepts_when_fits() {
        assert_eq!(getrandom_plan(16, 0), Ok(0));
        assert_eq!(getrandom_plan(16, 16), Ok(16));
        assert_eq!(getrandom_plan(16, 8), Ok(8));
    }

    #[test]
    fn getrandom_rejects_overrun() {
        assert_eq!(getrandom_plan(16, 17), Err(Errno::EINVAL));
        assert_eq!(getrandom_plan(0, 1), Err(Errno::EINVAL));
    }

    #[test]
    fn timespec_rejects_unsupported_clock() {
        // Стандартные id 0..=7 все принимаются (один монотонный источник);
        // отвергаются только нестандартные.
        assert!(ticks_to_timespec(100, 2, 1000).is_ok());
        assert_eq!(ticks_to_timespec(100, 99, 1000), Err(Errno::EINVAL));
    }

    #[test]
    fn timespec_rejects_zero_hz() {
        assert_eq!(
            ticks_to_timespec(100, CLOCK_MONOTONIC, 0),
            Err(Errno::EINVAL)
        );
    }

    #[test]
    fn timespec_normalizes() {
        // 1500 тиков при 1000 Гц = 1.5 с = 1 с + 500_000_000 нс.
        let ts = ticks_to_timespec(1500, CLOCK_REALTIME, 1000).unwrap();
        assert_eq!(
            ts,
            Timespec {
                tv_sec: 1,
                tv_nsec: 500_000_000
            }
        );
        // tv_nsec всегда в диапазоне.
        assert!(ts.tv_nsec >= 0 && ts.tv_nsec < NANOS_PER_SEC as i64);
    }

    #[test]
    fn timespec_total_ns_invariant() {
        let ts = ticks_to_timespec(7, CLOCK_MONOTONIC, 3).unwrap();
        let total = ts.tv_sec as i128 * NANOS_PER_SEC as i128 + ts.tv_nsec as i128;
        assert_eq!(total, (7u128 * NANOS_PER_SEC as u128 / 3) as i128);
    }
}
