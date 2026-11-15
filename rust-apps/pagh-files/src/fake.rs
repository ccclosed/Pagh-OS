//! Фейковая файловая система в памяти для тестов и headless-демо.
//!
//! Дерево близко к концепту `docs/file-manager-concept.png`: домашний каталог
//! содержит 24 папки и 4 файла общим размером 80 000 000 байт, что даёт в
//! строке состояния ровно `24 папки, 4 файла · 76,3 МБ`. Метаданные
//! детерминированы, поэтому скриншоты и тесты стабильны.

use std::collections::BTreeMap;

use crate::vfs::{
    file_name, is_hidden, normalize, DirEntry, EntryKind, FsError, Result, Stat, Vfs,
};

/// Метка «сейчас» для создаваемых файлов.
const CLOCK: u64 = 1_794_775_080; // 2026-11-15 20:38 UTC

#[derive(Clone, Debug)]
enum NodeKind {
    /// Файл: реальные байты плюс объявленный размер (для «разреженных» файлов).
    File {
        data: Vec<u8>,
        size: u64,
    },
    Dir {
        children: BTreeMap<String, Node>,
    },
    Symlink {
        target: String,
    },
}

#[derive(Clone, Debug)]
struct Node {
    kind: NodeKind,
    modified: u64,
    mode: u32,
}

impl Node {
    fn dir(modified: u64) -> Node {
        Node {
            kind: NodeKind::Dir {
                children: BTreeMap::new(),
            },
            modified,
            mode: 0o755,
        }
    }

    fn file(data: Vec<u8>, size: u64, modified: u64) -> Node {
        Node {
            kind: NodeKind::File { data, size },
            modified,
            mode: 0o644,
        }
    }

    fn is_dir(&self) -> bool {
        matches!(self.kind, NodeKind::Dir { .. })
    }

    fn size(&self) -> u64 {
        match &self.kind {
            NodeKind::File { data, size } => (*size).max(data.len() as u64),
            NodeKind::Dir { .. } => 0,
            NodeKind::Symlink { .. } => 0,
        }
    }

    fn entry_kind(&self) -> EntryKind {
        match &self.kind {
            NodeKind::File { .. } => EntryKind::File,
            NodeKind::Dir { .. } => EntryKind::Directory,
            NodeKind::Symlink { .. } => EntryKind::Symlink,
        }
    }
}

/// Фейковая ФС с деревом в памяти.
#[derive(Clone, Debug)]
pub struct FakeVfs {
    root: Node,
    home: String,
    clock: u64,
}

impl Default for FakeVfs {
    fn default() -> Self {
        FakeVfs::new()
    }
}

impl FakeVfs {
    /// Пустая ФС: `/`, `/home/user`, `/boot`, `/mnt/basic`, `/network`.
    pub fn new() -> FakeVfs {
        let mut vfs = FakeVfs {
            root: Node::dir(CLOCK),
            home: "/home/user".to_string(),
            clock: CLOCK,
        };
        vfs.mkdir_all("/home/user").expect("домашний каталог");
        vfs.mkdir_all("/boot").expect("/boot");
        vfs.mkdir_all("/mnt/basic").expect("/mnt/basic");
        vfs.mkdir_all("/network").expect("/network");
        vfs.mkdir_all("/home/user/Desktop").expect("рабочий стол");
        vfs.mkdir_all("/home/user/Documents").expect("документы");
        vfs.mkdir_all("/home/user/Downloads").expect("загрузки");
        vfs.mkdir_all("/home/user/Pictures").expect("изображения");
        vfs.mkdir_all("/home/user/Music").expect("музыка");
        vfs.mkdir_all("/home/user/Videos").expect("видео");
        vfs.mkdir_all("/home/user/.local/share/Trash")
            .expect("корзина");
        vfs
    }

