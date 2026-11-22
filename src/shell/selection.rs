//! Выделение текста мышью на консоли.
//!
//! Консоль держит клеточную модель напечатанного (сетка `FramebufferWriter`) и
//! умеет перерисовать одну клетку — вместе это делает выделение возможным без
//! правки пиксельного кода рисования: выделение — диапазон клеток, который
//! перерисовывается **только по фону**.
//!
//! Раньше здесь была инверсия: все выделенные клетки заливались одним цветом
//! (`SELECT_FG` поверх `SELECT_BG`). Для цветного вывода это уничтожало ровно то,
//! что выделение должно сохранить — цветные `#` логотипа fastfetch становились
//! тёмными блоками, ANSI-полоса палитры сливалась в один прямоугольник, а
//! цветные key/value серели. Теперь в фон каждой клетки подмешивается акцент
//! (`selection_math::ACCENT`, та же доля 60%, что у клиента `pagh-terminal`), а
//! глиф рисуется своим прежним цветом байт-в-байт; снятие выделения возвращает
//! и фон клетки из модели. Политика «протяжка сверху донизу — это Area»
//! (все строки целиком) живёт в [`selection_math`], чтобы её можно было
//! доказать host-свойством.
//!
//! Здесь встречаются две системы координат, и весь модуль о том, чтобы их не
//! путать:
//!
//! * **Клетки консоли** — `(col, row)` на экране. То, что сообщает мышь после
//!   деления на размер глифа.
//! * **Индексы строки** — позиции в редактируемой строке, где индекс 0 — первый
//!   символ *после промпта*. То, что понимает редактор.
//!
//! Их связывает промпт: он занимает `prompt_cols` клеток в начале строки, так
//! что клетка `prompt_cols + k` первой строки — это индекс строки `k`. Всё,
//! выделенное левее конца промпта, не является текстом строки и отсекается,
//! потому что выделение, включающее промпт, нельзя было бы удалить из буфера.
//!
//! Текст для буфера обмена читается обратно из сетки, а не из буфера, поэтому
//! выделение, охватывающее вывод, который пользователь прокрутил, всё равно
//! копирует то, что он видит.

use crate::drivers::framebuffer;
use crate::shell::caret::Cell;
use crate::shell::selection_math;

/// Диапазон клеток консоли от `a` до `b` включительно, в порядке чтения.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CellRange {
    pub a: Cell,
    pub b: Cell,
}

impl CellRange {
    /// Упорядочить две точки в порядок чтения (сверху вниз, затем слева
    /// направо), чтобы перетаскивание в любом направлении описывало один диапазон.
    pub fn new(from: Cell, to: Cell) -> Self {
        let ordered = (from.row, from.col) <= (to.row, to.col);
        if ordered {
            CellRange { a: from, b: to }
        } else {
            CellRange { a: to, b: from }
        }
    }

    /// Итерировать клетки диапазона построчно.
    pub fn cells(&self, cols: usize) -> alloc::vec::Vec<Cell> {
        let mut out = alloc::vec::Vec::new();
        if cols == 0 || (self.a.row, self.a.col) > (self.b.row, self.b.col) {
            return out;
        }
        for row in self.a.row..=self.b.row {
            let from = if row == self.a.row { self.a.col } else { 0 };
            let to = if row == self.b.row {
                self.b.col
            } else {
                cols - 1
            };
            for col in from..=to.min(cols - 1) {
                out.push(Cell { col, row });
            }
        }
        out
    }

    /// Выделенный текст, прочитанный обратно из модели консоли.

    /// Строки соединяются `\n`, и у каждой строки обрезаются хвостовые пробелы:
    /// консоль дополняет каждую строку до полной ширины, так что копирование
    /// выделения, кончающегося посреди строки, иначе притащило бы в буфер экран
    /// пробелов. Ведущие пробелы сохраняются — это реальное содержимое (отступы).
    pub fn text(&self, cols: usize) -> alloc::string::String {
        use alloc::string::String;
        let mut out = String::new();
        if cols == 0 {
            return out;
        }
        for row in self.a.row..=self.b.row {
            let from = if row == self.a.row { self.a.col } else { 0 };
            let to = if row == self.b.row {
                self.b.col
            } else {
                cols - 1
            };
            let mut line = framebuffer::console().read_row(row, from, to + 1);
            while line.ends_with(' ') {
                line.pop();
            }
            out.push_str(&line);
            if row != self.b.row {
                out.push('\n');
            }
        }
        out
    }

