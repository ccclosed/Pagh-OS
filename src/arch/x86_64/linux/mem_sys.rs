//! Эффектные обработчики memory-management syscall'ов Linux.
//!
//! **Ядерная** половина компонента `mem`: связывает чистые планировщики
//! [`super::mem`] (`plan_brk`/`plan_mmap`/`plan_munmap`/`prot_to_flags`) с
//! [`VmRegionSet`] работающего `Compat_Process` и с таблицами страниц через
//! `memory::vmm`/`memory::pmm`.
//!
//! Модуль вынесен в отдельный файл намеренно: `mem.rs` включается в `host-tests`
//! через `#[path]`, чтобы его чистые планировщики проверялись свойствами на хосте
//! (R11.6). Эти обработчики используют ядерные API paging, которых на хосте нет,
//! поэтому `mem.rs` остаётся чисто host-тестируемым, а этот файл компилируется
//! только в ядре.
//!
//! ## Адресное пространство
//!
//! Во время syscall активный CR3 — пользовательский PML4 вызывающего, поэтому
//! `vmm::map`/`vmm::release_user_leaf`/`vmm::virt_to_phys` работают напрямую с его
//! адресным пространством — переключать CR3 здесь не нужно. PMM/VMM используют
//! свои краткие спинлоки и никогда не ждут прерывания устройства, поэтому такую
//! работу безопасно выполнять внутри замыкания
//! [`crate::task::compat::with_current_compat`] (держащего лок `COMPAT_STATES`).
//!
//! ## Откат при OOM (R3.4, R4.4)
//!
//! Рост `brk` и `mmap` аллоцируют кадры постранично; при первом отказе
//! `pmm::alloc_frame` (или `vmm::map`) каждая уже отображённая в этом вызове
//! страница снимается через единый путь освобождения листа
//! (`vmm::release_user_leaf`, уважающий владение COW), и операция возвращается с
//! неизменённым состоянием памяти процесса: `brk` сообщает прежний break,
//! `mmap` возвращает `-ENOMEM`.
#![allow(dead_code)]

use alloc::sync::Arc;
use alloc::vec::Vec;

use x86_64::structures::paging::PageTableFlags;

use crate::drivers::framebuffer::FbInfo;
use crate::memory::layout::{USER_STACK_PAGES, USER_STACK_TOP};
use crate::memory::{pat, pmm, vmm};
use crate::task::compat;
use crate::vfs::VfsNode;

use super::errno::Errno;
use super::mem::{
    plan_brk, plan_mmap_base, plan_munmap, prot_to_flags, range_is_free, BrkOutcome, MmapBacking,
    MmapRegion, MunmapPlan, VmRegionSet, MAP_ANONYMOUS, MAP_PRIVATE, MAP_SHARED, PROT_READ,
};
use super::validate::USER_ADDR_MAX;

/// Архитектурный размер страницы (4 КиБ).
const PAGE_SIZE: u64 = 4096;

