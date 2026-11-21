//! `/dev/block/<name>` — read-only VFS-вид зарегистрированных блочных устройств.
//!
//! Зачем: `mount(2)` принимает имя устройства, но диагностике (`hexdump`,
//! `dd if=... count=1`) нужен обычный путь. Узлы **только для чтения**: запись
//! в сырой блок смонтированного тома обошла бы WAL и порядок данных ext2, а
//! такая дыра не окупается диагностикой. `write` здесь — `EROFS`-по-смыслу
//! (`NotSupported`), а не «тихо записать».
//!
//! Каталог не хранит список: `readdir`/`lookup` спрашивают
//! [`crate::drivers::list_blocks`] в момент вызова, поэтому устройство,
//! появившееся после `vfs::init`, видно без пересоздания дерева. Каждый
//! [`BlockNode`] держит `Arc<dyn BlockDevice>` на момент `lookup`: I/O идёт уже
//! без глобального реестра, как и у смонтированной ФС (см. инвариант 4 в
//! `AGENTS.md`).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::{VfsError, VfsNode, VfsResult};
use crate::drivers::BlockDevice;

/// `/dev/block` — каталог всех зарегистрированных блочных устройств.
pub struct BlockDirectory;

impl VfsNode for BlockDirectory {
    fn name(&self) -> &str {
        "block"
    }

    fn is_directory(&self) -> bool {
        true
    }

    fn readdir(&self) -> VfsResult<Vec<Arc<dyn VfsNode>>> {
        let mut out = Vec::new();
        for (name, _sectors) in crate::drivers::list_blocks() {
            if let Some(dev) = crate::drivers::get_block(&name) {
                out.push(Arc::new(BlockNode { name, dev }) as Arc<dyn VfsNode>);
            }
        }
        Ok(out)
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        let Some(dev) = crate::drivers::get_block(name) else {
            return Err(VfsError::NotFound);
        };
        Ok(Arc::new(BlockNode {
            name: String::from(name),
            dev,
        }) as Arc<dyn VfsNode>)
    }
}

/// Один блочный узел, привязанный к конкретному устройству.
struct BlockNode {
    name: String,
    dev: Arc<dyn BlockDevice>,
}

impl VfsNode for BlockNode {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_directory(&self) -> bool {
        false
    }

    fn is_blockdev(&self) -> bool {
        true
    }

    fn size(&self) -> u64 {
        self.dev.sector_count().saturating_mul(512)
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let total = self.size();
        if offset >= total {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(total - offset) as usize;
        let first = offset / 512;
        let last = (offset + want as u64 - 1) / 512;
        let sectors = (last - first + 1) as usize;
        let mut tmp = alloc::vec![0u8; sectors * 512];
        self.dev
            .read_block(first, &mut tmp)
            .map_err(|_| VfsError::IoError)?;
        let start = (offset % 512) as usize;
        buf[..want].copy_from_slice(&tmp[start..start + want]);
        Ok(want)
    }

    fn write(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::NotSupported)
    }

    /// Сырой блок — root-only: чтение чужих данных не должно быть доступно
    /// непривилегированному процессу только потому, что ему виден `/dev`.
    fn mode(&self) -> u32 {
        0o600
    }
}

/// `/dev/block` как `Arc<dyn VfsNode>`.
pub fn block_dir() -> Arc<dyn VfsNode> {
    Arc::new(BlockDirectory) as Arc<dyn VfsNode>
}
