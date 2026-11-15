//! Чистые преобразования времени для слоя совместимости с Linux.
//!
//! Только `core`, без аллокаций, общая с крейтом `host-tests` (R11.6). Здесь
//! декодирование байт/полей и календарная математика под syscall'ами реальных
//! часов (`gettimeofday`/`time`/`CLOCK_REALTIME`) и читателем CMOS RTC: реальный
//! port I/O остаётся эффектным в `rtc.rs`, а каждый арифметический шаг здесь
//! host-тестируем на известных датах:
//!
//!   * [`bcd_to_bin`] — байт BCD CMOS → двоичное (RTC по умолчанию отдаёт BCD).
//!   * [`days_from_civil`] — пролептический григорианский счёт дней Ховарда
//!     Хиннанта (дней с 1970-01-01), верен для любой гражданской даты.
//!   * [`civil_to_unix`] — полная раскладка `(Y,M,D,h,m,s)` → Unix-секунды.
//!   * [`encode_timeval`] — собрать `struct timeval` x86_64.
#![allow(dead_code)]

/// Секунд в сутках.
const SECS_PER_DAY: i64 = 86_400;

/// Linux `struct timeval` в раскладке ABI x86_64: два знаковых 64-битных поля.
/// `gettimeofday` пишет одну такую структуру в буфер пользователя.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeval {
    /// Целые секунды с Unix-эпохи.
    pub tv_sec: i64,
    /// Дробный остаток в микросекундах, обычно в `[0, 1_000_000)`.
    pub tv_usec: i64,
}

/// Декодировать байт CMOS BCD (например, `0x59`) в двоичное значение (`59`).
///
/// RTC хранит каждое двузначное поле как упакованный двоично-десятичный код:
/// старший ниббл — десятки, младший — единицы.
/// `((v >> 4) * 10) + (v & 0x0F)`.
#[inline]
pub fn bcd_to_bin(v: u8) -> u8 {
    ((v >> 4) * 10) + (v & 0x0F)
}

/// Дней с Unix-эпохи (1970-01-01) для пролептической григорианской даты
/// `(y, m, d)`, где `m ∈ [1, 12]`, `d ∈ [1, 31]` (алгоритм Ховарда Хиннанта).
///
/// Возвращает знаковое число, поэтому даты до 1970 дают отрицательные. Алгоритм
/// сдвигает год так, что первым месяцем становится март (делая високосный день
/// последним днём года), затем складывает день эры и эру, чтобы точно попасть в
/// число дней от эпохи.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64; // [0, 399]
    let m = m as i64;
    let d = d as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Преобразовать полную гражданскую раскладку `(год, месяц, день, час, минута,
/// секунда)` в Unix-секунды (секунды с 1970-01-01T00:00:00Z) в предположении UTC.
///
/// Чистая календарная математика на [`days_from_civil`]; тотальна для любого
/// входа.
pub fn civil_to_unix(year: i64, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> i64 {
    let days = days_from_civil(year, month, day);
    days * SECS_PER_DAY + (hour as i64) * 3600 + (minute as i64) * 60 + (second as i64)
}

/// Построить [`Timeval`] из целых `secs` и дробного остатка `usecs`.
#[inline]
pub fn encode_timeval(secs: i64, usecs: i64) -> Timeval {
    Timeval {
        tv_sec: secs,
        tv_usec: usecs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcd_round_trips_small_values() {
        assert_eq!(bcd_to_bin(0x00), 0);
        assert_eq!(bcd_to_bin(0x09), 9);
        assert_eq!(bcd_to_bin(0x10), 10);
        assert_eq!(bcd_to_bin(0x59), 59);
        assert_eq!(bcd_to_bin(0x23), 23);
    }

    #[test]
    fn epoch_is_zero() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_to_unix(1970, 1, 1, 0, 0, 0), 0);
    }

    #[test]
    fn known_dates() {
        // 2000-01-01 — это 30 лет после эпохи: 10957 дней.
        assert_eq!(days_from_civil(2000, 1, 1), 10957);
        assert_eq!(civil_to_unix(2000, 1, 1, 0, 0, 0), 946_684_800);
        // Високосный день 2024-02-29.
        assert_eq!(civil_to_unix(2024, 2, 29, 0, 0, 0), 1_709_164_800);
    }
}
