//! Клиентский список повреждённых прямоугольников (`Damage`).
//!
//! Клиент не перерисовывает и не отправляет всю поверхность, а копит изменения
//! от виджетов/событий в список прямоугольников. Список нормализуется в
//! координатах поверхности: прямоугольники обрезаются по границам, а
//! пересекающиеся **и соприкасающиеся** прямоугольники сливаются. Если после
//! слияния суммарная площадь превышает 70 % поверхности или число
//! прямоугольников превышает лимит (64), список схлопывается в один полный
//! прямоугольник — это дешевле, чем гонять длинный список по проводу.
//!
//! Семантика намеренно совпадает с серверной (`pagh-desktop::geometry::Damage`),
//! но реализация независимая: клиенту нужен только собственный список, чтобы
//! отправлять `DAMAGE`/shm-`COMMIT` с минимумом метаданных. Полная перерисовка
//! разрешена только на первый кадр, `configure`/resize, смену темы, expose и
//! открытие/закрытие popup (см. `docs/desktop-render-spec.md` §4 C2).
//!
//! ```
//! use pagh_ui::damage::Damage;
//! use pagh_ui::geom::Rect;
//!
//! let mut damage = Damage::new(Rect::new(0, 0, 1280, 800));
//! damage.add(Rect::new(10, 10, 20, 10));
//! damage.add(Rect::new(30, 10, 20, 10)); // касается предыдущего — слияние
//! assert_eq!(damage.rects(), &[Rect::new(10, 10, 40, 10)]);
//! ```

use crate::canvas::Canvas;
use crate::geom::Rect;

/// Максимум прямоугольников в списке по умолчанию.
pub const DEFAULT_RECT_CAP: usize = 64;
/// Сторона тайла по умолчанию для [`DamageTracker`] (пиксели).
pub const DEFAULT_TILE: i32 = 16;
/// Доля площади поверхности, после которой список схлопывается в полный кадр.
pub const COLLAPSE_NUMERATOR: i64 = 7;
/// Знаменатель доли схлопывания (7/10 = 70 %).
pub const COLLAPSE_DENOMINATOR: i64 = 10;

/// Нормализованный список повреждённых прямоугольников.
///
/// Список всегда обрезан по `bounds`, не содержит пустых прямоугольников и не
/// содержит пар, которые пересекаются или соприкасаются.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Damage {
    bounds: Rect,
    rects: Vec<Rect>,
    cap: usize,
}

impl Damage {
    /// Пустой список для поверхности `bounds` (обычно `(0, 0, width, height)`).
    pub fn new(bounds: Rect) -> Damage {
        Damage {
            bounds,
            rects: Vec::new(),
            cap: DEFAULT_RECT_CAP,
        }
    }

    /// Пустой список с явным лимитом прямоугольников (для тестов).
    pub fn with_cap(bounds: Rect, cap: usize) -> Damage {
        Damage {
            bounds,
            rects: Vec::new(),
            cap: cap.max(1),
        }
    }

    /// Полный кадр: единственный прямоугольник `bounds`.
    pub fn full(bounds: Rect) -> Damage {
        let mut damage = Damage::new(bounds);
        damage.add(bounds);
        damage
    }

    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    pub fn rects(&self) -> &[Rect] {
        &self.rects
    }

