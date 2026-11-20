//! Базовая целочисленная геометрия: точка, размер, прямоугольник и трекер damage.
//!
//! Координаты — логические пиксели экрана, начало в левом верхнем углу. Прямоугольники
//! полуоткрытые: `x..x+w`, `y..y+h`; пустой прямоугольник (`w <= 0 || h <= 0`) ничего не
//! пересекает и не рисуется.

use std::fmt;

/// Точка в логических координатах экрана.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    pub fn offset(self, dx: i32, dy: i32) -> Self {
        Self::new(self.x + dx, self.y + dy)
    }
}

impl fmt::Display for Point {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.x, self.y)
    }
}

/// Ширина и высота в пикселях.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Size {
    pub w: u32,
    pub h: u32,
}

impl Size {
    pub const fn new(w: u32, h: u32) -> Self {
        Self { w, h }
    }

    pub fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }
}

/// Прямоугольник в логических координатах.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const ZERO: Rect = Rect::new(0, 0, 0, 0);

    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub const fn from_size(x: i32, y: i32, size: Size) -> Self {
        Self::new(x, y, size.w as i32, size.h as i32)
    }

    pub fn from_points(a: Point, b: Point) -> Self {
        let x = a.x.min(b.x);
        let y = a.y.min(b.y);
        Self::new(x, y, (a.x - b.x).abs(), (a.y - b.y).abs())
    }

    pub fn is_empty(self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub fn right(self) -> i32 {
        self.x.saturating_add(self.w)
    }

    pub fn bottom(self) -> i32 {
        self.y.saturating_add(self.h)
    }

    pub fn center(self) -> Point {
        Point::new(self.x + self.w / 2, self.y + self.h / 2)
    }

    pub fn area(self) -> i64 {
        if self.is_empty() {
            0
        } else {
            i64::from(self.w) * i64::from(self.h)
        }
    }

    /// Содержит ли прямоугольник точку (полуоткрытые границы).
    pub fn contains(self, p: Point) -> bool {
        !self.is_empty()
            && p.x >= self.x
            && p.y >= self.y
            && p.x < self.right()
            && p.y < self.bottom()
    }

    pub fn contains_rect(self, other: Rect) -> bool {
        !other.is_empty()
            && !self.is_empty()
            && other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }

    pub fn intersects(self, other: Rect) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    pub fn intersection(self, other: Rect) -> Option<Rect> {
        if !self.intersects(other) {
            return None;
        }
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        Some(Rect::new(x, y, right - x, bottom - y))
    }

    /// Наименьший прямоугольник, содержащий оба. Пустой операнд игнорируется.
    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Rect::new(x, y, right - x, bottom - y)
    }

    pub fn offset(self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    /// Расширить прямоугольник на `amount` со всех сторон (отрицательное значение сжимает).
    pub fn inflate(self, amount: i32) -> Rect {
        if self.is_empty() {
            return self;
        }
        Rect::new(
            self.x - amount,
            self.y - amount,
            self.w + amount * 2,
            self.h + amount * 2,
        )
    }

    /// Внутренний прямоугольник с отступом `amount` со всех сторон.
    pub fn inset(self, amount: i32) -> Rect {
        self.inflate(-amount)
    }

    /// Пересечение с экраном, если оно непустое.
    pub fn clamped(self, screen: Rect) -> Option<Rect> {
        self.intersection(screen)
    }
}

impl fmt::Display for Rect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}+{},{}", self.w, self.h, self.x, self.y)
    }
}

/// Один кадр damage: набор прямоугольников, которые нужно перерисовать.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Damage {
    pub rects: Vec<Rect>,
    /// Если true — повреждён весь экран и `rects` можно игнорировать.
    pub whole: bool,
}

impl Damage {
    pub fn is_empty(&self) -> bool {
        !self.whole && self.rects.is_empty()
    }

    /// Суммарная площадь прямоугольников (для счётчиков present/байт).
    pub fn area(&self) -> i64 {
        self.rects.iter().map(|rect| rect.area()).sum()
    }

    pub fn merged(rects: Vec<Rect>) -> Vec<Rect> {
        merge_rects(rects)
    }
}

