// task/stack_map.rs — эффектное отображение начального стека (только ядро).
//
// **Эффектная половина** компонента `Stack_Initializer` (design §5). Оборачивает
// чистый кодировщик `task::stack::build_initial_stack`: отображает ring-3
// страницы пользовательского стека, просит чистый кодировщик точный SysV-образ,
// копирует его в отображённый стек и возвращает входной `rsp` строителю
// процесса.
//
// Модуль вынесен в отдельный файл намеренно: `task/stack.rs` включается в
// `host-tests` через `#[path]`, чтобы его чистый кодировщик проверялся свойствами
// на хосте (R11.6). Эта обёртка использует ядерные API paging
// `memory::{pmm, vmm}`, которых на хосте нет, поэтому `task/stack.rs` остаётся
// чисто host-тестируемым, а этот файл компилируется только в ядре.
//
// Покрытые требования: R6.6 (байты AT_RANDOM живут в отображённом стеке), R6.8
// (нехватка места обрывает запуск без старта процесса), R7.6 (страницы стека
// отображены USER_ACCESSIBLE, чтобы ring-3 до них доставал).
#![allow(dead_code)]

use core::cmp::min;
use core::ptr;

use x86_64::structures::paging::PageTableFlags;

use crate::memory::layout::{PAGE_SIZE, USER_STACK_PAGES, USER_STACK_TOP};
use crate::memory::{pmm, vmm};
use crate::task::stack::{build_initial_stack, AuxInputs, StackError, StackImage};

/// Режимы отказа [`map_initial_stack`].
///
/// [`TooLarge`](StackMapError::TooLarge) пробрасывает
/// [`StackError::TooLarge`] чистого кодировщика (R6.8). Остальные два покрывают
/// эффектный шаг отображения (исчерпание физических кадров / отказ таблицы
/// страниц); в любом случае обёртка снимает все отображённые страницы, и процесс
/// НЕ запускается (R6.8, R7.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StackMapError {
    /// Закодированный SysV-образ не помещается в регион пользовательского стека (R6.8).
    TooLarge,
    /// Не удалось выделить физический кадр под страницу стека.
    OutOfMemory,
    /// Не удалось отобразить страницу стека в пользовательский PML4.
    MapFailed,
}

impl From<StackError> for StackMapError {
    fn from(e: StackError) -> Self {
        match e {
            StackError::TooLarge => StackMapError::TooLarge,
        }
    }
}

