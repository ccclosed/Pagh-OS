//! Мини-раскладка: flex-ряды/колонки, сетка, деление прямоугольников.
//!
//! Полноценный layout-движок тулкиту не нужен: экраны Pagh — это жёсткая сетка
//! (заголовок / панель / контент / статус) и flex-раскладка внутри полос. Всё
//! считается в целых пикселях, без накопления ошибок округления.

use crate::geom::{Rect, Size};

/// Направление flex-раскладки.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// Элемент flex-раскладки: фиксированный размер или «забрать остаток».
#[derive(Clone, Copy, Debug)]
pub struct FlexItem {
    /// Фиксированный размер вдоль оси (для `grow > 0` — минимальный).
    pub basis: i32,
    /// Вес растяжения; 0 — размер фиксирован.
    pub grow: u8,
}

impl FlexItem {
    /// Элемент фиксированного размера.
    pub const fn fixed(size: i32) -> FlexItem {
        FlexItem {
            basis: size,
            grow: 0,
        }
    }

    /// Элемент, занимающий остаток места.
    pub const fn grow() -> FlexItem {
        FlexItem { basis: 0, grow: 1 }
    }

    /// Элемент с минимальным размером, растягивающийся при наличии места.
    pub const fn flexible(min: i32) -> FlexItem {
        FlexItem {
            basis: min,
            grow: 1,
        }
    }
}

fn axis_len(rect: Rect, axis: Axis) -> i32 {
    match axis {
        Axis::Horizontal => rect.w,
        Axis::Vertical => rect.h,
    }
}

fn axis_rect(rect: Rect, axis: Axis, offset: i32, len: i32) -> Rect {
    match axis {
        Axis::Horizontal => Rect::new(rect.x + offset, rect.y, len, rect.h),
        Axis::Vertical => Rect::new(rect.x, rect.y + offset, rect.w, len),
    }
}

/// Разложить элементы вдоль оси с промежутками `gap`.
pub fn flex(rect: Rect, axis: Axis, gap: i32, items: &[FlexItem]) -> Vec<Rect> {
    if items.is_empty() {
        return Vec::new();
    }
    let gap = gap.max(0);
    let total_gap = gap * (items.len() as i32 - 1);
    let fixed_sum: i32 = items
        .iter()
        .filter(|item| item.grow == 0)
        .map(|item| item.basis.max(0))
        .sum();
    let min_sum: i32 = items
        .iter()
        .filter(|item| item.grow > 0)
        .map(|item| item.basis.max(0))
        .sum();
    let grow_sum: i32 = items.iter().map(|item| item.grow as i32).sum();
    let available = (axis_len(rect, axis) - total_gap - fixed_sum - min_sum).max(0);

    let mut sizes = Vec::with_capacity(items.len());
    let mut used = 0;
    for item in items {
        if item.grow == 0 {
            sizes.push(item.basis.max(0));
        } else {
            let share = available * item.grow as i32 / grow_sum.max(1);
            let size = item.basis.max(0) + share;
            used += share;
            sizes.push(size);
        }
    }
    // Остаток от деления отдаём последнему растягиваемому элементу.
    if let Some(index) = items.iter().rposition(|item| item.grow > 0) {
        sizes[index] += available - used;
    }

    let mut result = Vec::with_capacity(items.len());
    let mut offset = 0;
    for size in sizes {
        result.push(axis_rect(rect, axis, offset, size));
        offset += size + gap;
    }
    result
}

/// Разложить элементы фиксированных размеров вдоль оси.
pub fn fixed(rect: Rect, axis: Axis, gap: i32, sizes: &[i32]) -> Vec<Rect> {
    let items: Vec<FlexItem> = sizes.iter().map(|size| FlexItem::fixed(*size)).collect();
    flex(rect, axis, gap, &items)
}

