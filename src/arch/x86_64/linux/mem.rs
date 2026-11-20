//! Планирование memory-management syscall'ов и эффективные обработчики.
//!
//! Здесь только **чистая** логика планирования `brk`, `mmap`, `munmap` и общее
//! отображение флагов защиты: функции без аллокаций, только `core`, вычисляющие
//! результат без обращения к VMM/PMM, per-process `VmRegionSet`, железу или
//! глобальному изменяемому состоянию (R11.6). Эффектные обработчики, связывающие
//! эти планы с реальными таблицами страниц, живут в `mem_sys`.
//!
//! Тип `Errno` берётся из соседнего `errno` через `super::errno`, а
//! `USER_ADDR_MAX` — из `super::validate`, чтобы один и тот же исходник
//! компилировался и в ядре (`crate::arch::x86_64::linux::{errno, validate}`), и
//! будучи включённым в крейт `host-tests` как соседние модули корня (R11.6).
#![allow(dead_code)]

use super::errno::Errno;
use super::validate::USER_ADDR_MAX;

/// Размер страницы для всего округления и арифметики страниц (x86_64, 4 КиБ).
const PAGE_SIZE: u64 = 4096;

/// Бит защиты `mmap`/`mprotect`: регион можно читать.
pub const PROT_READ: u32 = 1;
/// Бит защиты `mmap`/`mprotect`: регион можно писать.
pub const PROT_WRITE: u32 = 2;
/// Бит защиты `mmap`/`mprotect`: регион можно исполнять.
pub const PROT_EXEC: u32 = 4;

/// Флаг `mmap`: отображение приватное (copy-on-write, не разделяемое).
pub const MAP_PRIVATE: u32 = 0x2;
/// Флаг `mmap`: отображение разделяемое (для memfd — общие физические кадры
/// между отображениями и процессами после `fork`).
pub const MAP_SHARED: u32 = 0x1;
/// Флаг `mmap`: отображение не опирается на файл (заполняется нулями).
pub const MAP_ANONYMOUS: u32 = 0x20;

/// Идентичность файла, лежащего за file-backed отображением, для
/// `/proc/<pid>/maps`.
///
/// Чистым планировщикам ([`plan_mmap_base`], [`plan_munmap`], …) незачем знать
/// про VFS, но файл maps должен печатать, чем отображение реально подпёрто: без
/// этой таблицы каждая file-backed строка заявляла бы `00:00 0` и отсутствие
/// pathname (`docs/procfs.md` DEVIATION-7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MmapBacking {
    /// Путь VFS, с которым открывали дескриптор, отображающий регион; тот же
    /// pathname, что печатает Linux для отображения.
    pub path: alloc::string::String,
    /// `st_ino`, который VFS сообщает для этого пути (`io_sys::node_ino`), чтобы
    /// строка maps сходилась с `stat(2)`/`fdinfo` по тому же файлу.
    pub ino: u64,
    /// Смещение в файле (байты), с которого отображается первая страница
    /// региона. Хранится выровненным по странице на момент map; частичные
    /// `munmap`/`mprotect` сдвигают его, чтобы каждый выживший поддиапазон
    /// сообщал своё истинное смещение.
    pub file_offset: u64,
}

/// Ранее отображённый регион в том виде, как его отслеживает per-process
/// `VmRegionSet`. Используется [`plan_munmap`] для подтверждения покрытия
/// запрошенного диапазона.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MmapRegion {
    /// База региона, выровненная на 4 КиБ.
    pub base: u64,
    /// Число страниц 4 КиБ в регионе.
    pub pages: u64,
    /// Несут ли страницы региона бит записи (`PROT_WRITE`).
    pub writable: bool,
    /// Несут ли страницы региона бит no-execute (`!PROT_EXEC`).
    pub nx: bool,
    /// Сырая битовая маска `prot`, с которой создан регион. Страницы `0`
    /// (`PROT_NONE`) НИКОГДА не должны подпираться кадрами: обработчик page fault
    /// трактует отказ внутри `PROT_NONE`-региона как настоящий SIGSEGV (семантика
    /// guard-страницы), а не как повод для ленивого отображения.
    pub prot: u32,
    /// Анонимный ли регион (нулевой, ленивый — страницы подпираются по первому
    /// касанию обработчиком page fault) в отличие от file-backed (жадно
    /// копируется ядром при `mmap`).
    pub anon: bool,
    /// Идентичность файла file-backed региона; `None` для анонимных (и для
    /// синтетического региона покрытия кучи). `/proc/<pid>/maps` печатает её как
    /// `maj:min ino pathname`.
    pub backing: Option<MmapBacking>,
}

