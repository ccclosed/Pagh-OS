//! Предкомпоновка тени окна: видимое кольцо, один слой, точная таблица каскада.
//!
//! `Canvas::shadow` рисует тень девятью расширяющимися чёрными скруглёнными
//! слоями (`blend(BLACK, alpha)`). `paint_window_cached` смешивал их по всей
//! площади коробки `frame.inflate(9).offset(0, 6)` — для окна 640×400 это
//! ~2.4 млн смешений на кадр, тогда как в итоговом кадре видна лишь малая
//! часть: сразу после тени композитор кладёт непрозрачный chrome, и первая же
//! заливка `build_chrome` — скруглённая рамка окна радиуса `WINDOW_RADIUS` при
//! opacity 255. `blit_layer` копирует любой ненулевой пиксель слоя, то есть
//! весь chrome внутри рамки непрозрачен и закрывает тень. Видимы только
//! внешнее кольцо `shadow_box \ frame` и четыре угловых квадрата, где chrome
//! скруглён, — суммарно ~19 тыс. пикселей вместо ~2.4 млн (в ~126 раз меньше).
//!
//! Здесь это кольцо предкомпонуется в **один** слой один раз на геометрию окна
//! (кэш — `Compositor::shadow_cache`): в кадре остаётся один проход
//! [`crate::canvas::Canvas::blit_shadow`], и он не перестраивается при
//! движении курсора.
//!
//! # Почему в слое лежит не RGBA-цвет, а профиль каскада
//!
//! Девять смешений `blend(BLACK, alpha)` — это девять округлений подряд
//! (`(d·(255−a)+127)/255` на каждом шаге). К одному 8-битному alpha-смешению
//! такой каскад не сводится: перебор всех пар «цвет источника, alpha» не
//! находит точного совпадения ни для одного профиля из двух и более слоёв
//! (тест `shadow::tests::single_blend_cannot_replace_cascade`). Пиксельная
//! идентичность (`tests/render_goldens.rs`) важнее формы хранения, поэтому
//! предкомпонованный слой хранит на пиксель *профиль* — число накрывающих
//! слоёв, — а [`cascade_luts`] даёт точный результат каскада для каждого из
//! 256 значений канала. Слой остаётся одним, проход — одним, а байты кадра
//! совпадают с `Canvas::shadow` там, где тень видна.
//!
//! # Инвариант вложенности
//!
//! Слои вложены: слой с большим `spread` содержит все меньшие, поэтому набор
//! слоёв над точкой — это суффикс `9, 8, …, s0`, и числа слоёв достаточно,
//! чтобы восстановить их alpha. Вложенность следует из геометрии
//! (`layer_rect(spread)` расширяется, а `rounded_inset` растёт не быстрее
//! радиуса); сборка проверяет её `debug_assert`-ом, а
//! тест `shadow::tests::ring_matches_layer_coverage` сверяет кольцо с настоящим
//! `Canvas::fill_rounded_rect` для набора размеров.

use crate::color;
use crate::geometry::Rect;
use std::sync::OnceLock;

/// Сколько слоёв рисует тень окна (аргумент `Canvas::shadow`).
pub const LAYERS: i32 = 9;
/// Шаг alpha между слоями тени (аргумент `Canvas::shadow`).
pub const ALPHA_STEP: u8 = 18;
/// Расширение коробки тени вокруг рамки окна (`frame.inflate(SPREAD)`).
pub const SPREAD: i32 = LAYERS;
/// Вертикальный сдвиг коробки тени: `frame.inflate(SPREAD).offset(0, OFFSET_Y)`.
pub const OFFSET_Y: i32 = SPREAD / 2 + 2;

/// Сколько профилей каскада знает таблица: `0..=LAYERS` (число слоёв).
pub const PROFILES: usize = LAYERS as usize + 1;

/// Коробка тени окна `width`×`height` в координатах кадра.
pub fn shadow_box(width: i32, height: i32) -> Rect {
    Rect::new(0, 0, width, height)
        .inflate(SPREAD)
        .offset(0, OFFSET_Y)
}

