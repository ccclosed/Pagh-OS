//! `nano+` — небольшой полноэкранный текстовый редактор для framebuffer-shell.
//!
//! По сравнению с классическим nano он добавляет номера строк, постоянный
//! status bar, ограниченные undo/redo, инкрементальный поиск, переход к строке,
//! горизонтальную прокрутку и защиту от выхода с несохранённым файлом. Ввод
//! остаётся ASCII, потому что раскладка PS/2-клавиатуры — ASCII; существующие
//! байты UTF-8 показываются lossy, а не режутся небезопасно.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::keys::{Decoder, KeyEvent};
use super::nano_config::NanoConfig;
use crate::drivers::{cursor, framebuffer};
use crate::vfs::{VfsError, VfsNode};

const MAX_FILE: usize = 64 * 1024;
const MAX_LINE: usize = 4096;
const UNDO_LIMIT: usize = 32;
// Геометрия клеток приходит из консоли и больше ниоткуда. Здесь были `8`/`16`,
// пока консоль рисовала глифы 8x16; после перехода на клетку 8x18 у Fira Code
// редактор раскладывал каждую строку по 16 пикселей и залезал каждой строкой на
// два пикселя в следующую (а каретка была на два пикселя короче).
const CHAR_W: usize = framebuffer::CHAR_WIDTH;
const CHAR_H: usize = framebuffer::CHAR_HEIGHT;
/// Полоса заголовка: одна строка глифов плюс небольшой отступ.
const HEADER_H: usize = CHAR_H + 4;
/// Подвал: две текстовые строки (статус, подсказки клавиш) плюс небольшой отступ.
const FOOTER_H: usize = 2 * CHAR_H + 2;
/// Ширина сайдбара в символьных клетках.
const TREE_COLS: usize = 24;

#[derive(Clone)]
struct Snapshot {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PromptMode {
    Search,
    Goto,
    ReplaceFind,
    ReplaceWith,
}

/// Позиция в документе: строка и байтовая колонка в ней.
///
/// Колонки — «сразу за последним символом» на `line.len()`, так что выделение
/// может кончаться в конце строки, не указывая на символ.
#[derive(Clone, Copy, PartialEq, Eq)]
struct SelPoint {
    row: usize,
    col: usize,
}

/// Текстовое выделение в координатах документа.
///
/// `anchor` — где началось перетаскивание (или один конец клавиатурного
/// выделения) и стоит на месте; `head` — движущийся конец. Оба хранятся в
/// порядке создания пользователем и нормализуются [`Self::ordered`] везде, где
/// важен порядок чтения, так что перетаскивание в любую сторону выделяет один и
/// тот же текст.
#[derive(Clone, Copy)]
struct Selection {
    anchor: Option<SelPoint>,
    head: Option<SelPoint>,
}

impl Selection {
    const fn new() -> Self {
        Selection {
            anchor: None,
            head: None,
        }
    }

    /// Два конца в порядке чтения или `None`, когда выделения нет.
    fn ordered(&self) -> Option<(SelPoint, SelPoint)> {
        let (a, h) = (self.anchor?, self.head?);
        if (a.row, a.col) <= (h.row, h.col) {
            Some((a, h))
        } else {
            Some((h, a))
        }
    }

    fn is_empty(&self) -> bool {
        match self.ordered() {
            Some((a, b)) => a == b,
            None => true,
        }
    }

    fn clear(&mut self) {
        self.anchor = None;
        self.head = None;
    }
}

/// Куда попал клик мыши.
enum Hit {
    /// Позиция документа в текстовой панели.
    Text(SelPoint),
    /// Индекс строки в видимом списке сайдбара.
    Tree(usize),
}

pub struct Editor {
    path: String,
    lines: Vec<String>,
    row: usize,
    col: usize,
    top: usize,
    left: usize,
    dirty: bool,
    status: String,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    prompt: Option<(PromptMode, String)>,
    search_hit: Option<(usize, usize, usize)>,
    quit_armed: bool,
    config: NanoConfig,
    clipboard: Vec<String>,
    replace_needle: Option<String>,
    /// Каталог, который сайдбар должен открыть, когда редактор стартует с ним.
    tree_root: Option<String>,
    /// Буфер на момент последнего save/open, поэтому «изменён ли?» решается
    /// самим текстом, а не «было ли нажатие».
    ///
    /// Счётчик правок помечал файл грязным после набора символа и его удаления:
    /// буфер был идентичен, флаг — нет. Сравнение строк — единственный ответ,
    /// который не может соврать.
    clean: Vec<String>,
    /// Активное текстовое выделение (перетаскивание мышью или `^A`).
    sel: Selection,
    /// Буфер обмена для скопированного/вырезанного *выделения*, в отличие от
    /// построчного буфера, который используют `^C`/`^K`, когда ничего не выделено.
    sel_clip: String,
}

impl Editor {
    fn new(path: &str, text: &str, config: NanoConfig) -> Self {
        let mut lines: Vec<String> = text.split('\n').map(sanitize_line).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        let clean = lines.clone();
        Self {
            path: path.to_string(),
            lines,
            clean,
            row: 0,
            col: 0,
            top: 0,
            left: 0,
            dirty: false,
            status: "Ready".to_string(),
            undo: Vec::new(),
            redo: Vec::new(),
            prompt: None,
            search_hit: None,
            quit_armed: false,
            config,
            clipboard: Vec::new(),
            tree_root: None,
            replace_needle: None,
            sel: Selection::new(),
            sel_clip: String::new(),
        }
    }

    /// Направить редактор на другой файл, сохранив конфигурацию.
    ///
    /// История undo намеренно сбрасывается: она принадлежит прошлому файлу, и её
    /// сохранение позволило бы undo в одном файле переписать другой.
    fn reload(&mut self, path: &str, text: &str) {
        self.path = path.to_string();
        self.lines = {
            let mut l: Vec<String> = text.split('\n').map(sanitize_line).collect();
            if l.is_empty() {
                l.push(String::new());
            }
            l
        };
        self.row = 0;
        self.col = 0;
        self.top = 0;
        self.left = 0;
        self.mark_clean();
        self.undo.clear();
        self.redo.clear();
        self.search_hit = None;
        self.quit_armed = false;
        self.sel.clear();
        self.sel_clip.clear();
        self.status = alloc::format!("opened {}", path);
    }

    /// Пересчитать `dirty` из буфера — единственное честное определение «изменён».
    fn refresh_dirty(&mut self) {
        self.dirty = self.lines != self.clean;
        if !self.dirty {
            // Вернулись к сохранённому тексту: терять нечего, так что двухшаговой
            // защите выхода тоже нечего защищать.
            self.quit_armed = false;
        }
    }