/// Итог чистого шага планирования `brk` (R3.1–R3.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrkOutcome {
    /// Break не изменился; syscall сообщает это значение в `rax`.
    Unchanged(u64),
    /// Break растёт до `new_brk`; страницы `[map_from, map_to)` нужно отобразить.
    Grow {
        /// Новый program break, он же значение в `rax`.
        new_brk: u64,
        /// Выровненное (вниз) начало диапазона, который нужно подпереть страницами.
        map_from: u64,
        /// Выровненный (вверх) конец диапазона, который нужно подпереть страницами.
        map_to: u64,
    },
    /// Break сжимается до этого значения, оно же сообщается в `rax`.
    Shrink(u64),
}

/// Итог чистого шага планирования `mmap` (R4.1, R4.2, R4.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmapPlan {
    /// Запрос неверен; syscall возвращает это отрицательное errno.
    Reject(Errno),
    /// Отобразить `pages` заполненных нулями страниц по `base` с заданными
    /// битами защиты.
    Map {
        /// База нового отображения, выровненная на 4 КиБ.
        base: u64,
        /// Число страниц 4 КиБ для отображения (длина округлена вверх).
        pages: u64,
        /// Несут ли страницы бит записи (запрошен `PROT_WRITE`).
        writable: bool,
        /// Несут ли страницы бит no-execute (`PROT_EXEC` не запрошен).
        nx: bool,
    },
}

/// Итог чистого шага планирования `munmap` (R4.3, R4.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MunmapPlan {
    /// Запрос неверен; syscall возвращает это отрицательное errno.
    Reject(Errno),
    /// Снять отображение `pages` страниц начиная с `base`.
    Unmap {
        /// База для снятия отображения, выровненная на 4 КиБ.
        base: u64,
        /// Число страниц 4 КиБ для снятия (длина округлена вверх).
        pages: u64,
    },
}

/// Округлить `addr` вниз до базы содержащей его страницы.
#[inline]
fn page_down(addr: u64) -> u64 {
    addr & !(PAGE_SIZE - 1)
}

/// Округлить `addr` вверх до следующей границы страницы с насыщением на
/// `u64::MAX`.
///
/// Вызывающие передают только значения, уже известные как меньшие
/// `USER_ADDR_MAX`, поэтому saturating-сложение на практике никогда не
/// насыщается; оно делает хелпер тотальным.
#[inline]
fn page_up(addr: u64) -> u64 {
    page_down(addr.saturating_add(PAGE_SIZE - 1))
}

/// Вычислить `ceil(len / PAGE_SIZE)` без переполнения.
///
/// `None` только если `len + (PAGE_SIZE - 1)` переполнит `u64`; вызывающий
/// трактует это как неверный запрос.
#[inline]
fn pages_for_len(len: u64) -> Option<u64> {
    len.checked_add(PAGE_SIZE - 1).map(|n| n / PAGE_SIZE)
}

/// Отобразить маску битов защиты в пару атрибутов страницы `(writable, nx)`
/// (R4.2, R4.5 и — через `PF_W`/`PF_X` — R5.4).
///
/// * `writable` установлен тогда и только тогда, когда есть `PROT_WRITE`.
/// * `nx` (no-execute) установлен тогда и только тогда, когда `PROT_EXEC`
///   **отсутствует**.
///
/// Это единственное отображение переиспользуют `mmap`, `mprotect` и ELF-загрузчик,
/// поэтому политика writable/no-execute остаётся одинаковой во всех трёх
/// (Property 14).
pub fn prot_to_flags(prot: u32) -> (bool, bool) {
    let writable = prot & PROT_WRITE != 0;
    let nx = prot & PROT_EXEC == 0;
    (writable, nx)
}

