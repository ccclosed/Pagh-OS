//! Форматирование чисел, размеров и дат по-русски.
//!
//! Строка состояния Files выглядит как в Dolphin: `24 папки, 4 файла · 76,3 МБ`.
//! Русские склонения считаются по стандартному правилу (1 папка, 2 папки,
//! 5 папок, 11 папок), размеры — в двоичных единицах с одним знаком после
//! запятой, даты — в UTC (в Pagh нет часового пояса).

/// Склонение существительного по числу: `(1, "папка", "папки", "папок")`.
pub fn plural_ru(count: u64, one: &str, few: &str, many: &str) -> String {
    let tail100 = count % 100;
    let tail10 = count % 10;
    let form = if (11..=14).contains(&tail100) {
        many
    } else if tail10 == 1 {
        one
    } else if (2..=4).contains(&tail10) {
        few
    } else {
        many
    };
    format!("{count} {form}")
}

/// `24 папки`.
pub fn dirs_label(count: u64) -> String {
    plural_ru(count, "папка", "папки", "папок")
}

/// `4 файла`.
pub fn files_label(count: u64) -> String {
    plural_ru(count, "файл", "файла", "файлов")
}

/// Двоичный размер с одним знаком после запятой: `76,3 МБ`, `512 Б`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
    if bytes < 1024 {
        return format!("{bytes} Б");
    }
    let mut divisor: u128 = 1;
    let mut unit = 0usize;
    while unit + 1 < UNITS.len() && bytes as u128 / divisor >= 1024 {
        divisor *= 1024;
        unit += 1;
    }
    // Десятые доли с округлением половины вверх.
    let tenths = (bytes as u128 * 10 + divisor / 2) / divisor;
    let whole = tenths / 10;
    let frac = tenths % 10;
    format!("{whole},{frac} {}", UNITS[unit])
}

/// Строка состояния: `24 папки, 4 файла · 76,3 МБ`.
pub fn status_text(dirs: u64, files: u64, bytes: u64) -> String {
    let mut text = format!("{}, {}", dirs_label(dirs), files_label(files));
    if bytes > 0 {
        text.push_str(" · ");
        text.push_str(&format_size(bytes));
    }
    text
}

/// Гражданская дата из числа дней с 1970-01-01 (алгоритм Ховарда Хиннанта).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Дата и время в UTC: `15.11.2026 23:38`.
pub fn format_date(unix_seconds: u64) -> String {
    let seconds = unix_seconds as i64;
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{day:02}.{month:02}.{year:04} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Права доступа в виде `drwxr-xr-x`.
pub fn format_mode(mode: u32, is_dir: bool) -> String {
    let mut out = String::with_capacity(10);
    out.push(if is_dir { 'd' } else { '-' });
    for bit in [
        0o400, 0o200, 0o100, 0o040, 0o020, 0o010, 0o004, 0o002, 0o001,
    ] {
        let symbol = match bit {
            0o400 | 0o040 | 0o004 => 'r',
            0o200 | 0o020 | 0o002 => 'w',
            _ => 'x',
        };
        out.push(if mode & bit != 0 { symbol } else { '-' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn russian_plurals() {
        assert_eq!(dirs_label(1), "1 папка");
        assert_eq!(dirs_label(2), "2 папки");
        assert_eq!(dirs_label(4), "4 папки");
        assert_eq!(dirs_label(5), "5 папок");
        assert_eq!(dirs_label(11), "11 папок");
        assert_eq!(dirs_label(21), "21 папка");
        assert_eq!(files_label(24), "24 файла");
        assert_eq!(files_label(0), "0 файлов");
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(format_size(0), "0 Б");
        assert_eq!(format_size(512), "512 Б");
        assert_eq!(format_size(1024), "1,0 КБ");
        assert_eq!(format_size(4096), "4,0 КБ");
        assert_eq!(format_size(1_048_576), "1,0 МБ");
        assert_eq!(format_size(80_000_000), "76,3 МБ");
        assert_eq!(format_size(2_147_483_648), "2,0 ГБ");
    }

    #[test]
    fn status_matches_concept() {
        assert_eq!(
            status_text(24, 4, 80_000_000),
            "24 папки, 4 файла · 76,3 МБ"
        );
        assert_eq!(status_text(0, 0, 0), "0 папок, 0 файлов");
        assert_eq!(status_text(1, 0, 0), "1 папка, 0 файлов");
    }

    #[test]
    fn dates_and_modes() {
        // 2026-11-15 20:38 UTC = 1 794 775 080.
        assert_eq!(format_date(1_794_775_080), "15.11.2026 20:38");
        assert_eq!(format_date(0), "01.01.1970 00:00");
        assert_eq!(format_mode(0o755, true), "drwxr-xr-x");
        assert_eq!(format_mode(0o644, false), "-rw-r--r--");
    }
}