/// Сетка ячеек фиксированного размера, заполняемая слева направо.
pub fn grid(rect: Rect, cols: i32, cell: Size, gap: Size, count: i32) -> Vec<Rect> {
    let cols = cols.max(1);
    let mut result = Vec::with_capacity(count.max(0) as usize);
    for index in 0..count.max(0) {
        let col = index % cols;
        let row = index / cols;
        result.push(Rect::new(
            rect.x + col * (cell.w + gap.w),
            rect.y + row * (cell.h + gap.h),
            cell.w,
            cell.h,
        ));
    }
    result
}

/// Сколько колонок шириной не меньше `min_cell` помещается в `width`.
pub fn columns_for(width: i32, min_cell: i32, gap: i32) -> i32 {
    if min_cell <= 0 {
        return 1;
    }
    let gap = gap.max(0);
    let mut cols = (width + gap) / (min_cell + gap);
    if cols < 1 {
        cols = 1;
    }
    cols
}

/// Сколько строк нужно для `count` ячеек при `cols` колонках.
pub fn rows_for(count: i32, cols: i32) -> i32 {
    let cols = cols.max(1);
    (count.max(0) + cols - 1) / cols
}

/// Центрировать прямоугольник заданного размера внутри `outer`.
pub fn center_in(outer: Rect, size: Size) -> Rect {
    Rect::new(
        outer.x + (outer.w - size.w) / 2,
        outer.y + (outer.h - size.h) / 2,
        size.w,
        size.h,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flex_fixed_row() {
        let rect = Rect::new(0, 0, 100, 20);
        let parts = flex(
            rect,
            Axis::Horizontal,
            10,
            &[FlexItem::fixed(30), FlexItem::fixed(40)],
        );
        assert_eq!(parts[0], Rect::new(0, 0, 30, 20));
        assert_eq!(parts[1], Rect::new(40, 0, 40, 20));
    }

    #[test]
    fn flex_grow_takes_remainder() {
        let rect = Rect::new(0, 0, 100, 10);
        let parts = flex(
            rect,
            Axis::Horizontal,
            0,
            &[FlexItem::fixed(20), FlexItem::grow(), FlexItem::fixed(30)],
        );
        assert_eq!(parts[1].w, 50);
        assert_eq!(parts[2].x, 70);
        // Вся ширина распределена.
        assert_eq!(parts[2].right(), 100);
    }

    #[test]
    fn flex_grow_remainder_goes_to_last_item() {
        let rect = Rect::new(0, 0, 10, 10);
        let parts = flex(
            rect,
            Axis::Horizontal,
            0,
            &[FlexItem::grow(), FlexItem::grow(), FlexItem::grow()],
        );
        assert_eq!(parts.iter().map(|r| r.w).sum::<i32>(), 10);
    }

    #[test]
    fn flex_respects_minimum_basis() {
        let rect = Rect::new(0, 0, 10, 100);
        let parts = flex(
            rect,
            Axis::Vertical,
            0,
            &[FlexItem::fixed(10), FlexItem::flexible(20)],
        );
        assert_eq!(parts[0].h, 10);
        assert_eq!(parts[1].h, 90);
    }

    #[test]
    fn grid_positions() {
        let rect = Rect::new(5, 5, 400, 300);
        let cells = grid(rect, 3, Size::new(100, 60), Size::new(10, 10), 5);
        assert_eq!(cells.len(), 5);
        assert_eq!(cells[0], Rect::new(5, 5, 100, 60));
        assert_eq!(cells[1], Rect::new(115, 5, 100, 60));
        assert_eq!(cells[3], Rect::new(5, 75, 100, 60));
        assert_eq!(cells[4], Rect::new(115, 75, 100, 60));
    }

    #[test]
    fn columns_and_rows() {
        assert_eq!(columns_for(832, 120, 8), 6);
        assert_eq!(columns_for(100, 120, 8), 1);
        assert_eq!(rows_for(24, 6), 4);
        assert_eq!(rows_for(25, 6), 5);
    }

    #[test]
    fn center_in_rect() {
        let outer = Rect::new(0, 0, 100, 50);
        assert_eq!(
            center_in(outer, Size::new(20, 10)),
            Rect::new(40, 20, 20, 10)
        );
    }
}