/// Спланировать переход `brk` от `current` к `requested` при `initial` break
/// процесса, ничего не отображая (R3.1–R3.6, Property 11).
///
/// Ветки по порядку:
///   * `requested == 0` → [`BrkOutcome::Unchanged`] (запрос текущего break, R3.1).
///   * `requested >= USER_ADDR_MAX` → [`BrkOutcome::Unchanged`] (R3.5).
///   * `requested < initial` → [`BrkOutcome::Unchanged`] (ниже нижней границы, R3.6).
///   * `initial <= requested <= current` → [`BrkOutcome::Shrink`] (R3.3).
///   * `current < requested < USER_ADDR_MAX` → [`BrkOutcome::Grow`] с отображением
///     диапазона страниц `[page_down(current), page_up(requested))` (R3.2).
pub fn plan_brk(initial: u64, current: u64, requested: u64) -> BrkOutcome {
    if requested == 0 || requested >= USER_ADDR_MAX || requested < initial {
        return BrkOutcome::Unchanged(current);
    }
    if requested <= current {
        // initial <= requested <= current
        BrkOutcome::Shrink(requested)
    } else {
        // current < requested < USER_ADDR_MAX
        BrkOutcome::Grow {
            new_brk: requested,
            map_from: page_down(current),
            map_to: page_up(requested),
        }
    }
}

/// Спланировать размещение нефиксированного `mmap` из `pages` страниц: первый
/// свободный промежуток на `floor` или выше, вмещающий `[base, base + pages*4096)`
/// ниже `ceiling`, переиспользуя дыры от `munmap`/перемещений `mremap` вместо
/// вечного роста отметки максимума.
///
/// `None`, когда ниже `ceiling` не помещается ни один промежуток (вызывающий
/// сообщает `ENOMEM`). Скан идёт по регионам в произвольном порядке и
/// перезапускается после каждого попадания; каждое попадание строго двигает
/// `candidate`, поэтому цикл завершается.
pub fn plan_mmap_base(regions: &[MmapRegion], floor: u64, pages: u64, ceiling: u64) -> Option<u64> {
    let span = pages.checked_mul(PAGE_SIZE)?;
    let mut candidate = page_up(floor);
    loop {
        let end = candidate.checked_add(span)?;
        if end > ceiling {
            return None;
        }
        match regions.iter().find(|r| {
            let rend = r.base.saturating_add(r.pages.saturating_mul(PAGE_SIZE));
            r.base < end && candidate < rend
        }) {
            None => return Some(candidate),
            Some(r) => {
                let rend = r.base.saturating_add(r.pages.saturating_mul(PAGE_SIZE));
                candidate = page_up(rend);
            }
        }
    }
}

/// Выровнен ли `[start, start + pages*4096)` по страницам, лежит ли ниже
/// `ceiling` и не пересекает ли ни один отслеживаемый регион. Используется
/// `mremap`, чтобы решить, возможно ли расширение на месте (растущий регион сам
/// не пересекается: диапазон начинается у его конца).
pub fn range_is_free(regions: &[MmapRegion], start: u64, pages: u64, ceiling: u64) -> bool {
    if start & (PAGE_SIZE - 1) != 0 {
        return false;
    }
    let Some(span) = pages.checked_mul(PAGE_SIZE) else {
        return false;
    };
    let Some(end) = start.checked_add(span) else {
        return false;
    };
    if end > ceiling {
        return false;
    }
    regions.iter().all(|r| {
        let rend = r.base.saturating_add(r.pages.saturating_mul(PAGE_SIZE));
        !(r.base < end && start < rend)
    })
}

/// Спланировать анонимный `mmap` длины `len` с защитой `prot` и флагами `flags`
/// по заранее выровненной `hint_base`, ничего не отображая (R4.1, R4.2, R4.6,
/// Property 12).
///
/// Возвращает [`MmapPlan::Reject`] с `EINVAL`, когда:
///   * `len == 0`, или
///   * `flags` не равны ровно `MAP_ANONYMOUS | MAP_PRIVATE`, или
///   * `fd != -1`, или
///   * число страниц для `len` переполняется.
///
/// Иначе возвращает [`MmapPlan::Map`] с `pages = ceil(len / 4096)`, переданной
/// (предположительно выровненной на 4 КиБ) `hint_base` и `(writable, nx)` из
/// [`prot_to_flags`].
pub fn plan_mmap(len: u64, prot: u32, flags: u32, fd: i64, hint_base: u64) -> MmapPlan {
    if len == 0 || flags != (MAP_ANONYMOUS | MAP_PRIVATE) || fd != -1 {
        return MmapPlan::Reject(Errno::EINVAL);
    }
    let pages = match pages_for_len(len) {
        Some(p) => p,
        None => return MmapPlan::Reject(Errno::EINVAL),
    };
    let (writable, nx) = prot_to_flags(prot);
    MmapPlan::Map {
        base: hint_base,
        pages,
        writable,
        nx,
    }
}

