// memory/pmm.rs — Physical Memory Manager (битовый аллокатор кадров).

use crate::memory::cow::CowRefs;
use crate::sync::spinlock::Spinlock;
use limine::memmap;
use limine::request::MemmapResponse;

const FRAME_SIZE: u64 = 4096;

/// Всё ниже 1 МиБ зарезервировано навсегда (IVT реального режима, BIOS data
/// area и прочие legacy/прошивочные структуры). Освобождение адреса ниже этого
/// порога — всегда no-op.
const LOW_RESERVED: u64 = 0x100000;

/// Максимальное число физических диапазонов образа ядра, захватываемых в
/// [`init`] из записей memmap Limine `MEMMAP_EXECUTABLE_AND_MODULES`
/// (ядро и модули). Хранилище фиксированного размера используется потому, что
/// PMM инициализируется до появления аллокатора кучи, и динамической аллокации
/// здесь нет.
const MAX_KERNEL_RANGES: usize = 16;

/// Состояние Physical Memory Manager (битовый аллокатор).
///
/// Всё состояние PMM живёт здесь, за единым спинлоком [`PMM`]. Битмап
/// размещается в пригодной RAM в [`init`] и доступен через отображение HHDM,
/// поэтому `&'static mut [u64]` валиден всё время жизни ядра.
struct Pmm {
    /// Битмап: один бит на кадр 4 КБ. 1 = свободен, 0 = занят.
    bitmap: &'static mut [u64],
    /// Общее число отслеживаемых кадров.
    total_frames: usize,
    /// Число свободных кадров.
    free_count: usize,
    /// База физического адреса, с которой битмап начинает отсчёт.
    base_addr: u64,
    /// Старший отслеживаемый адрес + 1.
    top_addr: u64,
    /// Физическая база собственного хранилища битмапа.
    bitmap_phys: u64,
    /// Размер собственного хранилища битмапа в байтах.
    bitmap_bytes: u64,
    /// Физические диапазоны `[start, end)` образа ядра, захваченные в init из
    /// записей Limine `MEMMAP_EXECUTABLE_AND_MODULES`. Освобождение любого адреса
    /// внутри такого диапазона отвергается.
    kernel_ranges: [(u64, u64); MAX_KERNEL_RANGES],
    /// Число валидных записей в [`Pmm::kernel_ranges`].
    kernel_range_count: usize,
}

impl Pmm {
    /// `true`, если `addr` попадает в постоянно зарезервированный регион,
    /// который освобождение никогда не должно возвращать в пул: ниже 1 МиБ,
    /// внутрь собственных кадров битмапа или в захваченный диапазон образа ядра.
    ///
    /// На init эти кадры не помечались свободными, поэтому выставление их бита
    /// при случайном free испортило бы пул (Property 4).
    fn is_reserved(&self, addr: u64) -> bool {
        if addr < LOW_RESERVED {
            return true;
        }
        if addr >= self.bitmap_phys && addr < self.bitmap_phys + self.bitmap_bytes {
            return true;
        }
        for i in 0..self.kernel_range_count {
            let (start, end) = self.kernel_ranges[i];
            if addr >= start && addr < end {
                return true;
            }
        }
        false
    }
}

/// Глобальное состояние PMM под единым спинлоком.
///
/// `None`, пока не выполнен [`init`]. Лок даёт эксклюзивный доступ, и именно
/// поэтому хранение `&'static mut [u64]` внутри остаётся корректным между
/// вызывающими.
static PMM: Spinlock<Option<Pmm>> = Spinlock::new(None);