    /// Дерево из концепта Files.
    pub fn concept() -> FakeVfs {
        let mut vfs = FakeVfs::new();
        // 24 папки домашнего каталога — как в концепте.
        for name in [
            "bin",
            "boot",
            "Desktop",
            "dev",
            "Documents",
            "Downloads",
            "home",
            "lib",
            "lib64",
            "mnt",
            "Music",
            "opt",
            "Pictures",
            "proc",
            "projects",
            "root",
            "run",
            "sbin",
            "srv",
            "sys",
            "tmp",
            "usr",
            "var",
            "Videos",
        ] {
            vfs.mkdir_all(&format!("/home/user/{name}"))
                .expect("папка концепта");
        }
        // 4 файла: сумма ровно 80 000 000 байт → «76,3 МБ».
        let files = [
            ("archive.tar", 30_000_000u64),
            ("notes.md", 12_345_678),
            ("photo.png", 34_567_890),
            ("todo.txt", 3_086_432),
        ];
        for (name, size) in files {
            vfs.file_with_size(&format!("/home/user/{name}"), size)
                .expect("файл концепта");
        }

        // Содержимое папок для навигации и списка.
        vfs.file_with_size("/home/user/Documents/Договор.pdf", 1_250_000)
            .expect("документ");
        vfs.file_with_size("/home/user/Documents/Заметки.md", 4_096)
            .expect("заметки");
        vfs.file_with_size("/home/user/Documents/План.txt", 12_000)
            .expect("план");
        vfs.mkdir_all("/home/user/Documents/Проекты")
            .expect("проекты");
        vfs.file_with_size("/home/user/Downloads/pagh-2.23.0.iso", 12_000_000)
            .expect("iso");
        vfs.file_with_size("/home/user/Downloads/python-3.13.tar.xz", 24_000_000)
            .expect("архив python");
        vfs.file_with_size("/home/user/Pictures/desktop-concept.png", 161_467)
            .expect("концепт рабочего стола");
        vfs.file_with_size("/home/user/Pictures/file-manager-concept.png", 165_419)
            .expect("концепт Files");
        vfs.file_with_size("/home/user/Music/track01.ogg", 4_500_000)
            .expect("трек");
        vfs.file_with_size("/home/user/Videos/demo.mp4", 8_000_000)
            .expect("видео");
        vfs.mkdir_all("/home/user/projects/pagh-os")
            .expect("проект");
        vfs.mkdir_all("/home/user/projects/site").expect("проект");
        vfs.file_with_size("/home/user/.local/share/Trash/deleted.txt", 1_024)
            .expect("корзина");

        vfs.file_with_size("/boot/pagh.elf", 2_400_000)
            .expect("ядро");
        vfs.file_with_size("/boot/limine.conf", 1_200)
            .expect("limine");
        vfs.mkdir_all("/mnt/basic/Документы").expect("раздел");
        vfs
    }

    /// Корень дерева (для отладки/тестов).
    pub fn root_is_dir(&self) -> bool {
        self.root.is_dir()
    }

    /// Текущее «время» ФС.
    pub fn clock(&self) -> u64 {
        self.clock
    }

    /// Создать все каталоги пути.
    pub fn mkdir_all(&mut self, path: &str) -> Result<()> {
        let path = normalize(path);
        let mut node = &mut self.root;
        for component in path.split('/').filter(|part| !part.is_empty()) {
            let NodeKind::Dir { children } = &mut node.kind else {
                return Err(FsError::NotADirectory);
            };
            node = children
                .entry(component.to_string())
                .or_insert_with(|| Node::dir(self.clock));
            if !node.is_dir() {
                return Err(FsError::NotADirectory);
            }
        }
        Ok(())
    }

    /// Добавить файл с объявленным размером (данные пустые).
    pub fn file_with_size(&mut self, path: &str, size: u64) -> Result<()> {
        self.write_node(path, Node::file(Vec::new(), size, self.clock))
    }

    /// Добавить файл с содержимым.
    pub fn file_with_contents(&mut self, path: &str, contents: &[u8]) -> Result<()> {
        let size = contents.len() as u64;
        self.write_node(path, Node::file(contents.to_vec(), size, self.clock))
    }

    /// Список всех элементов каталога (включая скрытые).
    pub fn list_all(&mut self, path: &str) -> Result<Vec<DirEntry>> {
        self.read_dir(path)
    }