/// Проверить, что диапазон memory-syscall'а лежит в пользовательской половине, и
/// вернуть число страниц.
///
/// Это гейт диапазона для `mprotect`/`munmap`/`mremap`. Эти syscall'ы раньше
/// доверяли `addr`/`len`: `mprotect` на *ядерном* виртуальном адресе обходил
/// таблицы страниц самого ядра, переотображал ядерные страницы с
/// `USER_ACCESSIBLE` (через `leaf_flags`) и для `PROT_NONE` возвращал ядерные
/// кадры в PMM — примитив памяти ring 3 → ring 0. Чистый хелпер
/// [`check_user_range`] уже существовал для copy-in/copy-out; эта обёртка
/// добавляет арифметику страниц, нужную memory-syscall'ам.
///
/// `EINVAL` для невыровненной базы или не-пользовательского/переполняющегося
/// диапазона (операции с таблицами ниже предполагают выровненные на 4 КиБ
/// пользовательские VA), иначе число страниц (`len == 0` → 0 страниц, no-op).
fn user_pages(addr: u64, len: u64) -> Result<u64, Errno> {
    if addr & (PAGE_SIZE - 1) != 0 {
        return Err(Errno::EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }
    if super::validate::check_user_range(addr, len) != super::validate::PtrCheck::Ok {
        return Err(Errno::EINVAL);
    }
    len.checked_add(PAGE_SIZE - 1)
        .map(|n| n / PAGE_SIZE)
        .ok_or(Errno::EINVAL)
}

/// Обнулить физический кадр 4 КиБ по `frame` через HHDM-алиас.
fn zero_frame(frame: u64) {
    // SAFETY: `frame` только что выделен из PMM и отображён в окно HHDM,
    // поэтому `phys_to_virt(frame)` — валидный, записываемый, выровненный на
    // страницу указатель.
    unsafe {
        core::ptr::write_bytes(vmm::phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE as usize);
    }
}

/// Построить флаги листового PTE пользовательской страницы данных из
/// `(writable, nx)` (всегда `PRESENT | USER_ACCESSIBLE`).
fn leaf_flags(writable: bool, nx: bool) -> PageTableFlags {
    let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if writable {
        flags |= PageTableFlags::WRITABLE;
    }
    if nx {
        flags |= PageTableFlags::NO_EXECUTE;
    }
    flags
}

/// Отобразить заполненные нулями страницы на `[from, to)` (выровнено по
/// страницам), пропуская уже присутствующие. При первом отказе
/// аллокации/отображения снять все страницы, отображённые этим вызовом, и вернуть
/// `false` (вызывающий оставляет состояние неизменным).
fn map_zeroed_range(from: u64, to: u64, flags: PageTableFlags) -> bool {
    let mut mapped: Vec<u64> = Vec::new();
    let mut page = from;
    let mut ok = true;
    while page < to {
        if vmm::virt_to_phys(page).is_none() {
            match pmm::alloc_frame() {
                Some(frame) => {
                    zero_frame(frame);
                    if vmm::map(frame, page, flags).is_err() {
                        // Кадр не упомянут ни в одном PTE; освободить его напрямую.
                        pmm::free_frame(frame);
                        ok = false;
                        break;
                    }
                    mapped.push(page);
                }
                None => {
                    ok = false;
                    break;
                }
            }
        }
        page += PAGE_SIZE;
    }
    if !ok {
        unwind(&mapped);
    }
    ok
}

/// Снять отображение набора страниц, отображённых этим вызовом, вернув их кадры
/// в PMM.
fn unwind(pages: &[u64]) {
    for &p in pages {
        // Единый путь освобождения листа: снимает кадры, отображённые этим
        // вызовом (они никогда не COW-разделены, поэтому освобождаются), но
        // COW-лист по тому же VA учитывается, а не освобождается за спиной
        // другого адресного пространства.
        let _ = vmm::release_user_leaf(p);
    }
}

/// `brk` (12): запросить/сдвинуть program break (R3.1–R3.6). При росте, который
/// не удаётся подпереть физической памятью, break остаётся неизменным (R3.4).
pub fn sys_brk(addr: u64) -> Result<u64, Errno> {
    compat::with_current_compat(|cs| brk_impl(&mut *cs.vm.lock(), addr)).ok_or(Errno::EINVAL)
}

fn brk_impl(vm: &mut VmRegionSet, requested: u64) -> u64 {
    // Куча brk растёт вверх в том же диапазоне нижней половины, что использует
    // bump-аллокатор mmap, поэтому рост не должен наткнуться на отслеживаемый
    // mmap-регион: потолок урезается до младшей базы региона выше текущего break
    // (Linux держит эти области раздельно; провал роста — видимый для POSIX
    // результат).
    let ceiling = vm
        .mmaps
        .iter()
        .map(|r| r.base)
        .filter(|b| *b > vm.current_brk)
        .min()
        .unwrap_or(USER_ADDR_MAX);
    if requested > vm.current_brk && requested >= ceiling {
        return vm.current_brk;
    }
    match plan_brk(vm.initial_brk, vm.current_brk, requested) {
        BrkOutcome::Unchanged(v) => v,
        BrkOutcome::Shrink(v) => {
            vm.current_brk = v;
            v
        }
        BrkOutcome::Grow {
            new_brk, map_from, ..
        } => {
            // Лениво: без жадного отображения. Страницы в
            // [brk_lazy_from, page_up(new_brk)) подпираются обработчиком page
            // fault по первому касанию; записанная нижняя граница только
            // опускается.
            vm.brk_lazy_from = vm.brk_lazy_from.min(map_from);
            vm.current_brk = new_brk;
            new_brk
        }
    }
}

/// Дополнительные биты флагов `mmap` Linux, обрабатываемые здесь (сверх
/// `MAP_PRIVATE`/`MAP_SHARED`/`MAP_ANONYMOUS` из модуля чистых планировщиков).
const MAP_FIXED: u32 = 0x10;
/// Биты-подсказки/только-учёт, принимаются и игнорируются:
/// `MAP_DENYWRITE | MAP_EXECUTABLE | MAP_NORESERVE | MAP_POPULATE | MAP_STACK`.
const MAP_IGNORED: u32 = 0x800 | 0x1000 | 0x4000 | 0x8000 | 0x2_0000;

/// `mmap` (9): отобразить приватный регион (R4.1, R4.2) в трёх формах, которые
/// реально выдаёт динамический загрузчик (`ld.so`, отображающий разделяемые
/// библиотеки):
///
///   * анонимный `MAP_PRIVATE|MAP_ANONYMOUS` — заполненный нулями bump-регион
///     (исходное поведение, R4.2);
///   * file-backed `MAP_PRIVATE` с `fd`/`off` — регион подпирается свежими
///     обнулёнными кадрами, а байты файла `[off, off + len)` копируются жадно.
///     Разделяемых файловых отображений в этом ядре нет, так что жадная приватная
///     копия — *точная* семантика `MAP_PRIVATE`, а страницы за EOF остаются
///     нулевыми (bss-хвост, которого ждёт загрузчик);
///   * `MAP_FIXED` по выровненному адресу — заменяет любые существующие страницы
///     диапазона, точно как Linux (загрузчик отображает каждый ELF-сегмент
///     `MAP_FIXED` поверх диапазона, зарезервированного первым отображением).
///
/// Возвращает базу отображения; `-EINVAL` для неверных запросов и действительно
/// неподдерживаемых форм (`MAP_SHARED`, неизвестные биты — отказать громко, а не
/// отобразить неверно); `-EBADF`, когда file-backed запрос называет дескриптор,
/// не являющийся открытым regular-файлом; `-ENOMEM`, когда регион нельзя
/// разместить ниже пользовательского потолка или подпереть кадрами (R4.4).
pub fn sys_mmap(
    addr: u64,
    len: u64,
    prot: u64,
    flags: u64,
    fd: u64,
    off: u64,
) -> Result<u64, Errno> {
    let flags = flags as u32;
    let shared = flags & MAP_SHARED != 0;
    let private = flags & MAP_PRIVATE != 0;
    // Ровно один из MAP_SHARED/MAP_PRIVATE, как в Linux: оба или ни одного —
    // EINVAL (иначе неясно, чьи записи должны быть видны).
    if len == 0 || shared == private {
        return Err(Errno::EINVAL);
    }
    if flags & !(MAP_PRIVATE | MAP_SHARED | MAP_ANONYMOUS | MAP_FIXED | MAP_IGNORED) != 0 {
        return Err(Errno::EINVAL);
    }
    let anon = flags & MAP_ANONYMOUS != 0;
    let fixed = flags & MAP_FIXED != 0;
    // `offset` имеет смысл только у файла; анонимное отображение в Linux
    // принимает только 0.
    if anon && off != 0 {
        return Err(Errno::EINVAL);
    }

    // File-backed: разрешить и клонировать узел (и его путь VFS, для
    // `/proc/<pid>/maps`) ДО взятия compat-лока, как в `read`/`pread64` (I/O VFS
    // должен идти с отпущенным локом).
    let mut backing: Option<MmapBacking> = None;
    let node = if anon {
        None
    } else {
        if off & (PAGE_SIZE - 1) != 0 {
            return Err(Errno::EINVAL);
        }
        if (fd as i64) < 0 {
            return Err(Errno::EBADF);
        }
        match super::io_sys::file_backing_for_fd(fd as u32) {
            Some((n, path)) => {
                backing = Some(MmapBacking {
                    path,
                    // Та же идентичность, что `stat(2)`/`fdinfo` сообщают для
                    // этого файла.
                    ino: super::io_sys::node_ino(&n),
                    file_offset: off,
                });
                Some(n)
            }
            None => return Err(Errno::EBADF),
        }
    };

    let pages = len
        .checked_add(PAGE_SIZE - 1)
        .map(|n| n / PAGE_SIZE)
        .ok_or(Errno::ENOMEM)?;
    let span = pages.checked_mul(PAGE_SIZE).ok_or(Errno::ENOMEM)?;
    let (writable, nx) = prot_to_flags(prot as u32);
    let prot_none = prot == 0;

    // `/dev/fb0`: mmap отображает физический scanout по MMIO, а не копию
    // «содержимого файла». Только MAP_SHARED: приватная копия экрана не имеет
    // смысла и не должна выглядеть работающей.
    if let Some(info) = node.as_ref().and_then(|n| n.fb_info()) {
        if !shared {
            return Err(Errno::EINVAL);
        }
        return mmap_fb(info, &backing, fixed, addr, len, writable, nx, off);
    }

    // `/dev/vtconsole0`: read-only shared снимок kernel-VT/shell. Адресное
    // пространство пользователя получает те же кадры, что пишет ядро, но PTE
    // без WRITABLE и с `VT_RO_BIT`; mprotect(PROT_WRITE) отвергается.
    if let Some(info) = node.as_ref().and_then(|n| n.vtconsole_info()) {
        if !shared || prot != PROT_READ as u64 || off != 0 || len == 0 {
            return Err(Errno::EINVAL);
        }
        if len > info.total_bytes as u64 {
            return Err(Errno::EINVAL);
        }
        let node = node.as_ref().expect("vtconsole node");
        return mmap_vtconsole(node, &backing, fixed, addr, len);
    }

    // MAP_SHARED: общие физические кадры (memfd) либо общие анонимные страницы.
    if shared {
        // Запись в MAP_SHARED требует открытого на запись файла (Linux).
        if !anon && writable {
            let acc = super::io_sys::fd_open_flags(fd as u32).unwrap_or(0) & 0o3;
            if acc == 0 {
                return Err(Errno::EACCES);
            }
        }
        return mmap_shared(
            node, backing, anon, fixed, addr, len, prot, writable, nx, off,
        );
    }

    // Фаза 1 (под compat-локом): разместить регион, подпереть обнулёнными
    // кадрами, если жадно, записать в отслеживаемый набор.
    let base = compat::with_current_compat(|cs| -> Result<u64, Errno> {
        let vm = &mut cs.vm.lock();
        let base = if fixed {
            if addr == 0 || addr & (PAGE_SIZE - 1) != 0 {
                return Err(Errno::EINVAL);
            }
            addr
        } else {
            // Размещение first-fit: переиспользовать освобождённые дыры прежде
            // чем расширяться за отметку максимума (Linux переиспользует VA;
            // bump-only аллокатор заставляет долгоживущие процессы упереться в
            // USER_ADDR_MAX и умереть от ENOMEM при свободной памяти).
            plan_mmap_base(&vm.mmaps, vm.mmap_floor, pages, USER_ADDR_MAX).ok_or(Errno::ENOMEM)?
        };
        let end = base.checked_add(span).ok_or(Errno::ENOMEM)?;
        if end > USER_ADDR_MAX {
            return Err(Errno::ENOMEM);
        }
        if fixed {
            // MAP_FIXED заменяет то, что было: освободить старые листья, чтобы
            // `map_zeroed_range` подпёр весь диапазон свежими обнулёнными
            // страницами, и вырезать заменённый диапазон из отслеживаемых
            // регионов.
            for i in 0..pages {
                let page = base + i * PAGE_SIZE;
                // Заменённые страницы могут быть COW-разделены с другим
                // адресным пространством (загрузчик переотображает поверх
                // форкнутого образа): unref, а не освобождение кадра, который
                // другая сторона ещё отображает.
                let _ = vmm::release_user_leaf(page);
            }
            cut_regions(&mut vm.mmaps, base, pages);
        }
        // Анонимные регионы (и PROT_NONE-резервы любого вида) ленивы: здесь
        // записывается только регион; обработчик page fault подпирает страницы по
        // первому касанию. File-backed регионы с правами доступа сохраняют жадную
        // копию — кеша страниц, из которого их можно было бы подгружать, нет.
        if !anon && !prot_none && !map_zeroed_range(base, end, leaf_flags(writable, nx)) {
            // OOM: существующие отображения не тронуты (R4.4).
            return Err(Errno::ENOMEM);
        }
        vm.mmaps.push(MmapRegion {
            base,
            pages,
            writable,
            nx,
            prot: prot as u32,
            anon,
            backing: backing.clone(),
        });
        if !fixed && end > vm.mmap_next_hint {
            vm.mmap_next_hint = end;
        }
        Ok(base)
    })
    .unwrap_or(Err(Errno::EINVAL))?;

    // Фаза 2 (лок отпущен): скопировать байты файла через HHDM-алиас.
    // Только что установленные PTE могут быть read-only и/или NX под финальными
    // защитами, а CR0.WP заставляет supervisor-записи уважать read-only
    // пользовательские PTE, поэтому копирование идёт через всегда записываемый
    // HHDM-алиас, а не через пользовательский виртуальный адрес.
    if let Some(node) = node {
        let size = node.size();
        let mut buf = [0u8; PAGE_SIZE as usize];
        for i in 0..pages {
            let file_off = match off.checked_add(i * PAGE_SIZE) {
                Some(o) => o,
                None => break,
            };
            if file_off >= size {
                break; // Остаток региона остаётся нулевым (bss-хвост).
            }
            let want = core::cmp::min(PAGE_SIZE, size - file_off) as usize;
            let n = node
                .read(file_off, &mut buf[..want])
                .map_err(|_| Errno::EINVAL)?;
            if n == 0 {
                break;
            }
            let page = base + i * PAGE_SIZE;
            if let Some(phys) = vmm::virt_to_phys(page) {
                // SAFETY: `page` отображена именно этим вызовом в свежий кадр
                // 4 КиБ; HHDM-алиас этого кадра валиден и записываем, а
                // `n <= PAGE_SIZE`.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        buf.as_ptr(),
                        vmm::phys_to_virt(phys & !(PAGE_SIZE - 1)) as *mut u8,
                        n,
                    );
                }
            }
        }
    }
    Ok(base)
}