/// Инициализировать Physical Memory Manager по карте памяти Limine.
pub fn init(memmap: &MemmapResponse) {
    let entries = memmap.entries();

    // Найти диапазон: минимальная пригодная база и максимальная пригодная вершина.
    let mut base = u64::MAX;
    let mut top = 0u64;
    let mut total_usable = 0u64;

    // Первый проход: определить общий диапазон и объём пригодной памяти.
    for entry in entries {
        if entry.type_ == memmap::MEMMAP_USABLE {
            if entry.base < base {
                base = entry.base;
            }
            let entry_top = entry.base + entry.length;
            if entry_top > top {
                top = entry_top;
            }
            total_usable += entry.length;
        }
    }

    if base == u64::MAX {
        crate::error!("[PMM] no usable memory found!");
        return;
    }

    // Выровнять базу вниз, вершину — вверх до FRAME_SIZE.
    base &= !(FRAME_SIZE - 1);
    top = (top + FRAME_SIZE - 1) & !(FRAME_SIZE - 1);

    let total_frames = ((top - base) / FRAME_SIZE) as usize;
    // Размер битмапа в байтах: total_frames / 8 с округлением до выравнивания на 8.
    let bitmap_words = (total_frames + 63) / 64;
    let bitmap_bytes = bitmap_words * 8;

    crate::debug!(
        "Memory range: 0x{:x}..0x{:x} ({} MB)",
        base,
        top,
        (top - base) / (1024 * 1024)
    );
    crate::debug!(
        "Total frames: {}, bitmap: {} bytes",
        total_frames,
        bitmap_bytes
    );

    // Найти пригодный регион, достаточно большой для битмапа.
    // Каждый кандидат проверяется целиком: предпочесть 2 МиБ внутрь региона
    // (собственные кадры битмапа резервируются явно ниже, так что смещение —
    // лишь выбор места), иначе начало региона; регион годится, только когда ВЕСЬ
    // битмап помещается в него. Сканирование всех пригодных регионов (а не
    // только первого) не даёт маленькой низкой дыре выиграть у более позднего
    // региона, который действительно подходит.
    let mut bitmap_phys_addr: u64 = 0;
    for entry in entries {
        if entry.type_ == memmap::MEMMAP_USABLE {
            let region_end = entry.base + entry.length;
            if entry.length >= bitmap_bytes as u64 {
                let candidate = (entry.base + 0x200000) & !(FRAME_SIZE - 1); // смещение 2 МиБ
                if candidate + bitmap_bytes as u64 <= region_end {
                    bitmap_phys_addr = candidate;
                    break;
                }
                bitmap_phys_addr = entry.base;
                break;
            }
        }
    }

    if bitmap_phys_addr == 0 {
        // Ни один пригодный регион не вмещает весь битмап. Продолжать с
        // урезанным битмапом (или вылезающим за регион) значило бы позволить
        // аллокатору выдавать или портить кадры вне покрытого диапазона, поэтому
        // отказываемся грузиться, а не падаем молча позже.
        panic!(
            "[PMM] no usable memory region fits the {}-byte frame bitmap ({} frames in 0x{:x}..0x{:x}); cannot initialize the frame allocator",
            bitmap_bytes, total_frames, base, top
        );
    }

    crate::debug!("Bitmap at physical: 0x{:x}", bitmap_phys_addr);

    // Захватить физический диапазон(ы) образа ядра из записей memmap Limine
    // «ядро и модули». Эти кадры зарезервированы (на init не освобождаются) и
    // никогда не должны вернуться в пул при случайном free (R6.1/R6.2). Каждый
    // диапазон выравниваем по страницам наружу, чтобы частично-кадровая запись
    // полностью покрывала затронутые кадры.
    let mut kernel_ranges = [(0u64, 0u64); MAX_KERNEL_RANGES];
    let mut kernel_range_count = 0usize;
    for entry in entries {
        if entry.type_ == memmap::MEMMAP_EXECUTABLE_AND_MODULES {
            if kernel_range_count >= MAX_KERNEL_RANGES {
                crate::warn!(
                    "[PMM] more kernel/module ranges than MAX_KERNEL_RANGES; some not tracked"
                );
                break;
            }
            let start = align_down(entry.base, FRAME_SIZE);
            let end = align_up(entry.base + entry.length, FRAME_SIZE);
            kernel_ranges[kernel_range_count] = (start, end);
            kernel_range_count += 1;
            crate::debug!(
                "[PMM] reserved kernel/module range: 0x{:x}..0x{:x}",
                start,
                end
            );
        }
    }

    // Отобразить битмап в адресное пространство ядра через HHDM.
    let hhdm = crate::HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed);
    let bitmap_virt = (bitmap_phys_addr + hhdm) as *mut u64;
    let bitmap_slice: &'static mut [u64];

    // SAFETY: регион битмапа находится в пригодной RAM, поэтому доступ через
    // HHDM валиден. Сначала обнуляем его.
    unsafe {
        let slice_ptr = core::slice::from_raw_parts_mut(bitmap_virt, bitmap_words);
        slice_ptr.fill(0);
        bitmap_slice = slice_ptr;
    }

    // Пометить все кадры занятыми, затем освободить пригодные регионы.
    let mut pmm = Pmm {
        bitmap: bitmap_slice,
        total_frames,
        free_count: 0,
        base_addr: base,
        top_addr: top,
        bitmap_phys: bitmap_phys_addr,
        bitmap_bytes: bitmap_bytes as u64,
        kernel_ranges,
        kernel_range_count,
    };

    // ─── Логика резервирования (free/reserve кадра) ─────────────────────
    // Точное резервирование. Явных резерваций здесь только две:
    //   1. Ниже 1 МиБ (`addr < 0x100000`): там IVT реального режима, BIOS data
    //      area и legacy/прошивочные структуры. Их нельзя выдавать (Property 2),
    //      поэтому пропускаем каждый кадр ниже 1 МиБ.
    //   2. Собственные физические кадры битмапа (пропускаются ниже и повторно
    //      утверждаются занятыми на завершающем проходе).
    //
    // Сам образ ядра резервируется *неявно* классификацией, а не магическим
    // порогом: цикл освобождает только кадры, помеченные Limine как
    // MEMMAP_USABLE, а физические кадры ядра помечены как
    // MEMMAP_KERNEL_AND_MODULES (не usable). Поэтому они никогда не
    // освобождаются и остаются занятыми (0) — так ни один адрес зарезервированного
    // образа ядра не может вернуться из `alloc_frame` (Property 2). То же для
    // регионов MMIO/reserved/ACPI. Это заменяет прежнее грубое резервирование
    // «первые 8 МиБ» (`addr < 0x800000`), без нужды отнимавшее пригодную RAM
    // выше ядра.

    // Освободить кадры в пригодных регионах, но защитить сам битмап.
    for entry in entries {
        if entry.type_ == memmap::MEMMAP_USABLE {
            let region_start = entry.base;
            let region_end = entry.base + entry.length;

            // Пометить кадры региона свободными, кроме занятых битмапом.
            let frame_start = align_up(region_start, FRAME_SIZE);
            let frame_end = align_down(region_end, FRAME_SIZE);

            let mut addr = frame_start;
            while addr < frame_end {
                // (1) Зарезервировать всё ниже 1 МиБ (legacy/BIOS/IVT).
                if addr < 0x100000 {
                    addr += FRAME_SIZE;
                    continue;
                }
                // (2) Пропустить собственные физические кадры битмапа.
                if addr >= bitmap_phys_addr && addr < bitmap_phys_addr + bitmap_bytes as u64 {
                    addr += FRAME_SIZE;
                    continue;
                }

                let idx = ((addr - base) / FRAME_SIZE) as usize;
                let word = idx / 64;
                let bit = idx % 64;
                if word < pmm.bitmap.len() {
                    pmm.bitmap[word] |= 1u64 << bit;
                    pmm.free_count += 1;
                }
                addr += FRAME_SIZE;
            }
        }
    }

    // Также пометить собственные кадры битмапа занятыми.
    let mut bm_addr = bitmap_phys_addr;
    while bm_addr < bitmap_phys_addr + bitmap_bytes as u64 {
        let idx = ((bm_addr - base) / FRAME_SIZE) as usize;
        let word = idx / 64;
        let bit = idx % 64;
        if word < pmm.bitmap.len() {
            if pmm.bitmap[word] & (1u64 << bit) != 0 {
                pmm.bitmap[word] &= !(1u64 << bit);
                pmm.free_count -= 1;
            }
        }
        bm_addr += FRAME_SIZE;
    }

    let free_count = pmm.free_count;
    let total = pmm.total_frames;

    // Опубликовать полностью собранное состояние. Логирование держать вне секции лока.
    *PMM.lock() = Some(pmm);

    crate::debug!(
        "PMM Initialized: {} / {} frames free ({} MB / {} MB)",
        free_count,
        total,
        (free_count as u64 * FRAME_SIZE) / (1024 * 1024),
        (total as u64 * FRAME_SIZE) / (1024 * 1024),
    );

    let _ = total_usable;
}