    pub fn len(&self) -> usize {
        self.rects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// Единственный прямоугольник, покрывающий всю поверхность.
    pub fn is_full(&self) -> bool {
        self.rects.len() == 1 && self.rects[0] == self.bounds
    }

    /// Площадь повреждения (без учёта пересечений — их в списке нет).
    pub fn area(&self) -> i64 {
        self.rects.iter().map(|rect| rect.area() as i64).sum()
    }

    /// Доля покрытия поверхности (0..=1).
    pub fn coverage(&self) -> f64 {
        let total = self.bounds.area() as i64;
        if total <= 0 {
            return 0.0;
        }
        (self.area() as f64 / total as f64).min(1.0)
    }

    /// Очистить список, сохранив границы/лимит (переиспользование буфера).
    pub fn clear(&mut self) {
        self.rects.clear();
    }

    /// Прибавить прямоугольник: обрезать, слить с пересекающимися/касающимися,
    /// схлопнуть в полный кадр при переполнении.
    pub fn add(&mut self, rect: Rect) {
        if self.is_full() {
            return;
        }
        let Some(clipped) = rect.intersect(self.bounds) else {
            return;
        };
        if clipped.is_empty() {
            return;
        }
        let mut merged = clipped;
        let mut index = 0;
        while index < self.rects.len() {
            if touches(self.rects[index], merged) {
                merged = merged.union(self.rects.swap_remove(index));
            } else {
                index += 1;
            }
        }
        self.rects.push(merged);
        self.normalize();
    }

    /// Прибавить несколько прямоугольников.
    pub fn add_all(&mut self, rects: &[Rect]) {
        for rect in rects {
            self.add(*rect);
        }
    }

    /// Полный кадр (эквивалент `Damage::full(bounds)` на месте).
    pub fn add_full(&mut self) {
        self.clear();
        if !self.bounds.is_empty() {
            self.rects.push(self.bounds);
        }
    }

    /// Убрать пересечения и соприкосновения, схлопнуть при превышении лимитов.
    fn normalize(&mut self) {
        // После merge в `add` пары не должны пересекаться, но нормализуем
        // повторно: `add_all`/будущие вызовы могут нарушить инвариант.
        loop {
            let mut merged_any = false;
            'outer: for left in 0..self.rects.len() {
                for right in (left + 1)..self.rects.len() {
                    if touches(self.rects[left], self.rects[right]) {
                        let union = self.rects[left].union(self.rects[right]);
                        self.rects.swap_remove(right);
                        self.rects[left] = union;
                        merged_any = true;
                        break 'outer;
                    }
                }
            }
            if !merged_any {
                break;
            }
        }
        let coverage = self.area() * COLLAPSE_DENOMINATOR;
        let total = self.bounds.area() as i64 * COLLAPSE_NUMERATOR;
        if self.rects.len() > self.cap || (total > 0 && coverage > total) {
            self.add_full();
        }
    }
}

/// Прямоугольники пересекаются или соприкасаются сторонами/углами.
///
/// Объединение касающихся прямоугольников в один — это то же правило, по
/// которому работает сервер: не гонять два прямоугольника, если их можно
/// покрыть одним без заметного роста площади.
pub fn touches(a: Rect, b: Rect) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a.x <= b.right() && b.x <= a.right() && a.y <= b.bottom() && b.y <= a.bottom()
}

/// Перевести прямоугольник в проводной формат PDP/1: `i32 x, i32 y, u16 w, u16 h`.
pub fn to_wire(rect: Rect) -> (i32, i32, u16, u16) {
    (
        rect.x,
        rect.y,
        rect.w.clamp(0, u16::MAX as i32) as u16,
        rect.h.clamp(0, u16::MAX as i32) as u16,
    )
}

/// Собрать проводной список в переиспользуемый буфер (без аллокаций кадра).
pub fn to_wire_into(rects: &[Rect], out: &mut Vec<(i32, i32, u16, u16)>) {
    out.clear();
    out.extend(rects.iter().copied().map(to_wire));
}

impl Damage {
    /// Проводной список для `COMMIT`/`DAMAGE` (с аллокацией; для горячего пути
    /// используйте [`to_wire_into`]).
    pub fn to_wire(&self) -> Vec<(i32, i32, u16, u16)> {
        self.rects.iter().copied().map(to_wire).collect()
    }
}

/// Пиксельная поверхность, для которой [`DamageTracker`] считает damage.
///
/// Реализуется [`Canvas`] и (для собственного рендерера терминала) его
/// `Canvas`: трекеру нужны только размеры, stride, bpp и байты.
pub trait DamageSurface {
    fn dims(&self) -> (i32, i32);
    fn stride(&self) -> usize;
    fn bytes_per_pixel(&self) -> usize;
    fn bytes(&self) -> &[u8];
}

impl DamageSurface for Canvas {
    fn dims(&self) -> (i32, i32) {
        (self.width(), self.height())
    }

    fn stride(&self) -> usize {
        Canvas::stride(self)
    }

