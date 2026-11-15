//! Геометрия UI: точки, размеры, прямоугольники и внутренние отступы.
//!
//! Все координаты целочисленные и заданы в пикселях, начало координат — левый
//! верхний угол поверхности. Отрицательные координаты допустимы (окно может
//! выходить за экран), поэтому все проверки реализованы явно.

/// Точка на поверхности.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const ZERO: Point = Point { x: 0, y: 0 };

    pub const fn new(x: i32, y: i32) -> Point {
        Point { x, y }
    }

    /// Сдвиг на `(dx, dy)`.
    pub fn offset(self, dx: i32, dy: i32) -> Point {
        Point::new(self.x + dx, self.y + dy)
    }

    /// Расстояние до другой точки (евклидово, в пикселях).
    pub fn distance(self, other: Point) -> f32 {
        let dx = (self.x - other.x) as f32;
        let dy = (self.y - other.y) as f32;
        (dx * dx + dy * dy).sqrt()
    }
}

/// Размер прямоугольника.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct Size {
    pub w: i32,
    pub h: i32,
}

impl Size {
    pub const ZERO: Size = Size { w: 0, h: 0 };

    pub const fn new(w: i32, h: i32) -> Size {
        Size { w, h }
    }

    pub const fn area(self) -> i32 {
        self.w * self.h
    }

    pub const fn is_empty(self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// Размер, уменьшенный по каждой оси не меньше чем до нуля.
    pub fn saturating_sub(self, dx: i32, dy: i32) -> Size {
        Size::new((self.w - dx).max(0), (self.h - dy).max(0))
    }
}

/// Прямоугольник: положение и размер.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const ZERO: Rect = Rect {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };

    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    /// Прямоугольник из положения и размера.
    pub const fn from_parts(origin: Point, size: Size) -> Rect {
        Rect::new(origin.x, origin.y, size.w, size.h)
    }

    pub const fn size(self) -> Size {
        Size::new(self.w, self.h)
    }

    pub const fn origin(self) -> Point {
        Point::new(self.x, self.y)
    }

    pub const fn right(self) -> i32 {
        self.x + self.w
    }

    pub const fn bottom(self) -> i32 {
        self.y + self.h
    }

    pub const fn center(self) -> Point {
        Point::new(self.x + self.w / 2, self.y + self.h / 2)
    }

    pub const fn is_empty(self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub const fn area(self) -> i32 {
        self.w * self.h
    }

    /// Содержит ли точка прямоугольник. Правая и нижняя границы не входят.
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.x && p.y >= self.y && p.x < self.right() && p.y < self.bottom()
    }

    /// Пересечение прямоугольников; `None`, если они не пересекаются.
    pub fn intersect(self, other: Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= x || bottom <= y {
            None
        } else {
            Some(Rect::new(x, y, right - x, bottom - y))
        }
    }

    pub fn intersects(self, other: Rect) -> bool {
        self.intersect(other).is_some()
    }

    /// Наименьший прямоугольник, содержащий оба.
    pub fn union(self, other: Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Rect::new(x, y, right - x, bottom - y)
    }

