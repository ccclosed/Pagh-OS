//! Геометрия выделения: когда протяжка становится построчным Area.
//!
//! Выделение мышью в kernel-сессии — это диапазон клеток. Пока перетаскивание
//! живёт внутри нескольких строк, оно символьное (Char): первая и последняя
//! строки обрезаны по колонкам якоря и указателя, как в обычном терминале.
//! Но протяжка, которая начинается в верхней видимой строке и доходит до
//! нижней, означает «выделить весь экран/вывод», а не «первую строку с
//! колонки 7 до колонки 129»: держать в ней обрезанные края — значит копировать
//! обрезанный текст при визуально выделенной целой области. Поэтому такая
//! протяжка повышается до Area — все строки целиком, от колонки 0 до последней.
//!
//! Модуль `core`-only: ядро компилирует его, host-свойство P69 исполняет тот же
//! исходник. Решение чистое (только индексы), поэтому его место в host-тестах,
//! а не в in-QEMU рутине — та проверяет уже связку с настоящей сеткой.

/// Акцент выделения в цвете 0x00RRGGBB.
///
/// Тот же, что `theme.selection` клиента `pagh-terminal` (0x3DAEE9): окно
/// блитит поверхность ядра пиксель-в-пиксель, и два разных акцента читались бы
/// как «выделение в кадре ядра и в клиенте — разные вещи».
pub const ACCENT: u32 = 0x003D_AEE9;

/// Начинается ли протяжка в верхней строке и доходит ли до последней.
///
/// `a_row`/`b_row` — уже упорядоченный диапазон (в порядке чтения), `rows` —
/// высота видимой сетки. Пустая сетка не может быть Area: `rows == 0`
/// защищает вычитание.
pub fn covers_visible_rows(a_row: usize, b_row: usize, rows: usize) -> bool {
    rows > 0 && a_row == 0 && b_row >= rows - 1
}

/// Колонки, которые протяжка покрывает на каждой строке, по Area-политике.
///
/// Возвращает `(0, cols - 1)` для протяжки сверху донизу и исходные колонки
/// для любой другой. Ветка `cols == 0` не повышает диапазон: у пустой сетки нет
/// «полной ширины», и подставлять туда `cols - 1` значило бы underflow.
pub fn promoted_columns(
    a_col: usize,
    b_col: usize,
    a_row: usize,
    b_row: usize,
    rows: usize,
    cols: usize,
) -> (usize, usize) {
    if cols == 0 {
        return (a_col, b_col);
    }
    if covers_visible_rows(a_row, b_row, rows) {
        (0, cols - 1)
    } else {
        (a_col, b_col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_to_bottom_is_an_area() {
        // A drag from the very top-left cell to the bottom-right cell is the
        // whole visible output, even though its own endpoints are already the
        // corners.
        assert_eq!(promoted_columns(0, 79, 0, 24, 25, 80), (0, 79));
        // The bug this exists for: a drag from the top row to the bottom row but
        // with clipped endpoints must still cover every row's full width.
        assert_eq!(promoted_columns(7, 41, 0, 24, 25, 80), (0, 79));
        // A drag that stays inside a few rows keeps char granularity.
        assert_eq!(promoted_columns(7, 41, 3, 5, 25, 80), (7, 41));
        // Starting below the top row is not "the whole visible area".
        assert_eq!(promoted_columns(7, 41, 1, 24, 25, 80), (7, 41));
        // Ending above the bottom row is not either.
        assert_eq!(promoted_columns(7, 41, 0, 23, 25, 80), (7, 41));
        // An empty grid never promotes (and never underflows).
        assert_eq!(promoted_columns(7, 41, 0, 0, 0, 0), (7, 41));
        assert_eq!(promoted_columns(7, 41, 0, 0, 0, 80), (7, 41));
        assert!(!covers_visible_rows(0, 0, 0));
        assert!(covers_visible_rows(0, 0, 1));
    }
}