    /// Запомнить текущий буфер как сохранённое состояние (open, reload, save).
    fn mark_clean(&mut self) {
        self.clean = self.lines.clone();
        self.dirty = false;
        self.quit_armed = false;
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            lines: self.lines.clone(),
            row: self.row,
            col: self.col,
        }
    }
    fn checkpoint(&mut self) {
        if self.undo.len() == UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.undo.push(self.snapshot());
        self.redo.clear();
        // `dirty` решает текст, а не нажатие: вызывающий меняет и затем вызывает
        // `refresh_dirty`.
        self.quit_armed = false;
    }
    fn restore(&mut self, s: Snapshot) {
        self.lines = s.lines;
        self.row = s.row.min(self.lines.len().saturating_sub(1));
        self.col = s.col.min(self.lines[self.row].len());
        self.ensure_visible();
    }
    fn undo(&mut self) {
        if let Some(s) = self.undo.pop() {
            let cur = self.snapshot();
            self.redo.push(cur);
            self.restore(s);
            self.refresh_dirty();
            self.status = "Undo".into();
        } else {
            self.status = "Nothing to undo".into();
        }
    }
    fn redo(&mut self) {
        if let Some(s) = self.redo.pop() {
            let cur = self.snapshot();
            self.undo.push(cur);
            self.restore(s);
            self.refresh_dirty();
            self.status = "Redo".into();
        } else {
            self.status = "Nothing to redo".into();
        }
    }
    fn insert(&mut self, ch: char) {
        if self.lines[self.row].len() >= MAX_LINE {
            self.status = "Line length limit reached".into();
            return;
        }
        self.checkpoint();
        self.lines[self.row].insert(self.col, ch);
        self.col += ch.len_utf8();
        if self.config.wrap_column > 0 && self.col >= self.config.wrap_column {
            self.newline_without_checkpoint();
        }
        self.ensure_visible();
        self.refresh_dirty();
    }
    fn newline_without_checkpoint(&mut self) {
        let indent = if self.config.auto_indent {
            self.lines[self.row]
                .bytes()
                .take_while(|b| *b == b' ')
                .count()
        } else {
            0
        };
        let tail = self.lines[self.row].split_off(self.col);
        self.row += 1;
        let mut next = String::new();
        for _ in 0..indent {
            next.push(' ')
        }
        next.push_str(&tail);
        self.lines.insert(self.row, next);
        self.col = indent;
        self.ensure_visible();
    }
    fn newline(&mut self) {
        self.checkpoint();
        self.newline_without_checkpoint();
        self.refresh_dirty();
    }
    fn backspace(&mut self) {
        if self.col > 0 {
            self.checkpoint();
            self.col -= 1;
            self.lines[self.row].remove(self.col);
        } else if self.row > 0 {
            self.checkpoint();
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            if self.col + current.len() <= MAX_LINE {
                self.lines[self.row].push_str(&current);
            } else {
                self.lines.insert(self.row + 1, current);
                self.row += 1;
                self.col = 0;
            }
        }
        self.ensure_visible();
        self.refresh_dirty();
    }
    fn delete(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.checkpoint();
            self.lines[self.row].remove(self.col);
        } else if self.row + 1 < self.lines.len()
            && self.lines[self.row].len() + self.lines[self.row + 1].len() <= MAX_LINE
        {
            self.checkpoint();
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
        self.refresh_dirty();
    }
    fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len()
        }
        self.ensure_visible();
    }
    fn right(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.col += 1
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0
        }
        self.ensure_visible();
    }
    fn up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.lines[self.row].len())
        }
        self.ensure_visible();
    }
    fn down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].len())
        }
        self.ensure_visible();
    }
    fn copy_line(&mut self) {
        self.clipboard = alloc::vec![self.lines[self.row].clone()];
        self.status = "Line copied".into();
    }
    fn cut_line(&mut self) {
        self.checkpoint();
        self.clipboard = alloc::vec![self.lines.remove(self.row)];
        if self.lines.is_empty() {
            self.lines.push(String::new())
        }
        self.row = self.row.min(self.lines.len() - 1);
        self.col = self.col.min(self.lines[self.row].len());
        self.status = "Line cut".into();
        self.ensure_visible();
        self.refresh_dirty();
    }
    fn paste_line(&mut self) {
        if self.clipboard.is_empty() {
            self.status = "Clipboard empty".into();
            return;
        }
        self.checkpoint();
        let at = self.row + 1;
        for (i, line) in self.clipboard.clone().into_iter().enumerate() {
            self.lines.insert(at + i, line)
        }
        self.row = at;
        self.col = 0;
        self.status = "Pasted".into();
        self.ensure_visible();
        self.refresh_dirty();
    }
    fn ensure_visible(&mut self) {
        let (w, h) = framebuffer::dimensions();
        let rows = h.saturating_sub(HEADER_H + FOOTER_H) / CHAR_H;
        let cols = w / CHAR_W;
        let gutter = if self.config.line_numbers { 7 } else { 0 };
        if self.row < self.top {
            self.top = self.row
        } else if rows > 0 && self.row >= self.top + rows {
            self.top = self.row + 1 - rows
        }
        if self.col < self.left {
            self.left = self.col
        } else if cols > gutter && self.col >= self.left + cols - gutter {
            self.left = self.col + 1 - (cols - gutter)
        }
    }
    fn begin_prompt(&mut self, mode: PromptMode) {
        self.prompt = Some((mode, String::new()));
        self.status = match mode {
            PromptMode::Search => "Search",
            PromptMode::Goto => "Go to line",
            PromptMode::ReplaceFind => "Replace: find",
            PromptMode::ReplaceWith => "Replace with",
        }
        .into();
    }
    fn commit_prompt(&mut self) {
        let Some((mode, value)) = self.prompt.take() else {
            return;
        };
        match mode {
            PromptMode::Search => self.find_next(&value),
            PromptMode::Goto => match value.parse::<usize>() {
                Ok(n) if n > 0 => {
                    self.row = (n - 1).min(self.lines.len() - 1);
                    self.col = self.col.min(self.lines[self.row].len());
                    self.status = format!("Line {}", self.row + 1);
                    self.ensure_visible();
                }
                _ => self.status = "Invalid line number".into(),
            },
            PromptMode::ReplaceFind => {
                if value.is_empty() {
                    self.status = "Empty search".into()
                } else {
                    self.replace_needle = Some(value);
                    self.begin_prompt(PromptMode::ReplaceWith)
                }
            }
            PromptMode::ReplaceWith => {
                let Some(needle) = self.replace_needle.take() else {
                    return;
                };
                let count: usize = self.lines.iter().map(|l| l.matches(&needle).count()).sum();
                if count > 0 {
                    self.checkpoint();
                    for line in &mut self.lines {
                        *line = line.replace(&needle, &value)
                    }
                    self.refresh_dirty();
                }
                self.status = format!("Replaced {} occurrence(s)", count);
            }
        }
    }
    fn find_next(&mut self, needle: &str) {
        if needle.is_empty() {
            self.status = "Empty search".into();
            return;
        }
        let start_row = self.row;
        let start_col = (self.col + 1).min(self.lines[self.row].len());
        for pass in 0..2 {
            let from = if pass == 0 { start_row } else { 0 };
            let to = if pass == 0 {
                self.lines.len()
            } else {
                start_row + 1
            };
            for r in from..to {
                let off = if pass == 0 && r == start_row {
                    start_col
                } else {
                    0
                };
                if let Some(i) = self.lines[r][off..].find(needle) {
                    let c = off + i;
                    self.row = r;
                    self.col = c;
                    self.search_hit = Some((r, c, needle.len()));
                    self.status = format!("Found '{}'", needle);
                    self.ensure_visible();
                    return;
                }
            }
        }
        self.status = format!("'{}' not found", needle);
    }
    fn handle_prompt(&mut self, event: KeyEvent) {
        match event {
            KeyEvent::Char(c) => {
                if let Some((_, s)) = &mut self.prompt {
                    if s.len() < 64 {
                        s.push(c)
                    }
                }
            }
            KeyEvent::Backspace => {
                if let Some((_, s)) = &mut self.prompt {
                    s.pop();
                }
            }
            KeyEvent::Enter => self.commit_prompt(),
            KeyEvent::Escape | KeyEvent::Ctrl('q') => {
                self.prompt = None;
                self.status = "Cancelled".into();
            }
            _ => {}
        }
    }
    fn text(&self) -> String {
        self.lines.join("\n")
    }

    // ── выделение ────────────────────────────────────────────────────────

    /// Выделенный диапазон как `(start, end)` в порядке чтения или `None` для
    /// пустого/отсутствующего выделения. `start` включается, `end` — позиция сразу
    /// за последним выделенным символом той же строки или символьная позиция в
    /// последней строке для многострочного выделения.
    fn sel_bounds(&self) -> Option<(SelPoint, SelPoint)> {
        match self.sel.ordered() {
            Some((a, b)) if a != b => Some((a, b)),
            _ => None,
        }
    }

    fn has_selection(&self) -> bool {
        self.sel_bounds().is_some()
    }

    fn clear_selection(&mut self) {
        self.sel.clear();
    }

    fn begin_select(&mut self, p: SelPoint) {
        self.sel.anchor = Some(p);
        self.sel.head = Some(p);
    }

    fn extend_select(&mut self, p: SelPoint) {
        self.sel.head = Some(p);
    }

    /// Переместить каретку в `p`, с клампом в документ и прокруткой в вид.
    /// Возвращает, прокрутился ли viewport (тогда вызывающий перерисовывает кадр).
    fn set_cursor(&mut self, p: SelPoint) -> bool {
        let (top, left) = (self.top, self.left);
        self.row = p.row.min(self.lines.len().saturating_sub(1));
        self.col = p.col.min(self.lines[self.row].len());
        self.ensure_visible();
        self.top != top || self.left != left
    }

    /// Выделить весь буфер и поставить каретку в его конец.
    fn select_all(&mut self) {
        let last = self.lines.len() - 1;
        let end = self.lines[last].len();
        let col = end.saturating_sub(1);
        self.sel.anchor = Some(SelPoint { row: 0, col: 0 });
        self.sel.head = Some(SelPoint { row: last, col });
        self.row = last;
        self.col = col;
        self.ensure_visible();
        self.status = alloc::format!("Selected {} line(s)", self.lines.len());
    }

    /// Выделенный текст или `None`, когда ничего не выделено.
    ///
    /// Оба конца — *символьные* позиции (см. [`Selection`]): перетаскивание от
    /// строки к строке включает символ под указателем, что и показывает подсветка.
    fn sel_text(&self) -> Option<String> {
        let (a, b) = self.sel_bounds()?;
        let mut out = String::new();
        if a.row == b.row {
            let line = &self.lines[a.row];
            let ac = a.col.min(line.len());
            let bc = (b.col + 1).min(line.len());
            out.push_str(&line[ac..bc]);
        } else {
            let first = &self.lines[a.row];
            out.push_str(&first[a.col.min(first.len())..]);
            for r in (a.row + 1)..b.row {
                out.push('\n');
                out.push_str(&self.lines[r]);
            }
            out.push('\n');
            let last = &self.lines[b.row];
            let bc = (b.col + 1).min(last.len());
            out.push_str(&last[..bc]);
        }
        Some(out)
    }

    /// Удалить выделенный текст, сбросив выделение и оставив каретку в его начале.
    /// Возвращает, было ли что удалять.
    fn delete_selection(&mut self) -> bool {
        let Some((a, b)) = self.sel_bounds() else {
            self.sel.clear();
            return false;
        };
        self.checkpoint();
        // Оба конца — включительные символьные позиции: в одной строке удаляется
        // `[a.col, b.col]`, многострочный диапазон удаляет хвост первой строки,
        // целиком средние строки и начало последней, склеивая остатки в одну строку.
        if a.row == b.row {
            let line = &mut self.lines[a.row];
            let ac = a.col.min(line.len());
            let bc = (b.col + 1).min(line.len());
            line.replace_range(ac..bc, "");
            self.row = a.row;
            self.col = ac;
        } else {
            let ac = a.col.min(self.lines[a.row].len());
            let bc = (b.col + 1).min(self.lines[b.row].len());
            let tail: String = self.lines[b.row][bc..].into();
            self.lines[a.row].truncate(ac);
            self.lines[a.row].push_str(&tail);
            self.lines.drain(a.row + 1..=b.row);
            self.row = a.row;
            self.col = ac;
        }
        self.sel.clear();
        self.ensure_visible();
        self.refresh_dirty();
        true
    }

    /// Вставить `text` в каретку (newlines разбивают строки). Используется вставками;
    /// выделение сначала удаляет вызывающий.
    fn insert_text(&mut self, text: &str) {
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                self.newline();
            }
            for ch in part.chars() {
                self.insert(ch);
            }
        }
    }
}

