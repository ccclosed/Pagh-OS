// memory/heap.rs — куча ядра / глобальный аллокатор.
//
// Ядро использует `good_memory_allocator` (galloc) как `#[global_allocator]`,
// чтобы работали типы `alloc` (Vec, Box, Arc, ...).
//
// ПОЧЕМУ НЕ linked_list_allocator: это чистый first-fit список свободных
// блоков, O(n) на аллокацию по числу свободных блоков. На разборе индекса
// пакетов `apt` — десятки тысяч мелких объектов на стадию — список разрастается,
// и скан на каждую аллокацию деградирует почти до O(n²), что выглядело как
// многоминутное зависание. galloc добавляет «smallbins» по размерам
// (в стиле dlmalloc) и даёт типично ~O(1) на allocate/free.
//
// Куча — регион ФИКСИРОВАННОГО размера: `init()` отображает
// `HEAP_INITIAL_PAGES` страниц начиная с `layout::heap_base()` и отдаёт этот
// непрерывный регион аллокатору. galloc не растёт по требованию — если
// запрос не удовлетворить, он возвращает null (Requirement 10.4), и обвязка
// `alloc` превращает это в abort. Нужна куча больше — поднять
// `HEAP_INITIAL_PAGES` в `memory::layout`.
//
// `HEAP_INITIAL_PAGES` — желаемый размер для эталонной конфигурации (≥ 1 ГиБ
// RAM). На машинах с меньшей памятью `init` урезает кучу до того, что PMM может
// отдать (см. `cap_to_available`), вместо исчерпания пула кадров и паники при
// загрузке.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

use good_memory_allocator::SpinLockedAllocator;

/// Обёртка над galloc, безопасная к прерываниям.
///
/// Внутренний спинлок `SpinLockedAllocator` НЕ выключает прерывания, поэтому
/// таймер мог вытеснить поток ПОСРЕДИ АЛЛОКАЦИИ, пока тот держал лок кучи.
/// Любой другой контекст, аллоцирующий под одним из выключающих прерывания
/// `Spinlock` ядра (VFS, ext2, консоль, ...), крутился бы на локе кучи с
/// выключенными прерываниями, а вытесненный владелец никогда бы не
/// выполнился — ядро замирало. Разбор индекса apt аллоцирует миллионы раз,
/// что делало первый провижининг надёжным триггером.
///
/// Исправление: выключать прерывания на (микросекундное) время каждой операции
/// с кучей, как делает `crate::sync::spinlock::Spinlock`. Тогда владелец лока
/// кучи не может быть вытеснен посреди удержания, и каждый ожидающий крутится
/// против РАБОТАЮЩЕГО владельца — ожидание ограничено.
struct IrqSafeAllocator {
    inner: SpinLockedAllocator,
}

impl IrqSafeAllocator {
    #[inline]
    fn guarded<R>(&self, f: impl FnOnce(&SpinLockedAllocator) -> R) -> R {
        let were_enabled = crate::arch::cpu::interrupts_enabled();
        crate::arch::cpu::disable_interrupts();
        let r = f(&self.inner);
        if were_enabled {
            crate::arch::cpu::enable_interrupts();
        }
        r
    }
}

/// Живые байты, выданные успешными аллокациями (размер запроса прибавляется на
/// alloc/realloc-grow и вычитается на free/shrink). Только диагностика
/// (`stats`); считаются запрошенные байты, а не накладные расходы аллокатора,
/// поэтому реальный объём чуть больше.
static HEAP_USED: AtomicUsize = AtomicUsize::new(0);

/// Вычесть `bytes` из [`HEAP_USED`] без underflow (случайный/двойной free не
/// должен завернуть счётчик в астрономическое «used»).
fn heap_used_sub(bytes: usize) {
    let mut cur = HEAP_USED.load(Ordering::Relaxed);
    loop {
        if cur == 0 {
            return;
        }
        match HEAP_USED.compare_exchange_weak(
            cur,
            cur.saturating_sub(bytes),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(now) => cur = now,
        }
    }
}

// SAFETY: делегирует `SpinLockedAllocator` (корректный `GlobalAlloc`); обёртка
// лишь добавляет маскирование прерываний вокруг каждой операции и учёт живых
// байт для `stats`.
unsafe impl GlobalAlloc for IrqSafeAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = self.guarded(|a| a.alloc(layout));
        if !ptr.is_null() {
            HEAP_USED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.guarded(|a| a.dealloc(ptr, layout));
        heap_used_sub(layout.size());
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = self.guarded(|a| a.alloc_zeroed(layout));
        if !ptr.is_null() {
            HEAP_USED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = self.guarded(|a| a.realloc(ptr, layout, new_size));
        if !new_ptr.is_null() {
            // При ошибке galloc оставляет исходную аллокацию живой, поэтому
            // только успешное изменение размера двигает счётчик (старый размер
            // вычесть, новый прибавить).
            heap_used_sub(layout.size());
            HEAP_USED.fetch_add(new_size, Ordering::Relaxed);
        }
        new_ptr
    }
}

/// Экземпляр глобального аллокатора (Requirement 10.1). Объявлен ровно один раз.
#[global_allocator]
static ALLOCATOR: IrqSafeAllocator = IrqSafeAllocator {
    inner: SpinLockedAllocator::empty(),
};

/// Всего байт, отданных аллокатору в [`init`], записано для [`stats`].
static HEAP_SIZE: AtomicUsize = AtomicUsize::new(0);