    /// Тот же диапазон как индексы редактируемой строки.
    ///
    /// `prompt_cols` — ширина промпта в первой строке, `cols` — ширина консоли.
    /// Возвращает `None`, когда диапазон вообще не пересекает текст строки
    /// (выделение целиком внутри промпта или на строке вывода), потому что тогда
    /// нечего удалять или заменять.
    pub fn line_indices(
        &self,
        prompt_cols: usize,
        cols: usize,
        line_len: usize,
    ) -> Option<(usize, usize)> {
        if cols == 0 {
            return None;
        }
        let first = self.a.row * cols + self.a.col;
        let last = self.b.row * cols + self.b.col;
        // Текст строки начинается сразу после промпта и имеет длину `line_len`.
        let start_of_line = prompt_cols; // prompt is printed at the line's first cell
        let end_of_line = start_of_line + line_len;
        let from = first.max(start_of_line);
        let to = (last + 1).min(end_of_line);
        if from >= to {
            return None;
        }
        Some((from - start_of_line, to - start_of_line))
    }
}

/// Колонки `[from, to]`, которые диапазон покрывает на `row`, обрезанные по сетке.
fn row_interval(range: CellRange, row: usize, cols: usize) -> Option<(usize, usize)> {
    if cols == 0 || row < range.a.row || row > range.b.row {
        return None;
    }
    let from = if row == range.a.row { range.a.col } else { 0 };
    let to = if row == range.b.row {
        range.b.col
    } else {
        cols - 1
    };
    if from > to || from >= cols {
        return None;
    }
    Some((from, to.min(cols - 1)))
}

/// `a \ b` для двух включительных интервалов колонок, как до двух сегментов.
fn subtract(a: (usize, usize), b: (usize, usize)) -> [Option<(usize, usize)>; 2] {
    let (af, at) = a;
    let (bf, bt) = b;
    if bf > at || bt < af {
        return [Some(a), None];
    }
    let left = if af < bf { Some((af, bf - 1)) } else { None };
    let right = if bt < at { Some((bt + 1, at)) } else { None };
    [left, right]
}

/// Клетки для восстановления и клетки для подсветки при переходе от `old` к `new`.
///
/// Перетаскивание расширяется по одной клетке; старая реализация
/// перерисовывала весь диапазон (восстановить всё, затем покрасить всё) на каждом
/// событии мыши, что делало большое выделение O(клеток) на пиксель движения мыши
/// и роняло частоту кадров. Вычисление разности построчных интервалов держит
/// каждое движение пропорциональным изменившимся клеткам — обычно одной клетке и
/// максимум двум коротким сегментам строки.
fn paint_diff(
    old: Option<CellRange>,
    new: Option<CellRange>,
    cols: usize,
) -> (alloc::vec::Vec<Cell>, alloc::vec::Vec<Cell>) {
    let mut restore = alloc::vec::Vec::new();
    let mut paint = alloc::vec::Vec::new();
    if cols == 0 {
        return (restore, paint);
    }
    let mut first_row = usize::MAX;
    let mut last_row = 0usize;
    let mut any = false;
    for range in [old, new].into_iter().flatten() {
        first_row = first_row.min(range.a.row);
        last_row = last_row.max(range.b.row);
        any = true;
    }
    if !any {
        return (restore, paint);
    }
    for row in first_row..=last_row {
        let before = old.and_then(|r| row_interval(r, row, cols));
        let after = new.and_then(|r| row_interval(r, row, cols));
        if let (Some(before), Some(after)) = (before, after) {
            for (from, to) in subtract(before, after).into_iter().flatten() {
                for col in from..=to {
                    restore.push(Cell { col, row });
                }
            }
            for (from, to) in subtract(after, before).into_iter().flatten() {
                for col in from..=to {
                    paint.push(Cell { col, row });
                }
            }
        } else if let Some(before) = before {
            for col in before.0..=before.1 {
                restore.push(Cell { col, row });
            }
        } else if let Some(after) = after {
            for col in after.0..=after.1 {
                paint.push(Cell { col, row });
            }
        }
    }
    (restore, paint)
}

/// Выделение мышью в процессе или завершённое.
pub struct Selection {
    anchor: Option<Cell>,
    range: Option<CellRange>,
    /// Точные индексы строки, соответствующие выделению, когда они известны без
    /// вывода. Клавиатурное выделение знает их по построению (`Ctrl+A` выделяет
    /// буфер), так что ему не надо выводить их из клеток — а именно эта
    /// арифметика оказалась ненадёжной.
    line_range: Option<(usize, usize)>,
    painted: bool,
    /// Диапазон, который сейчас подсвечен на экране.
    ///
    /// Перерисовка инкрементальна: перетаскивание расширяет диапазон по клетке,
    /// поэтому перерисовка должна сначала восстановить клетки *предыдущего*
    /// диапазона. Без этой записи сокращающееся перетаскивание (или клик после
    /// него) оставляло выпавшие из диапазона клетки подсвеченными — те самые
    /// «дыры» и залипшие яркие пятна после отпускания мыши.
    painted_range: Option<CellRange>,
}

impl Selection {
    pub const fn new() -> Self {
        Selection {
            anchor: None,
            range: None,
            line_range: None,
            painted: false,
            painted_range: None,
        }
    }

    /// Задать диапазон напрямую (без перетаскивания) для клавиатурного
    /// выделения вместе с покрываемыми индексами строки.
    pub fn set_range(&mut self, range: CellRange, line_range: Option<(usize, usize)>) {
        self.anchor = None;
        self.range = Some(range);
        self.line_range = line_range;
    }