    fn node(&self, path: &str) -> Option<&Node> {
        let path = normalize(path);
        let mut node = &self.root;
        for component in path.split('/').filter(|part| !part.is_empty()) {
            let NodeKind::Dir { children } = &node.kind else {
                return None;
            };
            node = children.get(component)?;
        }
        Some(node)
    }

    fn node_mut(&mut self, path: &str) -> Option<&mut Node> {
        let path = normalize(path);
        let mut node = &mut self.root;
        for component in path.split('/').filter(|part| !part.is_empty()) {
            let NodeKind::Dir { children } = &mut node.kind else {
                return None;
            };
            node = children.get_mut(component)?;
        }
        Some(node)
    }

    fn write_node(&mut self, path: &str, node: Node) -> Result<()> {
        let path = normalize(path);
        if path == "/" {
            return Err(FsError::AlreadyExists);
        }
        let name = file_name(&path).to_string();
        let Some(parent) = self.node_mut(crate::vfs::parent(&path)) else {
            return Err(FsError::NotFound);
        };
        let NodeKind::Dir { children } = &mut parent.kind else {
            return Err(FsError::NotADirectory);
        };
        children.insert(name, node);
        Ok(())
    }
}

impl Vfs for FakeVfs {
    fn home(&self) -> &str {
        &self.home
    }

    fn read_dir(&mut self, path: &str) -> Result<Vec<DirEntry>> {
        let path = normalize(path);
        let Some(node) = self.node(&path) else {
            return Err(FsError::NotFound);
        };
        let NodeKind::Dir { children } = &node.kind else {
            return Err(FsError::NotADirectory);
        };
        Ok(children
            .iter()
            .map(|(name, child)| DirEntry {
                name: name.clone(),
                kind: child.entry_kind(),
                size: child.size(),
                modified: child.modified,
            })
            .collect())
    }

    fn stat(&mut self, path: &str) -> Result<Stat> {
        let path = normalize(path);
        let Some(node) = self.node(&path) else {
            return Err(FsError::NotFound);
        };
        let target = match &node.kind {
            NodeKind::Symlink { target } => Some(target.clone()),
            _ => None,
        };
        Ok(Stat {
            name: file_name(&path).to_string(),
            path: path.clone(),
            kind: node.entry_kind(),
            size: node.size(),
            modified: node.modified,
            mode: node.mode,
            target,
        })
    }

    fn mkdir(&mut self, path: &str) -> Result<()> {
        let path = normalize(path);
        if path == "/" {
            return Err(FsError::AlreadyExists);
        }
        if self.node(&path).is_some() {
            return Err(FsError::AlreadyExists);
        }
        let name = file_name(&path).to_string();
        if name.is_empty() || name == "/" || name.contains('/') {
            return Err(FsError::InvalidName);
        }
        let clock = self.clock;
        let parent_path = crate::vfs::parent(&path).to_string();
        match self.node(&parent_path) {
            None => return Err(FsError::NotFound),
            Some(node) if !node.is_dir() => return Err(FsError::NotADirectory),
            Some(_) => {}
        }
        let Some(parent) = self.node_mut(&parent_path) else {
            return Err(FsError::NotFound);
        };
        let NodeKind::Dir { children } = &mut parent.kind else {
            return Err(FsError::NotADirectory);
        };
        children.insert(name, Node::dir(clock));
        Ok(())
    }

    fn write_file(&mut self, path: &str, contents: &[u8]) -> Result<()> {
        let path = normalize(path);
        if path == "/" {
            return Err(FsError::IsADirectory);
        }
        if let Some(node) = self.node(&path) {
            if node.is_dir() {
                return Err(FsError::IsADirectory);
            }
        }
        self.write_node(
            &path,
            Node::file(contents.to_vec(), contents.len() as u64, self.clock),
        )
    }

    fn read_file(&mut self, path: &str) -> Result<Vec<u8>> {
        let path = normalize(path);
        let Some(node) = self.node(&path) else {
            return Err(FsError::NotFound);
        };
        match &node.kind {
            NodeKind::File { data, .. } => Ok(data.clone()),
            NodeKind::Dir { .. } => Err(FsError::IsADirectory),
            NodeKind::Symlink { .. } => Ok(Vec::new()),
        }
    }

