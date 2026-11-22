//! Смешивание акцента выделения с фоном клетки — чистая арифметика.
//!
//! Выделение текста мышью не имеет права перекрашивать сам глиф: логотип
//! fastfetch — это цветные `#`, а его палитра — ANSI *фоны*, поэтому заливка
//! клетки одним цветом (прежнее `SELECT_FG`/`SELECT_BG`) превращала логотип в
//! тёмные блоки, полосу палитры — в один плоский прямоугольник, а цветные
//! key/value — в серый текст. Вместо этого меняется только фон: в него
//! подмешивается акцент, глиф рисуется своим прежним цветом поверх.
//!
//! Формула — та же, что у клиента `pagh-terminal`
//! (`render::resolve_style`: `bg.lerp(theme.selection, SELECTION_BG_MIX)`,
//! 60%). Модуль `core`-only, поэтому host-свойство P69 исполняет ровно этот
//! источник, а не его копию; расхождение двух рендереров стало бы видно как
//! «в окне выделение одно, в кадре другое».

/// Сколько акцента подмешивается в фон, в 255-х: 153/255 = 0.6.
pub const MIX_NUM: u32 = 153;
/// Знаменатель доли подмешивания (полная шкала канала).
pub const MIX_DEN: u32 = 255;

/// Подмешать `accent` в `base` на [`MIX_NUM`]/[`MIX_DEN`], по каналам RGB.
///
/// Каждый канал результата лежит между своим значением в `base` и в `accent`
/// (включительно) и монотонен по `base`, поэтому две различимые заливки
/// остаются различимыми: выделение — полупрозрачная на вид подсветка, а не
/// плоская замена. `base == accent` — неподвижная точка.
pub fn wash_color(base: u32, accent: u32) -> u32 {
    let ch = |shift: u32| {
        let b = (base >> shift) & 0xFF;
        let a = (accent >> shift) & 0xFF;
        // +MIX_DEN/2 — округление, а не срез: без него канал темнел бы на
        // единицу у половины значений, и восстановление из модели переставало
        // бы совпадать с исходной заливкой.
        (((b * (MIX_DEN - MIX_NUM) + a * MIX_NUM + MIX_DEN / 2) / MIX_DEN) & 0xFF) << shift
    };
    ch(16) | ch(8) | ch(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-computed vector, independent of the host property: black washed
    /// toward the client's selection accent (0x3DAEE9) at 60%.
    #[test]
    fn hand_computed_wash() {
        // 0.6 * 0x3D = 36.6 -> 37 = 0x25; 0.6 * 0xAE = 104.4 -> 104 = 0x68;
        // 0.6 * 0xE9 = 139.8 -> 140 = 0x8C.
        assert_eq!(wash_color(0x000000, 0x3D_AEE9), 0x25_688C);
        // A coloured background shows through the wash: red and blue cells stay
        // distinguishable (and neither turns into the flat accent), which is what
        // keeps the fastfetch palette strip from collapsing into one bar.
        let red = wash_color(0xAA_0000, 0x3D_AEE9);
        let blue = wash_color(0x00_00AA, 0x3D_AEE9);
        assert_eq!(red, 0x69_688C);
        assert_eq!(blue, 0x25_68D0);
        assert_ne!(red, blue);
        assert_ne!(red, 0x3D_AEE9);
        assert_ne!(red, 0x00_0000);
    }
}
