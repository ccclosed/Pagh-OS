// drivers/scrollback.rs — ring скроллбека консоли и модель прокручиваемого вида
// 64-битное ядро ОС x86_64 на Rust (#![no_std])
//
// Framebuffer-консоль держит одну текстовую сетку в RAM (см. `framebuffer.rs`):
// это модель, которую правит shell, на которой стоит каретка и из которой
// копирует выделение. В этой модели ровно столько строк, сколько влезает на
// экран, так что ушедший вверх текст пропадал. Модуль — недостающая история:
// ограниченный ring *строк* — та же пара `(chars, fg)`, что framebuffer держит на
// клетку, — плюс view-смещение, говорящее «показать последние N строк истории над
// живой сеткой».
//
// Чистый намеренно: ни пикселя, ни лока, ни устройства. Framebuffer владеет одним
// таким, кормит его строкой каждый раз при `scroll()` и спрашивает, в какую
// строку отображается экранная позиция при прокрутке назад. Такое разделение
// позволяет host-тестам гонять точный исходник, который компилирует ядро
// (property `scrollback`), и держит правила wrap/offset в стороне от пиксельного
// пути.
//
// Терминология:
//   * row        — одна строка клеток консоли шириной `W` символов;
//   * history    — ring строк, уже ушедших с верха экрана;
//   * view       — сколько новейших строк истории показано *над* живой сеткой;
//                  `0` означает «живой вид, ничего не прокручено назад».
//
// Пока пользователь прокручен назад, новый вывод консоли не должен двигать то,
// что он читает: `push` сохраняет те же абсолютные строки в виде, увеличивая
// `view`, а старейшая строка отбрасывается, только когда ring полон — стандартное
// поведение терминала, и именно на него опирается путь колеса, чтобы оставаться
// стабильным, пока сетевой насос печатает INFO-строки в фоне.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

/// Одна строка консоли: символы и foreground-цвет, которым был нарисован каждый.
///
/// `fg` — часть строки по той же причине, по которой framebuffer держит `grid_fg`:
/// пиксели не помнят, как клетка была покрашена, так что re-render (прокрутка,
/// перерисовка темы) должен уметь вернуть цветной текст.
///
/// Ширина — параметр `Scrollback`, а не константа: сетка консоли выводится из
/// разрешения экрана (`drivers::console_layout`), поэтому одна и та же модель
/// истории живёт и на 1280×720 (160 колонок), и на 2560×1440 (320).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub chars: Vec<u8>,
    pub fg: Vec<u32>,
}

impl Row {
    /// A row of `width` spaces drawn in `fg` — what a freshly cleared console row is.
    pub fn blank(width: usize, fg: u32) -> Self {
        Self {
            chars: vec![b' '; width],
            fg: vec![fg; width],
        }
    }

    /// Copy one console row into an owned history row.
    pub fn from_cells(chars: &[u8], fg: &[u32]) -> Self {
        Self {
            chars: chars.to_vec(),
            fg: fg.to_vec(),
        }
    }

    /// Number of cells in the row (the console width it was captured at).
    pub fn width(&self) -> usize {
        self.chars.len()
    }
}

/// Bounded scrollback ring plus the scroll view offset.
pub struct Scrollback {
    /// Console width every retained row was captured at.
    width: usize,
    rows: VecDeque<Row>,
    capacity: usize,
    view: usize,
}

impl Scrollback {
    /// Create an empty ring of `width`-cell rows that holds at most `capacity` rows.
    ///
    /// Allocation is lazy (the deque grows as output scrolls), so a session that
    /// produces a screenful of text costs nothing; `capacity` only bounds the
    /// memory a long session can reach. A caller that wants a byte budget (the
    /// console sizes the ring from its real width) divides the budget by
    /// `width * 5` itself and passes the resulting row count.
    pub fn new(width: usize, capacity: usize) -> Self {
        Self {
            width: width.max(1),
            rows: VecDeque::new(),
            capacity: capacity.max(1),
            view: 0,
        }
    }

    /// Console width the retained rows are captured at.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Number of history rows currently retained.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether no rows have scrolled off yet.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// How many history rows are shown above the live grid (`0` = live view).
    pub fn view(&self) -> usize {
        self.view
    }

    /// Forget the history and return to the live view.
    ///
    /// Returns whether anything changed, so callers can skip a repaint.
    pub fn clear(&mut self) -> bool {
        let changed = !self.rows.is_empty() || self.view != 0;
        self.rows.clear();
        self.view = 0;
        changed
    }

    /// Append a row that has just left the top of the screen.
    ///
    /// A scrolled-back user keeps reading the same absolute rows: the view offset
    /// moves with the ring (`view + 1`), so the shown window does not jump when
    /// background output scrolls. When the ring is full the oldest row is dropped
    /// and the view is clamped — a window that was already at the oldest row can
    /// no longer hold all of its content, and that is the one case where scrolling
    /// up has to advance.
    pub fn push(&mut self, row: Row) {
        if self.rows.len() == self.capacity {
            self.rows.pop_front();
        }
        self.rows.push_back(row);
        if self.view > 0 {
            self.view = (self.view + 1).min(self.rows.len());
        }
    }

    /// Scroll the view by `delta` rows: positive shows older text, negative
    /// returns towards the live screen. Clamped to `[0, len]`; returns whether the
    /// view actually moved.
    pub fn scroll(&mut self, delta: i32) -> bool {
        let before = self.view;
        let target = self.view as i64 + delta as i64;
        let max = self.rows.len() as i64;
        self.view = target.clamp(0, max) as usize;
        self.view != before
    }

    /// Jump back to the live view. Returns whether the view moved.
    pub fn reset(&mut self) -> bool {
        let changed = self.view != 0;
        self.view = 0;
        changed
    }