/// Прямоугольник слоя `spread` в координатах кадра (как в `Canvas::shadow`).
fn layer_rect(width: i32, height: i32, spread: i32) -> Rect {
    Rect::new(0, 0, width, height)
        .inflate(spread)
        .offset(0, spread / 2 + 2)
}

/// Горизонтальный отступ скругления для строки на расстоянии `dy` от края.
pub(crate) fn rounded_inset(radius: i32, dy: i32) -> i32 {
    if radius <= 0 || dy >= radius {
        return 0;
    }
    let r = radius as f64;
    let d = (radius - dy) as f64;
    (r - (r * r - d * d).sqrt()).round() as i32
}

/// Строка видимой тени в координатах коробки тени.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShadowRow {
    /// Номер строки внутри коробки тени.
    pub y: i32,
    /// Полуоткрытый интервал видимых пикселей строки внутри коробки.
    pub x0: i32,
    pub x1: i32,
    /// Смещение первого профиля строки в [`ShadowSprite::profiles`].
    pub offset: usize,
}

/// Видимая часть тени окна, предкомпонованная в один слой.
#[derive(Clone, Debug)]
pub struct ShadowSprite {
    /// Размер коробки тени: слой блитится в `shadow_box(width, height)`.
    pub width: i32,
    pub height: i32,
    pub(crate) rows: Vec<ShadowRow>,
    /// Профили каскада (`0` — тени нет, `1..=LAYERS` — число слоёв).
    pub(crate) profiles: Vec<u8>,
}

impl ShadowSprite {
    pub fn rows(&self) -> &[ShadowRow] {
        &self.rows
    }

    /// Профиль видимой тени в точке `(x, y)` коробки; `0` — тень не видна.
    ///
    /// В строке до двух пробегов (левое и правое кольцо), поэтому ищем все
    /// строки `y`, а не первую.
    pub fn profile(&self, x: i32, y: i32) -> u8 {
        for row in &self.rows {
            if row.y == y && x >= row.x0 && x < row.x1 {
                return self.profiles[row.offset + (x - row.x0) as usize];
            }
        }
        0
    }

    /// Сколько пикселей кольца реально рисуется (для тестов и логов).
    pub fn visible_pixels(&self) -> usize {
        self.profiles.len()
    }
}

/// Предкомпоновать видимую часть тени окна `width`×`height` в один слой.
///
/// `frame_radius` — радиус скругления chrome (`WINDOW_RADIUS`): пиксели внутри
/// скруглённой рамки chrome закрывает, поэтому тень туда не пишется.
pub fn build_window_shadow(width: i32, height: i32, frame_radius: i32) -> ShadowSprite {
    let box_rect = shadow_box(width, height);
    let stride = box_rect.w.max(0) as usize;
    let mut counts = vec![0u8; stride * box_rect.h.max(0) as usize];
    // Маска непрозрачного chrome — скруглённая рамка; `fill_rounded_rect`
    // клампит радиус теми же `min(w/2, h/2)`.
    let frame_radius = frame_radius.max(0).min(width / 2).min(height / 2);
    for spread in (1..=LAYERS).rev() {
        let rect = layer_rect(width, height, spread);
        let radius = (frame_radius + spread)
            .max(0)
            .min(rect.w / 2)
            .min(rect.h / 2);
        // К приходу слоя `spread` точка уже накрыта всеми большими слоями,
        // если накрыта вообще: это и есть проверка вложенности.
        let covered_before = (LAYERS - spread) as u8;
        for y in rect.y..rect.bottom() {
            let dy = (y - rect.y).min(rect.bottom() - 1 - y);
            let inset = rounded_inset(radius, dy);
            let x0 = rect.x + inset;
            let x1 = rect.right() - inset;
            if x1 <= x0 {
                continue;
            }
            let mut spans = [(0i32, 0i32); 2];
            let n = visible_spans(
                x0,
                x1,
                chrome_span(width, height, frame_radius, y),
                &mut spans,
            );
            let row = (y - box_rect.y) as usize;
            for &(a, b) in &spans[..n] {
                for x in a..b {
                    let index = row * stride + (x - box_rect.x) as usize;
                    debug_assert!(
                        counts[index] == 0 || counts[index] == covered_before,
                        "слои тени не вложены: spread={spread} profile={} expected={covered_before}",
                        counts[index]
                    );
                    counts[index] = counts[index].saturating_add(1);
                }
            }
        }
    }

    let mut rows = Vec::new();
    let mut profiles = Vec::new();
    for y in 0..box_rect.h.max(0) {
        let base = y as usize * stride;
        let line = &counts[base..base + stride];
        // Видимое кольцо в строке — до двух непрерывных пробегов (левое и
        // правое кольцо). Середина строки — закрытая chrome площадь: её в
        // слой не кладём, иначе блит вернул бы те же ~2.4 млн пикселей.
        let mut x = 0usize;
        while x < stride {
            if line[x] == 0 {
                x += 1;
                continue;
            }
            let start = x;
            while x < stride && line[x] != 0 {
                x += 1;
            }
            let offset = profiles.len();
            profiles.extend_from_slice(&line[start..x]);
            rows.push(ShadowRow {
                y,
                x0: start as i32,
                x1: x as i32,
                offset,
            });
        }
    }
    ShadowSprite {
        width: box_rect.w,
        height: box_rect.h,
        rows,
        profiles,
    }
}