/// Спланировать `munmap` `[base, base + len)` относительно набора ранее
/// отображённых `regions`, ничего не снимая (R4.3, R4.7, Property 13).
///
/// Возвращает [`MunmapPlan::Reject`] с `EINVAL`, когда:
///   * `base` не выровнен на 4 КиБ, или
///   * `len == 0`, или
///   * арифметика числа страниц/диапазона переполняется, или
///   * какая-либо страница 4 КиБ диапазона не покрыта регионом из `regions`.
///
/// Иначе возвращает [`MunmapPlan::Unmap`] с `pages = ceil(len / 4096)`.
pub fn plan_munmap(base: u64, len: u64, regions: &[MmapRegion]) -> MunmapPlan {
    if base & (PAGE_SIZE - 1) != 0 || len == 0 {
        return MunmapPlan::Reject(Errno::EINVAL);
    }
    let pages = match pages_for_len(len) {
        Some(p) => p,
        None => return MunmapPlan::Reject(Errno::EINVAL),
    };
    // Каждая страница в [base, base + pages*4096) должна быть покрыта каким-то
    // регионом.
    for i in 0..pages {
        let page = match i
            .checked_mul(PAGE_SIZE)
            .and_then(|off| base.checked_add(off))
        {
            Some(p) => p,
            None => return MunmapPlan::Reject(Errno::EINVAL),
        };
        if !regions.iter().any(|r| region_contains_page(r, page)) {
            return MunmapPlan::Reject(Errno::EINVAL);
        }
    }
    MunmapPlan::Unmap { base, pages }
}

/// Лежит ли страница 4 КиБ по `page` внутри `[base, base + pages*4096)` региона.
#[inline]
fn region_contains_page(region: &MmapRegion, page: u64) -> bool {
    let span = match region.pages.checked_mul(PAGE_SIZE) {
        Some(s) => s,
        None => return false,
    };
    let end = match region.base.checked_add(span) {
        Some(e) => e,
        None => return false,
    };
    page >= region.base && page < end
}

/// Per-`Compat_Process` учёт виртуальной памяти: program break плюс набор
/// анонимных `mmap`-регионов (design "MmapRegion / VM tracking").
///
/// Это состояние изменяют эффективные обработчики
/// `brk`/`mmap`/`munmap`/`mprotect` после обращения к чистым планировщикам
/// `plan_*` выше; аппаратного состояния оно не держит и остаётся `core` + `alloc`,
/// чтобы модуль оставался host-тестируемым (R11.6). Заполненный экземпляр
/// засевает `run_linux_binary` из `initial_brk` загрузчика и mmap-подсказки
/// нижней половины.
#[derive(Clone, Debug)]
pub struct VmRegionSet {
    /// Program break на старте процесса (нижняя граница, ниже которой `brk` не
    /// может опуститься).
    pub initial_brk: u64,
    /// Текущий program break (верх региона кучи).
    pub current_brk: u64,
    /// Анонимные `mmap`-регионы, отображённые сейчас для процесса.
    pub mmaps: alloc::vec::Vec<MmapRegion>,
    /// Bump-подсказка для базы следующего анонимного `mmap` в пользовательском
    /// диапазоне.
    pub mmap_next_hint: u64,
    /// Фиксированная нижняя отметка для размещения нефиксированного `mmap`:
    /// `plan_mmap_base` ищет свободные промежутки начиная здесь (так дыры ниже
    /// отметки максимума переиспользуются).
    pub mmap_floor: u64,
    /// Младшая страница demand-paged кучи `brk`: страницы в
    /// `[brk_lazy_from, page_up(current_brk))` подпираются по первому касанию, а
    /// не жадно. Стартует с (округлённого вниз) начального break и только
    /// опускается — рост `brk` от `current` записывает `page_down(current)` как
    /// кандидатную нижнюю границу.
    pub brk_lazy_from: u64,
}

impl VmRegionSet {
    /// Создать пустой набор регионов для процесса, чей начальный (и текущий)
    /// program break — `initial_brk`, а анонимные `mmap`-аллокации начинают
    /// bump-рост от `mmap_hint_base`.
    pub fn new(initial_brk: u64, mmap_hint_base: u64) -> Self {
        Self {
            initial_brk,
            current_brk: initial_brk,
            mmaps: alloc::vec::Vec::new(),
            mmap_next_hint: mmap_hint_base,
            mmap_floor: mmap_hint_base,
            brk_lazy_from: initial_brk & !(PAGE_SIZE - 1),
        }
    }
}
