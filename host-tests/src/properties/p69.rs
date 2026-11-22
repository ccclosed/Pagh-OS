// Feature: mouse selection wash + Area policy in the kernel console, property P69.
//
// Два класса ошибок здесь не падают, а портят именно то, что пользователь
// выделяет: (1) выделение заливает клетку одним цветом вместо подмешивания
// акцента в её собственный фон — цветные `#` fastfetch становятся тёмными
// блоками, ANSI-полоса палитры сливается в один прямоугольник; (2) протяжка
// сверху донизу остаётся символьной и копирует обрезанные края визуально
// выделенной целой области. Свойства фиксируют законы, на которые опираются
// `framebuffer::paint_cell_selected` и `shell::selection::extend`.

use proptest::prelude::*;

use crate::cell_wash::{wash_color, MIX_DEN, MIX_NUM};
use crate::selection_math::{covers_visible_rows, promoted_columns, ACCENT};

/// Любой цвет 0x00RRGGBB (альфа в модели консоли не участвует).
fn rgb() -> impl Strategy<Value = u32> {
    0u32..=0x00FF_FFFF
}

fn channel(color: u32, shift: u32) -> u32 {
    (color >> shift) & 0xFF
}

/// Протяжка внутри реальной сетки: строки/колонки упорядочены, как их отдаёт
/// `cell_at` (обе точки уже в пределах сетки).
fn drag() -> impl Strategy<Value = (usize, usize, usize, usize, usize, usize)> {
    (1usize..80, 1usize..200).prop_flat_map(|(rows, cols)| {
        (
            Just(rows),
            Just(cols),
            0usize..rows,
            0usize..rows,
            0usize..cols,
            0usize..cols,
        )
            .prop_map(|(rows, cols, r1, r2, c1, c2)| {
                let (a_row, b_row) = if r1 <= r2 { (r1, r2) } else { (r2, r1) };
                let (a_col, b_col) = if c1 <= c2 { (c1, c2) } else { (c2, c1) };
                (rows, cols, a_row, b_row, a_col, b_col)
            })
    })
}

proptest! {
    /// Wash — это доля акцента в фоне: каждый канал результата лежит между
    /// своим значением в фоне и в акценте. Именно это делает подсветку
    /// «полупрозрачной»: она не может выйти за пределы двух цветов и потому не
    /// превращает произвольную заливку в плоскую.
    #[test]
    fn wash_stays_between_base_and_accent(base in rgb(), accent in rgb()) {
        let washed = wash_color(base, accent);
        // Никакой альфы/мусора в старших битах: модель цвета — 24 бита.
        prop_assert!(washed <= 0x00FF_FFFF);
        for shift in [16u32, 8, 0] {
            let b = channel(base, shift);
            let a = channel(accent, shift);
            let w = channel(washed, shift);
            prop_assert!(
                w >= b.min(a) && w <= b.max(a),
                "channel >>{}: washed {} outside [{}, {}]",
                shift, w, b.min(a), b.max(a)
            );
        }
    }

    /// `base == accent` — неподвижная точка: выделенный фон, уже равный акценту,
    /// не должен «дрожать» на единицу от округления.
    #[test]
    fn wash_identity(base in rgb()) {
        prop_assert_eq!(wash_color(base, base), base);
    }

    /// Округление не ломает монотонность: более светлый фон остаётся более
    /// светлым и после подмешивания. Без этого две соседние клетки палитры
    /// могли бы поменяться местами после выделения.
    #[test]
    fn wash_is_monotone(base1 in rgb(), base2 in rgb(), accent in rgb()) {
        let w1 = wash_color(base1, accent);
        let w2 = wash_color(base2, accent);
        for shift in [16u32, 8, 0] {
            let b1 = channel(base1, shift);
            let b2 = channel(base2, shift);
            if b1 <= b2 {
                prop_assert!(
                    channel(w1, shift) <= channel(w2, shift),
                    "washing broke channel >>{} order: {} > {}",
                    shift, channel(w1, shift), channel(w2, shift)
                );
            } else {
                prop_assert!(channel(w1, shift) >= channel(w2, shift));
            }
        }
    }

    /// Видимо разные фоны остаются разными: если хотя бы один канал двух
    /// заливок отличается на 3 и больше (шаг, который переживает 40%-сжатие),
    /// выделение не может склеить их в один цвет. Это и есть «полоса палитры не
    /// превращается в один плоский бар».
    #[test]
    fn visibly_different_backgrounds_stay_distinct(
        base in rgb(),
        other in rgb(),
        accent in rgb(),
    ) {
        let visibly_different = [16u32, 8, 0]
            .into_iter()
            .any(|shift| channel(base, shift).abs_diff(channel(other, shift)) >= 3);
        if visibly_different {
            prop_assert_ne!(
                wash_color(base, accent),
                wash_color(other, accent),
                "two visibly different backgrounds washed to the same colour"
            );
        }
    }

    /// Протяжка сверху донизу — Area (полная ширина всех строк), и ровно она:
    /// любая другая сохраняет колонки якоря и указателя. Свойство проверяет обе
    /// стороны на произвольной протяжке внутри сетки.
    #[test]
    fn only_top_to_bottom_drag_is_a_full_area(
        (rows, cols, a_row, b_row, a_col, b_col) in drag()
    ) {
        let (from, to) = promoted_columns(a_col, b_col, a_row, b_row, rows, cols);
        let full_height = covers_visible_rows(a_row, b_row, rows);
        let expected_area = a_row == 0 && b_row == rows - 1;
        prop_assert_eq!(full_height, expected_area);
        if full_height {
            prop_assert_eq!((from, to), (0, cols - 1));
        } else {
            prop_assert_eq!((from, to), (a_col, b_col));
        }
        // В любом случае результат — упорядоченный диапазон внутри сетки.
        prop_assert!(from <= to);
        prop_assert!(to < cols);
    }

    /// Повышение идемпотентно: Area, уже растянутая на всю ширину, не меняется.
    /// Иначе повторный `extend` на каждом движении мыши сдвигал бы подсветку.
    #[test]
    fn area_promotion_is_idempotent(
        (rows, cols, a_row, b_row, a_col, b_col) in drag()
    ) {
        let (from, to) = promoted_columns(a_col, b_col, a_row, b_row, rows, cols);
        let again = promoted_columns(from, to, a_row, b_row, rows, cols);
        prop_assert_eq!(again, (from, to));
    }

    /// Пустая сетка ничего не повышает и ничего не underflow-ит: у неё нет
    /// «полной ширины». Это путь serial-only загрузки, где framebuffer не поднят.
    #[test]
    fn empty_grid_never_promotes(a_col in 0usize..200, b_col in 0usize..200) {
        let (from, to) = promoted_columns(a_col, b_col, 0, 0, 0, 0);
        prop_assert_eq!((from, to), (a_col, b_col));
        prop_assert!(!covers_visible_rows(0, 0, 0));
    }

    /// Акцент клиента и ядра — один и тот же цвет: окно блитит поверхность
    /// пиксель-в-пиксель, и расхождение акцентов было бы видно как два разных
    /// выделения в одном окне.
    #[test]
    fn accent_matches_the_client_theme(_unused in 0u8..1) {
        prop_assert_eq!(ACCENT, 0x003D_AEE9);
        // Доля подмешивания совпадает с `SELECTION_BG_MIX = 0.6` клиента.
        prop_assert_eq!(MIX_NUM * 100 / MIX_DEN, 60);
        prop_assert!(MIX_NUM < MIX_DEN);
    }
}