    fn unlink(&mut self, path: &str) -> Result<()> {
        let path = normalize(path);
        if path == "/" {
            return Err(FsError::PermissionDenied);
        }
        let Some(node) = self.node(&path) else {
            return Err(FsError::NotFound);
        };
        if let NodeKind::Dir { children } = &node.kind {
            if !children.is_empty() {
                return Err(FsError::Io("папка не пуста".to_string()));
            }
        }
        let name = file_name(&path).to_string();
        let Some(parent) = self.node_mut(crate::vfs::parent(&path)) else {
            return Err(FsError::NotFound);
        };
        let NodeKind::Dir { children } = &mut parent.kind else {
            return Err(FsError::NotADirectory);
        };
        children.remove(&name);
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let from = normalize(from);
        let to = normalize(to);
        if from == "/" || to == "/" {
            return Err(FsError::PermissionDenied);
        }
        if self.node(&to).is_some() {
            return Err(FsError::AlreadyExists);
        }
        let Some(node) = self.node(&from).cloned() else {
            return Err(FsError::NotFound);
        };
        self.remove_node(&from)?;
        self.write_node(&to, node)
    }

    fn copy(&mut self, from: &str, to: &str) -> Result<()> {
        let from = normalize(from);
        let to = normalize(to);
        if self.node(&to).is_some() {
            return Err(FsError::AlreadyExists);
        }
        let Some(node) = self.node(&from).cloned() else {
            return Err(FsError::NotFound);
        };
        self.write_node(&to, node)
    }

    fn symlink(&mut self, target: &str, link: &str) -> Result<()> {
        let link = normalize(link);
        if self.node(&link).is_some() {
            return Err(FsError::AlreadyExists);
        }
        self.write_node(
            &link,
            Node {
                kind: NodeKind::Symlink {
                    target: target.to_string(),
                },
                modified: self.clock,
                mode: 0o777,
            },
        )
    }
}

impl FakeVfs {
    fn remove_node(&mut self, path: &str) -> Result<()> {
        let path = normalize(path);
        if path == "/" {
            return Err(FsError::PermissionDenied);
        }
        let name = file_name(path.as_str()).to_string();
        let Some(parent) = self.node_mut(crate::vfs::parent(&path)) else {
            return Err(FsError::NotFound);
        };
        let NodeKind::Dir { children } = &mut parent.kind else {
            return Err(FsError::NotADirectory);
        };
        children.remove(&name);
        Ok(())
    }