/// Инициализировать кучу ядра.
///
/// Берёт базу/размер кучи из `memory::layout` (Requirement 4.3), отображает
/// физические кадры и инициализирует аллокатор над этим регионом. Куча
/// критична для загрузки, поэтому ошибка отображения — паника.
///
/// Куча ФИКСИРОВАННОГО размера, но `HEAP_INITIAL_PAGES` описывает *желаемый*
/// размер для эталонной конфигурации (≥ 1 ГиБ RAM — см. `layout.rs`), а не
/// требование к машине. Гость с меньшей памятью иначе съел бы циклом
/// отображения все свободные кадры PMM (включая нужные таблицам страниц и
/// будущим пользовательским страницам) и упал бы при загрузке. Поэтому цикл
/// ниже ограничивает кучу тем, что PMM реально может отдать, не морируя
/// остальное ядро: резервирует четверть свободных кадров (минимум 8 МиБ) под
/// пост-загрузочных потребителей PMM (пользовательские страницы, COW-fork,
/// стеки ядра, промежуточные таблицы страниц самой кучи) и урезает кучу до
/// остатка, предупреждая на уровне `Warn`. На эталонной конфигурации предел не
/// срабатывает, и отображается вся куча 512 МиБ.
pub fn init() {
    let heap_base = crate::memory::layout::heap_base();
    let initial_pages = crate::memory::layout::HEAP_INITIAL_PAGES;
    let initial_pages = cap_to_available(initial_pages);
    let heap_size = initial_pages * crate::memory::layout::PAGE_SIZE;

    // Отобразить начальные страницы кучи: один физический кадр на страницу,
    // отображение W^X (запись, никогда исполнение).
    let mut addr = heap_base;
    for _ in 0..initial_pages {
        let frame = crate::memory::pmm::alloc_frame()
            .expect("PMM: failed to allocate frame for kernel heap");
        let flags = x86_64::structures::paging::PageTableFlags::PRESENT
            | x86_64::structures::paging::PageTableFlags::WRITABLE
            | x86_64::structures::paging::PageTableFlags::NO_EXECUTE;
        crate::memory::vmm::map(frame, addr, flags).expect("VMM: failed to map kernel heap page");
        addr += crate::memory::layout::PAGE_SIZE;
    }

    // Отдать только что отображённый регион аллокатору.
    //
    // SAFETY: регион `[heap_base, heap_base + heap_size)` только что отображён
    // выше как present + writable и принадлежит исключительно куче. Он валиден
    // на всё время `'static` (куча ядра никогда не сносится). `init` обязан
    // вызываться ровно один раз, до любой аллокации; загрузка вызывает его в
    // упорядоченной последовательности инициализации до запуска потребителей
    // кучи.
    unsafe {
        ALLOCATOR.inner.init(heap_base as usize, heap_size as usize);
    }
    HEAP_SIZE.store(heap_size as usize, Ordering::Relaxed);

    crate::debug!(
        "Kernel heap initialized: 0x{:x}..0x{:x} ({} KB, fixed, galloc)",
        heap_base,
        heap_base + heap_size,
        heap_size / 1024
    );
}

/// Урезать `requested` страниц кучи до того, что PMM может отдать при загрузке.
///
/// См. [`init`]: ядро оставляет четверть свободных кадров (минимум
/// `HEAP_MIN_RESERVE_FRAMES`) всему, что PMM должен обслужить после загрузки.
/// Возвращает `requested` без изменений, когда RAM хватает, поэтому поведение
/// на эталонной конфигурации идентично фиксированной куче прошлых релизов.
fn cap_to_available(requested: u64) -> u64 {
    let free = crate::memory::pmm::free_frames() as u64;
    let reserve = (free / 4).max(HEAP_MIN_RESERVE_FRAMES);
    let capped = requested.min(free.saturating_sub(reserve));
    if capped < requested {
        crate::warn!(
            "Kernel heap capped: {} pages ({} MiB) requested, RAM allows {} pages ({} MiB); \
             {} MiB left for PMM",
            requested,
            requested * crate::memory::layout::PAGE_SIZE / (1024 * 1024),
            capped,
            capped * crate::memory::layout::PAGE_SIZE / (1024 * 1024),
            free.saturating_sub(capped) * crate::memory::layout::PAGE_SIZE / (1024 * 1024)
        );
    }
    capped
}

/// Кадры, недоступные куче на машинах с дефицитом RAM (8 МиБ): собственные
/// таблицы страниц кучи, пользовательские страницы и COW-fork не должны быть
/// обнулены циклом отображения кучи.
const HEAP_MIN_RESERVE_FRAMES: u64 = 8 * 1024 * 1024 / crate::memory::layout::PAGE_SIZE;

/// Сообщить учёт кучи ядра как `(size, used, free)` в байтах.
///
/// ДИАГНОСТИЧЕСКИЙ хелпер: используется self-тестом `lx_bigindex` и
/// feature-гейтованным прогресс-логом `apt::update`. `used` — живые
/// запрошенные байты (успешные аллокации минус освобождения), без накладных
/// расходов на чанк; реальный объём чуть больше. `free` — размер кучи минус
/// `used`.
pub fn stats() -> (usize, usize, usize) {
    let size = HEAP_SIZE.load(Ordering::Relaxed);
    let used = HEAP_USED.load(Ordering::Relaxed).min(size);
    (size, used, size - used)
}
