//! Абстракция файловой системы и работа с путями для Files.
//!
//! Виджетам и приложению не важен источник данных: тесты используют
//! [`fake::FakeVfs`] (дерево в памяти), а на Pagh будет реализация поверх
//! `openat/readdir/stat/unlink/rename` из Linux-совместимого слоя ядра.
//!
//! Пути — абсолютные POSIX-строки (`/home/user/Документы`), имена — UTF-8.
//! Скрытые элементы (имя начинается с `.`) скрываются приложением, а не
//! файловой системой.

use std::fmt;

/// Тип элемента каталога.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Device,
    Other,
}

impl EntryKind {
    pub fn is_dir(self) -> bool {
        self == EntryKind::Directory
    }

    /// Человекочитаемое название для панели свойств.
    pub fn label(self) -> &'static str {
        match self {
            EntryKind::Directory => "Папка",
            EntryKind::File => "Файл",
            EntryKind::Symlink => "Символическая ссылка",
            EntryKind::Device => "Устройство",
            EntryKind::Other => "Другое",
        }
    }
}

/// Элемент каталога в листинге.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    /// Время изменения (unix seconds); 0 — неизвестно.
    pub modified: u64,
}

impl DirEntry {
    pub fn new(name: impl Into<String>, kind: EntryKind, size: u64) -> DirEntry {
        DirEntry {
            name: name.into(),
            kind,
            size,
            modified: 0,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind.is_dir()
    }
}

/// Подробная информация об элементе (панель свойств).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stat {
    pub path: String,
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: u64,
    /// Права доступа (rwx для владельца/группы/остальных), например `0o755`.
    pub mode: u32,
    /// Цель символической ссылки.
    pub target: Option<String>,
}

/// Ошибка файловой системы.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    AlreadyExists,
    NotADirectory,
    IsADirectory,
    InvalidName,
    PermissionDenied,
    Unsupported,
    Io(String),
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FsError::NotFound => write!(f, "объект не найден"),
            FsError::AlreadyExists => write!(f, "уже существует"),
            FsError::NotADirectory => write!(f, "это не папка"),
            FsError::IsADirectory => write!(f, "это папка"),
            FsError::InvalidName => write!(f, "недопустимое имя"),
            FsError::PermissionDenied => write!(f, "доступ запрещён"),
            FsError::Unsupported => write!(f, "операция не поддерживается"),
            FsError::Io(message) => write!(f, "ошибка ввода-вывода: {message}"),
        }
    }
}

impl std::error::Error for FsError {}

pub type Result<T> = std::result::Result<T, FsError>;

/// Интерфейс файловой системы, который использует Files.
pub trait Vfs {
    /// Домашний каталог пользователя.
    fn home(&self) -> &str;

    /// Содержимое каталога в порядке файловой системы; скрытые элементы
    /// возвращаются, фильтрует их приложение.
    fn read_dir(&mut self, path: &str) -> Result<Vec<DirEntry>>;

    /// Подробная информация об элементе.
    fn stat(&mut self, path: &str) -> Result<Stat>;

    /// Создать каталог.
    fn mkdir(&mut self, path: &str) -> Result<()>;

    /// Создать пустой файл (ошибка, если уже существует).
    fn create_file(&mut self, path: &str) -> Result<()> {
        self.write_file(path, &[])
    }

    /// Записать содержимое файла (создаёт файл).
    fn write_file(&mut self, path: &str, contents: &[u8]) -> Result<()>;

    /// Прочитать содержимое файла.
    fn read_file(&mut self, path: &str) -> Result<Vec<u8>>;

    /// Удалить файл или пустой каталог.
    fn unlink(&mut self, path: &str) -> Result<()>;

    /// Переименовать/переместить.
    fn rename(&mut self, from: &str, to: &str) -> Result<()>;

    /// Скопировать элемент (по умолчанию не поддерживается).
    fn copy(&mut self, _from: &str, _to: &str) -> Result<()> {
        Err(FsError::Unsupported)
    }

    /// Создать символическую ссылку (по умолчанию не поддерживается).
    fn symlink(&mut self, _target: &str, _link: &str) -> Result<()> {
        Err(FsError::Unsupported)
    }

    /// Проверить существование.
    fn exists(&mut self, path: &str) -> bool {
        self.stat(path).is_ok()
    }
}

/// Соединить базовый путь и имя.
pub fn join(base: &str, name: &str) -> String {
    if name.starts_with('/') {
        return normalize(name);
    }
    if base == "/" {
        normalize(&format!("/{name}"))
    } else {
        normalize(&format!("{base}/{name}"))
    }
}

/// Родительский каталог (`/` для корня и для относительных путей).
pub fn parent(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => "/",
        Some(index) => &trimmed[..index],
        None => "/",
    }
}

/// Последний компонент пути; для `/` — `/`.
pub fn file_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/";
    }
    match trimmed.rfind('/') {
        Some(index) => &trimmed[index + 1..],
        None => trimmed,
    }
}

