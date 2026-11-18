//! Чистая валидация диапазонов пользовательских указателей для слоя
//! совместимости с Linux.
//!
//! Только `core`, без аллокаций, общая с крейтом `host-tests` (R11.6). Это
//! *диапазонная* половина точки контроля указателей syscall: она отвергает
//! буферы, начинающиеся на границе user/kernel или выше, переполняющиеся при
//! прибавлении длины или заканчивающиеся за границей, — и всё это **без
//! разыменования** (R1.5). Эффектный обход *присутствия страниц* (R1.6) живёт
//! в обработчиках и перечисляет те же базы страниц, что даёт [`spanned_pages`].
#![allow(dead_code)]

/// Размер страницы для перечисления охваченных страниц (x86_64, 4 КиБ).
const PAGE_SIZE: u64 = 4096;

/// Исключающая верхняя граница канонического пользовательского диапазона
/// нижней половины (`User_Addr_Max`). Буфер в диапазоне, только если строго ниже
/// неё.
///
/// ПРИМЕЧАНИЕ: намеренно самостоятельная копия, а не реэкспорт `const` из
/// `src/vfs/elf.rs`. Тот модуль тянет ядерные зависимости (VMM/PMM и крейт
/// `x86_64` paging), поэтому его импорт сделал бы этот чистый модуль — и значит
/// крейт `host-tests`, включающий его через `#[path]`, — неспособным собраться
/// на хосте. Значение фиксировано архитектурой.
pub const USER_ADDR_MAX: u64 = 0x0000_8000_0000_0000;

/// Итог чистой проверки диапазона пользовательского указателя.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PtrCheck {
    /// Диапазон целиком в пользовательской половине (или пуст) и может обходиться.
    Ok,
    /// Диапазон вне границ; syscall обязан вернуть `-EFAULT` (R1.5).
    Efault,
}

/// Чистая проверка диапазона пользовательского буфера `[start, start + len)`.
///
/// Возвращает [`PtrCheck::Ok`], когда `len == 0` (пустой буфер допустим всегда и
/// ничего не разыменовывает). Иначе [`PtrCheck::Efault`], когда:
///   * `start >= USER_ADDR_MAX`,
///   * `start + len` переполняет `u64`, или
///   * `start + len > USER_ADDR_MAX`.
/// Во всех прочих случаях — [`PtrCheck::Ok`]. Функция никогда не читает по
/// переданному адресу (R1.5).
pub fn check_user_range(start: u64, len: u64) -> PtrCheck {
    if len == 0 {
        return PtrCheck::Ok;
    }
    if start >= USER_ADDR_MAX {
        return PtrCheck::Efault;
    }
    match start.checked_add(len) {
        Some(end) if end <= USER_ADDR_MAX => PtrCheck::Ok,
        _ => PtrCheck::Efault,
    }
}

/// Чистый перечислитель баз страниц 4 КиБ, охваченных `[start, start + len)`.
///
/// При `len == 0` итератор пуст. При `len > 0` он выдаёт каждую базу страницы от
/// `first_page` (страница, содержащая `start`) до `last_page` (страница,
/// содержащая последний байт `start + len - 1`), включительно, с шагом
/// [`PAGE_SIZE`]. Эффектный обход присутствия страниц и property-тест используют
/// это, чтобы подтвердить отображение каждой охваченной страницы (R1.6).
///
/// Ожидается, что входы уже прошли [`check_user_range`], поэтому
/// `start + len - 1` не переполняется; saturating-арифметика всё равно делает
/// перечислитель тотальным.
pub fn spanned_pages(start: u64, len: u64) -> impl Iterator<Item = u64> {
    // Границы выбраны так, чтобы включительный диапазон был пуст при `len == 0`.
    let (first_page, last_page) = if len == 0 {
        // `1..=0` — пустой RangeInclusive, поэтому ни одной базы страницы не выдаётся.
        (1, 0)
    } else {
        let last_addr = start.saturating_add(len - 1);
        (page_base(start), page_base(last_addr))
    };
    (first_page..=last_page).step_by(PAGE_SIZE as usize)
}

/// Округлить адрес вниз до базы содержащей его страницы.
#[inline]
fn page_base(addr: u64) -> u64 {
    addr & !(PAGE_SIZE - 1)
}