/// Выделить один физический кадр (4 КБ).
/// Возвращает физический адрес кадра или None при нехватке памяти.
pub fn alloc_frame() -> Option<u64> {
    let mut guard = PMM.lock();
    let pmm = match *guard {
        Some(ref mut pmm) => pmm,
        None => return None, // Не инициализирован
    };

    let base_addr = pmm.base_addr;

    // Простой линейный поиск свободного кадра.
    for word_idx in 0..pmm.bitmap.len() {
        let word = pmm.bitmap[word_idx];
        if word != 0 {
            let bit = word.trailing_zeros() as usize;
            pmm.bitmap[word_idx] &= !(1u64 << bit);
            pmm.free_count -= 1;

            let frame_idx = word_idx * 64 + bit;
            let addr = base_addr + (frame_idx as u64) * FRAME_SIZE;
            return Some(addr);
        }
    }

    None
}

/// Выделить `count` физически непрерывных кадров по 4 КиБ.
///
/// Сканирует битмап в поисках первого прогона из `count` подряд *свободных*
/// кадров (бит == 1), помечает их занятыми (бит == 0) и возвращает физическую
/// базу прогона. `None`, если `count == 0`, PMM не инициализирован или
/// непрерывного прогона из `count` свободных кадров нет.
///
/// Гарантии (как у [`alloc_frame`]): возвращённая база выровнена на страницу
/// 4096; база `>= 0x100000` — кадры ниже 1 МиБ и все прочие зарезервированные
/// регионы (образ ядра, кадры битмапа, MMIO/ACPI) помечены занятыми в [`init`],
/// поэтому не могут попасть в свободный прогон и не возвращаются здесь; все
/// `count` кадров были свободны и стали заняты, так что прогон не пересекается
/// с другой живой аллокацией.
///
/// `count == 1` ведёт себя как [`alloc_frame`] (первый свободный кадр). Работает
/// под единым спинлоком [`PMM`], не трогая существующий API одиночных кадров.
/// Каждый успешный вызов пар должен иметь ровно один
/// [`free_frames_contiguous`] с теми же `base`/`count`.
pub fn alloc_frames_contiguous(count: usize) -> Option<u64> {
    if count == 0 {
        return None;
    }

    let mut guard = PMM.lock();
    let pmm = match *guard {
        Some(ref mut pmm) => pmm,
        None => return None, // Не инициализирован
    };

    let total = pmm.total_frames;
    if count > total {
        return None;
    }

    // Линейный поиск первого прогона из `count` идущих подряд свободных кадров.
    //
    // Инвариант цикла: при `run_len > 0` кадры в
    // `[run_start, run_start + run_len)` все свободны для текущего `run_len`.
    // Биты за `total_frames` в последнем слове битмапа нулевые (заняты) и не
    // сканируются, поэтому кадры вне диапазона не могут стать частью прогона.
    let mut run_start: usize = 0;
    let mut run_len: usize = 0;
    let mut found: Option<usize> = None;

    let mut idx = 0usize;
    while idx < total {
        let word = idx / 64;
        let bit = idx % 64;
        let is_free = (pmm.bitmap[word] >> bit) & 1 == 1;

        if is_free {
            if run_len == 0 {
                run_start = idx;
            }
            run_len += 1;
            if run_len == count {
                found = Some(run_start);
                break;
            }
        } else {
            run_len = 0;
        }
        idx += 1;
    }

    let start = found?;

    // Пометить весь прогон занятым.
    for i in start..start + count {
        let word = i / 64;
        let bit = i % 64;
        pmm.bitmap[word] &= !(1u64 << bit);
    }
    pmm.free_count -= count;

    let addr = pmm.base_addr + (start as u64) * FRAME_SIZE;
    Some(addr)
}