/// `MAP_SHARED`: общие физические кадры.
///
/// * memfd (`node.is_memfd()`): каждый лист — кадр из страничного кэша узла;
///   `mmap` добавляет ссылку отображения (`pmm::cow_ref`), лист помечается
///   [`vmm::SHARED_BIT`]. Поэтому запись через одно отображение видна и второму
///   `mmap` того же fd, и fork-ребёнку, а `munmap`/exit только отпускают ссылку.
/// * `MAP_SHARED|MAP_ANONYMOUS`: кадры выделяются при `mmap` и тоже помечаются
///   `SHARED_BIT`; после fork стороны видят одни байты (Linux-семантика
///   анонимной разделяемой памяти).
///
/// `PROT_NONE` для shared отвергается: страницы отображены жадно, и
/// «перезащита из PROT_NONE обратно» потребовала бы ленивого дозаполнения из
/// файла, которого у этого слоя нет. Композитор всегда мапит `RW`.
fn mmap_shared(
    node: Option<Arc<dyn VfsNode>>,
    backing: Option<MmapBacking>,
    anon: bool,
    fixed: bool,
    addr: u64,
    len: u64,
    prot: u64,
    writable: bool,
    nx: bool,
    off: u64,
) -> Result<u64, Errno> {
    if prot == 0 {
        return Err(Errno::EINVAL);
    }
    if !anon {
        match node.as_ref() {
            Some(n) if n.is_memfd() => {}
            // MAP_SHARED обычного файла превращал бы чужие записи в видимые, не
            // имея ни page cache, ни writeback. Отказ громкий и явный.
            _ => return Err(Errno::EINVAL),
        }
    }
    let pages = len
        .checked_add(PAGE_SIZE - 1)
        .map(|n| n / PAGE_SIZE)
        .ok_or(Errno::ENOMEM)?;
    let span = pages.checked_mul(PAGE_SIZE).ok_or(Errno::ENOMEM)?;
    let first_page = off / PAGE_SIZE;

    let base = compat::with_current_compat(|cs| -> Result<u64, Errno> {
        let vm = &mut cs.vm.lock();
        let base = if fixed {
            if addr == 0 || addr & (PAGE_SIZE - 1) != 0 {
                return Err(Errno::EINVAL);
            }
            addr
        } else {
            plan_mmap_base(&vm.mmaps, vm.mmap_floor, pages, USER_ADDR_MAX).ok_or(Errno::ENOMEM)?
        };
        let end = base.checked_add(span).ok_or(Errno::ENOMEM)?;
        if end > USER_ADDR_MAX {
            return Err(Errno::ENOMEM);
        }
        if fixed {
            for i in 0..pages {
                let _ = vmm::release_user_leaf(base + i * PAGE_SIZE);
            }
            cut_regions(&mut vm.mmaps, base, pages);
        }

        let mut mapped: Vec<u64> = Vec::new();
        let pte_flags = leaf_flags(writable, nx) | vmm::SHARED_BIT;
        for i in 0..pages {
            let frame = if anon {
                let Some(frame) = pmm::alloc_frame() else {
                    unwind(&mapped);
                    return Err(Errno::ENOMEM);
                };
                zero_frame(frame);
                frame
            } else {
                match node.as_ref().and_then(|n| n.shared_page(first_page + i)) {
                    Some(frame) => frame,
                    None => {
                        unwind(&mapped);
                        return Err(Errno::ENOMEM);
                    }
                }
            };
            // Ссылка отображения: кадр memfd уже пиннится файлом, анонимный —
            // только этой ссылкой.
            pmm::cow_ref(frame);
            let page = base + i * PAGE_SIZE;
            if vmm::map(frame, page, pte_flags).is_err() {
                if pmm::cow_unref(frame) {
                    pmm::free_frame(frame);
                }
                unwind(&mapped);
                return Err(Errno::ENOMEM);
            }
            mapped.push(page);
        }
        vm.mmaps.push(MmapRegion {
            base,
            pages,
            writable,
            nx,
            prot: prot as u32,
            // Shared-страницы отображены жадно и НЕ должны лениво
            // переподпираться обработчиком fault: иначе fork-разделённое
            // отображение молча получило бы приватный нулевой кадр. Для
            // memfd backing печатает identity в `/proc/<pid>/maps`; у
            // анонимной shared-памяти её нет.
            anon: false,
            backing: backing.clone(),
        });
        if !fixed && end > vm.mmap_next_hint {
            vm.mmap_next_hint = end;
        }
        Ok(base)
    })
    .unwrap_or(Err(Errno::EINVAL))?;
    Ok(base)
}