/// Вид отображения, покрывающего страницу пользовательского буфера.
///
/// Это вход чистого правила [`page_acceptance`]: syscall-слой знает *вид*
/// региона (из `VmRegionSet`/границы ленивой кучи), а не «стоит ли требовать
/// PRESENT» — вопрос присутствия страницы решается уже внутри правила.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum MappingKind {
    /// Куча `brk` ниже границы ленивого роста: страница подпирается по касанию.
    LazyHeap,
    /// Анонимный `mmap`-регион: подпирается по касанию с правами региона.
    LazyAnon,
    /// Файловый (жадный) регион: отображён целиком, отсутствующий PTE — ошибка.
    Eager,
    /// `PROT_NONE`-регион: касание — SIGSEGV, а не повод что-то отобразить.
    ProtNone,
    /// Адрес не покрыт ни одним регионом и вне ленивой кучи.
    Outside,
}

/// Что делать со страницей пользовательского буфера перед копированием.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PageAcceptance {
    /// Страница резидентна — копировать можно (права проверяет копия).
    Accept,
    /// Страницы нет, но она законно ленивая — подпереть demand paging'ом.
    Populate,
    /// Отказ: `EFAULT`/SIGSEGV, страницу НЕ отображать.
    Refuse,
}

/// Чистое правило приёмки одной страницы пользовательского диапазона.
///
/// ПОЧЕМУ отдельная функция: раньше `read(2)`/`write(2)` требовали PRESENT на
/// каждой странице всего `count`, а обработчик #PF при этом умел обслуживать
/// отсутствующие страницы ленивого `brk`/anon-региона. Законный буфер (свежий
/// `Vec`/`Box` на куче или `mmap`-снимок) отвергался `EFAULT`'ом, хотя ядро
/// скопировало бы его без отказа. Правило ниже — единственное место, где
/// решается «подпереть или отказать», и его ровно же использует обработчик
/// page fault (`mem_sys::handle_user_page_fault`), поэтому подпирание из
/// syscall'а не может разойтись с политикой обработчика.
///
/// Инварианты безопасности (их pins host-свойство `user_buffer_paging`):
///   * отсутствие страницы само по себе НИКОГДА не даёт [`PageAcceptance::Accept`];
///   * `PROT_NONE` и адрес вне зарегистрированных регионов отвергаются всегда —
///     и когда страница отсутствует, и когда присутствует (иначе `mprotect`
///     стал бы совещательным на этом пути);
///   * `Populate` возможен только для ленивых видов (`LazyHeap`/`LazyAnon`);
///   * отсутствующая страница жадного файлового региона — ошибка ядра, а не
///     повод отобразить её молча.
pub fn page_acceptance(present: bool, kind: MappingKind) -> PageAcceptance {
    match kind {
        MappingKind::LazyHeap | MappingKind::LazyAnon => {
            if present {
                PageAcceptance::Accept
            } else {
                PageAcceptance::Populate
            }
        }
        MappingKind::Eager => {
            if present {
                PageAcceptance::Accept
            } else {
                PageAcceptance::Refuse
            }
        }
        MappingKind::ProtNone | MappingKind::Outside => PageAcceptance::Refuse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_range_rejects_kernel_and_out_of_half() {
        // Всё на границе и выше — адрес ядра; ровно этот класс значений
        // раньше доходил до mprotect() (см. `SECURITY.md`).
        assert_eq!(check_user_range(USER_ADDR_MAX, 4096), PtrCheck::Efault);
        assert_eq!(
            check_user_range(0xffff_ffff_801b_f000, 4096),
            PtrCheck::Efault
        );
        // Последняя пользовательская страница принимается; один байт за границей — нет.
        assert_eq!(check_user_range(USER_ADDR_MAX - 4096, 4096), PtrCheck::Ok);
        assert_eq!(check_user_range(USER_ADDR_MAX - 1, 2), PtrCheck::Efault);
        // Пустой диапазон ничего не разыменовывает и всегда допустим.
        assert_eq!(check_user_range(USER_ADDR_MAX, 0), PtrCheck::Ok);
        // Длина, переполняющая u64, отвергается, а не заворачивается.
        assert_eq!(check_user_range(u64::MAX - 10, 100), PtrCheck::Efault);
        // Обычные пользовательские буферы (стеки/кучи glibc) остаются принятыми.
        assert_eq!(check_user_range(0x7fff_ffff_f000, 4096), PtrCheck::Ok);
    }

    #[test]
    fn spanned_pages_is_bounded_by_the_range() {
        let pages: alloc::vec::Vec<u64> = spanned_pages(0x1000, 3 * 4096 + 1).collect();
        assert_eq!(pages, alloc::vec![0x1000, 0x2000, 0x3000, 0x4000]);
        assert_eq!(spanned_pages(0x1000, 0).count(), 0);
    }
}