/// Нормализовать путь: убрать `.`/`..`, повторы `/`, хвостовой `/`.
pub fn normalize(path: &str) -> String {
    let mut components: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            other => components.push(other),
        }
    }
    if components.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", components.join("/"))
    }
}

/// Отображаемое имя каталога: домашний каталог — «Домашняя папка».
pub fn display_name(path: &str, home: &str) -> String {
    if normalize(path) == normalize(home) {
        return "Домашняя папка".to_string();
    }
    if normalize(path) == "/" {
        return "Корень файловой системы".to_string();
    }
    let name = file_name(path);
    if name == "/" {
        "Корень файловой системы".to_string()
    } else {
        name.to_string()
    }
}

/// Уникальное имя в каталоге: `Новая папка`, затем `Новая папка 2`, `3`…
pub fn unique_name(entries: &[DirEntry], base: &str, extension: Option<&str>) -> String {
    let suffix = |index: u32| -> String {
        let stem = if index == 1 {
            base.to_string()
        } else {
            format!("{base} {index}")
        };
        match extension {
            Some(ext) => format!("{stem}.{ext}"),
            None => stem,
        }
    };
    for index in 1..10_000 {
        let candidate = suffix(index);
        if !entries.iter().any(|entry| entry.name == candidate) {
            return candidate;
        }
    }
    suffix(10_000)
}

/// Имя копии: `name`, затем `name (копия).txt`, `name (копия 2).txt`…
pub fn copy_name(entries: &[DirEntry], name: &str) -> String {
    if !entries.iter().any(|entry| entry.name == name) {
        return name.to_string();
    }
    let (stem, extension) = match name.rfind('.') {
        Some(index) if index > 0 => (&name[..index], &name[index..]),
        _ => (name, ""),
    };
    for index in 1..10_000 {
        let candidate = if index == 1 {
            format!("{stem} (копия){extension}")
        } else {
            format!("{stem} (копия {index}){extension}")
        };
        if !entries.iter().any(|entry| entry.name == candidate) {
            return candidate;
        }
    }
    name.to_string()
}

/// Скрытый ли элемент (имя начинается с точки).
pub fn is_hidden(name: &str) -> bool {
    name.starts_with('.') && name != "." && name != ".."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_and_normalize() {
        assert_eq!(join("/home/user", "Документы"), "/home/user/Документы");
        assert_eq!(join("/home/user/", "Документы"), "/home/user/Документы");
        assert_eq!(join("/", "boot"), "/boot");
        assert_eq!(normalize("/home//user/./a/../b/"), "/home/user/b");
        assert_eq!(normalize("/.."), "/");
        assert_eq!(normalize(""), "/");
    }

    #[test]
    fn parent_and_name() {
        assert_eq!(parent("/home/user/file.txt"), "/home/user");
        assert_eq!(parent("/file.txt"), "/");
        assert_eq!(parent("/"), "/");
        assert_eq!(file_name("/home/user/file.txt"), "file.txt");
        assert_eq!(file_name("/home/user/"), "user");
        assert_eq!(file_name("/"), "/");
    }

    #[test]
    fn display_name_special_cases() {
        assert_eq!(display_name("/home/user", "/home/user"), "Домашняя папка");
        assert_eq!(display_name("/", "/home/user"), "Корень файловой системы");
        assert_eq!(
            display_name("/home/user/Documents", "/home/user"),
            "Documents"
        );
    }

    #[test]
    fn unique_names() {
        let entries = vec![
            DirEntry::new("Новая папка", EntryKind::Directory, 0),
            DirEntry::new("Новая папка 2", EntryKind::Directory, 0),
        ];
        assert_eq!(unique_name(&entries, "Новая папка", None), "Новая папка 3");
        assert_eq!(
            unique_name(&entries, "Новый текстовый файл", Some("txt")),
            "Новый текстовый файл.txt"
        );
    }

    #[test]
    fn copy_names() {
        let entries = vec![
            DirEntry::new("notes.md", EntryKind::File, 0),
            DirEntry::new("notes (копия).md", EntryKind::File, 0),
            DirEntry::new("Папка", EntryKind::Directory, 0),
        ];
        assert_eq!(copy_name(&entries, "notes.md"), "notes (копия 2).md");
        assert_eq!(copy_name(&entries, "Папка"), "Папка (копия)");
        assert_eq!(copy_name(&entries, "other.txt"), "other.txt");
    }

    #[test]
    fn hidden_names() {
        assert!(is_hidden(".local"));
        assert!(!is_hidden("Документы"));
        assert!(!is_hidden("."));
    }

    #[test]
    fn error_display_is_russian() {
        assert_eq!(FsError::NotFound.to_string(), "объект не найден");
        assert_eq!(
            FsError::Io("диск".to_string()).to_string(),
            "ошибка ввода-вывода: диск"
        );
    }
}
