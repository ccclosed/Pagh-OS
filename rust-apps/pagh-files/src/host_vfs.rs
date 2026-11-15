//! Файловая система поверх реальных вызовов `std::fs`.
//!
//! На Pagh обычные Linux-вызовы (`opendir/readdir/stat/open/unlink/rename`)
//! обслуживает совместимый слой ядра, поэтому одна и та же реализация работает
//! и на хосте (для отладки), и в гостевой системе. Пути абсолютные — приложение
//! не делает chroot, как и любой файловый менеджер.

use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

use crate::vfs::{file_name, normalize, DirEntry, EntryKind, FsError, Result, Stat, Vfs};

/// ФС поверх `std::fs`.
pub struct HostVfs {
    home: String,
}

impl Default for HostVfs {
    fn default() -> Self {
        HostVfs::new()
    }
}

impl HostVfs {
    /// Домашний каталог из `HOME` (или `/home/user`).
    pub fn new() -> HostVfs {
        let home = std::env::var("HOME")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "/home/user".to_string());
        HostVfs {
            home: normalize(&home),
        }
    }

    /// ФС с явно заданным домашним каталогом.
    pub fn with_home(home: impl Into<String>) -> HostVfs {
        HostVfs {
            home: normalize(&home.into()),
        }
    }
}

fn map_error(error: io::Error) -> FsError {
    match error.kind() {
        io::ErrorKind::NotFound => FsError::NotFound,
        io::ErrorKind::AlreadyExists => FsError::AlreadyExists,
        io::ErrorKind::PermissionDenied => FsError::PermissionDenied,
        io::ErrorKind::InvalidInput => FsError::InvalidName,
        _ => FsError::Io(error.to_string()),
    }
}

fn kind_of(metadata: &fs::Metadata) -> EntryKind {
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_file() {
        EntryKind::File
    } else if file_type.is_fifo()
        || file_type.is_socket()
        || file_type.is_block_device()
        || file_type.is_char_device()
    {
        EntryKind::Device
    } else {
        EntryKind::Other
    }
}

impl Vfs for HostVfs {
    fn home(&self) -> &str {
        &self.home
    }

    fn read_dir(&mut self, path: &str) -> Result<Vec<DirEntry>> {
        let path = normalize(path);
        let read = fs::read_dir(Path::new(&path)).map_err(map_error)?;
        let mut entries = Vec::new();
        for item in read {
            let item = item.map_err(map_error)?;
            let name = item.file_name().to_string_lossy().into_owned();
            let path = item.path();
            // Не разыменовываем ссылки: тип и размер — у самой ссылки.
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            entries.push(DirEntry {
                name,
                kind: kind_of(&metadata),
                size: metadata.size(),
                modified: metadata.mtime().max(0) as u64,
            });
        }
        Ok(entries)
    }

    fn stat(&mut self, path: &str) -> Result<Stat> {
        let path = normalize(path);
        let metadata = fs::symlink_metadata(Path::new(&path)).map_err(map_error)?;
        let kind = kind_of(&metadata);
        let target = if kind == EntryKind::Symlink {
            fs::read_link(Path::new(&path))
                .ok()
                .map(|link| link.to_string_lossy().into_owned())
        } else {
            None
        };
        Ok(Stat {
            path: path.clone(),
            name: file_name(&path).to_string(),
            kind,
            size: metadata.size(),
            modified: metadata.mtime().max(0) as u64,
            mode: metadata.mode() & 0o777,
            target,
        })
    }

    fn mkdir(&mut self, path: &str) -> Result<()> {
        fs::create_dir(Path::new(&normalize(path))).map_err(map_error)
    }

    fn write_file(&mut self, path: &str, contents: &[u8]) -> Result<()> {
        fs::write(Path::new(&normalize(path)), contents).map_err(map_error)
    }

    fn read_file(&mut self, path: &str) -> Result<Vec<u8>> {
        fs::read(Path::new(&normalize(path))).map_err(map_error)
    }