/// Накопитель damage за кадр.
///
/// Композитор добавляет сюда затронутые области, сервер забирает накопленное перед
/// отрисовкой. Прямоугольники нормализуются: пересекающиеся и смежные объединяются,
/// количество ограничено, чтобы патологический клиент не раздувал список.
#[derive(Clone, Debug)]
pub struct DamageTracker {
    screen: Rect,
    pending: Vec<Rect>,
    whole: bool,
}

/// Верхняя граница количества damage-прямоугольников за кадр.
pub const DAMAGE_MAX_RECTS: usize = 64;
/// Если суммарная площадь damage превышает этот процент экрана, кадр
/// сворачивается в один полноэкранный прямоугольник.
pub const DAMAGE_COLLAPSE_PERCENT: i64 = 70;

impl DamageTracker {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            screen: Rect::new(0, 0, width as i32, height as i32),
            pending: Vec::new(),
            whole: false,
        }
    }

    pub fn screen(&self) -> Rect {
        self.screen
    }

    pub fn add(&mut self, rect: Rect) {
        if self.whole || rect.is_empty() {
            return;
        }
        if let Some(visible) = rect.intersection(self.screen) {
            self.pending.push(visible);
            if self.pending.len() > DAMAGE_MAX_RECTS {
                self.pending = merge_rects(std::mem::take(&mut self.pending));
            }
        }
    }

    pub fn add_all(&mut self, rects: impl IntoIterator<Item = Rect>) {
        for rect in rects {
            self.add(rect);
        }
    }

    pub fn add_whole(&mut self) {
        self.whole = true;
        self.pending.clear();
    }

    pub fn is_empty(&self) -> bool {
        !self.whole && self.pending.is_empty()
    }

    pub fn take(&mut self) -> Damage {
        if self.whole {
            self.whole = false;
            return Damage {
                rects: vec![self.screen],
                whole: true,
            };
        }
        let rects = merge_rects(std::mem::take(&mut self.pending));
        if covers_most_of_screen(&rects, self.screen) {
            return Damage {
                rects: vec![self.screen],
                whole: true,
            };
        }
        Damage {
            rects,
            whole: false,
        }
    }
}

fn covers_most_of_screen(rects: &[Rect], screen: Rect) -> bool {
    let total: i64 = rects.iter().map(|rect| rect.area()).sum();
    total > 0 && total * 100 >= screen.area() * DAMAGE_COLLAPSE_PERCENT
}

