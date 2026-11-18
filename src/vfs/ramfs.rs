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
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

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
    /// Биты прав плюс setuid/setgid/sticky (`chmod(2)`); по умолчанию `0755`.
    /// Раньше ramfs не хранил метаданных, поэтому `/tmp` нельзя было сделать
    /// sticky, а не-root не мог писать в него вовсе.
    mode: AtomicU32,
    /// `(uid, gid)` владельца: созданный не-root процессом узел принадлежит ему,
    /// иначе sticky-правило `/tmp` не смогло бы отличить свой файл от чужого.
    owner: Spinlock<(u32, u32)>,
    children: Spinlock<BTreeMap<String, Arc<dyn VfsNode>>>,
}

impl RamDir {
    pub fn new(name: &str) -> Self {
        Self::new_with_mode(name, 0o755)
    }

    /// Каталог с явными битами прав: `S_ISVTX` (`0o1000`) на `/tmp` — это
    /// POSIX-контракт общего временного каталога, который проверяет
    /// `Creds::may_delete_child` на границе unlink/rename.
    pub fn new_with_mode(name: &str, mode: u32) -> Self {
        RamDir {
            name: String::from(name),
            ino: next_ino(),
            mode: AtomicU32::new(mode),
            owner: Spinlock::new((0, 0)),
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

    fn mode(&self) -> u32 {
        self.mode.load(Ordering::Relaxed)
    }

    fn owner(&self) -> (u32, u32) {
        *self.owner.lock()
    }

    fn set_mode(&self, mode: u32) -> VfsResult<()> {
        self.mode.store(mode & 0o7777, Ordering::Relaxed);
        Ok(())
    }

    fn set_owner(&self, uid: u32, gid: u32) -> VfsResult<()> {
        *self.owner.lock() = (uid, gid);
        Ok(())
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
    /// `chmod(2)`-биты; по умолчанию `0644`, как у ext2-писателя.
    mode: AtomicU32,
    /// Владелец записи: нужен sticky-проверке `/tmp` (`unlink` чужого файла).
    owner: Spinlock<(u32, u32)>,
    data: Spinlock<Vec<u8>>,
}

impl RamFile {
    pub fn new(name: &str) -> Self {
        RamFile {
            name: String::from(name),
            ino: next_ino(),
            mode: AtomicU32::new(0o644),
            owner: Spinlock::new((0, 0)),
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
    fn mode(&self) -> u32 {
        self.mode.load(Ordering::Relaxed)
    }
    fn owner(&self) -> (u32, u32) {
        *self.owner.lock()
    }
    fn set_mode(&self, mode: u32) -> VfsResult<()> {
        self.mode.store(mode & 0o7777, Ordering::Relaxed);
        Ok(())
    }
    fn set_owner(&self, uid: u32, gid: u32) -> VfsResult<()> {
        *self.owner.lock() = (uid, gid);
        Ok(())
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