/// Освободить `count`-кадровый физически непрерывный прогон, ранее выданный
/// [`alloc_frames_contiguous`].
///
/// Очищает (помечает свободными) `count` идущих подряд кадров от `base`,
/// восстанавливая счётчик свободных ровно на число реально занятых кадров
/// (идемпотентно покадрово, как [`free_frame`]). Кадры вне диапазона
/// пропускаются. `base` должен быть выровненной на страницу базой, возвращённой
/// парным `alloc_frames_contiguous(count)`.
pub fn free_frames_contiguous(base: u64, count: usize) {
    if count == 0 {
        return;
    }

    let mut guard = PMM.lock();
    let pmm = match *guard {
        Some(ref mut pmm) => pmm,
        None => return, // Не инициализирован
    };

    if base < pmm.base_addr || base >= pmm.top_addr {
        return; // Вне нашего диапазона
    }

    let start = ((base - pmm.base_addr) / FRAME_SIZE) as usize;
    for i in start..start + count {
        if i >= pmm.total_frames {
            break;
        }
        let addr = pmm.base_addr + (i as u64) * FRAME_SIZE;
        // Отказать в освобождении зарезервированных кадров (ниже 1 МиБ, кадры
        // битмапа или диапазон образа ядра). Для них free_count и битмап не
        // меняются (R6.1).
        if pmm.is_reserved(addr) {
            continue;
        }
        let word = i / 64;
        let bit = i % 64;
        if pmm.bitmap[word] & (1u64 << bit) == 0 {
            pmm.bitmap[word] |= 1u64 << bit;
            pmm.free_count += 1;
        }
    }
}