fn merge_rects(mut rects: Vec<Rect>) -> Vec<Rect> {
    rects.retain(|r| !r.is_empty());
    // Пересекающиеся и соприкасающиеся прямоугольники объединяем: лишняя
    // площадь безопасна (перерисуем больше), зато список остаётся коротким.
    let mut merged = true;
    while merged {
        merged = false;
        let mut i = 0;
        while i < rects.len() {
            let mut j = i + 1;
            while j < rects.len() {
                if overlap_or_touch(rects[i], rects[j]) {
                    rects[i] = rects[i].union(rects[j]);
                    rects.swap_remove(j);
                    merged = true;
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
    }
    // Ограничиваем количество: сливаем пару с минимальным приростом площади.
    while rects.len() > DAMAGE_MAX_RECTS {
        let mut best = (0usize, 1usize, i64::MAX);
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                let union = rects[i].union(rects[j]).area();
                let cost = union - rects[i].area() - rects[j].area();
                if cost < best.2 {
                    best = (i, j, cost);
                }
            }
        }
        let (i, j, _) = best;
        rects[i] = rects[i].union(rects[j]);
        rects.swap_remove(j);
    }
    rects.sort_by_key(|r| (r.y, r.x, r.w, r.h));
    rects
}

fn overlap_or_touch(a: Rect, b: Rect) -> bool {
    !a.is_empty()
        && !b.is_empty()
        && a.x <= b.right()
        && b.x <= a.right()
        && a.y <= b.bottom()
        && b.y <= a.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_is_half_open() {
        let r = Rect::new(10, 20, 30, 40);
        assert!(r.contains(Point::new(10, 20)));
        assert!(r.contains(Point::new(39, 59)));
        assert!(!r.contains(Point::new(40, 20)));
        assert!(!r.contains(Point::new(10, 60)));
    }

    #[test]
    fn intersection_and_union() {
        let a = Rect::new(0, 0, 100, 100);
        let b = Rect::new(50, 50, 100, 100);
        assert_eq!(a.intersection(b), Some(Rect::new(50, 50, 50, 50)));
        assert_eq!(a.union(b), Rect::new(0, 0, 150, 150));
        assert_eq!(a.intersection(Rect::new(200, 200, 10, 10)), None);
    }

    #[test]
    fn empty_rects_are_ignored() {
        let empty = Rect::new(5, 5, 0, 10);
        assert!(empty.is_empty());
        assert!(!empty.contains(Point::new(5, 5)));
        assert_eq!(
            Rect::ZERO.union(Rect::new(1, 2, 3, 4)),
            Rect::new(1, 2, 3, 4)
        );
    }

    #[test]
    fn damage_merges_overlaps_and_clamps() {
        let mut tracker = DamageTracker::new(100, 100);
        tracker.add(Rect::new(-20, -20, 50, 50));
        tracker.add(Rect::new(40, 40, 10, 10));
        tracker.add(Rect::new(200, 200, 10, 10));
        let damage = tracker.take();
        assert!(!damage.whole);
        assert_eq!(damage.rects.len(), 2);
        assert!(damage.rects[0].contains(Point::new(0, 0)));
        assert!(damage
            .rects
            .iter()
            .all(|r| r.intersects(Rect::new(0, 0, 100, 100))));
        assert!(tracker.take().is_empty());
    }

    #[test]
    fn whole_damage_short_circuits() {
        let mut tracker = DamageTracker::new(10, 10);
        tracker.add(Rect::new(0, 0, 1, 1));
        tracker.add_whole();
        let d = tracker.take();
        assert!(d.whole);
        assert_eq!(d.rects, vec![Rect::new(0, 0, 10, 10)]);
    }

    #[test]
    fn touching_rects_are_merged() {
        let mut tracker = DamageTracker::new(100, 100);
        tracker.add(Rect::new(10, 10, 20, 20));
        // Касается правой гранью (x=30) — должен слиться, хотя пересечения нет.
        tracker.add(Rect::new(30, 10, 20, 20));
        let damage = tracker.take();
        assert!(!damage.whole);
        assert_eq!(damage.rects, vec![Rect::new(10, 10, 40, 20)]);
        assert_eq!(damage.area(), 800);
    }

    #[test]
    fn damage_rect_count_is_capped() {
        let mut tracker = DamageTracker::new(1000, 1000);
        for i in 0..300 {
            let x = (i % 25) * 39;
            let y = (i / 25) * 39;
            tracker.add(Rect::new(x, y, 20, 20));
        }
        let damage = tracker.take();
        assert!(!damage.whole, "площадь мала для collapse");
        assert!(
            damage.rects.len() <= DAMAGE_MAX_RECTS,
            "прямоугольников {}, ожидался предел {DAMAGE_MAX_RECTS}",
            damage.rects.len()
        );
        assert!(!damage.rects.is_empty());
    }

    #[test]
    fn large_damage_collapses_to_screen() {
        let mut tracker = DamageTracker::new(100, 100);
        tracker.add(Rect::new(0, 0, 100, 50));
        tracker.add(Rect::new(0, 50, 100, 25));
        tracker.add(Rect::new(0, 0, 100, 100));
        let damage = tracker.take();
        assert!(damage.whole);
        assert_eq!(damage.rects, vec![Rect::new(0, 0, 100, 100)]);
        assert_eq!(damage.area(), 10_000);
    }

    #[test]
    fn small_scattered_damage_stays_partial() {
        let mut tracker = DamageTracker::new(100, 100);
        tracker.add(Rect::new(0, 0, 10, 10));
        tracker.add(Rect::new(80, 80, 10, 10));
        let damage = tracker.take();
        assert!(!damage.whole);
        assert_eq!(damage.rects.len(), 2);
        assert_eq!(damage.area(), 200);
    }
}