/// `mmap(/dev/fb0)`: отобразить физический scanout Limine как MMIO.
///
/// Кадры framebuffer не принадлежат PMM, поэтому листья несут
/// [`vmm::MMIO_BIT`]: `munmap`, exit и `fork` снимают/копируют только PTE и
/// никогда не зовут `free_frame` на экране. `NO_CACHE|WRITE_THROUGH` — чтобы
/// записи композитора доходили до scanout, а не застревали в кэше CPU.
fn mmap_fb(
    info: FbInfo,
    backing: &Option<MmapBacking>,
    fixed: bool,
    addr: u64,
    len: u64,
    writable: bool,
    nx: bool,
    off: u64,
) -> Result<u64, Errno> {
    if info.phys & (PAGE_SIZE - 1) != 0 {
        crate::error!(
            "[fb0] framebuffer phys 0x{:x} is not page aligned - refusing mmap",
            info.phys
        );
        return Err(Errno::EINVAL);
    }
    let pages = len
        .checked_add(PAGE_SIZE - 1)
        .map(|n| n / PAGE_SIZE)
        .ok_or(Errno::ENOMEM)?;
    let span = pages.checked_mul(PAGE_SIZE).ok_or(Errno::ENOMEM)?;
    // Границу проверяем по ЗАПРОШЕННОЙ длине, а не по округлённому span:
    // `smem_len = pitch * height` не обязан быть кратен 4096, а композитор
    // мапит ровно `smem_len` байт. Последняя неполная страница всё равно
    // принадлежит scanout (framebuffer выделен страницами).
    if off > info.smem_len || off.checked_add(len).map_or(true, |e| e > info.smem_len) {
        return Err(Errno::EINVAL);
    }

    let base = compat::with_current_compat(|cs| -> Result<u64, Errno> {
        let vm = &mut cs.vm.lock();
        let base = if fixed {
            if addr == 0 || addr & (PAGE_SIZE - 1) != 0 {
                return Err(Errno::EINVAL);
            }
            addr
        } else {
            plan_mmap_base(&vm.mmaps, vm.mmap_floor, pages, USER_ADDR_MAX).ok_or(Errno::ENOMEM)?
        };
        let region_end = base.checked_add(span).ok_or(Errno::ENOMEM)?;
        if region_end > USER_ADDR_MAX {
            return Err(Errno::ENOMEM);
        }
        if fixed {
            for i in 0..pages {
                let _ = vmm::release_user_leaf(base + i * PAGE_SIZE);
            }
            cut_regions(&mut vm.mmaps, base, pages);
        }
        // Write-combining, если boot запрограммировал IA32_PAT entry 4.
        // WC-лист несёт PAT (bit 7) и не несёт NO_CACHE/WRITE_THROUGH;
        // без PAT остаётся точная прежняя некешируемая пара (см.
        // `memory::pat::fb0_pte_flags`, host-тесты проверяют обе композиции).
        let pte_flags = PageTableFlags::from_bits_truncate(pat::fb0_pte_flags(
            writable,
            nx,
            pat::wc_supported(),
        ));
        let mut mapped: Vec<u64> = Vec::new();
        for i in 0..pages {
            let page = base + i * PAGE_SIZE;
            let phys = info.phys + off + i * PAGE_SIZE;
            if vmm::map(phys, page, pte_flags).is_err() {
                unwind(&mapped);
                return Err(Errno::ENOMEM);
            }
            mapped.push(page);
        }
        vm.mmaps.push(MmapRegion {
            base,
            pages,
            writable,
            nx,
            prot: super::mem::PROT_READ | if writable { super::mem::PROT_WRITE } else { 0 },
            anon: false,
            backing: backing.clone(),
        });
        if !fixed && region_end > vm.mmap_next_hint {
            vm.mmap_next_hint = region_end;
        }
        Ok(base)
    })
    .unwrap_or(Err(Errno::EINVAL))?;
    // Takeover подтверждаем и на mmap: дескриптор мог быть открыт до того, как
    // консоль успела инициализироваться (в этом ядре это невозможно, но
    // контракт «экран принадлежит тому, кто его отобразил» не должен зависеть
    // от порядка open/mmap).
    crate::drivers::framebuffer::takeover_acquire();
    Ok(base)
}

/// `mmap(/dev/vtconsole0)`: read-only отображение shared VT surface.
///
/// Кадры поверхности запинены ядром; каждый лист получает ссылку отображения
/// (`cow_ref`), которая отпускается `munmap`/exit. `WRITABLE` не ставится
/// никогда, а `VT_RO_BIT` заставляет `mprotect` отказать в попытке добавить
/// запись — userspace не может подменить экран, который публикует только ядро.
fn mmap_vtconsole(
    node: &Arc<dyn VfsNode>,
    backing: &Option<MmapBacking>,
    fixed: bool,
    addr: u64,
    len: u64,
) -> Result<u64, Errno> {
    let pages = len
        .checked_add(PAGE_SIZE - 1)
        .map(|n| n / PAGE_SIZE)
        .ok_or(Errno::ENOMEM)?;
    let span = pages.checked_mul(PAGE_SIZE).ok_or(Errno::ENOMEM)?;
    let base = compat::with_current_compat(|cs| -> Result<u64, Errno> {
        let vm = &mut cs.vm.lock();
        let base = if fixed {
            if addr == 0 || addr & (PAGE_SIZE - 1) != 0 {
                return Err(Errno::EINVAL);
            }
            addr
        } else {
            plan_mmap_base(&vm.mmaps, vm.mmap_floor, pages, USER_ADDR_MAX).ok_or(Errno::ENOMEM)?
        };
        let end = base.checked_add(span).ok_or(Errno::ENOMEM)?;
        if end > USER_ADDR_MAX {
            return Err(Errno::ENOMEM);
        }
        if fixed {
            for i in 0..pages {
                let _ = vmm::release_user_leaf(base + i * PAGE_SIZE);
            }
            cut_regions(&mut vm.mmaps, base, pages);
        }
        let pte_flags = leaf_flags(false, true) | vmm::VT_RO_BIT;
        let mut mapped: Vec<u64> = Vec::new();
        for i in 0..pages {
            let Some(frame) = node.vtconsole_page(i) else {
                unwind(&mapped);
                return Err(Errno::ENOMEM);
            };
            pmm::cow_ref(frame);
            let page = base + i * PAGE_SIZE;
            if vmm::map(frame, page, pte_flags).is_err() {
                if pmm::cow_unref(frame) {
                    pmm::free_frame(frame);
                }
                unwind(&mapped);
                return Err(Errno::ENOMEM);
            }
            mapped.push(page);
        }
        vm.mmaps.push(MmapRegion {
            base,
            pages,
            writable: false,
            nx: true,
            prot: PROT_READ,
            anon: false,
            backing: backing.clone(),
        });
        if !fixed && end > vm.mmap_next_hint {
            vm.mmap_next_hint = end;
        }
        Ok(base)
    })
    .unwrap_or(Err(Errno::EINVAL))?;
    Ok(base)
}