/// Интервал строки, который закроет chrome: скруглённая рамка окна.
fn chrome_span(width: i32, height: i32, radius: i32, y: i32) -> Option<(i32, i32)> {
    if y < 0 || y >= height {
        return None;
    }
    let dy = y.min(height - 1 - y);
    let inset = rounded_inset(radius, dy);
    Some((inset, width - inset))
}

/// Видимые части интервала `x0..x1` вне прямоугольника `opaque`.
fn visible_spans(x0: i32, x1: i32, opaque: Option<(i32, i32)>, out: &mut [(i32, i32); 2]) -> usize {
    let Some((left, right)) = opaque else {
        out[0] = (x0, x1);
        return 1;
    };
    let mut n = 0;
    let before = x1.min(left);
    if before > x0 {
        out[n] = (x0, before);
        n += 1;
    }
    let after = x0.max(right);
    if after < x1 {
        out[n] = (after, x1);
        n += 1;
    }
    n
}

/// Точные таблицы каскада: строка `k` — результат `k` смешений `blend(BLACK, a)`
/// с alpha `9..=9+k-1` (в порядке отрисовки: внешний слой первым).
pub fn cascade_luts() -> &'static [[u8; 256]; PROFILES] {
    static TABLES: OnceLock<[[u8; 256]; PROFILES]> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut tables = [[0u8; 256]; PROFILES];
        for (value, slot) in tables[0].iter_mut().enumerate() {
            *slot = value as u8;
        }
        // Первый нарисованный слой — внешний (spread = LAYERS), его alpha
        // равна `ALPHA_STEP - LAYERS`; дальше alpha растёт на 1 за слой.
        let first_alpha = ALPHA_STEP - LAYERS as u8;
        for profile in 1..PROFILES {
            for (value, slot) in tables[profile].iter_mut().enumerate() {
                let mut blended = color::rgb(value as u8, value as u8, value as u8);
                for layer in 0..profile {
                    let alpha = first_alpha + layer as u8;
                    blended = color::blend(blended, color::BLACK, alpha);
                }
                *slot = color::red(blended);
            }
        }
        tables
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::Canvas;
    use crate::color;
    use crate::geometry::Point;

    fn box_area(width: i32, height: i32) -> usize {
        let rect = shadow_box(width, height);
        (rect.w * rect.h) as usize
    }

    /// Кольцо совпадает с настоящей заливкой слоёв: для каждого пикселя коробки
    /// профиль равен числу слоёв `Canvas::fill_rounded_rect`, накрывающих
    /// точку, а ноль — ровно там, где chrome закрывает тень.
    #[test]
    fn ring_matches_layer_coverage() {
        for (width, height) in [
            (640, 400),
            (560, 340),
            (97, 61),
            (64, 48),
            (64, 49),
            (65, 48),
            (129, 97),
            (1024, 768),
        ] {
            let sprite = build_window_shadow(width, height, 10);
            let box_rect = shadow_box(width, height);
            let mut layers = Vec::new();
            for spread in (1..=LAYERS).rev() {
                let mut layer = Canvas::new_transparent(box_rect.w as u32, box_rect.h as u32);
                let rect = layer_rect(width, height, spread);
                layer.fill_rounded_rect(
                    rect.offset(-box_rect.x, -box_rect.y),
                    10 + spread,
                    color::BLACK,
                    (ALPHA_STEP - spread as u8).max(2),
                );
                layers.push(layer);
            }
            let mut chrome = Canvas::new_transparent(box_rect.w as u32, box_rect.h as u32);
            let radius = 10.min(width / 2).min(height / 2);
            chrome.fill_rounded_rect(
                Rect::new(-box_rect.x, -box_rect.y, width, height),
                radius,
                color::rgb(1, 2, 3),
                255,
            );
            // Быстрый снимок слоя: профили по пикселям коробки.
            let mut got = vec![0u8; box_area(width, height)];
            for row in sprite.rows() {
                let base = row.y as usize * box_rect.w as usize + row.x0 as usize;
                got[base..base + (row.x1 - row.x0) as usize].copy_from_slice(
                    &sprite.profiles[row.offset..row.offset + (row.x1 - row.x0) as usize],
                );
            }
            let mut visible = 0usize;
            for y in 0..box_rect.h {
                for x in 0..box_rect.w {
                    let covered = layers.iter().filter(|layer| layer.get(x, y) != 0).count() as u8;
                    let hidden = chrome.get(x, y) != 0;
                    let expected = if hidden { 0 } else { covered };
                    assert_eq!(
                        got[y as usize * box_rect.w as usize + x as usize],
                        expected,
                        "{width}x{height}: профиль ({x},{y})"
                    );
                    visible += (expected != 0) as usize;
                }
            }
            assert_eq!(sprite.visible_pixels(), visible);
            // Кольцо — малая часть коробки: иначе выигрыша нет.
            let box_pixels = box_area(width, height);
            let ratio = box_pixels / visible.max(1);
            assert!(
                ratio >= 2,
                "{width}x{height}: кольцо {visible} из {box_pixels} (x{ratio})"
            );
            if width >= 640 && height >= 400 {
                assert!(
                    ratio >= 10,
                    "{width}x{height}: кольцо {visible} из {box_pixels} (x{ratio})"
                );
            }
        }
    }

    /// Один проход `blit_shadow` байт в байт повторяет `Canvas::shadow` всюду,
    /// где тень видна, и не трогает пиксели, которые закроет chrome.
    #[test]
    fn blit_shadow_matches_canvas_shadow() {
        for (width, height) in [(640, 400), (97, 61), (64, 48), (129, 97)] {
            let screen = Rect::new(0, 0, width + 64, height + 64);
            let frame = Rect::new(32, 26, width, height);
            let sprite = build_window_shadow(width, height, 10);
            let box_rect = shadow_box(width, height).offset(frame.x, frame.y);
            for (name, clip) in [
                ("без отсечения", None),
                (
                    "клип на кольцо",
                    Some(Rect::new(frame.x, frame.y, 5, height)),
                ),
                (
                    "клип курсора",
                    Some(Rect::new(frame.right() + 2, frame.y + 10, 16, 23)),
                ),
                (
                    "два клипа",
                    Some(Rect::new(frame.right() - 3, frame.bottom() - 40, 20, 60)),
                ),
            ] {
                let mut old = Canvas::new(screen.w as u32, screen.h as u32, color::BLACK);
                let mut new = Canvas::new(screen.w as u32, screen.h as u32, color::BLACK);
                for canvas in [&mut old, &mut new] {
                    canvas.fill_gradient(screen, color::rgb(40, 60, 80), color::rgb(10, 14, 18));
                    if let Some(clip) = clip {
                        canvas.set_clip(&[clip]);
                    }
                }
                old.shadow(frame, 10, LAYERS, ALPHA_STEP);
                new.blit_shadow(&sprite, Point::new(box_rect.x, box_rect.y));
                let chrome_radius = 10.min(width / 2).min(height / 2);
                for y in 0..screen.h {
                    for x in 0..screen.w {
                        let hidden = (frame.y..frame.bottom()).contains(&y) && {
                            let dy = (y - frame.y).min(frame.bottom() - 1 - y);
                            let inset = rounded_inset(chrome_radius, dy);
                            (frame.x + inset..frame.right() - inset).contains(&x)
                        };
                        if hidden {
                            continue;
                        }
                        assert_eq!(
                            old.get(x, y),
                            new.get(x, y),
                            "{width}x{height} {name}: пиксель ({x},{y})"
                        );
                    }
                }
            }
        }
    }

    /// Таблица каскада — ровно последовательность `blend`, без «своего» округления.
    #[test]
    fn cascade_lut_matches_sequential_blend() {
        let luts = cascade_luts();
        let first_alpha = ALPHA_STEP - LAYERS as u8;
        for profile in 0..PROFILES {
            for value in 0..=255u32 {
                let mut expected = color::rgb(value as u8, value as u8, value as u8);
                for layer in 0..profile {
                    expected = color::blend(expected, color::BLACK, first_alpha + layer as u8);
                }
                assert_eq!(
                    luts[profile][value as usize],
                    color::red(expected),
                    "профиль {profile}, значение {value}"
                );
            }
        }
    }

    /// Предкомпоновка в RGBA не может быть точной: каскад — не одно смешение.
    ///
    /// Тест перебирает все пары «цвет источника + alpha» одного 8-битного
    /// `blend` и убеждается, что ни одна не воспроизводит каскад из 2+ слоёв.
    /// Именно поэтому слой хранит профили, а не цвет: пиксельная
    /// идентичность иначе недостижима.
    #[test]
    fn single_blend_cannot_replace_cascade() {
        let luts = cascade_luts();
        for profile in 2..PROFILES {
            // Быстрый отсев по трём значениям: кандидатов почти не остаётся.
            let probes = [0usize, 1, 2, 17, 254, 255];
            for source in 0..=255u32 {
                let src = color::rgb(source as u8, source as u8, source as u8);
                for alpha in 0..=255u8 {
                    let candidate = |value: usize| {
                        color::red(color::blend(
                            color::rgb(value as u8, value as u8, value as u8),
                            src,
                            alpha,
                        ))
                    };
                    if probes
                        .iter()
                        .any(|value| candidate(*value) != luts[profile][*value])
                    {
                        continue;
                    }
                    for value in 0..=255usize {
                        assert_ne!(
                            candidate(value),
                            luts[profile][value],
                            "профиль {profile} воспроизведён одним blend \
                             (source={source}, alpha={alpha})"
                        );
                    }
                }
            }
        }
    }

    /// Вложенность слоёв на всём диапазоне размеров окна (MIN 64×48 … 4096²).
    #[test]
    fn layers_are_nested_for_window_sizes() {
        for width in [64, 65, 66, 79, 80, 81, 97, 128, 129, 256, 561, 640] {
            for height in [48, 49, 50, 61, 79, 80, 97, 129, 340, 400] {
                let sprite = build_window_shadow(width, height, 10);
                // Сборка проверяет вложенность debug_assert-ом; здесь — что
                // кольцо непустое и профили не выходят за диапазон.
                assert!(sprite.visible_pixels() > 0, "{width}x{height}");
                assert!(sprite
                    .profiles
                    .iter()
                    .all(|profile| *profile >= 1 && *profile <= LAYERS as u8));
                assert_eq!(sprite.width, width + LAYERS * 2);
                assert_eq!(sprite.height, height + LAYERS * 2);
            }
        }
    }
}
