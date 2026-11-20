//! `memfd_create`-узел: анонимный файл в RAM с общей страничной памятью для
//! `mmap(MAP_SHARED)`.
//!
//! Зачем отдельный тип, а не `RamFile` (`/tmp`): `mmap(MAP_SHARED)` обязан
//! отображать **одни и те же физические кадры** в разные адресные пространства и
//! в повторные отображения одного файла. Байтовый `Vec<u8>` этого не даёт.
//! Поэтому страницы memfd — кадры PMM, живущие в `pages: page_index -> phys`:
//!
//!   * каждый кадр держит одну pin-ссылку кэша (`pmm::cow_ref`, освобождается в
//!     [`Drop`] или `truncate`, когда страница уходит из файла);
//!   * каждое отображение держит свою ссылку (`cow_ref` при `mmap`, отпускается
//!     `munmap`/exit через программный бит `SHARED_BIT` листа);
//!   * `fork` добавляет ребёнку ещё одну ссылку, не копируя кадр;
//!   * кадр возвращается в PMM только когда ушла последняя ссылка, поэтому
//!     закрытие fd не вырывает память из-под живого отображения.
//!
//! Чтение/запись через fd идут по тем же кадрам, поэтому
//! `write(2)`/`read(2)`/`ftruncate(2)` и `MAP_SHARED` видят одно содержимое, как
//! в Linux. `MAP_PRIVATE` по-прежнему получает приватную копию: `mem_sys`
//! копирует байты через `VfsNode::read`, не отображая кадры файла.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use super::{VfsError, VfsNode, VfsResult};
use crate::memory::pmm;
use crate::memory::vmm;
use crate::sync::spinlock::Spinlock;

/// Пространство inode memfd — выше ramfs (0x0054_0000) и ниже ext2, чтобы
/// `(st_dev, st_ino)` не пересекались ни с одной настоящей ФС.
static NEXT_INO: AtomicU64 = AtomicU64::new(0x0056_0000);

fn next_ino() -> u64 {
    NEXT_INO.fetch_add(1, Ordering::Relaxed)
}

struct MemFdState {
    /// Логический размер файла: `ftruncate` меняет его без обязательной
    /// аллокации страниц (Linux тоже не выделяет кадры на `ftruncate`).
    size: u64,
    /// Страницы с данными: индекс страницы (offset / 4096) -> физический кадр.
    pages: BTreeMap<u64, u64>,
}

pub struct MemFd {
    name: String,
    ino: u64,
    state: Spinlock<MemFdState>,
}

impl MemFd {
    pub fn new(name: &str) -> Self {
        MemFd {
            name: String::from(name),
            ino: next_ino(),
            state: Spinlock::new(MemFdState {
                size: 0,
                pages: BTreeMap::new(),
            }),
        }
    }

    /// Кадр страницы `index`, выделенный и обнулённый при первом обращении.
    ///
    /// Возвращённый кадр уже держит pin-ссылку кэша: вызывающий (mmap) обязан
    /// добавить собственную ссылку отображения через `pmm::cow_ref` и пометить
    /// лист `SHARED_BIT`, иначе первый `munmap` освободит кадр под файлом.
    fn page_for_mapping(&self, index: u64) -> Option<u64> {
        let mut st = self.state.lock();
        if let Some(&frame) = st.pages.get(&index) {
            return Some(frame);
        }
        let frame = pmm::alloc_frame()?;
        // SAFETY: кадр только что выделен из PMM и отображён в HHDM; пишем ровно
        // одну страницу по валидному выровненному указателю.
        unsafe {
            core::ptr::write_bytes(vmm::phys_to_virt(frame) as *mut u8, 0, 4096);
        }
        // Pin кэша: файл владеет страницей, пока она в `pages`.
        pmm::cow_ref(frame);
        st.pages.insert(index, frame);
        Some(frame)
    }

    /// Освободить одну страницу кэша (вызывается из `truncate`/`Drop`).
    fn unpin_page(frame: u64) {
        if pmm::cow_unref(frame) {
            pmm::free_frame(frame);
        }
    }

    fn copy_in(&self, page: u64, offset_in_page: usize, dst: &mut [u8]) {
        let frame = {
            let st = self.state.lock();
            st.pages.get(&page).copied()
        };
        match frame {
            Some(frame) => {
                // SAFETY: страница файла — кадр PMM, доступный через HHDM;
                // `offset_in_page + dst.len() <= 4096` гарантировано вызывающим.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        (vmm::phys_to_virt(frame) as *const u8).add(offset_in_page),
                        dst.as_mut_ptr(),
                        dst.len(),
                    );
                }
            }
            // Дыра/страница за EOF — нули, как у sparse-файла.
            None => dst.fill(0),
        }
    }

    fn copy_out(&self, page: u64, offset_in_page: usize, src: &[u8]) -> Option<()> {
        let frame = self.page_for_mapping(page)?;
        // SAFETY: `page_for_mapping` вернул валидный кадр; `offset_in_page +
        // src.len() <= 4096` гарантировано вызывающим.
        unsafe {
            core::ptr::copy_nonoverlapping(
                src.as_ptr(),
                (vmm::phys_to_virt(frame) as *mut u8).add(offset_in_page),
                src.len(),
            );
        }
        Some(())
    }
}

impl VfsNode for MemFd {
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
        self.state.lock().size
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let size = self.state.lock().size;
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let want = core::cmp::min(buf.len() as u64, size - offset) as usize;
        let mut done = 0usize;
        while done < want {
            let pos = offset + done as u64;
            let page = pos / 4096;
            let in_page = (pos % 4096) as usize;
            let chunk = core::cmp::min(want - done, 4096 - in_page);
            self.copy_in(page, in_page, &mut buf[done..done + chunk]);
            done += chunk;
        }
        Ok(want)
    }

    fn write(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(VfsError::InvalidArgument)?;
        {
            let mut st = self.state.lock();
            if end > st.size {
                st.size = end;
            }
        }
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            let page = pos / 4096;
            let in_page = (pos % 4096) as usize;
            let chunk = core::cmp::min(buf.len() - done, 4096 - in_page);
            self.copy_out(page, in_page, &buf[done..done + chunk])
                .ok_or(VfsError::IoError)?;
            done += chunk;
        }
        Ok(buf.len())
    }

    fn truncate(&self, size: u64) -> VfsResult<()> {
        let mut removed: Vec<u64> = Vec::new();
        {
            let mut st = self.state.lock();
            st.size = size;
            // Страница, начинающаяся на/после нового EOF, больше не принадлежит
            // файлу. Последняя частичная страница остаётся: её байты за EOF
            // невидимы через read/size, а `ftruncate` вверх не обязан их обнулять
            // (Linux сохраняет содержимое страницы).
            let first_dead = size.div_ceil(4096);
            let dead: Vec<u64> = st
                .pages
                .keys()
                .copied()
                .filter(|idx| *idx >= first_dead)
                .collect();
            for idx in dead {
                if let Some(frame) = st.pages.remove(&idx) {
                    removed.push(frame);
                }
            }
        }
        for frame in removed {
            Self::unpin_page(frame);
        }
        Ok(())
    }

    /// Кадр страницы для `mmap(MAP_SHARED)`.
    fn shared_page(&self, index: u64) -> Option<u64> {
        self.page_for_mapping(index)
    }

    fn is_memfd(&self) -> bool {
        true
    }
}

impl Drop for MemFd {
    fn drop(&mut self) {
        let pages = core::mem::take(&mut self.state.lock().pages);
        for (_, frame) in pages {
            Self::unpin_page(frame);
        }
    }
}