fn sanitize_line(s: &str) -> String {
    s.chars()
        .take(MAX_LINE)
        .map(|c| {
            if c == '\t' {
                ' '
            } else if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect()
}

fn open_or_create(path: &str) -> Result<(Arc<dyn VfsNode>, String), VfsError> {
    match crate::vfs::lookup_path(path) {
        Ok(node) if !node.is_directory() => {
            let size = (node.size() as usize).min(MAX_FILE);
            let mut bytes = alloc::vec![0u8;size];
            let n = node.read(0, &mut bytes)?;
            bytes.truncate(n);
            Ok((node, String::from_utf8_lossy(&bytes).into_owned()))
        }
        Ok(_) => Err(VfsError::InvalidArgument),
        Err(_) => {
            let trimmed = path.trim_end_matches('/');
            let i = trimmed.rfind('/').ok_or(VfsError::InvalidArgument)?;
            let leaf = &trimmed[i + 1..];
            if leaf.is_empty() {
                return Err(VfsError::InvalidArgument);
            }
            let parent = if i == 0 { "/" } else { &trimmed[..i] };
            let dir = crate::vfs::lookup_path(parent)?;
            let node = dir.create_file(leaf)?;
            Ok((node, String::new()))
        }
    }
}

fn write_path(path: &str, data: &[u8]) -> Result<(), VfsError> {
    let t = path.trim_end_matches('/');
    let i = t.rfind('/').ok_or(VfsError::InvalidArgument)?;
    let parent = if i == 0 { "/" } else { &t[..i] };
    let leaf = &t[i + 1..];
    let dir = crate::vfs::lookup_path(parent)?;
    let file = match dir.lookup(leaf) {
        Ok(n) => n,
        Err(VfsError::NotFound) => dir.create_file(leaf)?,
        Err(e) => return Err(e),
    };
    file.truncate(0)?;
    if !data.is_empty() && file.write(0, data)? != data.len() {
        return Err(VfsError::IoError);
    }
    file.sync();
    Ok(())
}
fn save(editor: &mut Editor, node: &Arc<dyn VfsNode>) -> Result<(), VfsError> {
    if editor.config.backup && node.size() > 0 {
        let size = (node.size() as usize).min(MAX_FILE);
        let mut old = alloc::vec![0u8;size];
        let n = node.read(0, &mut old)?;
        old.truncate(n);
        write_path(&format!("{}.bak", editor.path), &old)?;
    }
    let text = if editor.config.trim_trailing {
        editor
            .lines
            .iter()
            .map(|l| l.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        editor.text()
    };
    if text.len() > MAX_FILE {
        return Err(VfsError::InvalidArgument);
    }
    node.truncate(0)?;
    if !text.is_empty() {
        let n = node.write(0, text.as_bytes())?;
        if n != text.len() {
            return Err(VfsError::IoError);
        }
    }
    node.sync();
    editor.mark_clean();
    editor.status = format!("Saved {} bytes", text.len());
    Ok(())
}

fn clipped_ascii(s: &str, start: usize, width: usize, spaces: bool) -> String {
    s.bytes()
        .skip(start)
        .take(width)
        .map(|b| {
            if b == b' ' && spaces {
                '.'
            } else if (32..=126).contains(&b) {
                b as char
            } else {
                '?'
            }
        })
        .collect()
}

// ─── рендеринг ────────────────────────────────────────────────────────────
//
// Три правила держат пути рисования честными:
//
// * **Damage, а не кадры.** Нажатие меняет одну строку и каретку. Перерисовка
//   всей поверхности для него измерялась десятками миллисекунд под TCG (полный
//   кадр — ~10 000 глифов = ~1.4 M записей пикселей) и делала набор вязким.
//   Каждая точка входа ниже перерисовывает наименьшую область, которая могла
//   измениться; dirty-rect flush консоли затем копирует только эту область на
//   устройство.
// * **Стрелка показывается заново после каждого кадра.** Полноэкранные рисовальщики
//   скрывают программный курсор мыши перед рисованием и должны вернуть его поверх
//   готового кадра — путь, который скрыл и не показал, оставляет указатель
//   невидимым, пока редактор открыт.
// * **RAM, затем ровно один flush.** Пиксели доходят до экрана только через
//   `framebuffer::flush`; путь, вернувшийся без него, оставляет работу в shadow-буфере
//   (так и пропала каретка: мигание писало полосу, но никогда её не пушило).

/// Пиксельная геометрия кадра редактора, выводимая один раз на отрисовку.
struct Layout {
    w: usize,
    h: usize,
    /// Ширина сайдбара в клетках (0, когда сайдбар скрыт).
    tree_cols: usize,
    /// Текстовые колонки рядом с сайдбаром.
    cols: usize,
    /// Видимые строки редактора.
    rows: usize,
    /// Ширина gutter номеров строк в клетках (0, когда номера выключены).
    gutter: usize,
    /// Верхний пиксель полосы подвала.
    foot: usize,
}

/// Видимые строки редактора для экрана высотой `h` пикселей.
fn body_rows(h: usize) -> usize {
    h.saturating_sub(HEADER_H + FOOTER_H) / CHAR_H
}

fn layout(editor: &Editor, tree: Option<&super::tree::FileTree>) -> Option<Layout> {
    let (w, h) = framebuffer::dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let tree_cols = if tree.is_some() { TREE_COLS } else { 0 };
    Some(Layout {
        w,
        h,
        tree_cols,
        cols: (w / CHAR_W).saturating_sub(tree_cols),
        rows: body_rows(h),
        gutter: if editor.config.line_numbers { 7 } else { 0 },
        foot: h.saturating_sub(FOOTER_H),
    })
}

/// Полоса заголовка сверху: путь и маркер изменений.
fn paint_header(fb: &mut framebuffer::FramebufferWriter, editor: &Editor, l: &Layout) {
    let blue = editor.config.palette()[4];
    fb.fill_rect(0, 0, l.w, HEADER_H, blue);
    // Несохранённая работа сказана в заголовке словами, а не одинокой звёздочкой:
    // «сделала ли моя правка что-нибудь» должно отвечаться с одного взгляда, а `*`
    // рядом с длинным путём — нет.
    let title = if editor.dirty {
        format!(" nano+  {}   [modified - ^S saves]", editor.path)
    } else {
        format!(" nano+  {}", editor.path)
    };
    fb.draw_text_px(
        6,
        3,
        &clipped_ascii(&title, 0, l.cols.saturating_sub(1), false),
        if editor.dirty { 0xFFE066 } else { 0xFFFFFF },
        blue,
    );
}

/// Сайдбар: дерево проекта с корнем в каталоге редактируемого файла.
fn paint_tree(
    fb: &mut framebuffer::FramebufferWriter,
    editor: &Editor,
    tree: &super::tree::FileTree,
    l: &Layout,
) {
    let [_, surface, text, muted, _, _, _, _] = editor.config.palette();
    let tw = l.tree_cols * CHAR_W;
    fb.fill_rect(
        0,
        HEADER_H,
        tw,
        l.h.saturating_sub(HEADER_H + FOOTER_H),
        surface,
    );
    let header = alloc::format!(" {} ", tree.root());
    fb.draw_text_px(
        2,
        HEADER_H + 2,
        &clipped_ascii(&header, 0, l.tree_cols.saturating_sub(1), false),
        muted,
        surface,
    );
    let list_y = HEADER_H + CHAR_H + 4;
    let list_rows = l.rows.saturating_sub(1);
    for vr in 0..list_rows {
        let i = tree.top + vr;
        let Some(row) = tree.rows().get(i) else {
            break;
        };
        let y = list_y + vr * CHAR_H;
        // Отступ на глубину и маркер, который сразу говорит open/closed.
        let indent = row.depth * 2;
        let mark = if row.is_dir {
            if row.expanded {
                "v "
            } else {
                "> "
            }
        } else {
            "  "
        };
        let text_line = alloc::format!("{}{}{}", " ".repeat(indent), mark, row.name);
        let (fg, bg) = if i == tree.cursor {
            (0x000000, 0xCCCCCC)
        } else if row.is_dir {
            (text, surface)
        } else {
            (muted, surface)
        };
        fb.draw_text_px(
            2,
            y,
            &clipped_ascii(&text_line, 0, l.tree_cols.saturating_sub(1), false),
            fg,
            bg,
        );
    }
}

/// Перерисовать одну строку редактора (фон, gutter, текст, попадание поиска).
///
/// Заливает собственный фон: у инкрементальных вызывающих нет полной заливки
/// кадра позади, так что хвост только что укоротившейся строки надо стереть здесь.
fn paint_editor_row(
    fb: &mut framebuffer::FramebufferWriter,
    editor: &Editor,
    l: &Layout,
    r: usize,
    vr: usize,
) {
    let [bg, surface, text, muted, blue, _, _, select] = editor.config.palette();
    let y = HEADER_H + vr * CHAR_H;
    let x0 = l.tree_cols * CHAR_W;
    fb.fill_rect(x0, y, l.w.saturating_sub(x0), CHAR_H, bg);
    if r >= editor.lines.len() {
        fb.draw_text_px(x0 + 8, y, "~", blue, bg);
        return;
    }
    if editor.config.line_numbers {
        fb.fill_rect(x0, y, l.gutter * CHAR_W, CHAR_H, surface);
        let num = format!("{:>5} ", r + 1);
        fb.draw_text_px(x0, y, &num, muted, surface);
    }
    let visible = clipped_ascii(
        &editor.lines[r],
        editor.left,
        l.cols.saturating_sub(l.gutter),
        editor.config.show_whitespace,
    );
    let tx = x0 + l.gutter * CHAR_W;
    match editor.search_hit {
        Some((hr, hc, hlen))
            if hr == r && hc >= editor.left && hc < editor.left + visible.len() =>
        {
            let before = clipped_ascii(
                &editor.lines[r],
                editor.left,
                hc - editor.left,
                editor.config.show_whitespace,
            );
            let hit = clipped_ascii(&editor.lines[r], hc, hlen, editor.config.show_whitespace);
            fb.draw_text_px(tx, y, &visible, text, bg);
            fb.draw_text_px(tx + before.len() * CHAR_W, y, &hit, 0xFFFFFF, select);
        }
        _ => fb.draw_text_px(tx, y, &visible, text, bg),
    }
    // Подсветка выделения идёт последней, поверх текста и поверх попадания
    // поиска: то, что пользователь выделил, — то, с чем он будет действовать. У
    // консоли нет blending, поэтому здесь перекрашиваются покрытые клетки
    // инверсией (чёрным по светлому), а не тонированием.
    if let Some((c0, c1)) = selected_cols(editor, r) {
        let vis_last = editor.left + visible.len().saturating_sub(1);
        if !visible.is_empty() && c0 <= vis_last && c1 >= editor.left {
            let from = c0.max(editor.left);
            let to = c1.min(vis_last);
            if from <= to {
                let seg = clipped_ascii(
                    &editor.lines[r],
                    from,
                    to - from + 1,
                    editor.config.show_whitespace,
                );
                fb.draw_text_px(
                    tx + (from - editor.left) * CHAR_W,
                    y,
                    &seg,
                    0x000000,
                    select,
                );
            }
        }
    }
}

/// Колонки документа строки `r`, покрытые выделением, включительно, или `None`,
/// когда в этой строке нет выделенных символов.
///
/// Концы выделения — символьные позиции (перетаскивание включает символ под
/// указателем); последний символ последней строки многострочного выделения —
/// `b.col`, а строки между ними выделены целиком. В пустой строке подсвечивать
/// нечего.
fn selected_cols(editor: &Editor, r: usize) -> Option<(usize, usize)> {
    let (a, b) = editor.sel_bounds()?;
    if r < a.row || r > b.row {
        return None;
    }
    let line_len = editor.lines.get(r).map(|l| l.len()).unwrap_or(0);
    if line_len == 0 {
        return None;
    }
    let from = if r == a.row { a.col } else { 0 };
    let to = if r == b.row { b.col } else { line_len - 1 };
    let to = to.min(line_len - 1);
    if from > to {
        return None;
    }
    Some((from, to))
}

/// Отобразить пиксельную позицию в позицию документа, когда она над текстовой панелью.
///
/// Экран — клетки [`CHAR_W`]×[`CHAR_H`] начиная ниже заголовка; сайдбар и gutter
/// номеров строк вычитаются до того, как колонка переводится смещением
/// горизонтальной прокрутки. Клик за концом строки ставит каретку в её конец,
/// никогда — на несуществующий символ.
fn mouse_text_point(
    editor: &Editor,
    tree: Option<&super::tree::FileTree>,
    mx: usize,
    my: usize,
) -> Option<SelPoint> {
    let l = layout(editor, tree)?;
    if my < HEADER_H || my >= l.foot {
        return None;
    }
    let vr = (my - HEADER_H) / CHAR_H;
    if vr >= l.rows {
        return None;
    }
    let x0 = l.tree_cols * CHAR_W;
    if mx < x0 {
        return None;
    }
    let cell = (mx - x0) / CHAR_W;
    let col_in_text = cell.saturating_sub(l.gutter);
    let row = (editor.top + vr).min(editor.lines.len().saturating_sub(1));
    let col = (editor.left + col_in_text).min(editor.lines[row].len());
    Some(SelPoint { row, col })
}

/// Что попадает под клик в `(mx, my)`: позиция документа, строка сайдбара или ничего.
fn mouse_target(
    editor: &Editor,
    tree: Option<&super::tree::FileTree>,
    mx: usize,
    my: usize,
) -> Option<Hit> {
    let l = layout(editor, tree)?;
    if my < HEADER_H || my >= l.foot {
        return None;
    }
    if let Some(t) = tree {
        if mx < l.tree_cols * CHAR_W {
            // Те же строки, что перечисляет `paint_tree`: строка корня, затем по
            // строке на запись.
            let list_y = HEADER_H + CHAR_H + 4;
            if my < list_y {
                return None;
            }
            let vr = (my - list_y) / CHAR_H;
            let i = t.top + vr;
            if i < t.rows().len() {
                return Some(Hit::Tree(i));
            }
            return None;
        }
    }
    mouse_text_point(editor, tree, mx, my).map(Hit::Text)
}

/// Нарисовать полосу подвала: строку статуса (или активный prompt) и подсказки клавиш.
fn paint_footer(fb: &mut framebuffer::FramebufferWriter, editor: &Editor, l: &Layout) {
    let [_, surface, text, _, _, green, red, _] = editor.config.palette();
    let foot = l.foot;
    fb.fill_rect(0, foot, l.w, FOOTER_H, surface);
    let status = if let Some((mode, value)) = &editor.prompt {
        format!(
            "{}: {}_",
            if *mode == PromptMode::Search {
                "Search"
            } else {
                "Line"
            },
            value
        )
    } else {
        format!(
            "{}   Ln {}, Col {}",
            editor.status,
            editor.row + 1,
            editor.col + 1
        )
    };
    fb.draw_text_px(
        6,
        foot + 2,
        &clipped_ascii(&status, 0, l.cols.saturating_sub(1), false),
        if editor.status.starts_with("Error") {
            red
        } else {
            green
        },
        surface,
    );
    let dirty_hint = if editor.dirty {
        "^S Save (modified)  ^Q Quit"
    } else {
        "^S Save  ^Q Quit"
    };
    fb.draw_text_px(
        6,
        foot + CHAR_H + 2,
        &alloc::format!(
            "{}  ^A Sel  ^C Copy  ^X Cut  ^B Files  ^F Find  ^K Line  ^U Paste",
            dirty_hint
        ),
        if editor.dirty { 0xFFE066 } else { text },
        surface,
    );
}

/// Пиксельная позиция каретки или `None`, когда она за экраном или подвалом
/// владеет prompt.
fn caret_position(editor: &Editor, l: &Layout) -> Option<(usize, usize)> {
    if editor.prompt.is_some() || editor.row < editor.top || editor.col < editor.left {
        return None;
    }
    let vr = editor.row - editor.top;
    if vr >= l.rows {
        return None;
    }
    let cx = (l.tree_cols + l.gutter + editor.col - editor.left) * CHAR_W;
    if cx + 1 >= l.w {
        return None;
    }
    Some((cx, HEADER_H + vr * CHAR_H))
}

/// Нарисовать полосу каретки поверх только что перерисованной строки.
///
/// Рисовать нужно только фазу «включена»: строка под ней перерисована
/// непосредственно перед этим, что и восстанавливает символ под полосой.
fn paint_caret_bar(
    fb: &mut framebuffer::FramebufferWriter,
    editor: &Editor,
    caret_on: bool,
    l: &Layout,
) {
    let Some((cx, cy)) = caret_position(editor, l) else {
        return;
    };
    if caret_on {
        fb.fill_rect(cx, cy, 2, CHAR_H, super::caret::CARET_COLOR);
    }
}

/// Перерисовать весь кадр: заголовок, сайдбар, каждую строку редактора, подвал, каретку.
fn render_full(editor: &Editor, caret_on: bool, tree: Option<&super::tree::FileTree>) {
    let Some(l) = layout(editor, tree) else {
        return;
    };
    // Отложено: восстановленный фон едет вместе с flush кадра ниже, так что
    // стрелка остаётся на устройстве, пока кадр рисуется.
    cursor::hide_deferred();
    let _ = framebuffer::with(|fb| {
        let bg = editor.config.palette()[0];
        fb.fill_rect(0, 0, l.w, l.h, bg);
        paint_header(fb, editor, &l);
        if let Some(t) = tree {
            paint_tree(fb, editor, t, &l);
        }
        for vr in 0..l.rows {
            paint_editor_row(fb, editor, &l, editor.top + vr, vr);
        }
        paint_footer(fb, editor, &l);
        paint_caret_bar(fb, editor, caret_on, &l);
    });
    cursor::place_here_deferred();
    framebuffer::flush();
}

/// Перерисовать строки тела `first..=last` (индексы видимых строк), подвал и
/// каретку. Заголовок и сайдбар не трогаются.
fn render_body(
    editor: &Editor,
    caret_on: bool,
    tree: Option<&super::tree::FileTree>,
    first: usize,
    last: usize,
) {
    let Some(l) = layout(editor, tree) else {
        return;
    };
    if l.rows == 0 {
        return;
    }
    let last = last.min(l.rows - 1);
    let first = first.min(last);
    // Отложено: восстановленный фон едет вместе с flush кадра ниже, так что
    // стрелка остаётся на устройстве, пока кадр рисуется.
    cursor::hide_deferred();
    let _ = framebuffer::with(|fb| {
        for vr in first..=last {
            paint_editor_row(fb, editor, &l, editor.top + vr, vr);
        }
        paint_footer(fb, editor, &l);
        paint_caret_bar(fb, editor, caret_on, &l);
    });
    cursor::place_here_deferred();
    framebuffer::flush();
}

/// Перерисовать сайдбар и подвал (фокус дерева), плюс строку каретки, чтобы
/// мигающая полоса не осталась висеть, пока текстовую панель не редактируют.
fn render_tree(editor: &Editor, caret_on: bool, tree: Option<&super::tree::FileTree>) {
    let Some(l) = layout(editor, tree) else {
        return;
    };
    // Отложено: восстановленный фон едет вместе с flush кадра ниже, так что
    // стрелка остаётся на устройстве, пока кадр рисуется.
    cursor::hide_deferred();
    let _ = framebuffer::with(|fb| {
        if let Some(t) = tree {
            paint_tree(fb, editor, t, &l);
        }
        if editor.prompt.is_none() {
            if let Some(vr) = caret_position(editor, &l).map(|(_, y)| (y - HEADER_H) / CHAR_H) {
                if vr < l.rows {
                    paint_editor_row(fb, editor, &l, editor.top + vr, vr);
                }
            }
        }
        paint_footer(fb, editor, &l);
        paint_caret_bar(fb, editor, caret_on, &l);
    });
    cursor::place_here_deferred();
    framebuffer::flush();
}

/// Перерисовать только подвал (в него набирают живой prompt). Каретка не
/// рисуется, пока подвалом владеет prompt, так что больше ничего измениться не могло.
fn render_footer_only(editor: &Editor, tree: Option<&super::tree::FileTree>) {
    let Some(l) = layout(editor, tree) else {
        return;
    };
    cursor::hide_deferred();
    let _ = framebuffer::with(|fb| paint_footer(fb, editor, &l));
    cursor::place_here_deferred();
    framebuffer::flush();
}

/// Перерисовать строку каретки и саму каретку — путь мигания.
///
/// Стереть полосу значит вернуть символ под ней, поэтому строка перерисовывается,
/// а не забивается цветом фона: 2-пиксельная фоновая полоса стирала бы левую
/// колонку глифа в середине строки на каждом мигании.
fn render_blink(editor: &Editor, caret_on: bool, tree: Option<&super::tree::FileTree>) {
    let Some(l) = layout(editor, tree) else {
        return;
    };
    let Some(vr) = caret_position(editor, &l).map(|(_, y)| (y - HEADER_H) / CHAR_H) else {
        return;
    };
    // Отложено: восстановленный фон едет вместе с flush кадра ниже, так что
    // стрелка остаётся на устройстве, пока кадр рисуется.
    cursor::hide_deferred();
    let _ = framebuffer::with(|fb| {
        paint_editor_row(fb, editor, &l, editor.top + vr, vr);
        paint_caret_bar(fb, editor, caret_on, &l);
    });
    cursor::place_here_deferred();
    framebuffer::flush();
}

pub fn run(path_arg: &str) {
    let path = super::path::resolve(&super::path::cwd(), path_arg);
    let (node, text) = match open_or_create(&path) {
        Ok(v) => v,
        Err(e) => {
            super::render::error_line(&format!("nano: {}: {:?}", path, e));
            return;
        }
    };
    let node = node;
    let mut editor = Editor::new(&path, &text, NanoConfig::load());
    editor.tree_root = Some(parent_dir(&path));
    // Сайдбар виден с самого начала: видеть проект — смысл IDE-подобного
    // редактора, а `^B` скрывает его для текста во всю ширину.
    let mut tree: Option<super::tree::FileTree> = None;
    let mut tree_focus = false;
    crate::kprintln!("nano+: editing {}", path);
    event_loop(editor, node, &mut tree, &mut tree_focus);
    framebuffer::clear_screen();
    cursor::hide();
    crate::kprintln!("nano+: closed {}", path);
}

/// Какая панель управляется клавиатурой. Сайдбар достижим, только пока открыт.
#[derive(PartialEq, Clone, Copy)]
enum Pane {
    Text,
    Tree,
}

/// Цикл событий редактора с продетым через него сайдбаром.
///
/// Выделен из [`run`], чтобы дерево и редактор были отдельными заимствованиями:
/// циклу нужны `&mut` на оба, и два локальных значения одного кадра — то, что
/// делает это легальным без внутренней изменяемости.
fn event_loop(
    mut editor: Editor,
    mut node: alloc::sync::Arc<dyn crate::vfs::VfsNode>,
    tree: &mut Option<super::tree::FileTree>,
    tree_focus: &mut bool,
) {
    let mut decoder = Decoder::new();
    // Клавиатура стартует в ТЕКСТЕ, всегда — в том числе когда сайдбар открыт.
    // Открытие файла — намерение его редактировать; направление первого нажатия в
    // файловый браузер вместо этого делало редактор сломанным (печатать нельзя, а
    // `^Q` ничего не делает, потому что клавиши у сайдбара). `Tab` переводит в
    // дерево, когда это нужно.
    let mut pane = Pane::Text;
    // Открыть сайдбар в каталоге редактируемого файла.
    if let Some(root) = editor.tree_root.clone() {
        *tree = Some(super::tree::FileTree::new(&root));
        if let Some(t) = tree.as_mut() {
            let (_, h) = framebuffer::dimensions();
            t.view_rows = body_rows(h).saturating_sub(1);
            // Поставить подсветку на редактируемый файл, а не на первую запись:
            // дерево должно показывать, где вы находитесь.
            let current = editor.path.clone();
            if let Some(i) = t.rows().iter().position(|r| r.path == current) {
                t.cursor = i;
            }
            t.ensure_visible();
        }
    }
    let mut last_phase = blink();
    // Предыдущее состояние левой кнопки для определения фронтов. Драйвер PS/2
    // сообщает кнопку уровнем, так что «нажато» — это фронт, а не `left == true`.
    let mut prev_left = false;
    render_full(&editor, last_phase, tree.as_ref());
    'app: loop {
        crate::arch::cpu::halt();
        while let Some(sc) = super::try_read_scancode() {
            let Some(event) = decoder.feed(sc) else {
                continue;
            };
            if editor.prompt.is_some() {
                editor.handle_prompt(event);
                // Завершённый/отменённый prompt двигает курсор и может подсветить
                // попадание поиска в другом месте экрана; набор в него меняет только
                // строку подвала.
                if matches!(
                    event,
                    KeyEvent::Enter | KeyEvent::Escape | KeyEvent::Ctrl('q')
                ) {
                    last_phase = blink();
                    render_full(&editor, last_phase, tree.as_ref());
                } else {
                    last_phase = blink();
                    render_footer_only(&editor, tree.as_ref());
                }
                continue;
            }

            // ── Клавиши, принадлежащие редактору независимо от фокуса ──
            // `^Q`, `^S` и `^B` — как выйти, сохранить и вернуться; состояние фокуса,
            // которое их проглатывает, — ловушка, и именно это случилось, когда
            // сайдбар открывался в фокусе.
            match event {
                KeyEvent::Ctrl('q') => {
                    if editor.dirty && !editor.quit_armed {
                        editor.quit_armed = true;
                        editor.status = "Unsaved changes — press ^Q again to quit".into();
                    } else {
                        break 'app;
                    }
                    last_phase = blink();
                    render_full(&editor, last_phase, tree.as_ref());
                    continue;
                }
                KeyEvent::Ctrl('s') => {
                    if let Err(e) = save(&mut editor, &node) {
                        editor.status = format!("Error saving: {:?}", e)
                    }
                    last_phase = blink();
                    render_full(&editor, last_phase, tree.as_ref());
                    continue;
                }
                KeyEvent::Ctrl('b') => {
                    if tree.is_some() {
                        *tree = None;
                        pane = Pane::Text;
                    } else {
                        *tree = Some(super::tree::FileTree::new(&parent_dir(&editor.path)));
                        pane = Pane::Tree;
                        if let Some(t) = tree.as_mut() {
                            let (_, h) = framebuffer::dimensions();
                            t.view_rows = body_rows(h).saturating_sub(1);
                        }
                    }
                    *tree_focus = pane == Pane::Tree;
                    last_phase = blink();
                    render_full(&editor, last_phase, tree.as_ref());
                    continue;
                }
                _ => {}
            }

            // ── Фокус сайдбара: дерево владеет стрелками и Enter ──
            if pane == Pane::Tree {
                let mut switched = false;
                // Ставится, когда смена файла отвергнута из-за грязного буфера:
                // тогда сайдбар должен вернуть подсветку на файл, который реально
                // открыт, иначе он утверждает, что вы правите файл, который не правите.
                let mut refused = false;
                match event {
                    KeyEvent::Up => {
                        if let Some(t) = tree.as_mut() {
                            t.move_up()
                        }
                    }
                    KeyEvent::Down => {
                        if let Some(t) = tree.as_mut() {
                            t.move_down()
                        }
                    }
                    KeyEvent::Left => {
                        if let Some(t) = tree.as_mut() {
                            t.collapse_or_parent();
                        }
                    }
                    KeyEvent::Right | KeyEvent::Enter => {
                        if let Some(path) = tree.as_mut().and_then(|t| t.activate()) {
                            // Смена файла с несохранёнными изменениями молча их бы
                            // потеряла, поэтому она отвергается.
                            if editor.dirty {
                                editor.status =
                                    "Unsaved changes — ^S to save, ^Q to discard".into();
                                refused = true;
                            } else if let Ok((n, text)) = open_or_create(&path) {
                                editor.reload(&path, &text);
                                node = n;
                                switched = true;
                                // Открытие файла — намерение его редактировать:
                                // клавиатура возвращается в текст, так что следующее
                                // нажатие — набор, а не навигация по дереву. (Открытие
                                // каталога оставляет фокус здесь для ходьбы стрелками.)
                                pane = Pane::Text;
                                *tree_focus = false;
                            }
                        }
                    }
                    // `Tab` возвращает в текст; `Esc` тоже. Редакторные
                    // `^Q`/`^S`/`^B` выше уже обработаны.
                    KeyEvent::Tab | KeyEvent::Escape => {
                        pane = Pane::Text;
                        *tree_focus = false;
                    }
                    _ => {}
                }
                if let Some(t) = tree.as_mut() {
                    let (_, h) = framebuffer::dimensions();
                    t.view_rows = body_rows(h).saturating_sub(1);
                    if refused {
                        // Смена отвергнута: подсветка уже сдвинулась на кликнутую
                        // строку, так что вернуть её на открытый файл.
                        focus_tree_on(&editor, t);
                    }
                    t.ensure_visible();
                }
                last_phase = blink();
                if switched {
                    render_full(&editor, last_phase, tree.as_ref());
                } else {
                    render_tree(&editor, last_phase, tree.as_ref());
                }
                continue;
            }

            // ── Фокус текста ──
            let before = (
                editor.row,
                editor.col,
                editor.top,
                editor.left,
                editor.lines.len(),
            );
            match event {
                // Клавиши редактирования действуют на выделение, когда оно есть:
                // набор заменяет его, Backspace/Delete удаляют, а `^A` + Delete
                // очищает всю строку (или весь файл) вместо удаления одного
                // символа. Игнорирование видимого выделения здесь было багом,
                // который первый пользователь встретил через минуту после открытия.
                KeyEvent::Char(c) => {
                    editor.delete_selection();
                    editor.insert(c)
                }
                KeyEvent::Enter => {
                    editor.delete_selection();
                    editor.newline()
                }
                KeyEvent::Backspace => {
                    if !editor.delete_selection() {
                        editor.backspace()
                    }
                }
                KeyEvent::Delete => {
                    if !editor.delete_selection() {
                        editor.delete()
                    }
                }
                // Обычное движение каретки отменяет выделение, как в любом редакторе.
                KeyEvent::Left => {
                    editor.clear_selection();
                    editor.left()
                }
                KeyEvent::Right => {
                    editor.clear_selection();
                    editor.right()
                }
                KeyEvent::Up => {
                    editor.clear_selection();
                    editor.up()
                }
                KeyEvent::Down => {
                    editor.clear_selection();
                    editor.down()
                }
                KeyEvent::Home => {
                    editor.clear_selection();
                    editor.col = 0;
                    editor.ensure_visible()
                }
                KeyEvent::End => {
                    editor.clear_selection();
                    editor.col = editor.lines[editor.row].len();
                    editor.ensure_visible()
                }
                KeyEvent::PageUp => {
                    editor.clear_selection();
                    for _ in 0..12 {
                        editor.up()
                    }
                }
                KeyEvent::PageDown => {
                    editor.clear_selection();
                    for _ in 0..12 {
                        editor.down()
                    }
                }
                KeyEvent::Tab => {
                    editor.delete_selection();
                    for _ in 0..editor.config.tab_size {
                        editor.insert(' ')
                    }
                }
                KeyEvent::Escape => {
                    if editor.has_selection() {
                        // Esc сначала сбрасывает выделение; выход — следующая клавиша.
                        editor.clear_selection();
                    } else if editor.dirty && !editor.quit_armed {
                        editor.quit_armed = true;
                        editor.status = "Unsaved changes — press ^Q again to quit".into()
                    } else {
                        break 'app;
                    }
                }
                // Войти в сайдбар без мыши.
                KeyEvent::Ctrl('t') => {
                    if tree.is_some() {
                        pane = Pane::Tree;
                        *tree_focus = true;
                    }
                }
                KeyEvent::Ctrl('f') => editor.begin_prompt(PromptMode::Search),
                KeyEvent::Ctrl('r') => editor.begin_prompt(PromptMode::ReplaceFind),
                KeyEvent::Ctrl('g') => editor.begin_prompt(PromptMode::Goto),
                KeyEvent::Ctrl('z') => editor.undo(),
                KeyEvent::Ctrl('y') => editor.redo(),
                // `^A` выделяет всё; `^C`/`^K`/`^X` копируют и вырезают
                // выделение, когда оно есть, и откатываются к построчному поведению,
                // когда нет. Это сохраняет классические привязки и даёт выделению
                // мышью куда деться.
                KeyEvent::Ctrl('a') => editor.select_all(),
                KeyEvent::Ctrl('c') => {
                    if let Some(text) = editor.sel_text() {
                        let n = text.chars().count();
                        editor.sel_clip = text;
                        editor.status = format!("Copied {} char(s)", n);
                    } else {
                        editor.copy_line();
                    }
                }
                KeyEvent::Ctrl('x') | KeyEvent::Ctrl('k') => {
                    if let Some(text) = editor.sel_text() {
                        let n = text.chars().count();
                        editor.sel_clip = text;
                        editor.delete_selection();
                        editor.status = format!("Cut {} char(s)", n);
                    } else {
                        editor.cut_line();
                    }
                }
                KeyEvent::Ctrl('u') | KeyEvent::Ctrl('v') => {
                    if editor.sel_clip.is_empty() {
                        editor.paste_line();
                    } else {
                        let text = editor.sel_clip.clone();
                        editor.delete_selection();
                        editor.insert_text(&text);
                        editor.status = "Pasted".into();
                    }
                }
                _ => {}
            }
            last_phase = blink();
            if editor.prompt.is_some() {
                // ^F/^R/^G открыли prompt: перерисовать кадр, чтобы полоса каретки
                // на старой строке стёрлась, а подвал показал prompt.
                render_full(&editor, last_phase, tree.as_ref());
            } else if editor.top != before.2 || editor.left != before.3 {
                // Прокрутка (или очень длинная строка) сдвинула весь viewport.
                render_full(&editor, last_phase, tree.as_ref());
            } else if editor.lines.len() != before.4 {
                // Строки ниже правки сдвинулись: перерисовать от изменившейся строки вниз.
                let first = before.0.min(editor.row).saturating_sub(editor.top);
                render_body(
                    &editor,
                    last_phase,
                    tree.as_ref(),
                    first,
                    body_rows(framebuffer::dimensions().1).saturating_sub(1),
                );
            } else {
                // Изменились только строки между старым и новым положением каретки
                // (обычно одна).
                let a = before.0.saturating_sub(editor.top);
                let b = editor.row.saturating_sub(editor.top);
                render_body(&editor, last_phase, tree.as_ref(), a.min(b), a.max(b));
            }
        }
        // ── Мышь ────────────────────────────────────────────────────────
        // Раньше клик в редакторе не делал ничего: стрелка рисовалась, а кнопки
        // никогда не читались. Клик ставит каретку, перетаскивание выделяет текст
        // (и каретка следует за ним), а клик по строке сайдбара фокусирует/активирует её.
        let mouse = crate::drivers::ps2_mouse::poll();
        let pressed = mouse.left && !prev_left;
        let released = !mouse.left && prev_left;
        prev_left = mouse.left;
        let mut mouse_changed = false;
        let mut mouse_full = false;
        if pressed {
            match mouse_target(&editor, tree.as_ref(), mouse.x, mouse.y) {
                Some(Hit::Text(p)) => {
                    let old_row = editor.row;
                    editor.clear_selection();
                    mouse_full = editor.set_cursor(p);
                    editor.begin_select(p);
                    if !mouse_full {
                        let a = old_row.saturating_sub(editor.top);
                        let b = editor.row.saturating_sub(editor.top);
                        last_phase = blink();
                        render_body(&editor, last_phase, tree.as_ref(), a.min(b), a.max(b));
                    }
                    mouse_changed = true;
                }
                Some(Hit::Tree(i)) => {
                    pane = Pane::Tree;
                    *tree_focus = true;
                    if let Some(t) = tree.as_mut() {
                        t.cursor = i;
                    }
                    // Клик по строке файла открывает его; по каталогу — только
                    // фокусирует строку (Enter/Right всё равно разворачивают).
                    let target = tree.as_mut().and_then(|t| t.activate());
                    if let Some(path) = target {
                        if editor.dirty {
                            editor.status = "Unsaved changes — ^S to save, ^Q to discard".into();
                            if let Some(t) = tree.as_mut() {
                                focus_tree_on(&editor, t);
                            }
                        } else if let Ok((n, text)) = open_or_create(&path) {
                            editor.reload(&path, &text);
                            node = n;
                            // Клик по строке файла открывает его *и* возвращает
                            // клавиатуру тексту: оставленный в фокусе сайдбар делал
                            // редактор мёртвым после нескольких смен файла — каждое
                            // нажатие уходило в дерево.
                            pane = Pane::Text;
                            *tree_focus = false;
                        }
                    }
                    mouse_full = true;
                    mouse_changed = true;
                }
                None => {}
            }
        } else if mouse.left && editor.sel.anchor.is_some() {
            // Перетаскивание: расширить выделение до клетки под указателем и
            // двигать каретку вместе с ним, чтобы набор после перетаскивания попал
            // под указатель.
            if let Some(p) = mouse_text_point(&editor, tree.as_ref(), mouse.x, mouse.y) {
                let prev_head = editor.sel.head.map(|q| q.row);
                let old_row = editor.row;
                editor.extend_select(p);
                mouse_full = editor.set_cursor(p);
                if !mouse_full {
                    let mut lo = p.row;
                    let mut hi = p.row;
                    for r in [old_row, p.row]
                        .into_iter()
                        .chain(prev_head)
                        .chain(editor.sel.anchor.map(|q| q.row))
                    {
                        lo = lo.min(r);
                        hi = hi.max(r);
                    }
                    let lo_vr = lo.saturating_sub(editor.top);
                    let hi_vr = hi
                        .saturating_sub(editor.top)
                        .min(body_rows(framebuffer::dimensions().1).saturating_sub(1));
                    if lo_vr <= hi_vr {
                        last_phase = blink();
                        render_body(&editor, last_phase, tree.as_ref(), lo_vr, hi_vr);
                    }
                }
                mouse_changed = true;
            }
        } else if released && editor.sel.is_empty() {
            editor.clear_selection();
        }
        if mouse_changed && mouse_full {
            last_phase = blink();
            render_full(&editor, last_phase, tree.as_ref());
        }
        // Стрелка: `move_to` идемпотентен (рано выходит, когда указатель не
        // двигался), так что это сравнение на каждый тик, а не перерисовка.
        cursor::move_to(mouse.x, mouse.y);
        // Мигание: перерисовать строку каретки и полосу, только когда сменилась
        // фаза. Пробуждения на 1 кГц слишком часты, чтобы это было чем-то, кроме
        // двух изменений экрана в секунду.
        let phase = blink();
        if phase != last_phase {
            last_phase = phase;
            render_blink(&editor, phase, tree.as_ref());
        }
    }
    let _ = tree_focus;
}

/// Фаза мигания каретки, общая с shell, чтобы обе каретки мигали вместе.
fn blink() -> bool {
    super::caret::Caret::blink_on(crate::task::scheduler::ticks())
}

/// Вернуть подсветку сайдбара на файл, который реально открыт.
///
/// Используется, когда смена отвергнута (несохранённые изменения): дерево уже
/// сдвинуло подсветку на кликнутую строку, и оставлять её там — утверждать, что
/// редактор показывает не тот файл. Возвращает, сдвинулась ли подсветка.
fn focus_tree_on(editor: &Editor, tree: &mut super::tree::FileTree) -> bool {
    let current = editor.path.clone();
    match tree.rows().iter().position(|r| r.path == current) {
        Some(i) if i != tree.cursor => {
            tree.cursor = i;
            tree.ensure_visible();
            true
        }
        _ => false,
    }
}

/// Каталог, содержащий `path`, — корень дерева рядом с редактируемым файлом.
fn parent_dir(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => String::from("/"),
        Some(i) => String::from(&trimmed[..i]),
    }
}