/// `munmap` (11): снять отображение ранее `mmap`-нутого диапазона (R4.3) или
/// отвергнуть невыровненный/непокрытый запрос с `-EINVAL` (R4.7).
pub fn sys_munmap(addr: u64, len: u64) -> Result<u64, Errno> {
    // `munmap` снимает только отслеживаемые регионы, поэтому ядерный адрес
    // отвергался косвенно; всё равно проверяем диапазон заранее, чтобы три
    // memory-syscall'а с длиной делили один гейт, а не полагались на три разных
    // пути кода для отказа ядерному VA.
    let pages = user_pages(addr, len)?;
    if pages == 0 {
        return Ok(0);
    }
    compat::with_current_compat(|cs| munmap_impl(&mut cs.vm.lock(), addr, len))
        .unwrap_or(Err(Errno::EINVAL))
}

/// `mremap` (25): изменить размер анонимного приватного отображения. `realloc`
/// из glibc сначала пробует mremap для больших блоков и откатывается к
/// malloc+memcpy+free на ENOSYS, что логировало предупреждение о
/// неподдерживаемом syscall'е при каждом старте CPython. Сжатие отбрасывает
/// хвостовые страницы на месте; рост требует `MREMAP_MAYMOVE` (рост на месте не
/// пробуется, как в Linux, когда следующие страницы заняты): отображается свежий
/// обнулённый диапазон, старые кадры копируются через HHDM-алиас (невосприимчив к
/// read-only/NX пользовательским PTE при CR0.WP), а старый диапазон снимается.
pub fn sys_mremap(
    old_addr: u64,
    old_size: u64,
    new_size: u64,
    flags: u64,
    _new_addr: u64,
) -> Result<u64, Errno> {
    const MREMAP_MAYMOVE: u64 = 1;
    if old_size == 0 || new_size == 0 {
        return Err(Errno::EINVAL);
    }
    // Тот же гейт пользовательской половины, что у mprotect/munmap: `old_addr`
    // участвует в арифметике таблиц/кадров, а `new_size` решает, сколько страниц
    // выделить и отобразить. Длина не может быть «в пользовательской половине»
    // (это не адрес), поэтому она ограничена размером половины: ни один mremap не
    // может запросить VA больше, чем половина вмещает.
    if user_pages(old_addr, old_size)? == 0 {
        return Err(Errno::EINVAL);
    }
    if new_size > USER_ADDR_MAX {
        return Err(Errno::EINVAL);
    }
    if flags & !MREMAP_MAYMOVE != 0 {
        // MREMAP_FIXED / MREMAP_DONTUNMAP — неподдерживаемые формы: отказать
        // громко, а не отобразить неверно.
        return Err(Errno::EINVAL);
    }
    let old_pages = old_size
        .checked_add(PAGE_SIZE - 1)
        .map(|n| n / PAGE_SIZE)
        .ok_or(Errno::ENOMEM)?;
    let new_pages = new_size
        .checked_add(PAGE_SIZE - 1)
        .map(|n| n / PAGE_SIZE)
        .ok_or(Errno::ENOMEM)?;
    if new_pages == 0 {
        return Err(Errno::EINVAL);
    }
    // `mremap` копирует/переставляет листья как приватные. Для MAP_SHARED и
    // MMIO это сломало бы владение (общий кадр или кадр устройства попал бы в
    // приватную копию/освобождение), поэтому честный отказ, а не тихая порча.
    for i in 0..old_pages {
        if let Some(pte) = vmm::walk_pte(old_addr + i * PAGE_SIZE) {
            let f = pte.flags();
            if f.contains(vmm::SHARED_BIT)
                || f.contains(vmm::VT_RO_BIT)
                || f.contains(vmm::MMIO_BIT)
            {
                return Err(Errno::EINVAL);
            }
        }
    }
    compat::with_current_compat(|cs| -> Result<u64, Errno> {
        let vm = &mut cs.vm.lock();
        // Весь старый диапазон обязан лежать внутри одного отслеживаемого
        // mmap-региона; его защита переносится на новое размещение.
        let old_span = old_pages.checked_mul(PAGE_SIZE).ok_or(Errno::ENOMEM)?;
        let idx = vm
            .mmaps
            .iter()
            .position(|r| r.base <= old_addr && old_addr + old_span <= r.base + r.pages * PAGE_SIZE)
            .ok_or(Errno::EFAULT)?;
        let old = vm.mmaps[idx].clone();
        let flags_of = leaf_flags(old.writable, old.nx);

        if new_pages <= old_pages {
            if new_pages < old_pages {
                // Сжатие на месте: отбросить хвостовые страницы.
                let _ = munmap_impl(
                    vm,
                    old_addr + new_pages * PAGE_SIZE,
                    (old_pages - new_pages) * PAGE_SIZE,
                );
            }
            return Ok(old_addr);
        }
        let delta = new_pages - old_pages;

        // Рост на месте: когда диапазон сразу за регионом свободен, просто
        // расширить отслеживаемый регион — без копирования и перемещения.
        // Анонимные хвосты остаются ленивыми; жадные (file-backed) хвосты здесь
        // отображаются нулями (mremap никогда не подгружает больше данных файла,
        // как в Linux, где новые страницы просто принадлежат той же VMA).
        if range_is_free(&vm.mmaps, old_addr + old_span, delta, USER_ADDR_MAX) {
            let tail = old_addr + old_span;
            if !old.anon && !map_zeroed_range(tail, tail + delta * PAGE_SIZE, flags_of) {
                return Err(Errno::ENOMEM);
            }
            vm.mmaps[idx].pages = new_pages;
            let end = old_addr + new_pages * PAGE_SIZE;
            if end > vm.mmap_next_hint {
                vm.mmap_next_hint = end;
            }
            return Ok(old_addr);
        }

        if flags & MREMAP_MAYMOVE == 0 {
            return Err(Errno::ENOMEM);
        }
        let span = new_pages.checked_mul(PAGE_SIZE).ok_or(Errno::ENOMEM)?;
        let base = plan_mmap_base(&vm.mmaps, vm.mmap_floor, new_pages, USER_ADDR_MAX)
            .ok_or(Errno::ENOMEM)?;
        let end = base.checked_add(span).ok_or(Errno::ENOMEM)?;
        if end > USER_ADDR_MAX {
            return Err(Errno::ENOMEM);
        }
        // Анонимная цель: ленивая — копирование трогает только страницы с
        // данными. Жадная (file-backed) цель: отобразить весь диапазон, затем
        // копировать.
        if !old.anon && !map_zeroed_range(base, end, flags_of) {
            // OOM: старое отображение не тронуто.
            return Err(Errno::ENOMEM);
        }
        let mut copied: Vec<u64> = Vec::new();
        for i in 0..old_pages {
            let src = vmm::virt_to_phys(old_addr + i * PAGE_SIZE);
            let Some(src) = src else {
                continue; // нетронутая исходная страница: цель остаётся нулевой/ленивой
            };
            let dst_page = base + i * PAGE_SIZE;
            if old.anon {
                // Убедиться, что страница-приёмник подпёрта, прежде чем копировать
                // в неё (её кадр нельзя записать через отсутствующий PTE; HHDM-
                // алиасу нужно, чтобы кадр сначала существовал).
                match pmm::alloc_frame() {
                    Some(frame) => {
                        zero_frame(frame);
                        if vmm::map(frame, dst_page, flags_of).is_err() {
                            pmm::free_frame(frame);
                            unwind(&copied);
                            return Err(Errno::ENOMEM);
                        }
                        copied.push(dst_page);
                    }
                    None => {
                        unwind(&copied);
                        return Err(Errno::ENOMEM);
                    }
                }
            }
            // SAFETY: оба кадра по 4 КиБ, отображены этим процессом / этим
            // вызовом; HHDM-алиасы валидны и записываемы.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    vmm::phys_to_virt(src & !(PAGE_SIZE - 1)) as *const u8,
                    vmm::phys_to_virt(vmm::virt_to_phys(dst_page).unwrap()) as *mut u8,
                    PAGE_SIZE as usize,
                );
            }
        }
        vm.mmaps.insert(
            idx,
            MmapRegion {
                base,
                pages: new_pages,
                writable: old.writable,
                nx: old.nx,
                prot: old.prot,
                anon: old.anon,
                // Перемещённое отображение сохраняет идентичность файла; хвостовые
                // страницы за старым концом заполняются нулями копированием выше,
                // точно как жадное копирование файла оставляет хвост короткого
                // файла нулевым.
                backing: old.backing.clone(),
            },
        );
        if end > vm.mmap_next_hint {
            vm.mmap_next_hint = end;
        }
        let _ = munmap_impl(vm, old_addr, old_span);
        Ok(base)
    })
    .unwrap_or(Err(Errno::EINVAL))
}