    fn bytes_per_pixel(&self) -> usize {
        self.pixel_order().bytes_per_pixel()
    }

    fn bytes(&self) -> &[u8] {
        self.pixels()
    }
}

/// Автоматический damage между двумя кадрами поверхности.
///
/// Приложение рисует кадр целиком в `Canvas` (в том числе прямо в mmap
/// `ShmSurface`), а трекер сравнивает хеши тайлов с предыдущим кадром и
/// возвращает только изменившиеся прямоугольники. Это дешёвый и точный способ
/// не гонять по сокету неизменившиеся области: курсор терминала даёт damage
/// одной клетки, открытое меню — прямоугольник меню, а не весь кадр.
///
/// Первый кадр (и кадр после [`DamageTracker::reset`]) всегда считается
/// полным. Тайлы склеиваются обычным [`Damage::add`], поэтому лимит
/// прямоугольников и схлопывание в полный кадр работают как у ручного списка.
#[derive(Clone, Debug)]
pub struct DamageTracker {
    bounds: Rect,
    tile: i32,
    cols: i32,
    rows: i32,
    hashes: Vec<u64>,
    damage: Damage,
    first: bool,
}

impl DamageTracker {
    /// Трекер для поверхности `bounds` с тайлами `tile` пикселей.
    pub fn new(bounds: Rect, tile: i32) -> DamageTracker {
        let tile = tile.clamp(1, 256);
        let (cols, rows) = tile_counts(bounds, tile);
        DamageTracker {
            bounds,
            tile,
            cols,
            rows,
            hashes: vec![0; (cols * rows) as usize],
            damage: Damage::new(bounds),
            first: true,
        }
    }

    /// Трекер с тайлом по умолчанию.
    pub fn for_canvas(canvas: &Canvas) -> DamageTracker {
        DamageTracker::new(canvas.rect(), DEFAULT_TILE)
    }

    /// Следующий кадр считается полным (expose, resize, смена темы).
    pub fn reset(&mut self) {
        self.first = true;
    }

    /// Перестроить трекер под новую геометрию (следующий кадр — полный).
    pub fn resize(&mut self, bounds: Rect) {
        self.bounds = bounds;
        (self.cols, self.rows) = tile_counts(bounds, self.tile);
        self.hashes.clear();
        self.hashes.resize((self.cols * self.rows) as usize, 0);
        self.damage = Damage::new(bounds);
        self.first = true;
    }

    /// Актуальный damage последнего [`DamageTracker::update`].
    pub fn damage(&self) -> &Damage {
        &self.damage
    }

    /// Сравнить `canvas` с предыдущим кадром и вернуть изменившиеся области.
    ///
    /// Возвращает ссылку на внутренний буфер, чтобы не аллоцировать список на
    /// каждом кадре; вызывающий обязан использовать его до следующего
    /// `update`.
    pub fn update<S: DamageSurface + ?Sized>(&mut self, canvas: &S) -> &Damage {
        let (width, height) = canvas.dims();
        let rect = Rect::new(0, 0, width, height);
        if rect != self.bounds {
            self.resize(rect);
        }
        self.damage.clear();
        if rect.is_empty() {
            return &self.damage;
        }
        let bytes = canvas.bytes();
        let stride = canvas.stride();
        let bpp = canvas.bytes_per_pixel();
        let mut index = 0usize;
        for row in 0..self.rows {
            let y0 = (row * self.tile).max(0) as usize;
            let y1 = ((row + 1) * self.tile).min(rect.h).max(0) as usize;
            for col in 0..self.cols {
                let x0 = (col * self.tile).max(0) as usize;
                let x1 = ((col + 1) * self.tile).min(rect.w).max(0) as usize;
                let hash = hash_region(bytes, stride, bpp, x0, y0, x1, y1);
                let changed = self.first || hash != self.hashes[index];
                self.hashes[index] = hash;
                index += 1;
                if changed {
                    self.damage.add(Rect::new(
                        (col * self.tile).max(0),
                        (row * self.tile).max(0),
                        (x1 - x0) as i32,
                        (y1 - y0) as i32,
                    ));
                }
            }
        }
        if self.first {
            self.damage.add_full();
            self.first = false;
        }
        &self.damage
    }
}