/// Освободить ранее выделенный физический кадр.
///
/// Кадр, у которого ещё есть зарегистрированные COW-шарящие, ОТВЕРГАЕТСЯ
/// (с логом), а не возвращается в битмап: у каждого такого шарящего есть живой
/// PTE в кадр, и освобождение отдало бы всё ещё отображённую память следующему
/// `alloc_frame` — use-after-free. Легитимный путь освобождения
/// ([`crate::memory::vmm::release_user_leaf`]) сначала зовёт [`cow_unref`] и
/// приходит сюда только когда последняя ссылка ушла.
pub fn free_frame(addr: u64) {
    // Проверка владения берёт COW_REFS до PMM и отпускает его первым; ни один
    // путь не держит COW_REFS во время захвата PMM, поэтому два спинлока никогда
    // не вложены (оба маскируют прерывания на время удержания).
    if frame_is_cow_shared(addr) {
        crate::error!(
            "[PMM] refusing to free COW-shared frame=0x{:016x} - release path must cow_unref first",
            addr
        );
        return;
    }
    let mut guard = PMM.lock();
    let pmm = match *guard {
        Some(ref mut pmm) => pmm,
        None => return, // Не инициализирован
    };

    if addr < pmm.base_addr || addr >= pmm.top_addr {
        return; // Вне нашего диапазона
    }

    // Отказать в освобождении зарезервированных кадров (ниже 1 МиБ, кадры
    // битмапа или диапазон образа ядра). free_count и битмап не меняются (R6.1).
    if pmm.is_reserved(addr) {
        return;
    }

    let idx = ((addr - pmm.base_addr) / FRAME_SIZE) as usize;
    let word = idx / 64;
    let bit = idx % 64;

    let mut double_free = false;
    if word < pmm.bitmap.len() {
        if pmm.bitmap[word] & (1u64 << bit) == 0 {
            pmm.bitmap[word] |= 1u64 << bit;
            pmm.free_count += 1;
        } else {
            // Кадр уже помечен свободным -> double free (или освобождение кадра,
            // которым этот вызывающий никогда не владел). Это ровно та ошибка
            // учёта, которая позже проявляется как «страница постороннего
            // процесса вдруг читается нулями», когда кадр заново выделяется и
            // обнуляется новым владельцем.
            double_free = true;
        }
    }
    drop(guard);
    if double_free {
        crate::error!(
            "[PMM] DOUBLE FREE frame=0x{:016x} - frame ownership bug (see caller)",
            addr
        );
    }
}