fn munmap_impl(vm: &mut VmRegionSet, base: u64, len: u64) -> Result<u64, Errno> {
    match plan_munmap(base, len, &vm.mmaps) {
        MunmapPlan::Reject(e) => Err(e),
        MunmapPlan::Unmap { base, pages } => {
            for i in 0..pages {
                let page = base + i * PAGE_SIZE;
                // Один путь освобождения для каждого отцепленного листа:
                // приватные кадры освобождаются, COW-разделённые теряют только
                // ссылку этого адресного пространства и остаются отображёнными в
                // другом.
                let _ = vmm::release_user_leaf(page);
            }
            cut_regions(&mut vm.mmaps, base, pages);
            Ok(0)
        }
    }
}

/// Удалить диапазон страниц `[base, base + pages*4096)` из набора
/// отслеживаемых регионов, разрезая любой частично перекрытый регион на выжившие
/// поддиапазоны.
fn cut_regions(regions: &mut Vec<MmapRegion>, base: u64, pages: u64) {
    let ustart = base;
    let uend = base + pages * PAGE_SIZE;
    let mut out: Vec<MmapRegion> = Vec::new();
    for r in regions.iter() {
        let rstart = r.base;
        let rend = r.base + r.pages * PAGE_SIZE;
        if rend <= ustart || rstart >= uend {
            // Перекрытия нет: оставить целиком.
            out.push(r.clone());
            continue;
        }
        // Выживший левый поддиапазон.
        if rstart < ustart {
            out.push(MmapRegion {
                base: rstart,
                pages: (ustart - rstart) / PAGE_SIZE,
                writable: r.writable,
                nx: r.nx,
                prot: r.prot,
                anon: r.anon,
                backing: r.backing.clone(),
            });
        }
        // Выживший правый поддиапазон: его первая страница отображает более
        // позднее смещение файла, поэтому `file_offset` backing'а сдвигается на
        // отрезанные байты.
        if rend > uend {
            let shift = uend - rstart;
            out.push(MmapRegion {
                base: uend,
                pages: (rend - uend) / PAGE_SIZE,
                writable: r.writable,
                nx: r.nx,
                prot: r.prot,
                anon: r.anon,
                backing: r.backing.clone().map(|mut b| {
                    b.file_offset = b.file_offset.saturating_add(shift);
                    b
                }),
            });
        }
    }
    *regions = out;
}

/// `mprotect` (10): сменить защиту диапазона отображённых пользовательских
/// страниц (R4.5) или `-ENOMEM`, если хоть одна страница диапазона сейчас не
/// отображена (R4.8). Невыровненная база — `-EINVAL`.
pub fn sys_mprotect(addr: u64, len: u64, prot: u64) -> Result<u64, Errno> {
    compat::with_current_compat(|cs| mprotect_impl(&mut cs.vm.lock(), addr, len, prot as u32))
        .unwrap_or(Err(Errno::EINVAL))
}