    /// Индексы строки, которые покрывает выделение, когда они известны точно.
    pub fn known_line_range(&self) -> Option<(usize, usize)> {
        self.line_range
    }

    /// Начать перетаскивание в `cell`.
    pub fn begin(&mut self, cell: Cell) {
        self.clear();
        self.anchor = Some(cell);
        self.range = None;
        self.line_range = None;
    }

    /// Расширить перетаскивание до `cell` и сообщить, выделено ли что-то.
    ///
    /// `rows`/`cols` — видимая сетка: по ним работает Area-политика. Протяжка,
    /// начатая в верхней строке и доведённая до нижней, — это «выделить весь
    /// видимый вывод», поэтому её колонки расширяются до полной ширины
    /// (`selection_math::promoted_columns`). Протяжка внутри нескольких строк
    /// остаётся символьной — обрезанные первая/последняя строки там ожидаемы.
    pub fn extend(&mut self, cell: Cell, rows: usize, cols: usize) -> bool {
        let Some(anchor) = self.anchor else {
            return false;
        };
        let mut range = CellRange::new(anchor, cell);
        let (from, to) = selection_math::promoted_columns(
            range.a.col,
            range.b.col,
            range.a.row,
            range.b.row,
            rows,
            cols,
        );
        range.a.col = from;
        range.b.col = to;
        self.range = Some(range);
        self.range.is_some()
    }

    /// Выделить весь видимый текст как Area (все строки целиком).
    ///
    /// Возвращает `false` для пустой сетки: у неё нет «полной ширины», и
    /// подставлять `cols - 1` значило бы underflow. Точки `line_range` нет —
    /// это выделение вывода, а не строки ввода.
    pub fn select_area(&mut self, rows: usize, cols: usize) -> bool {
        if rows == 0 || cols == 0 {
            return false;
        }
        self.anchor = None;
        self.range = Some(CellRange {
            a: Cell { col: 0, row: 0 },
            b: Cell {
                col: cols - 1,
                row: rows - 1,
            },
        });
        self.line_range = None;
        true
    }

    /// Завершить перетаскивание. Возвращает готовый диапазон, если перетаскивание
    /// покрыло больше одной клетки — простой клик не выделение и не должен
    /// оставлять подсветку.
    pub fn finish(&mut self) -> Option<CellRange> {
        self.anchor = None;
        match self.range {
            Some(r) if r.a != r.b => Some(r),
            _ => {
                self.clear();
                None
            }
        }
    }

    pub fn range(&self) -> Option<CellRange> {
        self.range
    }

    /// Идёт ли перетаскивание.
    pub fn is_active(&self) -> bool {
        self.anchor.is_some()
    }

    /// Перекрасить диапазон как выделенный, сначала восстановив то, что покрывал
    /// предыдущий диапазон. Повторные вызовы дёшевы: перетаскивание двигает
    /// подсветку на клетку-две, и обратно перекрашиваются только покинутые клетки.
    ///
    /// `None` у `paint_cell_selected` — восстановление из модели (свой фон
    /// клетки), `Some(ACCENT)` — подмешать акцент в этот фон. Глиф в обоих
    /// случаях остаётся своим.
    pub fn paint(&mut self, cols: usize) {
        if self.painted_range == self.range {
            return;
        }
        let (restore, paint) = paint_diff(self.painted_range, self.range, cols);
        for cell in restore {
            framebuffer::console().paint_cell_selected(cell.col, cell.row, None);
        }
        for cell in paint {
            framebuffer::console().paint_cell_selected(
                cell.col,
                cell.row,
                Some(selection_math::ACCENT),
            );
        }
        self.painted_range = self.range;
        self.painted = self.painted_range.is_some();
    }

    /// Вернуть подсвеченный диапазон к обычным цветам консоли.
    pub fn unpaint(&mut self) {
        let cols = framebuffer::console().text_grid().0;
        self.unpaint_painted(cols);
        self.painted = false;
    }

    /// Восстановить ровно те клетки, что подсвечены прямо сейчас.
    fn unpaint_painted(&mut self, cols: usize) {
        let Some(range) = self.painted_range.take() else {
            return;
        };
        for cell in range.cells(cols) {
            // `None`: каждая клетка возвращается к цвету, которым была
            // нарисована (и глиф, и фон), так что отпускание выделения поверх
            // зелёного промпта оставляет его зелёным, а поверх ANSI-арта —
            // восстанавливает его фон.
            framebuffer::console().paint_cell_selected(cell.col, cell.row, None);
        }
        self.painted = false;
    }

    /// Сбросить выделение, восстановив покрытые клетки.
    pub fn clear(&mut self) {
        self.unpaint();
        self.anchor = None;
        self.range = None;
        self.line_range = None;
        self.painted = false;
    }
}

impl Default for Selection {
    fn default() -> Self {
        Self::new()
    }
}
