//! Минимальная in-memory ФС для `/tmp`.
//!
//! Кроме неё `create_dir` в дереве никто не реализует (корень ext2 в основном
//! read-only, а создание каталога там — настоящая on-disk аллокация), но
//! Linux-userspace рассчитывает на записываемый `/tmp`: `vim_mktempdir` в nvim
//! (mkdir /tmp/nvim.XXXXXX), mkstemp, пути unix-сокетов. Крошечная ramfs даёт
//! всю эту POSIX-поверхность, не трогая драйвер ext2.
//!
//! Устройство: каталоги держат `BTreeMap<name, Arc<dyn VfsNode>>` под спинлоком;
//! файлы — `Spinlock<Vec<u8>>`. Каждый узел получает уникальный `fs_ino` из
//! глобального счётчика, чтобы идентичность `(st_dev, st_ino)` оставалась
//! корректной (glibc дедуплицирует по этой паре).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use super::{VfsError, VfsNode, VfsResult};
use crate::sync::spinlock::Spinlock;

/// Номера inode ram-узлов начинаются высоко над диапазоном ext2, чтобы пары
/// никогда не совпали с настоящими on-disk inode.
static NEXT_INO: AtomicU64 = AtomicU64::new(0x0054_0000);

fn next_ino() -> u64 {
    NEXT_INO.fetch_add(1, Ordering::Relaxed)
}

// ─── каталог ───

pub struct RamDir {
    name: String,
    ino: u64,
    children: Spinlock<BTreeMap<String, Arc<dyn VfsNode>>>,
}

impl RamDir {
    pub fn new(name: &str) -> Self {
        RamDir {
            name: String::from(name),
            ino: next_ino(),
            children: Spinlock::new(BTreeMap::new()),
        }
    }
}

impl VfsNode for RamDir {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_directory(&self) -> bool {
        true
    }
    fn fs_ino(&self) -> u64 {
        self.ino
    }

    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        Ok(self.children.lock().values().cloned().collect())
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        self.children
            .lock()
            .get(name)
            .cloned()
            .ok_or(VfsError::NotFound)
    }

    fn create_dir(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        if name.is_empty() || name.contains('/') {
            return Err(VfsError::InvalidArgument);
        }
        let mut ch = self.children.lock();
        if ch.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        let node = Arc::new(RamDir::new(name)) as Arc<dyn VfsNode>;
        ch.insert(String::from(name), Arc::clone(&node));
        Ok(node)
    }

    fn create_file(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        if name.is_empty() || name.contains('/') {
            return Err(VfsError::InvalidArgument);
        }
        let mut ch = self.children.lock();
        if ch.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        let node = Arc::new(RamFile::new(name)) as Arc<dyn VfsNode>;
        ch.insert(String::from(name), Arc::clone(&node));
        Ok(node)
    }

    fn remove(&self, name: &str) -> VfsResult<()> {
        let mut ch = self.children.lock();
        match ch.get(name) {
            None => Err(VfsError::NotFound),
            Some(node) => {
                if node.is_directory() {
                    if let Ok(list) = node.readdir() {
                        if !list.is_empty() {
                            // Linux сказал бы ENOTEMPTY; NotSupported сохраняет
                            // прежнее отображение errno для вызывающего.
                            return Err(VfsError::NotSupported);
                        }
                    }
                }
                ch.remove(name);
                Ok(())
            }
        }
    }
}

// ─── обычный файл ───

pub struct RamFile {
    name: String,
    ino: u64,
    data: Spinlock<Vec<u8>>,
}

impl RamFile {
    pub fn new(name: &str) -> Self {
        RamFile {
            name: String::from(name),
            ino: next_ino(),
            data: Spinlock::new(Vec::new()),
        }
    }
}

impl VfsNode for RamFile {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn fs_ino(&self) -> u64 {
        self.ino
    }
    fn size(&self) -> u64 {
        self.data.lock().len() as u64
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let data = self.data.lock();
        let off = offset as usize;
        if off >= data.len() {
            return Ok(0);
        }
        let n = core::cmp::min(buf.len(), data.len() - off);
        buf[..n].copy_from_slice(&data[off..off + n]);
        Ok(n)
    }

    fn write(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        let mut data = self.data.lock();
        let off = offset as usize;
        let end = off
            .checked_add(buf.len())
            .ok_or(VfsError::InvalidArgument)?;
        let old_len = data.len();
        if old_len < end {
            // Дыра sparse-записи (off > len) заливается нулями через resize, как
            // требует POSIX для записи за EOF. Сначала try_reserve, чтобы
            // огромный пользовательский offset не исчерпал фиксированную кучу
            // ядра (ошибка аллокации — аборт ядра, а не возврат ошибки).
            data.try_reserve(end - old_len)
                .map_err(|_| VfsError::IoError)?;
            data.resize(end, 0);
        }
        data[off..end].copy_from_slice(buf);
        Ok(buf.len())
    }

    fn truncate(&self, size: u64) -> VfsResult<()> {
        let mut data = self.data.lock();
        let old_len = data.len();
        if size as usize > old_len {
            data.try_reserve(size as usize - old_len)
                .map_err(|_| VfsError::IoError)?;
        }
        data.resize(size as usize, 0);
        Ok(())
    }
}