fn mprotect_impl(vm: &mut VmRegionSet, addr: u64, len: u64, prot: u32) -> Result<u64, Errno> {
    let pages = user_pages(addr, len)?;
    if pages == 0 {
        return Ok(0);
    }

    // Проверка покрытия: каждая страница диапазона должна быть отображена в
    // адресном пространстве вызывающего. Большинство страниц отслеживаются как
    // mmap-регионы или куча brk, но ELF-загрузчик отображает PT_LOAD-сегменты (а
    // ld.so из glibc перезащищает свой RELRO-сегмент) без записи в VmRegionSet;
    // резидентный PTE — такое же реальное отображение, поэтому откатываемся к
    // таблицам страниц вместо ответа ENOMEM. glibc сообщает этот отказ как
    // "cannot apply additional memory protection after relocation", и fastfetch
    // не стартовал именно так.
    {
        let mut covered = vm.mmaps.clone();
        if vm.current_brk > vm.brk_lazy_from {
            covered.push(MmapRegion {
                base: vm.brk_lazy_from,
                pages: (vm.current_brk - vm.brk_lazy_from).div_ceil(PAGE_SIZE),
                writable: true,
                nx: true,
                prot: super::mem::PROT_READ | super::mem::PROT_WRITE,
                anon: true,
                backing: None,
            });
        }
        let region_rejected = matches!(plan_munmap(addr, len, &covered), MunmapPlan::Reject(_));
        // Страница приемлема, когда её покрывает планировщик регионов ИЛИ она
        // реально отображена (PTE загрузчика/стека/иные). ENOMEM означает, что
        // хотя бы одна страница действительно отсутствует.
        let unmapped = (0..pages)
            .filter(|i| vmm::virt_to_phys(addr + i * PAGE_SIZE).is_none())
            .count();
        if region_rejected && unmapped > 0 {
            return Err(Errno::ENOMEM);
        }
    }

    let (writable, nx) = prot_to_flags(prot);
    let prot_none = prot == 0;
    // Read-only shared VT surface нельзя сделать записываемой: экран публикует
    // только ядро. Проверяем ВЕСЬ диапазон до первой мутации, чтобы отказ не
    // оставил половину диапазона перезащищённой.
    if writable {
        for i in 0..pages {
            if let Some(pte) = vmm::walk_pte(addr + i * PAGE_SIZE) {
                if pte.flags().contains(vmm::VT_RO_BIT) {
                    return Err(Errno::EACCES);
                }
            }
        }
    }
    // Второй проход: аппаратное состояние меняют только резидентные страницы.
    // PROT_NONE полностью убирает PTE (последующее касание обязано дать SIGSEGV,
    // а не заново подпереть страницу); всё остальное переотображается на месте с
    // новыми битами.
    for i in 0..pages {
        let page = addr + i * PAGE_SIZE;
        // `user_pages` уже доказал, что весь диапазон ниже `USER_ADDR_MAX`;
        // проверка повторяется, чтобы инвариант жил рядом с вызовами таблиц
        // страниц, которые на него опираются. Ядерный кадр здесь нельзя снять с
        // отображения, освободить (путь PROT_NONE: UAF PMM) или переотобразить с
        // пользовательскими флагами.
        if page >= USER_ADDR_MAX {
            return Err(Errno::EINVAL);
        }
        if vmm::virt_to_phys(page).is_some() {
            if prot_none {
                // PROT_NONE убирает трансляцию; COW-разделённый кадр делает unref
                // (освобождается, только когда это был последний шарящий), никогда
                // не освобождается безусловно.
                let _ = vmm::release_user_leaf(page);
            } else {
                // Сырые флаги листа (`page_flags` пересекает весь обход и потому
                // маскирует программные листовые биты вроде COW_BIT; маркер
                // владения нужно читать из самого листового PTE).
                let Some(old_flags) = vmm::walk_pte(page).map(|pte| pte.flags()) else {
                    continue;
                };
                // COW-лист остаётся разделяемым: сохранить его маркер и никогда не
                // ставить аппаратную запись в кадр, который всё ещё отображает
                // другое адресное пространство. Право записи выдаёт следующий write
                // fault, копирующий страницу приватно (cow_copy_page) и
                // восстанавливающий WRITABLE только на приватной копии.
                let mut new_flags = leaf_flags(writable, nx);
                if old_flags.contains(vmm::MMIO_BIT) {
                    // Framebuffer: сохранить MMIO-маркер (иначе следующий
                    // munmap/exit попытается вернуть кадр устройства в PMM) и
                    // ТУ ЖЕ политику кэша, что уже стоит на листе. Раньше здесь
                    // безусловно возвращались NO_CACHE|WRITE_THROUGH, и первый
                    // же mprotect на fb-регионе молча отменял write-combining.
                    let cache = PageTableFlags::from_bits_truncate(pat::mmio_cache_bits_from(
                        old_flags.bits(),
                        pat::wc_supported(),
                    ));
                    new_flags |= vmm::MMIO_BIT | cache;
                } else if old_flags.contains(vmm::VT_RO_BIT) {
                    // Read-only VT-поверхность: маркер владения сохраняется,
                    // WRITABLE сюда не приходит (см. pre-scan выше).
                    new_flags |= vmm::VT_RO_BIT;
                } else if old_flags.contains(vmm::SHARED_BIT) {
                    // MAP_SHARED: кадр общий с файлом/другим процессом. Права
                    // отображения меняются на месте, маркер и ссылка владения
                    // сохраняются; write в PROT_READ-диапазон обязан дать
                    // SIGSEGV (cow_copy_page не увидит COW_BIT).
                    new_flags |= vmm::SHARED_BIT;
                } else if old_flags.contains(vmm::COW_BIT) {
                    new_flags |= vmm::COW_BIT;
                    new_flags = new_flags - PageTableFlags::WRITABLE;
                    // Записать право записи ОТОБРАЖЕНИЯ на разделяемом листе:
                    // `RO_BIT` — это то, что говорит следующему write fault «карта
                    // запрещает запись -> SIGSEGV» вместо «COW-запись отложена ->
                    // скопировать страницу приватно» (mprotect(PROT_READ) раньше был
                    // совещательным для каждой fork-разделённой страницы).
                    if writable {
                        new_flags = new_flags - vmm::RO_BIT;
                    } else {
                        new_flags |= vmm::RO_BIT;
                    }
                }
                // Переставить флаги на месте: тот же кадр, без аллокации PMM,
                // поэтому OOM не может оставить половину диапазона отцепленной.
                let _ = vmm::set_user_leaf_flags(page, new_flags);
            }
        }
    }

    update_region_flags(&mut vm.mmaps, addr, pages, writable, nx, prot);
    Ok(0)
}

/// Обновить отслеживаемую защиту байтов региона, покрытых `mprotect`, разрезая
/// регион, который диапазон перекрывает лишь частично, на поддиапазоны
/// до/покрытый/после.
///
/// Таблицы страниц всегда обновляются выше, но учёт важен для ещё НЕ резидентных
/// страниц: обработчик page fault подпирает анонимную страницу
/// `leaf_flags(r.writable, r.nx)` и трактует `r.prot == 0` как настоящий
/// `PROT_NONE`-отказ. Прежнее правило «только полностью покрытые регионы»
/// оставляло частично покрытый регион полностью устаревшим, поэтому
/// `mprotect(page, PAGE, PROT_NONE)` на первой странице 8-страничного отображения
/// снимал PTE (верно), а следующее касание переотображало эту страницу
/// **записываемой** из старой записи региона. Разрезание приводит запись в
/// соответствие байтам, которые syscall реально перезащитил.
///
/// Правый поддиапазон file-backed региона продолжает отображать те же байты
/// файла, поэтому его `backing.file_offset` сдвигается на вставленные перед ним
/// байты — то же правило, что использует [`cut_regions`] для `munmap`.
fn update_region_flags(
    regions: &mut Vec<MmapRegion>,
    base: u64,
    pages: u64,
    writable: bool,
    nx: bool,
    prot: u32,
) {
    let ustart = base;
    let uend = base.saturating_add(pages.saturating_mul(PAGE_SIZE));
    if uend <= ustart {
        return;
    }
    let mut out: Vec<MmapRegion> = Vec::with_capacity(regions.len().saturating_add(2));
    for r in regions.iter() {
        let rstart = r.base;
        let rend = r.base.saturating_add(r.pages.saturating_mul(PAGE_SIZE));
        if rend <= ustart || rstart >= uend {
            // Перекрытия нет: без изменений.
            out.push(r.clone());
            continue;
        }
        // Выживший левый поддиапазон сохраняет старую защиту.
        if rstart < ustart {
            out.push(MmapRegion {
                base: rstart,
                pages: (ustart - rstart) / PAGE_SIZE,
                writable: r.writable,
                nx: r.nx,
                prot: r.prot,
                anon: r.anon,
                backing: r.backing.clone(),
            });
        }
        // Покрытая середина получает новую защиту. У file-backed региона её первая
        // страница отображает более позднее смещение файла, поэтому `backing`
        // сдвигается на байты, оказавшиеся теперь перед ней.
        let mstart = core::cmp::max(rstart, ustart);
        let mend = core::cmp::min(rend, uend);
        out.push(MmapRegion {
            base: mstart,
            pages: (mend - mstart) / PAGE_SIZE,
            writable,
            nx,
            prot,
            anon: r.anon,
            backing: r.backing.clone().map(|mut b| {
                b.file_offset = b.file_offset.saturating_add(mstart - rstart);
                b
            }),
        });
        // Выживший правый поддиапазон сохраняет старую защиту и свой сдвинутый
        // offset файла.
        if rend > uend {
            out.push(MmapRegion {
                base: uend,
                pages: (rend - uend) / PAGE_SIZE,
                writable: r.writable,
                nx: r.nx,
                prot: r.prot,
                anon: r.anon,
                backing: r.backing.clone().map(|mut b| {
                    b.file_offset = b.file_offset.saturating_add(uend - rstart);
                    b
                }),
            });
        }
    }
    *regions = out;
}