    /// The history row shown at screen row `screen_row`, when the view is scrolled.
    ///
    /// Screen row `r` with a view of `v` shows history index `len - v + r` for
    /// `r < v`, and live grid row `r - v` otherwise (the caller supplies those).
    /// Returns `None` when `screen_row` is not part of the history part of the
    /// view, or when no rows are retained.
    pub fn view_row(&self, screen_row: usize) -> Option<&Row> {
        if screen_row >= self.view || self.view == 0 {
            return None;
        }
        let first = self.rows.len().checked_sub(self.view)?;
        self.rows.get(first + screen_row)
    }

    /// The text of the top row of the current view (history when scrolled, or the
    /// caller's live row when not), for diagnostics; trailing blanks are trimmed.
    pub fn view_top_text(&self) -> Option<alloc::string::String> {
        let row = if self.view == 0 {
            return None;
        } else {
            self.view_row(0)?
        };
        let end = row
            .chars
            .iter()
            .rposition(|&c| c != b' ')
            .map(|i| i + 1)
            .unwrap_or(0);
        let mut out = alloc::string::String::with_capacity(end);
        for &c in &row.chars[..end] {
            out.push(c as char);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::{Row, Scrollback};
    use alloc::vec;

    fn row(marker: u8) -> Row {
        let mut r = Row::blank(4, 0xFFFFFF);
        r.chars[0] = marker;
        r
    }

    #[test]
    fn starts_empty_and_live() {
        let sb = Scrollback::new(4, 8);
        assert_eq!((sb.len(), sb.view()), (0, 0));
        assert!(sb.is_empty());
        assert!(sb.view_row(0).is_none());
        assert!(sb.view_top_text().is_none());
    }

    #[test]
    fn width_is_carried_with_the_ring() {
        // The console grid is derived from the framebuffer resolution, so the
        // ring must hold rows of whatever width the screen produced.
        let mut sb = Scrollback::new(320, 8);
        assert_eq!(sb.width(), 320);
        let mut wide = Row::blank(320, 0x102030);
        wide.chars[319] = b'x';
        sb.push(wide);
        sb.scroll(1);
        let row = sb.view_row(0).expect("history row");
        assert_eq!(row.width(), 320);
        assert_eq!(row.chars[319], b'x');
    }

    #[test]
    fn push_keeps_live_view_at_the_bottom() {
        let mut sb = Scrollback::new(4, 8);
        sb.push(row(b'a'));
        sb.push(row(b'b'));
        assert_eq!((sb.len(), sb.view()), (2, 0));
        // The live view is not part of the history mapping.
        assert!(sb.view_row(0).is_none());
    }

    #[test]
    fn scroll_clamps_to_retained_rows() {
        let mut sb = Scrollback::new(4, 8);
        for c in b"abc" {
            sb.push(row(*c));
        }
        assert!(sb.scroll(1));
        assert_eq!(sb.view(), 1);
        assert!(sb.scroll(100));
        assert_eq!(sb.view(), 3);
        assert!(!sb.scroll(100));
        assert!(sb.scroll(-2));
        assert_eq!(sb.view(), 1);
        assert!(sb.scroll(-100));
        assert_eq!(sb.view(), 0);
        assert!(!sb.scroll(0));
    }

    #[test]
    fn view_maps_history_rows_in_order() {
        let mut sb = Scrollback::new(4, 8);
        for c in b"abcd" {
            sb.push(row(*c));
        }
        sb.scroll(2);
        // Two newest history rows sit above the live grid: c then d.
        assert_eq!(sb.view_row(0).unwrap().chars[0], b'c');
        assert_eq!(sb.view_row(1).unwrap().chars[0], b'd');
        assert!(sb.view_row(2).is_none());
        assert_eq!(sb.view_top_text().as_deref(), Some("c"));
    }

    #[test]
    fn capacity_drops_the_oldest_row() {
        let mut sb = Scrollback::new(4, 2);
        sb.push(row(b'a'));
        sb.push(row(b'b'));
        sb.push(row(b'c'));
        assert_eq!(sb.len(), 2);
        sb.scroll(2);
        assert_eq!(sb.view_row(0).unwrap().chars[0], b'b');
        assert_eq!(sb.view_row(1).unwrap().chars[0], b'c');
    }

    #[test]
    fn push_anchors_a_scrolled_view_to_the_same_rows() {
        let mut sb = Scrollback::new(4, 8);
        for c in b"abc" {
            sb.push(row(*c));
        }
        sb.scroll(1); // shows "c"
        assert_eq!(sb.view_top_text().as_deref(), Some("c"));
        sb.push(row(b'd')); // new output arrives while reading
        assert_eq!(sb.view(), 2);
        assert_eq!(sb.view_top_text().as_deref(), Some("c"));
        // Returning to the bottom then reading again starts from the live view.
        sb.reset();
        assert_eq!(sb.view(), 0);
    }

    #[test]
    fn clear_forgets_history_and_view() {
        let mut sb = Scrollback::new(4, 8);
        sb.push(row(b'a'));
        sb.scroll(1);
        assert!(sb.clear());
        assert_eq!((sb.len(), sb.view()), (0, 0));
        assert!(!sb.clear());
    }

    #[test]
    fn blank_rows_are_spaces() {
        let r = Row::blank(4, 0x123456);
        assert_eq!(r.chars, vec![b' '; 4]);
        assert_eq!(r.fg, vec![0x123456; 4]);
        assert_eq!(r.width(), 4);
    }

    #[test]
    fn from_cells_copies_exactly_one_row() {
        let r = Row::from_cells(b"abc ", &[1, 2, 3, 4]);
        assert_eq!(r.chars, b"abc ".to_vec());
        assert_eq!(r.fg, vec![1, 2, 3, 4]);
    }
}
