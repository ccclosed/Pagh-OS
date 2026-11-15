//! Сайдбар дерева файлов для редактора.
//!
//! У редактора не было способа видеть файловую систему во время правки: вы
//! открывали один путь, и это был весь контекст. Это половина изменения
//! «выглядит как IDE» — дерево проекта рядом с текстом, с раскрывающимися и
//! сворачивающимися каталогами, навигацией с клавиатуры и Enter для смены
//! редактируемого файла.
//!
//! Дерево хранится **плоско**: `rows` держит ровно то, что отображается, по
//! порядку, каждая строка несёт свою глубину отступа. Рисование и перемещение
//! курсора — тогда обход одного `Vec`, а сворачивание каталога — перестройка, а не
//! рекурсивный пропуск. Видимый список перестраивается из файловой системы, так
//! что файл, созданный командой (или `apt install`), появляется без refresh.
//!
//! Размеры ограничены намеренно: каталоги сортируются, скрытые dotfiles
//! пропускаются (собственные dotfiles shell — не содержимое проекта), а глубина
//! ограничена, чтобы цикл симлинков не растил список бесконечно.

use crate::vfs;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Одна видимая строка дерева.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Отображаемое имя (без пути).
    pub name: String,
    /// Абсолютный путь, используется для открытия файла.
    pub path: String,
    /// Уровень отступа: 0 для записи корня, 1 для его детей, …
    pub depth: usize,
    pub is_dir: bool,
    /// Раскрыта ли запись (для файла всегда false).
    pub expanded: bool,
}

/// Сайдбар: корневой путь, множество раскрытых и плоский список видимых строк.
pub struct FileTree {
    root: String,
    rows: Vec<Row>,
    /// Индекс в `rows` подсвеченной записи.
    pub cursor: usize,
    /// Верхнее смещение прокрутки, поддерживается [`Self::ensure_visible`].
    pub top: usize,
    /// Сколько строк влезает на экран; ставится рендерером каждый кадр.
    pub view_rows: usize,
    expanded: Vec<String>,
}

/// Каталоги глубже этого не обходятся. Иначе симлинки на родителя делали бы
/// видимый список неограниченным.
const MAX_DEPTH: usize = 8;

/// Записей на каталог. Каталог размера `/usr/bin` — не содержимое проекта, а
/// неограниченный список строк — утечка памяти с красивым названием.
const MAX_ENTRIES: usize = 512;

impl FileTree {
    /// Построить дерево с корнем `root` (путь каталога), с раскрытым `root`.
    pub fn new(root: &str) -> Self {
        let mut t = FileTree {
            root: root.to_string(),
            rows: Vec::new(),
            cursor: 0,
            top: 0,
            view_rows: 1,
            expanded: alloc::vec![root.to_string()],
        };
        t.rebuild();
        t
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Подсвеченная запись, если дерево непусто.
    pub fn selected(&self) -> Option<&Row> {
        self.rows.get(self.cursor)
    }

    /// Перестроить видимые строки из файловой системы, удерживая курсор на том же
    /// пути, пока этот путь существует.
    pub fn rebuild(&mut self) {
        let keep = self.selected().map(|r| r.path.clone());
        let mut rows = Vec::new();
        self.walk(&self.root.clone(), 0, &mut rows);
        self.rows = rows;
        if let Some(path) = keep {
            if let Some(i) = self.rows.iter().position(|r| r.path == path) {
                self.cursor = i;
            }
        }
        if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len().saturating_sub(1);
        }
    }

    /// Дописать записи `dir` в `out`, затем рекурсивно войти в раскрытые подкаталоги.
    ///
    /// Ошибки проглатываются: нечитаемый каталог просто не даёт строк. Редактор —
    /// не место сообщать о проблеме файловой системы, и сайдбар, отказывающийся
    /// рисоваться из-за одной нечитаемой записи, хуже сайдбара без одной строки.
    fn walk(&self, dir: &str, depth: usize, out: &mut Vec<Row>) {
        if depth > MAX_DEPTH || out.len() >= MAX_ENTRIES {
            return;
        }
        let Ok(node) = vfs::lookup_path(dir) else {
            return;
        };
        let Ok(children) = node.readdir() else {
            return;
        };
        let mut entries: Vec<(String, bool, String)> = Vec::new();
        for child in children.iter() {
            let name = child.name();
            if name.is_empty() || name == "." || name == ".." || name.starts_with('.') {
                continue;
            }
            let path = join(dir, &name);
            entries.push((name.to_string(), child.is_directory(), path));
        }
        // Сначала каталоги, затем по алфавиту: порядок, в котором показывает project explorer.
        entries.sort_by(|a, b| match (a.1, b.1) {
            (true, false) => core::cmp::Ordering::Less,
            (false, true) => core::cmp::Ordering::Greater,
            _ => a.0.cmp(&b.0),
        });
        for (name, is_dir, path) in entries {
            if out.len() >= MAX_ENTRIES {
                return;
            }
            let expanded = is_dir && self.is_expanded(&path);
            out.push(Row {
                name,
                path: path.clone(),
                depth,
                is_dir,
                expanded,
            });
            if expanded {
                self.walk(&path, depth + 1, out);
            }
        }
    }

    fn is_expanded(&self, path: &str) -> bool {
        self.expanded.iter().any(|p| p == path)
    }

    /// Открыть подсвеченный каталог или закрыть его, если он уже открыт.
    /// Возвращает путь для открытия, когда подсвечен файл.
    pub fn activate(&mut self) -> Option<String> {
        let row = self.selected()?.clone();
        if row.is_dir {
            if let Some(i) = self.expanded.iter().position(|p| *p == row.path) {
                self.expanded.remove(i);
            } else {
                self.expanded.push(row.path.clone());
            }
            self.rebuild();
            None
        } else {
            Some(row.path)
        }
    }

    /// Свернуть подсвеченный каталог или перейти в родительский каталог.
    /// Возвращает `true`, когда дерево изменилось.
    pub fn collapse_or_parent(&mut self) -> bool {
        let Some(row) = self.selected().cloned() else {
            return false;
        };
        if row.is_dir && row.expanded {
            if let Some(i) = self.expanded.iter().position(|p| *p == row.path) {
                self.expanded.remove(i);
            }
            self.rebuild();
            return true;
        }
        // Прыгнуть на строку объемлющего каталога.
        let depth = row.depth;
        if depth == 0 {
            return false;
        }
        if let Some(i) = self.rows[..self.cursor]
            .iter()
            .rposition(|r| r.is_dir && r.depth + 1 == depth)
        {
            self.cursor = i;
            return true;
        }
        false
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.cursor + 1 < self.rows.len() {
            self.cursor += 1;
        }
    }

    /// Держать курсор внутри видимого окна.
    pub fn ensure_visible(&mut self) {
        let view = self.view_rows.max(1);
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + view {
            self.top = self.cursor + 1 - view;
        }
    }
}

/// Соединить путь каталога и имя, терпя завершающий слэш у каталога.
fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        alloc::format!("{}{}", dir, name)
    } else {
        alloc::format!("{}/{}", dir, name)
    }
}