/// Write-fault на присутствующей странице: обслуживание copy-on-write. Лист
/// обязан быть COW-разделённым (`COW_BIT`, оставленный
/// [`crate::memory::vmm::fork_user_space_cow`]) И его отображение обязано разрешать
/// запись: `mprotect(PROT_READ)` на разделяемой странице помечает лист read-only
/// (`RO_BIT`), поэтому запись туда всё равно даёт SIGSEGV, а не приватную
/// записываемую копию. Работает и для отказов ring 3, и для отказов режима ядра на
/// пользовательских адресах (`copy_out` по fork-разделённому буферу).
fn handle_cow_fault(addr: u64, caused_by_write: bool) -> bool {
    if !caused_by_write || compat::compat_lock_held() {
        return false;
    }
    // `cow_copy_page` сам проверяет оба маркера: присутствующая read-only
    // страница без `COW_BIT` (текст/rodata ELF, PROT_READ mmap) и COW-лист, чьё
    // отображение запрещает запись (`RO_BIT`), — обе дают false и сохраняют путь
    // SIGSEGV. Гейт по учёту VMA здесь был ошибкой: пользовательский стек, стеки
    // потоков и fork-разделённые страницы кучи — настоящие записываемые
    // отображения, которые `fork_user_space_cow` делает read-only, и промах учёта
    // убивал fastfetch и в ребёнке, и в родителе сразу после fork. Авторитетный
    // ответ — маркер на листе.
    let page = addr & !(PAGE_SIZE - 1);
    crate::memory::vmm::cow_copy_page(page)
}

/// Подпереть страницу по `addr` по требованию (анонимные `mmap`-регионы и куча
/// `brk`). Вызывается обработчиком #PF на каждом page fault, до любого
/// диагностического/SIGSEGV-пути.
///
/// Возвращает `true`, когда отказ обслужен: отображён нулевой кадр с битами защиты
/// региона, и инструкция с отказом может просто повториться. `false`, когда адрес
/// необслуживаем лениво — применяется существующий путь SIGSEGV/диагностики
/// вызывающего:
///
///   * отказ PROTECTION_VIOLATION (страница присутствует, тип доступа плохой —
///     нарушения NX/RO обязаны дать SIGSEGV; COW подключится здесь позже);
///   * адрес на `USER_ADDR_MAX` или выше (дикий указатель ядра);
///   * страница внутри `PROT_NONE`-региона (семантика stack guard);
///   * страница внутри жадного (file-backed) региона — они всегда отображены
///     полностью, поэтому отсутствующий PTE — ошибка ядра;
///   * адрес не покрыт ни одним регионом и вне кучи brk.
///
/// Контракт со слоем syscall (см. doc модуля): код ядра никогда не трогает память
/// пользователя под compat-локом реестра, поэтому этот обработчик всегда может
/// взять лок. `compat::compat_lock_held()` — растяжка нарушений этого правила:
/// такой отказ сообщается как segfault, а не блокирует CPU на нереентерабельном
/// спинлоке.
pub fn handle_user_page_fault(
    addr: u64,
    protection_violation: bool,
    caused_by_write: bool,
) -> bool {
    if addr >= USER_ADDR_MAX {
        return false;
    }
    if protection_violation {
        return handle_cow_fault(addr, caused_by_write);
    }
    if compat::compat_lock_held() {
        crate::error!(
            "[PF] fault on user addr 0x{:x} inside with_current_compat - refusing re-entry",
            addr
        );
        return false;
    }
    let page = addr & !(PAGE_SIZE - 1);
    compat::with_current_compat(|cs| {
        let vm = &mut cs.vm.lock();
        // Куча brk: страницы в [brk_lazy_from, page_up(current_brk)) подпираются
        // по первому касанию, всегда writable + NX.
        let brk_end = vm.current_brk.saturating_add(PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        if page >= vm.brk_lazy_from && page < brk_end {
            if vmm::virt_to_phys(page).is_some() {
                return true; // снова резидентна (гонка сжатия/роста); повтор
            }
            return map_zeroed_range(page, page + PAGE_SIZE, leaf_flags(true, true));
        }
        // Отслеживаемые mmap-регионы.
        let Some(r) = vm
            .mmaps
            .iter()
            .find(|r| page >= r.base && page < r.base + r.pages * PAGE_SIZE)
        else {
            return false;
        };
        if r.prot == 0 {
            // PROT_NONE (например, guard-страница стека потока): касание — это
            // настоящий SIGSEGV, а не повод для отображения.
            return false;
        }
        if vmm::virt_to_phys(page).is_some() {
            return true; // уже резидентна; повтор
        }
        if !r.anon {
            // File-backed регионы отображены жадно; отсутствующий PTE здесь —
            // ошибка ядра, а не ленивое отображение.
            crate::error!(
                "[PF] missing PTE in eager file-backed region base=0x{:x} addr=0x{:x}",
                r.base,
                addr
            );
            return false;
        }
        map_zeroed_range(page, page + PAGE_SIZE, leaf_flags(r.writable, r.nx))
    })
    .unwrap_or(false)
}

/// Подпереть page-by-page пользовательский диапазон, в который ядро собирается
/// писать, до [`super::io_sys::copy_out_checked`].
///
/// Зачем это нужно отдельно: общая проверка присутствия (`check_user_ptr*`)
/// отвергает ещё не подпертые страницы `EFAULT`'ом, а `copy_in`/`copy_out` по
/// ним штатно обслуживаются обработчиком page fault (см. `idt.rs`). Для
/// `read(2)` из `/dev/vtconsole0` это единственный путь, где приёмник — свежий
/// анонимный `mmap` клиента: снимок в сотню килобайт копируется целиком, и без
/// этой подготовки вызов возвращал `EFAULT` на законном буфере.
///
/// Политика — ровно [`handle_user_page_fault`]: присутствующие страницы
/// принимаются как есть; отсутствующие подпираются только внутри ленивой кучи
/// `brk` или анонимного `mmap`-региона; `PROT_NONE`, eager-регион без PTE и
/// адрес вне регионов остаются отказом (`EFAULT`), а не поводом что-то
/// отобразить. Права листа при этом берутся из региона, поэтому приёмник
/// `PROT_READ` подпирается read-only и всё равно отвергается `copy_out`'ом.
///
/// ПРЕДУСЛОВИЕ: вызывающий не держит `COMPAT_STATES` — [`handle_user_page_fault`]
/// берёт его сам и в реентерабельном контексте отказывает.
pub fn populate_user_out(start: u64, len: u64) -> Result<(), Errno> {
    use super::validate::{check_user_range, spanned_pages, PtrCheck};
    if len == 0 {
        return Ok(());
    }
    if check_user_range(start, len) != PtrCheck::Ok {
        return Err(Errno::EFAULT);
    }
    for page in spanned_pages(start, len) {
        if vmm::virt_to_phys(page).is_some() {
            continue;
        }
        if !handle_user_page_fault(page, false, true) {
            return Err(Errno::EFAULT);
        }
    }
    Ok(())
}