    fn unlink(&mut self, path: &str) -> Result<()> {
        let path = normalize(path);
        let metadata = fs::symlink_metadata(Path::new(&path)).map_err(map_error)?;
        if metadata.is_dir() {
            fs::remove_dir(Path::new(&path)).map_err(map_error)
        } else {
            fs::remove_file(Path::new(&path)).map_err(map_error)
        }
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        fs::rename(Path::new(&normalize(from)), Path::new(&normalize(to))).map_err(map_error)
    }

    fn copy(&mut self, from: &str, to: &str) -> Result<()> {
        let from = normalize(from);
        let to = normalize(to);
        let metadata = fs::symlink_metadata(Path::new(&from)).map_err(map_error)?;
        if metadata.is_dir() {
            copy_tree(Path::new(&from), Path::new(&to)).map_err(map_error)
        } else {
            fs::copy(Path::new(&from), Path::new(&to))
                .map(|_| ())
                .map_err(map_error)
        }
    }

    fn symlink(&mut self, target: &str, link: &str) -> Result<()> {
        std::os::unix::fs::symlink(target, Path::new(&normalize(link))).map_err(map_error)
    }
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir(to)?;
    for item in fs::read_dir(from)? {
        let item = item?;
        let source = item.path();
        let target = to.join(item.file_name());
        let metadata = fs::symlink_metadata(&source)?;
        if metadata.is_dir() {
            copy_tree(&source, &target)?;
        } else if metadata.file_type().is_symlink() {
            let link = fs::read_link(&source)?;
            std::os::unix::fs::symlink(link, &target)?;
        } else {
            fs::copy(&source, &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "pagh-files-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn roundtrip_over_real_files() {
        let dir = temp_dir("vfs");
        let mut vfs = HostVfs::with_home(dir.to_string_lossy());
        vfs.mkdir(&format!("{}/Папка", vfs.home())).unwrap();
        vfs.write_file(
            &format!("{}/Папка/файл.txt", vfs.home()),
            "привет".as_bytes(),
        )
        .unwrap();
        let entries = vfs.read_dir(&format!("{}/Папка", vfs.home())).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "файл.txt");
        assert_eq!(entries[0].size, 12);
        let stat = vfs.stat(&format!("{}/Папка/файл.txt", vfs.home())).unwrap();
        assert_eq!(stat.kind, EntryKind::File);
        assert!(stat.mode & 0o600 == 0o600);

        vfs.rename(
            &format!("{}/Папка/файл.txt", vfs.home()),
            &format!("{}/Папка/второй.txt", vfs.home()),
        )
        .unwrap();
        vfs.copy(
            &format!("{}/Папка/второй.txt", vfs.home()),
            &format!("{}/Папка/копия.txt", vfs.home()),
        )
        .unwrap();
        assert!(vfs.stat(&format!("{}/Папка/копия.txt", vfs.home())).is_ok());
        vfs.unlink(&format!("{}/Папка/второй.txt", vfs.home()))
            .unwrap();
        vfs.unlink(&format!("{}/Папка", vfs.home())).unwrap_err();
        assert_eq!(
            vfs.stat(&format!("{}/нет-такого", vfs.home())),
            Err(FsError::NotFound)
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn symlinks_are_reported_with_target() {
        let dir = temp_dir("link");
        let mut vfs = HostVfs::with_home(dir.to_string_lossy());
        vfs.write_file(&format!("{}/target.txt", vfs.home()), b"x")
            .unwrap();
        vfs.symlink("target.txt", &format!("{}/link.txt", vfs.home()))
            .unwrap();
        let stat = vfs.stat(&format!("{}/link.txt", vfs.home())).unwrap();
        assert_eq!(stat.kind, EntryKind::Symlink);
        assert_eq!(stat.target.as_deref(), Some("target.txt"));
        let _ = fs::remove_dir_all(dir);
    }
}