    /// Число элементов, скрытых от UI (для проверки фильтра).
    pub fn hidden_count(&mut self, path: &str) -> usize {
        self.read_dir(path)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| is_hidden(&entry.name))
                    .count()
            })
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_names(entries: &[DirEntry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.name.as_str()).collect()
    }

    #[test]
    fn concept_tree_matches_status_line() {
        let mut vfs = FakeVfs::concept();
        let entries = vfs.list_all("/home/user").unwrap();
        let dirs = entries.iter().filter(|entry| entry.is_dir()).count() as u64;
        let files = entries.iter().filter(|entry| !entry.is_dir()).count() as u64;
        let bytes: u64 = entries.iter().filter(|e| !e.is_dir()).map(|e| e.size).sum();
        let visible_dirs = entries
            .iter()
            .filter(|entry| entry.is_dir() && !is_hidden(&entry.name))
            .count() as u64;
        let visible_files = entries
            .iter()
            .filter(|entry| !entry.is_dir() && !is_hidden(&entry.name))
            .count() as u64;
        assert_eq!(visible_dirs, 24, "папки: {:?}", entry_names(&entries));
        assert_eq!(visible_files, 4);
        assert_eq!(dirs + files, 29, "24 папки + 4 файла + скрытая .local");
        assert_eq!(bytes, 80_000_000);
        assert_eq!(
            crate::format::status_text(visible_dirs, visible_files, bytes),
            "24 папки, 4 файла · 76,3 МБ"
        );
    }

    #[test]
    fn mkdir_stat_and_duplicates() {
        let mut vfs = FakeVfs::new();
        vfs.mkdir("/home/user/Новая папка").unwrap();
        assert_eq!(
            vfs.stat("/home/user/Новая папка").unwrap().kind,
            EntryKind::Directory
        );
        assert_eq!(
            vfs.mkdir("/home/user/Новая папка"),
            Err(FsError::AlreadyExists)
        );
        assert_eq!(vfs.mkdir("/home/user/нет/такой"), Err(FsError::NotFound));
        vfs.write_file("/home/user/notes.md", b"x").unwrap();
        assert_eq!(
            vfs.mkdir("/home/user/notes.md/внутри"),
            Err(FsError::NotADirectory)
        );
    }

    #[test]
    fn write_read_and_unlink() {
        let mut vfs = FakeVfs::new();
        vfs.write_file("/home/user/notes.md", "привет".as_bytes())
            .unwrap();
        assert_eq!(
            vfs.read_file("/home/user/notes.md").unwrap(),
            "привет".as_bytes()
        );
        let stat = vfs.stat("/home/user/notes.md").unwrap();
        assert_eq!(stat.kind, EntryKind::File);
        assert_eq!(stat.size, 12);
        assert_eq!(stat.mode, 0o644);

        vfs.unlink("/home/user/notes.md").unwrap();
        assert_eq!(vfs.stat("/home/user/notes.md"), Err(FsError::NotFound));
        assert_eq!(vfs.read_file("/home/user"), Err(FsError::IsADirectory));
    }

    #[test]
    fn unlink_non_empty_directory_is_rejected() {
        let mut vfs = FakeVfs::new();
        vfs.mkdir("/home/user/Папка").unwrap();
        vfs.create_file("/home/user/Папка/файл").unwrap();
        assert!(vfs.unlink("/home/user/Папка").is_err());
        vfs.unlink("/home/user/Папка/файл").unwrap();
        vfs.unlink("/home/user/Папка").unwrap();
    }

    #[test]
    fn rename_and_copy() {
        let mut vfs = FakeVfs::new();
        vfs.write_file("/home/user/a.txt", "данные".as_bytes())
            .unwrap();
        vfs.rename("/home/user/a.txt", "/home/user/b.txt").unwrap();
        assert_eq!(vfs.stat("/home/user/a.txt"), Err(FsError::NotFound));
        assert_eq!(
            vfs.read_file("/home/user/b.txt").unwrap(),
            "данные".as_bytes()
        );
        assert_eq!(vfs.copy("/home/user/b.txt", "/home/user/c.txt"), Ok(()));
        assert_eq!(
            vfs.read_file("/home/user/c.txt").unwrap(),
            "данные".as_bytes()
        );
        let _ = vfs.copy("/home/user/b.txt", "/home/user/c.txt");
        assert!(vfs.stat("/home/user/c.txt").is_ok());
        let names = vfs.read_dir("/home/user").unwrap();
        let names: Vec<&str> = names.iter().map(|entry| entry.name.as_str()).collect();
        assert!(names.contains(&"b.txt") && names.contains(&"c.txt"));
    }

    #[test]
    fn symlink_roundtrip() {
        let mut vfs = FakeVfs::new();
        vfs.write_file("/home/user/target.txt", b"x").unwrap();
        vfs.symlink("target.txt", "/home/user/link.txt").unwrap();
        let stat = vfs.stat("/home/user/link.txt").unwrap();
        assert_eq!(stat.kind, EntryKind::Symlink);
        assert_eq!(stat.target.as_deref(), Some("target.txt"));
    }

    #[test]
    fn hidden_entries_are_listed_but_marked() {
        let mut vfs = FakeVfs::concept();
        assert_eq!(vfs.hidden_count("/home/user"), 1);
        let entries = vfs.read_dir("/home/user").unwrap();
        assert!(entries.iter().any(|entry| entry.name == ".local"));
    }

    #[test]
    fn paths_are_normalized_for_all_operations() {
        let mut vfs = FakeVfs::new();
        vfs.mkdir("/home/user/./a/../b").unwrap();
        assert!(vfs.stat("/home/user/b").is_ok());
        assert_eq!(vfs.read_dir("/home/user/Documents/../b").unwrap().len(), 0);
    }
}