    /// Сдвиг прямоугольника.
    pub fn offset(self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    /// Прямоугольник с тем же положением и другим размером.
    pub fn with_size(self, size: Size) -> Rect {
        Rect::new(self.x, self.y, size.w, size.h)
    }

    /// Сжатие на одинаковый отступ со всех сторон.
    pub fn inset(self, amount: i32) -> Rect {
        Rect::new(
            self.x + amount,
            self.y + amount,
            (self.w - 2 * amount).max(0),
            (self.h - 2 * amount).max(0),
        )
    }

    /// Сжатие на разные отступы по сторонам.
    pub fn inset_by(self, insets: Insets) -> Rect {
        Rect::new(
            self.x + insets.left,
            self.y + insets.top,
            (self.w - insets.left - insets.right).max(0),
            (self.h - insets.top - insets.bottom).max(0),
        )
    }

    /// Расширение прямоугольника на одинаковый отступ.
    pub fn expand(self, amount: i32) -> Rect {
        self.inset(-amount)
    }

    /// Уменьшение размера с сохранением положения (не выходит за границы).
    pub fn clamp(self, bounds: Rect) -> Rect {
        match self.intersect(bounds) {
            Some(r) => r,
            None => Rect::new(self.x, self.y, 0, 0),
        }
    }

    /// Прямоугольник, центрированный внутри `outer`.
    pub fn centered_in(self, outer: Rect) -> Rect {
        Rect::new(
            outer.x + (outer.w - self.w) / 2,
            outer.y + (outer.h - self.h) / 2,
            self.w,
            self.h,
        )
    }

    /// Верхняя полоса высотой `height`, остаток — второй элемент.
    pub fn split_top(self, height: i32) -> (Rect, Rect) {
        let h = height.clamp(0, self.h);
        (
            Rect::new(self.x, self.y, self.w, h),
            Rect::new(self.x, self.y + h, self.w, self.h - h),
        )
    }

    /// Нижняя полоса высотой `height`, остаток — второй элемент.
    pub fn split_bottom(self, height: i32) -> (Rect, Rect) {
        let h = height.clamp(0, self.h);
        (
            Rect::new(self.x, self.y, self.w, self.h - h),
            Rect::new(self.x, self.y + self.h - h, self.w, h),
        )
    }

    /// Левая полоса шириной `width`, остаток — второй элемент.
    pub fn split_left(self, width: i32) -> (Rect, Rect) {
        let w = width.clamp(0, self.w);
        (
            Rect::new(self.x, self.y, w, self.h),
            Rect::new(self.x + w, self.y, self.w - w, self.h),
        )
    }

    /// Правая полоса шириной `width`, остаток — второй элемент.
    pub fn split_right(self, width: i32) -> (Rect, Rect) {
        let w = width.clamp(0, self.w);
        (
            Rect::new(self.x, self.y, self.w - w, self.h),
            Rect::new(self.x + self.w - w, self.y, w, self.h),
        )
    }
}

/// Внутренние отступы (как `padding` в CSS).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct Insets {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

impl Insets {
    pub const ZERO: Insets = Insets {
        top: 0,
        right: 0,
        bottom: 0,
        left: 0,
    };

    pub const fn uniform(amount: i32) -> Insets {
        Insets {
            top: amount,
            right: amount,
            bottom: amount,
            left: amount,
        }
    }

    pub const fn symmetric(horizontal: i32, vertical: i32) -> Insets {
        Insets {
            top: vertical,
            right: horizontal,
            bottom: vertical,
            left: horizontal,
        }
    }

    pub const fn new(top: i32, right: i32, bottom: i32, left: i32) -> Insets {
        Insets {
            top,
            right,
            bottom,
            left,
        }
    }

    pub const fn horizontal(self) -> i32 {
        self.left + self.right
    }

    pub const fn vertical(self) -> i32 {
        self.top + self.bottom
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_excludes_right_and_bottom_edges() {
        let r = Rect::new(10, 20, 30, 40);
        assert!(r.contains(Point::new(10, 20)));
        assert!(r.contains(Point::new(39, 59)));
        assert!(!r.contains(Point::new(40, 59)));
        assert!(!r.contains(Point::new(39, 60)));
        assert!(!r.contains(Point::new(9, 20)));
    }

    #[test]
    fn intersect_and_union() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(b), Some(Rect::new(5, 5, 5, 5)));
        assert_eq!(a.union(b), Rect::new(0, 0, 15, 15));
        assert_eq!(a.intersect(Rect::new(20, 20, 1, 1)), None);
    }

    #[test]
    fn inset_does_not_go_negative() {
        let r = Rect::new(0, 0, 4, 2).inset(3);
        assert_eq!(r.size(), Size::new(0, 0));
    }

    #[test]
    fn splits_preserve_area() {
        let r = Rect::new(2, 3, 100, 50);
        let (top, bottom) = r.split_top(20);
        assert_eq!(top.h, 20);
        assert_eq!(bottom.y, 23);
        assert_eq!(top.area() + bottom.area(), r.area());

        let (left, right) = r.split_left(30);
        assert_eq!(left.w, 30);
        assert_eq!(right.x, 32);
        assert_eq!(left.area() + right.area(), r.area());
    }

    #[test]
    fn centered_rect() {
        let outer = Rect::new(0, 0, 100, 40);
        let inner = Rect::new(0, 0, 20, 10).centered_in(outer);
        assert_eq!(inner, Rect::new(40, 15, 20, 10));
    }

    #[test]
    fn insets_by() {
        let r = Rect::new(0, 0, 100, 50).inset_by(Insets::new(5, 10, 5, 10));
        assert_eq!(r, Rect::new(10, 5, 80, 40));
    }
}