fn tile_counts(bounds: Rect, tile: i32) -> (i32, i32) {
    let cols = if bounds.w > 0 {
        (bounds.w + tile - 1) / tile
    } else {
        0
    };
    let rows = if bounds.h > 0 {
        (bounds.h + tile - 1) / tile
    } else {
        0
    };
    (cols, rows)
}

/// FNV-1a по 8-байтным словам: быстрый и достаточный для поиска изменений.
fn hash_region(
    bytes: &[u8],
    stride: usize,
    bpp: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET ^ ((x0 as u64) << 32) ^ y0 as u64;
    for y in y0..y1 {
        let start = y * stride + x0 * bpp;
        let end = (y * stride + x1 * bpp).min(bytes.len());
        if start >= end {
            continue;
        }
        let mut chunks = bytes[start..end].chunks_exact(8);
        for chunk in &mut chunks {
            let word = u64::from_le_bytes(chunk.try_into().expect("8 bytes"));
            hash = (hash ^ word).wrapping_mul(PRIME);
        }
        let tail = chunks.remainder();
        if !tail.is_empty() {
            let mut word = [0u8; 8];
            word[..tail.len()].copy_from_slice(tail);
            hash = (hash ^ u64::from_le_bytes(word)).wrapping_mul(PRIME);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface() -> Rect {
        Rect::new(0, 0, 1280, 800)
    }

    #[test]
    fn empty_damage_has_no_rects() {
        let damage = Damage::new(surface());
        assert!(damage.is_empty());
        assert!(!damage.is_full());
        assert_eq!(damage.area(), 0);
        assert_eq!(damage.coverage(), 0.0);
    }

    #[test]
    fn overlapping_rects_merge() {
        let mut damage = Damage::new(surface());
        damage.add(Rect::new(10, 10, 100, 30));
        damage.add(Rect::new(60, 20, 100, 30));
        assert_eq!(damage.rects(), &[Rect::new(10, 10, 150, 40)]);
    }

    #[test]
    fn touching_rects_merge_on_edges_and_corners() {
        let mut damage = Damage::new(surface());
        damage.add(Rect::new(0, 0, 10, 10));
        damage.add(Rect::new(10, 0, 10, 10)); // правый край касается левого
        assert_eq!(damage.rects(), &[Rect::new(0, 0, 20, 10)]);

        let mut corner = Damage::new(surface());
        corner.add(Rect::new(0, 0, 10, 10));
        corner.add(Rect::new(10, 10, 10, 10)); // соприкасаются углом
        assert_eq!(corner.rects(), &[Rect::new(0, 0, 20, 20)]);
    }

    #[test]
    fn disjoint_rects_stay_separate_and_ordered() {
        let mut damage = Damage::new(surface());
        damage.add(Rect::new(10, 10, 10, 10));
        damage.add(Rect::new(100, 100, 10, 10));
        damage.add(Rect::new(500, 40, 20, 20));
        assert_eq!(damage.len(), 3);
        assert_eq!(damage.rects()[0], Rect::new(10, 10, 10, 10));
        assert_eq!(damage.rects()[1], Rect::new(100, 100, 10, 10));
        assert_eq!(damage.rects()[2], Rect::new(500, 40, 20, 20));
    }

    #[test]
    fn rects_are_clipped_to_surface() {
        let mut damage = Damage::new(surface());
        damage.add(Rect::new(-20, -10, 100, 50));
        assert_eq!(damage.rects(), &[Rect::new(0, 0, 80, 40)]);
        damage.add(Rect::new(1200, 780, 200, 200));
        assert_eq!(damage.rects()[1], Rect::new(1200, 780, 80, 20));
        // Полностью вне поверхности — отбрасывается.
        damage.add(Rect::new(2000, 2000, 10, 10));
        assert_eq!(damage.len(), 2);
    }

    #[test]
    fn huge_damage_collapses_to_full_frame() {
        let mut damage = Damage::new(surface());
        // 20 полос по 30 px через промежуток 10 px: 75 % площади → схлопывание.
        for y in (0..800).step_by(40) {
            damage.add(Rect::new(0, y, 1280, 30));
        }
        assert!(damage.is_full());
        assert_eq!(damage.area(), 1280 * 800);
    }

    #[test]
    fn too_many_rects_collapse() {
        let mut damage = Damage::with_cap(surface(), 4);
        for i in 0..10 {
            damage.add(Rect::new(10 + i * 120, 10, 20, 20));
        }
        assert!(damage.is_full());
    }

    #[test]
    fn full_add_is_idempotent() {
        let mut damage = Damage::new(surface());
        damage.add(surface());
        assert!(damage.is_full());
        damage.add(Rect::new(1, 1, 1, 1));
        assert_eq!(damage.len(), 1);
        damage.add_full();
        assert!(damage.is_full());
    }

    #[test]
    fn zero_sized_rects_are_ignored() {
        let mut damage = Damage::new(surface());
        damage.add(Rect::new(5, 5, 0, 10));
        damage.add(Rect::new(5, 5, 10, 0));
        assert!(damage.is_empty());
    }

    #[test]
    fn wire_roundtrip() {
        let mut damage = Damage::new(surface());
        damage.add(Rect::new(10, 20, 30, 40));
        damage.add(Rect::new(100, 200, 50, 60));
        let wire = damage.to_wire();
        assert_eq!(wire, vec![(10, 20, 30, 40), (100, 200, 50, 60)]);
        let mut reused = vec![(0, 0, 1, 1)];
        to_wire_into(damage.rects(), &mut reused);
        assert_eq!(reused, wire);
    }

    #[test]
    fn clear_keeps_configuration() {
        let mut damage = Damage::with_cap(surface(), 8);
        damage.add(Rect::new(1, 2, 3, 4));
        damage.clear();
        assert!(damage.is_empty());
        damage.add(Rect::new(10, 10, 10, 10));
        assert_eq!(damage.len(), 1);
        assert_eq!(damage.cap, 8);
    }

    #[test]
    fn tracker_first_frame_is_full_then_idle_is_empty() {
        let mut canvas = Canvas::filled(64, 48, crate::color::Color::BLACK);
        let mut tracker = DamageTracker::for_canvas(&canvas);
        assert!(tracker.update(&canvas).is_full());
        assert!(tracker.update(&canvas).is_empty());

        canvas.set_pixel(crate::geom::Point::new(5, 5), crate::color::Color::WHITE);
        let damage = tracker.update(&canvas);
        assert!(!damage.is_empty() && !damage.is_full());
        assert!(
            damage
                .rects()
                .iter()
                .any(|rect| rect.contains(crate::geom::Point::new(5, 5))),
            "damage must cover the changed pixel: {:?}",
            damage.rects()
        );
        // Повторный кадр без изменений пуст.
        assert!(tracker.update(&canvas).is_empty());
    }

    #[test]
    fn tracker_isolates_two_separate_changes() {
        let mut canvas = Canvas::filled(128, 64, crate::color::Color::BLACK);
        let mut tracker = DamageTracker::new(canvas.rect(), 8);
        let _ = tracker.update(&canvas);
        canvas.set_pixel(crate::geom::Point::new(1, 1), crate::color::Color::WHITE);
        // Меняем пиксель во втором тайле-столбце >= 120 px правее.
        canvas.set_pixel(crate::geom::Point::new(120, 40), crate::color::Color::WHITE);
        let damage = tracker.update(&canvas);
        assert_eq!(damage.len(), 2, "{:?}", damage.rects());
        assert!(damage.area() <= 2 * 8 * 8);
    }

    #[test]
    fn tracker_resize_requests_full_frame() {
        let canvas = Canvas::filled(32, 32, crate::color::Color::BLACK);
        let mut tracker = DamageTracker::for_canvas(&canvas);
        let _ = tracker.update(&canvas);
        let bigger = Canvas::filled(48, 32, crate::color::Color::BLACK);
        assert!(tracker.update(&bigger).is_full());
        assert_eq!(tracker.damage().bounds(), bigger.rect());
    }
}