// ─── разделение COW-кадров (fork copy-on-write) ──────────────────────────────
//
// `fork_user_space_cow` разделяет каждый листовой кадр между адресными
// пространствами родителя и ребёнка вместо копирования. Физический адрес кадра
// индексируется в [`CowRefs`] с числом адресных пространств, ссылающихся на него
// как COW-шарящие. При fork кадр входит со счётчиком 2, а уходит либо через
// copy-on-write fault (одна сторона получает приватную копию), либо через
// освобождение листа (`munmap`, `mprotect(PROT_NONE)`, замена `MAP_FIXED`, снос
// адресного пространства). Кадр возвращается в битмап ровно когда счётчик
// достигает нуля — никогда раньше, поэтому первым вышедший ребёнок не вырвет
// данные родителя.
//
// Карта намеренно отделена от битмапа: считаются только COW-кадры, ядерные и
// приватные доку-кадры держат обычный протокол alloc/free.
//
// Учёт живёт в `memory::cow` (чистый, host-тестируемый модуль), поэтому правила
// владения проверяются `host-tests` по тому же исходнику, что компилирует ядро.
// Этот файл лишь оборачивает его в безопасный к прерываниям спинлок.

static COW_REFS: Spinlock<CowRefs> = Spinlock::new(CowRefs::new());

/// Зарегистрировать ещё одного COW-шарящего `frame`. `fork_user_space_cow`
/// зовёт это по разу на каждую сторону, которая заново ссылается на лист как
/// COW: два вызова для прежде приватного листа (родитель входит в карту вместе
/// с ребёнком), один — при fork уже разделяемого листа.
pub fn cow_ref(frame: u64) {
    COW_REFS.lock().acquire(frame);
}

/// Отпустить одного COW-шарящего `frame`. Возвращает `true`, когда вызывающий
/// обязан вернуть кадр в битмап свободных (на него больше не ссылается ни одно
/// адресное пространство); `false`, пока остался хотя бы один шарящий.
pub fn cow_unref(frame: u64) -> bool {
    COW_REFS.lock().release(frame)
}

/// Текущий COW-счётчик `frame` (0, когда кадр приватный/неучтённый).
///
/// Используется диагностикой и in-QEMU self-тестом владения; пути
/// освобождения обязаны использовать возврат [`cow_unref`], а не
/// последовательность «прочитать, потом освободить».
pub fn cow_refs(frame: u64) -> u32 {
    COW_REFS.lock().refs(frame)
}

/// Число кадров, отслеживаемых сейчас как COW-разделяемые (диагностика/selftest).
pub fn cow_tracked_frames() -> usize {
    COW_REFS.lock().tracked_frames()
}

/// `true`, пока у `frame` есть хотя бы один зарегистрированный COW-шарящий.
fn frame_is_cow_shared(frame: u64) -> bool {
    COW_REFS.lock().refs(frame) != 0
}

/// Общее число отслеживаемых кадров.
pub fn total_frames() -> usize {
    match *PMM.lock() {
        Some(ref pmm) => pmm.total_frames,
        None => 0,
    }
}

/// Число свободных кадров.
pub fn free_frames() -> usize {
    match *PMM.lock() {
        Some(ref pmm) => pmm.free_count,
        None => 0,
    }
}

// ─── Хелперы ─────────────────────────────────────────────────────────────

fn align_up(addr: u64, align: u64) -> u64 {
    (addr + align - 1) & !(align - 1)
}

fn align_down(addr: u64, align: u64) -> u64 {
    addr & !(align - 1)
}