/// Отобразить начальный пользовательский стек и заполнить его образом System V
/// (R6.6, R6.8, R7.6).
///
/// Повторяет подход к пользовательским отображениям из
/// `task::process::create_user_process`: пользовательское адресное пространство —
/// `pml4_phys` (свежий PML4 от загрузчика как `ElfProcess::pml4_phys`), поэтому
/// обёртка временно ставит этот CR3, пока отображает и пишет стек, и
/// восстанавливает ядерный CR3 на каждом пути возврата. Ядерная верхняя половина
/// (этот код, его стек, куча под `Vec` чистого кодировщика и HHDM для
/// копирования) остаётся отображённой под пользовательским PML4, так что
/// выполнение при этом CR3 безопасно.
///
/// ПРЕДУСЛОВИЕ: вызывающий выполняет это с выключенными прерываниями — на время
/// исполнения кода ядра ставится пользовательский CR3, как в
/// `create_user_process`/`ElfLoader::load`.
///
/// Шаги:
///  1. Вывести регион `[stack_low, USER_STACK_TOP)` из констант
///     (`USER_STACK_TOP`, `USER_STACK_PAGES`) — тот же, что у
///     `create_user_process`.
///  2. Отобразить каждую страницу стека `WRITABLE | NO_EXECUTE |
///     USER_ACCESSIBLE` (R7.6), подложив кадр PMM, и считать число отображённых.
///  3. Позвать чистый кодировщик [`build_initial_stack`] для этого региона.
///  4. При `Ok` скопировать `StackImage::bytes` в отображённый стек по
///     `argc_addr` (постранично через HHDM, не полагаясь на физическую
///     непрерывность свежих кадров) и вернуть `initial_rsp`.
///  5. При любой ошибке (кодировщик `TooLarge`, OOM PMM или отказ VMM)
///     снять все отображённые на шаге 2 страницы и пробросить ошибку, чтобы
///     процесс не стартовал (R6.8, R7.3).
///
/// Возвращает начальный `rsp` (выровненный на 16 адрес `argc`) при успехе.
pub fn map_initial_stack(
    pml4_phys: u64,
    argv: &[&[u8]],
    envp: &[&[u8]],
    aux: &AuxInputs,
    random16: [u8; 16],
) -> Result<u64, StackMapError> {
    let stack_top = USER_STACK_TOP;
    let stack_low = USER_STACK_TOP - USER_STACK_PAGES * PAGE_SIZE;

    // Ring-3 страницы стека: записываемые данные, никогда не исполняемые,
    // достижимые из ring 3 (R7.6). Набор флагов идентичен стеку пользователя в
    // `create_user_process`.
    let uflags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_EXECUTE;

    let kernel_cr3 = vmm::current_pml4_phys();
    // SAFETY: `pml4_phys` — валидный PML4 с клонированной ядерной верхней
    // половиной, поэтому код/стек/куча/HHDM ядра остаются отображёнными, пока он
    // установлен. `kernel_cr3` восстанавливается на каждом пути ниже перед
    // возвратом.
    unsafe {
        vmm::load_cr3(pml4_phys);
    }

    // ── Отобразить регион стека, запоминая, сколько страниц заняли ──────────
    let mut mapped_pages: u64 = 0;
    let mut map_err: Option<StackMapError> = None;
    for page in 0..USER_STACK_PAGES {
        let vaddr = stack_low + page * PAGE_SIZE;
        match pmm::alloc_frame() {
            Some(frame) => {
                if vmm::map(frame, vaddr, uflags).is_err() {
                    // Отображение не удалось: кадр не упомянут ни в одном PTE,
                    // поэтому освобождаем его напрямую (цикл отката видит только
                    // отображённые страницы).
                    pmm::free_frame(frame);
                    map_err = Some(StackMapError::MapFailed);
                    break;
                }
                mapped_pages += 1;
            }
            None => {
                map_err = Some(StackMapError::OutOfMemory);
                break;
            }
        }
    }

    // ── Построить чистый образ и скопировать его (только если отображение удалось) ─
    let result: Result<u64, StackMapError> = match map_err {
        None => match build_initial_stack(stack_top, stack_low, argv, envp, aux, random16) {
            Ok(image) => {
                copy_image_to_user_stack(&image);
                Ok(image.initial_rsp)
            }
            Err(e) => Err(StackMapError::from(e)),
        },
        Some(e) => Err(e),
    };

    // ── При любой ошибке снять отображённые страницы (R6.8) ────────────────
    if result.is_err() {
        for page in 0..mapped_pages {
            let vaddr = stack_low + page * PAGE_SIZE;
            // Перевести адрес до снятия отображения, чтобы вернуть лежащий кадр.
            if let Some(phys) = vmm::virt_to_phys(vaddr) {
                let _ = vmm::unmap(vaddr);
                pmm::free_frame(phys);
            }
        }
    }

    // SAFETY: восстановить ядерный PML4 до возврата вызывающему, чтобы никогда
    // не отдавать управление с чужим адресным пространством.
    unsafe {
        vmm::load_cr3(kernel_cr3);
    }

    result
}

/// Скопировать собранный [`StackImage`] в уже отображённый пользовательский
/// стек.
///
/// Образ занимает `[argc_addr, stack_top)`; `bytes[0]` живёт по `argc_addr`.
/// Копирование идёт постранично, переводя каждую страницу через HHDM
/// (`virt_to_phys` → `phys_to_virt`) — тем же способом пользуется `ElfLoader`, —
/// поэтому корректно даже при физической неоднородности свежих кадров стека.
///
/// ПРЕДУСЛОВИЕ: пользовательский PML4, лежащий за `argc_addr`, — активный CR3, и
/// каждая страница образа отображена на запись (гарантирует вызывающий).
fn copy_image_to_user_stack(image: &StackImage) {
    let bytes = &image.bytes;
    let dst_base = image.argc_addr;
    let mut written: usize = 0;
    while written < bytes.len() {
        let dst_vaddr = dst_base + written as u64;
        let page_base = dst_vaddr & !(PAGE_SIZE - 1);
        let page_off = (dst_vaddr - page_base) as usize;
        let chunk = min(PAGE_SIZE as usize - page_off, bytes.len() - written);

        if let Some(phys) = vmm::virt_to_phys(page_base) {
            let dst = (vmm::phys_to_virt(phys) + page_off as u64) as *mut u8;
            // SAFETY: `dst` — HHDM-алиас отображённой записываемой
            // пользовательской страницы стека; `chunk` не выходит за эту страницу
            // и за `bytes`.
            unsafe {
                ptr::copy_nonoverlapping(bytes.as_ptr().add(written), dst, chunk);
            }
        }
        written += chunk;
    }
}
