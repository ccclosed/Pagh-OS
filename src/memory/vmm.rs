// memory/vmm.rs — 4-уровневый paging (Virtual Memory Manager).

use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::tlb;
use x86_64::structures::paging::page_table::{PageTableEntry, PageTableIndex};
use x86_64::structures::paging::{PageTable, PageTableFlags, PhysFrame};
use x86_64::{PhysAddr, VirtAddr};

/// Типизированные ошибки менеджера виртуальной памяти.
///
/// Заменяют прежние ad-hoc `&'static str`, чтобы вызывающие могли
/// сопоставлять вид отказа вместо сравнения строк.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmError {
    /// У PMM не нашлось свободного кадра под промежуточную таблицу страниц.
    OutOfMemory,
    /// Одна из записей на пути обхода таблиц страниц отсутствует.
    NotMapped,
    /// Запрошенный физический диапазон недостижим (неправдоподобная база или
    /// виртуальный адрес, до которого окно HHDM не достаёт).
    InvalidAddress,
    /// Листовой PTE разделён по COW, а операция перезаписала бы его или дала
    /// аппаратную запись в разделяемый кадр. Отцеплять его нужно через
    /// [`release_user_leaf`]/[`cow_copy_page`], которые сохраняют счётчик ссылок
    /// PMM корректным.
    CowShared,
}

impl core::fmt::Display for VmError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            VmError::OutOfMemory => "out of memory",
            VmError::NotMapped => "not mapped",
            VmError::InvalidAddress => "invalid address",
            VmError::CowShared => "COW-shared leaf",
        };
        f.write_str(s)
    }
}

/// Физический адрес PML4 ядра/загрузки, снятый в `init` (до появления любого
/// пользовательского адресного пространства, когда CR3 ещё содержит загрузочную
/// таблицу). Очистка при выходе использует его, чтобы отличить общий PML4 ядра —
/// который сносить НЕЛЬЗЯ НИКОГДА — от per-process пользовательских PML4,
/// созданных загрузчиком и `fork`.
static KERNEL_PML4: AtomicU64 = AtomicU64::new(0);

/// Инициализировать VMM. Сохраняет HHDM-смещение Limine.
pub fn init(hhdm_offset: u64) {
    crate::HHDM_OFFSET.store(hhdm_offset, Ordering::Relaxed);
    KERNEL_PML4.store(current_pml4_phys(), Ordering::Relaxed);

    crate::debug!(
        "VMM Initialized: HHDM offset=0x{:x}, PML4=0x{:x}",
        hhdm_offset,
        current_pml4_phys(),
    );
}

/// Физический адрес общего PML4 ядра (снят при загрузке).
pub fn kernel_pml4_phys() -> u64 {
    KERNEL_PML4.load(Ordering::Relaxed)
}

/// Перевести физический адрес в виртуальный через HHDM.
pub fn phys_to_virt(phys: u64) -> u64 {
    phys + crate::HHDM_OFFSET.load(Ordering::Relaxed)
}

/// Получить физический адрес *активного* PML4, прочитав CR3.
///
/// Читается живой CR3, а не кэш из static, поэтому обходы таблиц
/// (`map`/`unmap`/`virt_to_phys`) всегда целятся в реально установленное
/// адресное пространство. Это важно для пользовательских процессов: пока
/// текущая ring-3 задача, CR3 содержит её пользовательский PML4, и
/// отображение/трансляция должны работать с ним (а не с PML4 ядра). При
/// загрузке — и всегда, когда текущий поток ядерный — CR3 содержит PML4 ядра,
/// так что результат совпадает с прежним поведением кэша-static.
pub fn current_pml4_phys() -> u64 {
    // SAFETY: чтение CR3 — привилегированное чтение без побочных эффектов; валидно всегда.
    let (cr3_frame, _): (PhysFrame, _) = x86_64::registers::control::Cr3::read();
    cr3_frame.start_address().as_u64()
}

/// Перезагрузить CR3 таблицей страниц по физическому адресу `phys`.
///
/// Единственное место в ядре, которое пишет CR3 при переключении контекста
/// (Requirement 11.5). И путь вытесняющего тика (`scheduler_tick_irq`), и
/// кооперативный `yield_current` идут через него, поэтому точка перезагрузки
/// CR3 ровно одна. Запись безусловная: перезапись CR3 тем же или новым PML4
/// также сбрасывает неглобальный TLB, гарантируя перезагрузку отображений
/// стека и адресного пространства следующей задачи.
///
/// # Safety
/// `phys` обязан быть физической базой валидного, полностью инициализированного
/// PML4 с ядерными отображениями верхней половины. Загрузка испорченной таблицы
/// упадёт на следующем обращении к памяти.
#[inline]
pub unsafe fn load_cr3(phys: u64) {
    x86_64::registers::control::Cr3::write(
        PhysFrame::containing_address(PhysAddr::new(phys)),
        x86_64::registers::control::Cr3Flags::empty(),
    );
}

/// Безопасная абстракция над рекурсивным обходом таблиц страниц.
///
/// Весь сырой `unsafe`, нужный, чтобы превратить HHDM-отображённый физический
/// адрес в `&PageTable`/`&mut PageTable`, заперт в хелперах `table`/`table_mut`
/// этого типа. Вызывающие в `map`/`unmap`/`virt_to_phys` поэтому выглядят как
/// безопасный код.
struct PageTableWalker {
    hhdm: u64,
}

impl PageTableWalker {
    /// Создать обходчик, привязанный к активному HHDM-смещению.
    fn new() -> Self {
        Self {
            hhdm: crate::HHDM_OFFSET.load(Ordering::Relaxed),
        }
    }

    /// Позаимствовать таблицу страниц по физическому адресу `phys`.
    fn table(&self, phys: u64) -> &'static PageTable {
        // SAFETY: каждый кадр таблицы страниц отображён Limine в окно HHDM,
        // поэтому `phys + hhdm` — валидный, выровненный, читаемый указатель на
        // `PageTable` на всё время жизни адресного пространства ядра.
        unsafe { &*((phys + self.hhdm) as *const PageTable) }
    }

    /// Изменяемо позаимствовать таблицу страниц по физическому адресу `phys`.
    fn table_mut(&self, phys: u64) -> &'static mut PageTable {
        // SAFETY: тот же инвариант валидности HHDM, что у `table`. Каждый
        // уровень обхода указывает на отдельный кадр, поэтому `'static mut`,
        // выданные для последовательных уровней, никогда не алиасят одну память.
        unsafe { &mut *((phys + self.hhdm) as *mut PageTable) }
    }

    /// Активный PML4 (только чтение).
    fn root(&self) -> &'static PageTable {
        self.table(current_pml4_phys())
    }

    /// Активный PML4 (изменяемо).
    fn root_mut(&self) -> &'static mut PageTable {
        self.table_mut(current_pml4_phys())
    }

    /// Проследовать по присутствующей записи к таблице следующего уровня или
    /// `None`, если её нет.
    fn next_mut(&self, entry: &PageTableEntry) -> Option<&'static mut PageTable> {
        if !entry.flags().contains(PageTableFlags::PRESENT) {
            return None;
        }
        Some(self.table_mut(entry.addr().as_u64()))
    }

    /// Убедиться, что промежуточная таблица существует по `table[idx]`,
    /// выделив и обнулив свежий кадр из PMM при отсутствии записи, затем вернуть
    /// таблицу следующего уровня.
    ///
    /// # Политика флагов промежуточных записей
    ///
    /// Промежуточная запись PML4/PDPT/PD — *не* листовое отображение, она лишь
    /// указывает на таблицу следующего уровня, поэтому должна нести минимальные
    /// флаги, сохраняющие работоспособность всего поддерева, а не флаги листа:
    ///
    /// - Всегда `PRESENT | WRITABLE`. Записываемость промежуточной записи сама
    ///   по себе не делает лист записываемым (этим управляет листовой PTE), а её
    ///   принудительная установка сохраняет работу будущих записываемых листьев
    ///   под тем же узлом.
    /// - `USER_ACCESSIBLE` распространяется *только если* его запросил лист
    ///   (`leaf_flags & USER_ACCESSIBLE`). Это выполняет Property 4: каждая
    ///   промежуточная запись на пути к `USER_ACCESSIBLE`-странице тоже должна
    ///   нести `USER_ACCESSIBLE`, иначе CPU откажет ring 3 в доступе к листу.
    /// - Листовые флаги (`NO_EXECUTE`, `NO_CACHE`, `HUGE_PAGE`, `GLOBAL`) и
    ///   физический адрес листа намеренно *не* распространяются. NX на записи
    ///   верхнего уровня отключает исполнение для всего поддерева, а `NO_CACHE`
    ///   на промежуточной сделал бы соседние отображения некешируемыми — оба
    ///   отравили бы посторонние отображения.
    ///
    /// Если промежуточная запись уже есть, она *повышается* до
    /// `USER_ACCESSIBLE`, когда это нужно новому листу, а существующая запись
    /// (например, созданная сначала под ядерное отображение) его не имеет;
    /// адрес и прочие флаги записи сохраняются.
    fn ensure_next(
        &self,
        table: &mut PageTable,
        idx: PageTableIndex,
        flags: PageTableFlags,
        level: EntryLevel,
    ) -> Result<&'static mut PageTable, VmError> {
        // Флаги, которые должна нести промежуточная запись: present + writable
        // плюс доступность пользователю, только если её запросил лист.
        let user = flags & PageTableFlags::USER_ACCESSIBLE;
        let intermediate_flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | user;

        // HUGE PAGE здесь — не таблица страниц.
        //
        // Limine отображает HHDM страницами 2 МиБ (а на некоторых машинах
        // 1 ГиБ), поэтому обход, дошедший до этого уровня, встречает
        // присутствующую запись с битом HUGE_PAGE, чей «адрес кадра» — начало
        // большого региона RAM, а не таблица из 512 записей. Отношение к ней как
        // к таблице (прежнее поведение) заводило обход в произвольные байты и
        // давало неканонический указатель — GENERAL PROTECTION fault, а не page
        // fault. Именно это убивало `map_mmio` на реальном железе, тогда как
        // QEMU, чей HHDM в этом месте не покрыт огромной страницей, грузился
        // нормально.
        //
        // Уровень решает, чем заменить: страница 2 МиБ становится полной
        // таблицей записей 4 КиБ, страница 1 ГиБ — таблицей огромных записей
        // 2 МиБ (следующий уровень при необходимости расщепит и их).
        if table[idx].flags().contains(PageTableFlags::HUGE_PAGE) {
            self.split_huge(table, idx, level)?;
        }

        if !table[idx].flags().contains(PageTableFlags::PRESENT) {
            let frame = crate::memory::pmm::alloc_frame().ok_or(VmError::OutOfMemory)?;
            // SAFETY: `frame` только что выделен и отображён через HHDM, поэтому
            // эта запись обнуляет ровно один принадлежащий нам выровненный кадр.
            unsafe {
                ptr::write_bytes((frame + self.hhdm) as *mut u8, 0, 4096);
            }
            table[idx].set_addr(PhysAddr::new(frame), intermediate_flags);
        } else if user.contains(PageTableFlags::USER_ACCESSIBLE)
            && !table[idx].flags().contains(PageTableFlags::USER_ACCESSIBLE)
        {
            // Случай повышения (Property 4): промежуточная запись, созданная
            // сначала под ядерное отображение, должна получить USER_ACCESSIBLE,
            // чтобы более позднее пользовательское отображение под ней было
            // реально достижимо из ring 3. Сохранить существующий адрес и все
            // прочие уже установленные флаги.
            let addr = table[idx].addr();
            let upgraded = table[idx].flags() | PageTableFlags::USER_ACCESSIBLE;
            table[idx].set_addr(addr, upgraded);
        }
        Ok(self.table_mut(table[idx].addr().as_u64()))
    }

    /// Заменить запись огромной страницы настоящей таблицей страниц,
    /// покрывающей ту же память.
    ///
    /// `level` — уровень расщепляемой записи: запись в PD (`level` `Pd`)
    /// описывает 2 МиБ и заменяется PT из отображений 4 КиБ; запись в PDPT
    /// (`Pdpt`) описывает 1 ГиБ и заменяется PD из огромных отображений 2 МиБ.
    /// Флаги самой огромной страницы копируются в каждый лист, поэтому
    /// read-only или non-executable регион HHDM сохраняет свои свойства — их
    /// потеря молча расширила бы права на память, которая ядру не принадлежит.
    fn split_huge(
        &self,
        table: &mut PageTable,
        idx: PageTableIndex,
        level: EntryLevel,
    ) -> Result<(), VmError> {
        let huge_flags = table[idx].flags();
        let base = table[idx].addr().as_u64();
        let leaf_flags = huge_flags & !PageTableFlags::HUGE_PAGE;

        let frame = crate::memory::pmm::alloc_frame().ok_or(VmError::OutOfMemory)?;
        // SAFETY: `frame` — свежевыделенная, отображённая через HHDM страница, принадлежащая нам.
        unsafe { ptr::write_bytes((frame + self.hhdm) as *mut u8, 0, 4096) };

        // SAFETY: `frame` — наша HHDM-отображённая страница; записываемые в неё
        // записи точно воспроизводят регион, который описывала огромная
        // запись, поэтому ничто отображённое не перестаёт быть отображённым.
        unsafe {
            let new_table = &mut *((frame + self.hhdm) as *mut PageTable);
            match level {
                // Запись 1 ГиБ -> 512 записей 2 МиБ (всё ещё огромные, расщепятся позже).
                EntryLevel::Pdpt => {
                    let sub = leaf_flags | PageTableFlags::HUGE_PAGE;
                    for i in 0..512usize {
                        new_table[i].set_addr(PhysAddr::new(base + (i as u64) * 0x20_0000), sub);
                    }
                }
                // Запись 2 МиБ -> 512 записей 4 КиБ.
                EntryLevel::Pd => {
                    for i in 0..512usize {
                        new_table[i]
                            .set_addr(PhysAddr::new(base + (i as u64) * 0x1000), leaf_flags);
                    }
                }
                // Огромная страница не может законно появиться на этих уровнях;
                // оставить запись как есть безопаснее, чем расщеплять неизвестное.
                EntryLevel::Pml4 | EntryLevel::Pt => {
                    crate::warn!("[VMM] unexpected huge page at {:?}; not splitting", level);
                    return Ok(());
                }
            }
        }

        table[idx].set_addr(PhysAddr::new(frame), leaf_flags);
        Ok(())
    }
}

/// Уровень иерархии таблиц страниц, к которому принадлежит запись.
///
/// Нужен потому, что смысл `HUGE_PAGE` зависит от него: бит зарезервирован в
/// записи PML4 и PT, означает 1 ГиБ в записи PDPT и 2 МиБ в записи PD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryLevel {
    Pml4,
    Pdpt,
    Pd,
    Pt,
}

/// Пройти таблицы страниц и перевести виртуальный адрес в физический.
pub fn virt_to_phys(virt: u64) -> Option<u64> {
    let virt_addr = VirtAddr::new(virt);
    let p4_idx = virt_addr.p4_index();
    let p3_idx = virt_addr.p3_index();
    let p2_idx = virt_addr.p2_index();
    let p1_idx = virt_addr.p1_index();

    let walker = PageTableWalker::new();

    let pml4 = walker.root();
    let pml4_entry = &pml4[p4_idx];
    if !pml4_entry.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }

    let pdpt = walker.table(pml4_entry.addr().as_u64());
    let pdpt_entry = &pdpt[p3_idx];
    if !pdpt_entry.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }

    // Проверить огромную страницу 1 ГиБ
    if pdpt_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        let offset = virt_addr.as_u64() & 0x3FFF_FFFF; // 1 ГиБ - 1
        return Some(pdpt_entry.addr().as_u64() + offset);
    }

    let pd = walker.table(pdpt_entry.addr().as_u64());
    let pd_entry = &pd[p2_idx];
    if !pd_entry.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }

    // Проверить огромную страницу 2 МиБ
    if pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        let offset = virt_addr.as_u64() & 0x1F_FFFF; // 2 МиБ - 1
        return Some(pd_entry.addr().as_u64() + offset);
    }

    let pt = walker.table(pd_entry.addr().as_u64());
    let pt_entry = &pt[p1_idx];
    if !pt_entry.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }

    let offset = virt_addr.as_u64() & 0xFFF; // 4 КиБ - 1
    Some(pt_entry.addr().as_u64() + offset)
}

/// Вернуть эффективные флаги листа для отображённого виртуального адреса.
/// Каждый уровень должен присутствовать; доступность пользователю пересекается
/// по всему обходу, чтобы ядерный предок не был принят за пользовательскую
/// страницу.
pub fn page_flags(virt: u64) -> Option<PageTableFlags> {
    let va = VirtAddr::new(virt);
    let walker = PageTableWalker::new();
    let p4e = &walker.root()[va.p4_index()];
    if !p4e.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    let p3 = walker.table(p4e.addr().as_u64());
    let p3e = &p3[va.p3_index()];
    if !p3e.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    let mut effective = p4e.flags() & p3e.flags();
    if p3e.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Some(effective);
    }
    let p2 = walker.table(p3e.addr().as_u64());
    let p2e = &p2[va.p2_index()];
    if !p2e.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    effective &= p2e.flags();
    if p2e.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Some(effective);
    }
    let p1 = walker.table(p2e.addr().as_u64());
    let p1e = &p1[va.p1_index()];
    if !p1e.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    Some(effective & p1e.flags())
}

/// Отобразить физический кадр в виртуальную страницу.
///
/// При необходимости выделяет промежуточные таблицы через PMM.
/// Исключающая верхняя граница пользовательского диапазона нижней половины.
///
/// Зеркалит `linux::validate::USER_ADDR_MAX` (тот модуль только `core` и
/// host-тестируемый, а этот — ядерный). Держа значение здесь, [`map`] может
/// обеспечить инвариант «ядерный виртуальный адрес никогда не отображается
/// `USER_ACCESSIBLE`» в единственном месте, через которое проходят все
/// отображения, вместо доверия флагам каждого вызывающего.
const USER_ADDR_MAX: u64 = 0x0000_8000_0000_0000;

pub fn map(phys_addr: u64, virt_addr: u64, flags: PageTableFlags) -> Result<(), VmError> {
    // `USER_ACCESSIBLE`-лист в ядерной половине — всегда ошибка: ring 3 смог бы
    // читать/писать эту ядерную страницу (так и произошло, когда построитель
    // флагов пользовательской страницы прогнали по ядерному VA; см.
    // `SECURITY.md`). Отказываем в узком месте; `mem_sys` тоже валидирует свои
    // диапазоны, так что это эшелонированная копия того же правила.
    if virt_addr >= USER_ADDR_MAX && flags.contains(PageTableFlags::USER_ACCESSIBLE) {
        crate::error!(
            "[VMM] refusing USER_ACCESSIBLE mapping of kernel VA {:#x} (phys {:#x})",
            virt_addr,
            phys_addr
        );
        return Err(VmError::InvalidAddress);
    }
    let phys = PhysAddr::new(phys_addr);
    let virt = VirtAddr::new(virt_addr);

    let walker = PageTableWalker::new();

    // Обход (с выделением промежуточных таблиц) вниз до PT. Все сырые
    // разыменования таблиц страниц заперты в обходчике, поэтому код выглядит
    // безопасным.
    let pml4 = walker.root_mut();
    let pdpt = walker.ensure_next(pml4, virt.p4_index(), flags, EntryLevel::Pml4)?;
    let pd = walker.ensure_next(pdpt, virt.p3_index(), flags, EntryLevel::Pdpt)?;
    let pt = walker.ensure_next(pd, virt.p2_index(), flags, EntryLevel::Pd)?;

    // Никогда не перезаписывать живую трансляцию молча.
    //
    // Присутствующий COW-лист ОТВЕРГАЕТСЯ: его кадр подсчитан, и другое
    // адресное пространство может всё ещё отображать его, поэтому перезапись PTE
    // либо утечёт кадр, либо (хуже) установит записываемый алиас памяти, которой
    // эта сторона уже не владеет. Заменяющие отображение вызывающие обязаны
    // сначала освободить его (`release_user_leaf`) или скопировать приватно
    // (`cow_copy_page`).
    //
    // Присутствующий не-COW PTE, указывающий на *другой* кадр, означает, что два
    // владельца считают эту виртуальную страницу своей: старый кадр утекает, а
    // тот, кто ещё использует старое отображение, в одной переаллокации от
    // чтения чужой памяти. Сохраняем историческую перезапись, но логируем.
    {
        let entry = &pt[virt.p1_index()];
        if entry.flags().contains(PageTableFlags::PRESENT) {
            let old_flags = entry.flags();
            let old_phys = entry.addr().as_u64();
            if old_flags.contains(COW_BIT) {
                // COW-лист разделён хотя бы с одним другим адресным
                // пространством, и его кадр подсчитан в PMM. Перезапись здесь
                // либо утекла бы старый кадр, пока другая сторона продолжает его
                // отображать, либо дала бы запись в ещё разделяемую память —
                // use-after-free. Отцеплять через `release_user_leaf` или
                // сначала скопировать приватно (`cow_copy_page`).
                if old_phys != phys.as_u64()
                    || !flags.contains(COW_BIT)
                    || flags.contains(PageTableFlags::WRITABLE)
                {
                    crate::error!(
                        "[VMM] refusing to overwrite COW leaf virt=0x{:016x} old_phys=0x{:x} new_phys=0x{:x} flags={:?}",
                        virt_addr, old_phys, phys.as_u64(), flags
                    );
                    return Err(VmError::CowShared);
                }
            } else if old_flags.contains(SHARED_BIT)
                || old_flags.contains(VT_RO_BIT)
                || old_flags.contains(MMIO_BIT)
            {
                // MAP_SHARED/MMIO-лист нельзя молча перезаписать другой
                // трансляцией: у первого есть посчитанная ссылка владения, у
                // второго кадр вообще не из PMM. Заменяющие пути обязаны сперва
                // снять лист (`release_user_leaf`).
                crate::error!(
                    "[VMM] refusing to overwrite SHARED/MMIO leaf virt=0x{:016x} old_phys=0x{:x} new_phys=0x{:x}",
                    virt_addr, old_phys, phys.as_u64()
                );
                return Err(VmError::CowShared);
            } else if old_phys != phys.as_u64() {
                crate::warn!(
                    "[VMM] remap virt=0x{:016x}: old_phys=0x{:x} -> new_phys=0x{:x} (old frame leaked/aliased)",
                    virt_addr, old_phys, phys.as_u64()
                );
            }
        }
    }
    // Установить PTE (Page Table Entry)
    pt[virt.p1_index()].set_addr(phys, flags | PageTableFlags::PRESENT);

    // Сбросить TLB для этого виртуального адреса
    tlb::flush(virt);

    Ok(())
}

/// Напечатать сырой 4-уровневый обход таблиц страниц для `virt_addr` в
/// *текущем активном* адресном пространстве, по строке на уровень, до первой
/// отсутствующей записи. В постмортеме это отличает «исчез один PTE» от
/// «исчезла целая промежуточная таблица» (например, её кадр был дважды выделен и
/// обнулён другим владельцем) — две картины отказа указывают на разных
/// виновников.
/// Вернуть копию листового PTE для `virt_addr` в ТЕКУЩЕМ адресном пространстве,
/// если весь обход присутствует. Используется обработчиком page fault для
/// диагностики copy-on-write.
pub fn walk_pte(virt_addr: u64) -> Option<PageTableEntry> {
    const SHIFTS: [u64; 3] = [39, 30, 21];
    let mut table_phys = current_pml4_phys();
    for shift in SHIFTS {
        let idx = ((virt_addr >> shift) & 0x1ff) as usize;
        // SAFETY: кадры таблиц страниц всегда читаемы через HHDM.
        let entry =
            unsafe { core::ptr::read_volatile((phys_to_virt(table_phys) as *const u64).add(idx)) };
        if entry & 1 == 0 {
            return None;
        }
        table_phys = entry & 0x000f_ffff_ffff_f000;
    }
    let idx = (virt_addr >> 12) & 0x1ff;
    // SAFETY: как выше.
    let pte = unsafe {
        core::ptr::read_volatile(
            (phys_to_virt(table_phys) as *const PageTableEntry).add(idx as usize),
        )
    };
    if !pte.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    Some(pte)
}

pub fn dump_translation(virt_addr: u64) {
    const NAMES: [&str; 4] = ["PML4E", "PDPTE", "PDE", "PTE"];
    const SHIFTS: [u64; 4] = [39, 30, 21, 12];
    let mut table_phys = current_pml4_phys();
    for level in 0..4 {
        let idx = ((virt_addr >> SHIFTS[level]) & 0x1ff) as usize;
        // SAFETY: кадры таблиц страниц всегда читаемы через HHDM.
        let entry =
            unsafe { core::ptr::read_volatile((phys_to_virt(table_phys) as *const u64).add(idx)) };
        if entry & 1 == 0 {
            crate::error!(
                "[VMM] walk 0x{:012x}: {}[{}] = 0x{:016x} NOT PRESENT (table@phys=0x{:x})",
                virt_addr,
                NAMES[level],
                idx,
                entry,
                table_phys
            );
            return;
        }
        crate::error!(
            "[VMM] walk 0x{:012x}: {}[{}] = 0x{:016x} (table@phys=0x{:x})",
            virt_addr,
            NAMES[level],
            idx,
            entry,
            table_phys
        );
        if level > 0 && level < 3 && entry & (1 << 7) != 0 {
            return; // огромная страница — обход законно заканчивается здесь
        }
        table_phys = entry & 0x000f_ffff_ffff_f000;
    }
}

/// Снять отображение виртуальной страницы.
pub fn unmap(virt_addr: u64) -> Result<(), VmError> {
    let virt = VirtAddr::new(virt_addr);

    let walker = PageTableWalker::new();

    let pml4 = walker.root_mut();
    let pdpt = walker
        .next_mut(&pml4[virt.p4_index()])
        .ok_or(VmError::NotMapped)?;
    let pd = walker
        .next_mut(&pdpt[virt.p3_index()])
        .ok_or(VmError::NotMapped)?;
    let pt = walker
        .next_mut(&pd[virt.p2_index()])
        .ok_or(VmError::NotMapped)?;

    // Очистить PTE
    pt[virt.p1_index()].set_unused();
    tlb::flush(virt);

    Ok(())
}

/// Создать новый PML4 для пользовательского процесса.
/// Копирует ядерные отображения верхней половины из текущего PML4.
pub fn new_user_pml4() -> Result<u64, VmError> {
    let walker = PageTableWalker::new();

    let new_pml4_frame = crate::memory::pmm::alloc_frame().ok_or(VmError::OutOfMemory)?;

    // Обнулить новый PML4, затем скопировать ядерные записи верхней половины (256..512).
    let new_pml4 = walker.table_mut(new_pml4_frame);
    new_pml4.zero();

    let current_pml4 = walker.root();
    for i in 256usize..512 {
        new_pml4[i] = current_pml4[i].clone();
    }

    crate::debug!("Created new user PML4 at phys=0x{:x}", new_pml4_frame);

    Ok(new_pml4_frame)
}

/// Отобразить регион физического MMIO в адресное пространство ядра как
/// некешируемый и вернуть его виртуальную базу.
///
/// `len` байт от `phys` отображаются постранично (с округлением до целых страниц
/// 4 КиБ) с `PRESENT | WRITABLE | NO_CACHE | NO_EXECUTE`. MMIO достигается через
/// окно HHDM (`virt = phys_to_virt(phys)`, то же соглашение, что у
/// `crate::memory::layout::mmio_virt_for`), поэтому возвращённая база совпадает
/// с тем, как сейчас отображено MMIO LAPIC/IOAPIC.
///
/// MMIO — память устройства: отображается `NO_CACHE`, чтобы чтения/записи
/// доходили до устройства, `NO_EXECUTE`, так как это никогда не код, и *никогда*
/// не `USER_ACCESSIBLE` — эти регионы принадлежат только ядру.
pub fn map_mmio(phys: u64, len: u64) -> Result<u64, VmError> {
    let page_size = 4096u64;

    // Выровнять базу вниз, конец вверх, чтобы весь запрошенный регион был
    // покрыт, даже если `phys`/`len` не выровнены по странице.
    let start = phys & !(page_size - 1);
    let end = (phys + len + (page_size - 1)) & !(page_size - 1);

    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::NO_CACHE
        | PageTableFlags::NO_EXECUTE;

    // Отвергнуть адрес, который нельзя отобразить, до обхода таблиц страниц.
    //
    // ПОЧЕМУ: на реальной машине `apic::init` брал адрес LAPIC из MADT и звал
    // это, и загрузка умирала с GENERAL PROTECTION fault внутри `map` — не с
    // page fault. #GP там означает, что вычисленный виртуальный адрес
    // неканонический, а это возможно только при мусорной физической базе:
    // ошибочное значение BAR/MADT вроде всех единиц даёт переполнение
    // `phys + hhdm` в неканоническую дыру. Сообщить значение лучше, чем упасть
    // внутри обходчика, где адрес уже потерян.
    const MAX_PHYS: u64 = 0x0000_FFFF_FFFF_FFFF; // 52-битное физическое адресное пространство
    if phys > MAX_PHYS || start > MAX_PHYS {
        crate::error!(
            "[VMM] map_mmio: refusing implausible physical base 0x{:x}",
            phys
        );
        crate::drivers::earlyfb::errorln(format_args!(
            "map_mmio: refusing physical base {:#x} (implausible)",
            phys
        ));
        return Err(VmError::InvalidAddress);
    }

    let mut p = start;
    while p < end {
        // `map` возвращает Err, а не падает, по той же причине: плохой
        // физический диапазон должен всплыть как ошибка загрузки, а не #GP.
        map(p, phys_to_virt(p), flags)?;
        p += page_size;
    }

    Ok(phys_to_virt(phys))
}

// ─── поддержка fork ──────────────────────────────────────────────────

/// Глубоко скопировать пользовательскую нижнюю половину (записи PML4 0..256)
/// из `src_pml4` в `dst_pml4`, жадно дублируя каждый отображённый кадр 4 КиБ
/// (без copy-on-write). Весь доступ идёт через окно HHDM, поэтому ни одно
/// адресное пространство не обязано быть активным в CR3 и TLB-shootdown не
/// нужен (приёмник никогда не загружался). Флаги листьев сохраняются;
/// промежуточные таблицы перестраиваются по стандартной пользовательской
/// политике. Огромные страницы в пользовательской нижней половине здесь не
/// появляются (загрузчик и mmap отображают только 4 КиБ), поэтому огромный лист
/// сообщается как `NotMapped`, а не молча делится между двумя адресными
/// пространствами.
pub fn clone_user_space(src_pml4: u64, dst_pml4: u64) -> Result<(), VmError> {
    let walker = PageTableWalker::new();
    let user = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
    let src4 = walker.table(src_pml4);
    for i4 in 0..256usize {
        let e4 = &src4[i4];
        if !e4.flags().contains(PageTableFlags::PRESENT) {
            continue;
        }
        let src3 = walker.table(e4.addr().as_u64());
        for i3 in 0..512usize {
            let e3 = &src3[i3];
            if !e3.flags().contains(PageTableFlags::PRESENT) {
                continue;
            }
            if e3.flags().contains(PageTableFlags::HUGE_PAGE) {
                return Err(VmError::NotMapped);
            }
            let src2 = walker.table(e3.addr().as_u64());
            for i2 in 0..512usize {
                let e2 = &src2[i2];
                if !e2.flags().contains(PageTableFlags::PRESENT) {
                    continue;
                }
                if e2.flags().contains(PageTableFlags::HUGE_PAGE) {
                    return Err(VmError::NotMapped);
                }
                let src1 = walker.table(e2.addr().as_u64());
                // Лениво строить цепочку приёмника — только когда у этого P1
                // реально есть хотя бы один присутствующий лист.
                let mut dst1: Option<&mut PageTable> = None;
                for i1 in 0..512usize {
                    let e1 = &src1[i1];
                    if !e1.flags().contains(PageTableFlags::PRESENT) {
                        continue;
                    }
                    if dst1.is_none() {
                        let p4 = walker.table_mut(dst_pml4);
                        // Уровни названы по ЗАПИСИ, через которую идёт обход:
                        // именно она решает смысл HUGE_PAGE на этом уровне.
                        let p3 = walker.ensure_next(
                            p4,
                            x86_64::structures::paging::PageTableIndex::new(i4 as u16),
                            user,
                            EntryLevel::Pml4,
                        )?;
                        let p2 = walker.ensure_next(
                            p3,
                            x86_64::structures::paging::PageTableIndex::new(i3 as u16),
                            user,
                            EntryLevel::Pdpt,
                        )?;
                        let p1 = walker.ensure_next(
                            p2,
                            x86_64::structures::paging::PageTableIndex::new(i2 as u16),
                            user,
                            EntryLevel::Pd,
                        )?;
                        dst1 = Some(p1);
                    }
                    if e1.flags().contains(MMIO_BIT) {
                        // Framebuffer: новое адресное пространство получает ту
                        // же трансляцию на кадр устройства, без аллокации.
                        if let Some(pt) = dst1.as_deref_mut() {
                            pt[i1].set_addr(e1.addr(), e1.flags());
                        }
                        continue;
                    }
                    if e1.flags().contains(SHARED_BIT) || e1.flags().contains(VT_RO_BIT) {
                        // MAP_SHARED/VT-поверхность: копия адресного
                        // пространства разделяет кадр, а не дублирует его;
                        // ссылка учитывается так же, как в fork-COW.
                        crate::memory::pmm::cow_ref(e1.addr().as_u64());
                        if let Some(pt) = dst1.as_deref_mut() {
                            pt[i1].set_addr(e1.addr(), e1.flags());
                        }
                        continue;
                    }
                    let frame = crate::memory::pmm::alloc_frame().ok_or(VmError::OutOfMemory)?;
                    // SAFETY: оба кадра — обычная RAM, достижимая через окно
                    // HHDM; копируем целую страницу. MMIO/SHARED-листья сюда не
                    // доходят (обработаны выше): они алиасятся, а не копируются.
                    unsafe {
                        ptr::copy_nonoverlapping(
                            phys_to_virt(e1.addr().as_u64()) as *const u8,
                            phys_to_virt(frame) as *mut u8,
                            4096,
                        );
                    }
                    if let Some(pt) = dst1.as_deref_mut() {
                        pt[i1].set_addr(x86_64::PhysAddr::new(frame), e1.flags());
                    }
                }
            }
        }
    }
    Ok(())
}

/// Перевести пользовательский виртуальный адрес в (неактивном) адресном
/// пространстве с корнем `pml4` — используется для записи CLONE_CHILD_SETTID в
/// скопированную память ребёнка. Только обход 4 КиБ; None на любой
/// отсутствующей записи.
pub fn virt_to_phys_in(pml4: u64, virt: u64) -> Option<u64> {
    let walker = PageTableWalker::new();
    let idx4 = ((virt >> 39) & 0x1ff) as usize;
    let idx3 = ((virt >> 30) & 0x1ff) as usize;
    let idx2 = ((virt >> 21) & 0x1ff) as usize;
    let idx1 = ((virt >> 12) & 0x1ff) as usize;
    let e4 = &walker.table(pml4)[idx4];
    if !e4.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    let e3 = &walker.table(e4.addr().as_u64())[idx3];
    if !e3.flags().contains(PageTableFlags::PRESENT)
        || e3.flags().contains(PageTableFlags::HUGE_PAGE)
    {
        return None;
    }
    let e2 = &walker.table(e3.addr().as_u64())[idx2];
    if !e2.flags().contains(PageTableFlags::PRESENT)
        || e2.flags().contains(PageTableFlags::HUGE_PAGE)
    {
        return None;
    }
    let e1 = &walker.table(e2.addr().as_u64())[idx1];
    if !e1.flags().contains(PageTableFlags::PRESENT) {
        return None;
    }
    Some(e1.addr().as_u64() + (virt & 0xfff))
}

/// Программный бит PTE (свободный бит 9), помечающий COW-разделённый лист:
/// кадр разделён с другим адресным пространством, и его владение подсчитано в
/// PMM (`pmm::cow_ref`/`cow_unref`). Ставится на ОБЕИХ сторонах
/// [`fork_user_space_cow`]; снимается, когда сторона копирует страницу приватно
/// ([`cow_copy_page`]) или выходит (`drop_user_space` делает unref вместо
/// освобождения).
pub(crate) const COW_BIT: PageTableFlags = PageTableFlags::BIT_9;

/// Программный бит PTE (свободный бит 10), означающий «ОТОБРАЖЕНИЕ этого листа
/// запрещает запись»: аппаратный бит `WRITABLE` снят потому, что защита —
/// read-only (или execute-only), а не потому, что кадр COW-разделён.
///
/// Бит нужен, потому что иначе эти две причины на COW-листе неразличимы — обе
/// оставляют присутствующий read-only PTE, — а обработчик write-fault
/// ([`cow_copy_page`]) должен копировать страницу приватно, только когда
/// отображение реально разрешает запись. Без этого бита `mprotect(PROT_READ)` на
/// fork-разделённой странице был совещательным: следующий write fault копировал
/// страницу и ставил `WRITABLE` на приватную копию.
///
/// Поддерживается только на COW-листах (приватному read-only листу маркер не
/// нужен: [`cow_copy_page`] отвергает его из-за отсутствия `COW_BIT`):
///   * [`fork_user_space_cow`] ставит его на листьях, чьё отображение было
///     read-only, и сохраняет на уже-COW листьях (отложенный COW-запись бита не
///     имеет);
///   * `mprotect` ставит/снимает его вместе с отслеживаемой защитой;
///   * [`cow_copy_page`] снимает его, когда делает страницу приватно
///     записываемой.
pub(crate) const RO_BIT: PageTableFlags = PageTableFlags::BIT_10;

/// Программный бит PTE (свободный бит 11), помечающий лист `MAP_SHARED`:
/// кадр принадлежит файлу (memfd) и подсчитан в PMM как общая ссылка. Такой
/// лист НЕ становится COW при `fork` — обе стороны должны видеть одни байты, —
/// а освобождается через `cow_unref`, как COW-лист. Ссылку отображения ставит
/// `mem_sys` при `mmap`, кэш-ссылку — сам memfd-узел.
pub(crate) const SHARED_BIT: PageTableFlags = PageTableFlags::BIT_11;

/// Программный бит PTE (свободный бит 52), помечающий MMIO-лист: физический
/// кадр устройства (framebuffer), который НИКОГДА не принадлежит PMM. `munmap`,
/// exit и `fork` обязаны не освобождать и не копировать его — только снять/
/// продублировать трансляцию. Бит 52 взят из диапазона 52..=58: младшие
/// программные биты уже заняты `COW_BIT`/`RO_BIT`/`SHARED_BIT`.
pub(crate) const MMIO_BIT: PageTableFlags = PageTableFlags::BIT_52;

// Маркер MMIO обязан совпадать с чистым `pat::PTE_MMIO`: иначе host-тест
// композиции fb0-флагов доказывал бы не тот бит, а `release_user_leaf`
// проверял бы не тот маркер.
const _: () = assert!(MMIO_BIT.bits() == crate::memory::pat::PTE_MMIO);

/// Программный бит PTE (свободный бит 53), помечающий лист read-only
/// shared-поверхности ядра (`/dev/vtconsole0`): кадр запинен поверхностью,
/// отображение пользователя не должно становиться записываемым через
/// `mprotect`, а `munmap`/exit лишь отпускают ссылку (`cow_unref`).
pub(crate) const VT_RO_BIT: PageTableFlags = PageTableFlags::BIT_53;

/// Результат отцепления одного пользовательского листа через [`release_user_leaf`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeafRelease {
    /// Кадр возвращён в PMM: приватный или последний COW-шарящий.
    Freed,
    /// Лист был COW-разделён, и другое адресное пространство всё ещё отображает
    /// его; отпущена только ссылка PMM этой стороны. PTE убран, кадр остаётся.
    Shared,
}

/// Освободить один листовой кадр сообразно его COW-маркеру владения.
///
/// ЕДИНСТВЕННОЕ место, где листовой кадр пользователя возвращается в PMM (или
/// остаётся у других шарящих): `munmap`, `mprotect(PROT_NONE)`, замена
/// `MAP_FIXED`, `mremap` и снос адресного пространства сводят освобождение
/// листьев сюда. Лист с `COW_BIT` потребляет одну ссылку `pmm::cow_unref` и
/// освобождается, только когда вызов сообщил о последней; обычный приватный
/// лист освобождается напрямую. Ни один вызывающий не должен звать
/// `pmm::free_frame` на отцеплённом им листе пользователя.
fn release_leaf_frame(frame: u64, flags: PageTableFlags) -> bool {
    if flags.contains(MMIO_BIT) {
        // Физический кадр устройства: PMM им не владеет, возвращать нечего.
        false
    } else if flags.contains(VT_RO_BIT) || flags.contains(SHARED_BIT) {
        // MAP_SHARED: ушла наша ссылка отображения; кадр живёт, пока его держит
        // файл-кэш или другое адресное пространство.
        if crate::memory::pmm::cow_unref(frame) {
            crate::memory::pmm::free_frame(frame);
            true
        } else {
            false
        }
    } else if flags.contains(COW_BIT) {
        if crate::memory::pmm::cow_unref(frame) {
            crate::memory::pmm::free_frame(frame);
            true
        } else {
            false
        }
    } else {
        crate::memory::pmm::free_frame(frame);
        true
    }
}

/// Отцепить лист 4 КиБ по `virt_addr` от АКТИВНОГО пользовательского
/// адресного пространства и освободить его кадр ровно один раз сообразно
/// COW-владению.
///
/// Это половина освобождения в инварианте владения: PTE очищается до того, как
/// кадр (возможно) вернётся в PMM, поэтому ни одна живая трансляция не указывает
/// на свободный кадр, а COW-разделённый кадр не освобождается, пока другое
/// адресное пространство его отображает.
///
/// Возвращает [`VmError::NotMapped`], если листа 4 КиБ нет (огромные страницы в
/// пользовательской половине — ошибка ядра, и они отвергаются, а не
/// обходятся неверно), и [`VmError::InvalidAddress`] для не-пользовательского
/// адреса — хелпер существует только для листьев пользователя.
pub fn release_user_leaf(virt_addr: u64) -> Result<LeafRelease, VmError> {
    if virt_addr >= USER_ADDR_MAX {
        return Err(VmError::InvalidAddress);
    }
    let walker = PageTableWalker::new();
    let virt = VirtAddr::new(virt_addr);
    let pml4 = walker.root_mut();
    let p4e = &pml4[virt.p4_index()];
    if !p4e.flags().contains(PageTableFlags::PRESENT)
        || p4e.flags().contains(PageTableFlags::HUGE_PAGE)
    {
        return Err(VmError::NotMapped);
    }
    let Some(pdpt) = walker.next_mut(p4e) else {
        return Err(VmError::NotMapped);
    };
    let p3e = &pdpt[virt.p3_index()];
    if !p3e.flags().contains(PageTableFlags::PRESENT)
        || p3e.flags().contains(PageTableFlags::HUGE_PAGE)
    {
        return Err(VmError::NotMapped);
    }
    let Some(pd) = walker.next_mut(p3e) else {
        return Err(VmError::NotMapped);
    };
    let p2e = &pd[virt.p2_index()];
    if !p2e.flags().contains(PageTableFlags::PRESENT)
        || p2e.flags().contains(PageTableFlags::HUGE_PAGE)
    {
        return Err(VmError::NotMapped);
    }
    let Some(pt) = walker.next_mut(p2e) else {
        return Err(VmError::NotMapped);
    };
    let entry = &mut pt[virt.p1_index()];
    if !entry.flags().contains(PageTableFlags::PRESENT) {
        return Err(VmError::NotMapped);
    }
    let frame = entry.addr().as_u64();
    let flags = entry.flags();
    // Очистить трансляцию ДО того, как кадр сможет стать свободным: промах TLB
    // не должен воскресить устаревшую трансляцию на кадр, который PMM уже может
    // выдать.
    entry.set_unused();
    tlb::flush(virt);
    Ok(if release_leaf_frame(frame, flags) {
        LeafRelease::Freed
    } else {
        LeafRelease::Shared
    })
}

/// Переставить флаги существующего листа 4 КиБ АКТИВНОГО пользовательского
/// адресного пространства, НЕ меняя его кадр. Используется `mprotect(2)`: не
/// требует аллокации PMM и потому не может упасть с `ENOMEM` на середине
/// диапазона.
///
/// COW-лист сохраняет `COW_BIT` и никогда не становится аппаратно записываемым:
/// право записи выдаётся лениво следующим write fault ([`cow_copy_page`]) — как
/// Linux `can_change_pte_writable` оставляет write-protected PTE, пока страница
/// разделена. Снять маркер или поставить `WRITABLE` здесь значило бы молча
/// создать записываемый алиас кадра, которым всё ещё владеет другое адресное
/// пространство.
///
/// Защиту ОТОБРАЖЕНИЯ решает вызывающий: `mprotect` ставит [`RO_BIT`], когда
/// делает COW-лист read-only, и снимает его, когда даёт запись, чтобы обработчик
/// fault отличал «защищён read-only» от «COW-запись отложена».
pub fn set_user_leaf_flags(virt_addr: u64, flags: PageTableFlags) -> Result<(), VmError> {
    if virt_addr >= USER_ADDR_MAX {
        return Err(VmError::InvalidAddress);
    }
    if flags.contains(COW_BIT) && flags.contains(PageTableFlags::WRITABLE) {
        crate::error!(
            "[VMM] refusing WRITABLE|COW leaf flags for virt=0x{:016x}",
            virt_addr
        );
        return Err(VmError::CowShared);
    }
    let walker = PageTableWalker::new();
    let virt = VirtAddr::new(virt_addr);
    let pml4 = walker.root_mut();
    let p4e = &pml4[virt.p4_index()];
    if !p4e.flags().contains(PageTableFlags::PRESENT)
        || p4e.flags().contains(PageTableFlags::HUGE_PAGE)
    {
        return Err(VmError::NotMapped);
    }
    let Some(pdpt) = walker.next_mut(p4e) else {
        return Err(VmError::NotMapped);
    };
    let p3e = &pdpt[virt.p3_index()];
    if !p3e.flags().contains(PageTableFlags::PRESENT)
        || p3e.flags().contains(PageTableFlags::HUGE_PAGE)
    {
        return Err(VmError::NotMapped);
    }
    let Some(pd) = walker.next_mut(p3e) else {
        return Err(VmError::NotMapped);
    };
    let p2e = &pd[virt.p2_index()];
    if !p2e.flags().contains(PageTableFlags::PRESENT)
        || p2e.flags().contains(PageTableFlags::HUGE_PAGE)
    {
        return Err(VmError::NotMapped);
    }
    let Some(pt) = walker.next_mut(p2e) else {
        return Err(VmError::NotMapped);
    };
    let entry = &mut pt[virt.p1_index()];
    if !entry.flags().contains(PageTableFlags::PRESENT) {
        return Err(VmError::NotMapped);
    }
    let old_flags = entry.flags();
    if old_flags.contains(COW_BIT)
        && (!flags.contains(COW_BIT) || flags.contains(PageTableFlags::WRITABLE))
    {
        crate::error!(
            "[VMM] refusing to clear COW marker on virt=0x{:016x} (old={:?} new={:?})",
            virt_addr,
            old_flags,
            flags
        );
        return Err(VmError::CowShared);
    }
    let frame = entry.addr();
    entry.set_addr(frame, flags | PageTableFlags::PRESENT);
    tlb::flush(virt);
    Ok(())
}

/// Освободить каждый кадр, принадлежащий *приватному* пользовательскому
/// адресному пространству: все отображённые листовые кадры 4 КиБ нижней половины
/// (записи PML4 0..256), промежуточные кадры таблиц страниц под ними и сам кадр
/// PML4.
///
/// Вызывающий ОБЯЗАН обеспечить, что адресное пространство принадлежит
/// эксклюзивно (PML4 не делится с другой задачей) и что это не PML4 ядра —
/// функция освобождает безусловно и иначе снесла бы живые отображения.
/// Адресному пространству не нужно быть активным в CR3: весь доступ идёт через
/// окно HHDM (как в `clone_user_space`).
pub fn drop_user_space(pml4: u64) {
    let walker = PageTableWalker::new();
    let p4 = walker.table(pml4);
    for i4 in 0..256usize {
        let e4 = &p4[i4];
        if !e4.flags().contains(PageTableFlags::PRESENT) {
            continue;
        }
        let p3_phys = e4.addr().as_u64();
        let p3 = walker.table(p3_phys);
        for i3 in 0..512usize {
            let e3 = &p3[i3];
            if !e3.flags().contains(PageTableFlags::PRESENT) {
                continue;
            }
            if e3.flags().contains(PageTableFlags::HUGE_PAGE) {
                // Лист 1 ГиБ: кадр записи И ЕСТЬ отображённая страница (PD-
                // таблицы за ней нет). На этом уровне больше ничего не
                // освобождается. COW пока не создаёт огромных листьев (fork
                // откатывается к жадному копированию), но общий хелпер
                // освобождения сохранит учёт корректным, если это изменится.
                release_leaf_frame(e3.addr().as_u64(), e3.flags());
                continue;
            }
            let p2_phys = e3.addr().as_u64();
            let p2 = walker.table(p2_phys);
            for i2 in 0..512usize {
                let e2 = &p2[i2];
                if !e2.flags().contains(PageTableFlags::PRESENT) {
                    continue;
                }
                if e2.flags().contains(PageTableFlags::HUGE_PAGE) {
                    // Лист 2 МиБ: кадр записи И ЕСТЬ отображённая страница.
                    release_leaf_frame(e2.addr().as_u64(), e2.flags());
                    continue;
                }
                let p1_phys = e2.addr().as_u64();
                let p1 = walker.table(p1_phys);
                for i1 in 0..512usize {
                    let e1 = &p1[i1];
                    if !e1.flags().contains(PageTableFlags::PRESENT) {
                        continue;
                    }
                    // Разделён с другим адресным пространством: отпускаем
                    // только ссылку этой стороны. Освобождает последняя уходящая
                    // сторона — `release_leaf_frame` сообщает это возвратом
                    // cow_unref. Приватные листья освобождаются напрямую.
                    release_leaf_frame(e1.addr().as_u64(), e1.flags());
                }
                // Сам кадр PT: освобождается, даже если все листья были сняты
                // раньше (munmap чистит листовые PTE, но оставляет таблицу) —
                // после смерти этого дерева таблица недостижима.
                crate::memory::pmm::free_frame(p1_phys);
            }
            // ОДИН кадр PD на запись PDPT — освобождается один раз, ПОСЛЕ
            // цикла PT (если освобождать внутри цикла, заполненный PD
            // освобождался бы по разу на каждую присутствующую запись PT,
            // срабатывая на детекторе double-free PMM).
            crate::memory::pmm::free_frame(p2_phys);
        }
        // ОДИН кадр PDPT на запись PML4 — освобождается один раз, после цикла PD.
        crate::memory::pmm::free_frame(p3_phys);
    }
    crate::memory::pmm::free_frame(pml4);
}

/// Форкнуть активное адресное пространство нижней половины в `child_pml4`
/// copy-on-write: каждый присутствующий листовой кадр РАЗДЕЛЯЕТСЯ (учтён в PMM
/// через `pmm::cow_ref`), оба PTE помечаются программным битом `COW_BIT`, а
/// прежде записываемые записи теряют WRITABLE на обеих сторонах, чтобы
/// следующая запись попала в обработчик page fault, который копирует страницу
/// приватно ([`cow_copy_page`]). Read-only листья (текст/rodata ELF,
/// `PROT_READ`-mmap) сохраняют флаги И несут [`RO_BIT`], поэтому запись туда
/// по-прежнему даёт SIGSEGV, а не приватную записываемую копию; подсчитывается
/// только владение.
///
/// Промежуточные кадры таблиц страниц НЕ разделяются: ребёнок получает свежие
/// обнулённые таблицы, поэтому мутации таблиц каждой стороны независимы.
///
/// Возвращает `Err` (родитель не тронут), если родитель использует огромные
/// страницы в нижней половине — вызывающий откатывается к жадному копированию.
/// Родительское адресное пространство должно быть АКТИВНЫМ (fork идёт на CR3
/// родителя): RO-flip требует сброса TLB, выполняемого здесь полным сбросом на
/// возврате.
pub fn fork_user_space_cow(parent_pml4: u64, child_pml4: u64) -> Result<(), VmError> {
    // Контракт вызывающего: адресное пространство родителя — активный CR3,
    // потому что RO-flip ниже должен сбросить его записи TLB; полный сброс в
    // конце корректен только для активного пространства. `fork_linux_process`
    // зовёт это из контекста syscall родителя. Debug-сборки проверяют контракт
    // здесь, а не позволяют будущему вызывающему молча пропустить сброс.
    debug_assert_eq!(
        parent_pml4,
        crate::memory::vmm::current_pml4_phys(),
        "fork_user_space_cow requires the parent PML4 to be active"
    );
    let walker = PageTableWalker::new();
    let p4p = walker.table(parent_pml4);

    // Проход 1: отвергнуть огромные страницы до любой мутации, чтобы откат
    // вызывающего начинался с чистого родителя.
    for i4 in 0..256usize {
        let e4 = &p4p[i4];
        if !e4.flags().contains(PageTableFlags::PRESENT) {
            continue;
        }
        if e4.flags().contains(PageTableFlags::HUGE_PAGE) {
            return Err(VmError::NotMapped);
        }
        let p3 = walker.table(e4.addr().as_u64());
        for i3 in 0..512usize {
            let e3 = &p3[i3];
            if !e3.flags().contains(PageTableFlags::PRESENT) {
                continue;
            }
            if e3.flags().contains(PageTableFlags::HUGE_PAGE) {
                return Err(VmError::NotMapped);
            }
            let p2 = walker.table(e3.addr().as_u64());
            for i2 in 0..512usize {
                let e2 = &p2[i2];
                if e2.flags().contains(PageTableFlags::PRESENT)
                    && e2.flags().contains(PageTableFlags::HUGE_PAGE)
                {
                    return Err(VmError::NotMapped);
                }
            }
        }
    }

    // Проход 2: отразить дерево в ребёнке, разделить листья, переставить записываемые.
    for i4 in 0..256usize {
        let e4 = &p4p[i4];
        if !e4.flags().contains(PageTableFlags::PRESENT) {
            continue;
        }
        let p3p = walker.table(e4.addr().as_u64());
        let p3c_phys = fork_child_table_phys(child_pml4, PageTableIndex::new(i4 as u16))?;
        let p3c = walker.table(p3c_phys);
        for i3 in 0..512usize {
            let e3 = &p3p[i3];
            if !e3.flags().contains(PageTableFlags::PRESENT) {
                continue;
            }
            let p2p = walker.table(e3.addr().as_u64());
            let p2c_phys = fork_child_table_phys(p3c_phys, PageTableIndex::new(i3 as u16))?;
            let p2c = walker.table(p2c_phys);
            for i2 in 0..512usize {
                let e2 = &p2p[i2];
                if !e2.flags().contains(PageTableFlags::PRESENT) {
                    continue;
                }
                let p1p = walker.table_mut(e2.addr().as_u64());
                let p1c_phys = fork_child_table_phys(p2c_phys, PageTableIndex::new(i2 as u16))?;
                let p1c = walker.table_mut(p1c_phys);
                for i1 in 0..512usize {
                    let e1 = &p1p[i1];
                    if !e1.flags().contains(PageTableFlags::PRESENT) {
                        continue;
                    }
                    let frame = e1.addr();
                    let flags = e1.flags();
                    // MAP_SHARED и MMIO НЕ становятся COW: запись обязана идти
                    // в тот же кадр у обеих сторон. Трансляция копируется как
                    // есть; для shared добавляется ссылка нового адресного
                    // пространства, для MMIO ссылок нет вовсе (кадр не из PMM).
                    if flags.contains(MMIO_BIT) {
                        p1p[i1].set_addr(frame, flags);
                        p1c[i1].set_addr(frame, flags);
                        continue;
                    }
                    if flags.contains(SHARED_BIT) || flags.contains(VT_RO_BIT) {
                        p1p[i1].set_addr(frame, flags);
                        p1c[i1].set_addr(frame, flags);
                        crate::memory::pmm::cow_ref(frame.as_u64());
                        continue;
                    }
                    // Две разные причины не-записываемого листа должны
                    // оставаться различимыми после fork:
                    //   * записываемое отображение -> COW с отложенной записью:
                    //     write fault копирует страницу приватно (без RO_BIT);
                    //   * read-only отображение -> COW без права записи: write
                    //     fault обязан дать SIGSEGV (RO_BIT установлен).
                    // Уже-COW лист сохраняет свой RO_BIT: защита его отображения
                    // уже записана первым fork или mprotect, а «нет бита
                    // WRITABLE» само по себе не отличает отложенную COW-запись
                    // от read-only отображения.
                    let shared = if flags.contains(PageTableFlags::WRITABLE) {
                        ((flags - PageTableFlags::WRITABLE) | COW_BIT) - RO_BIT
                    } else if flags.contains(COW_BIT) {
                        // Уже COW-лист: `RO_BIT` фиксирует, запрещает ли его
                        // отображение запись; сохраняем.
                        flags | COW_BIT
                    } else {
                        // Приватный read-only лист (текст/rodata ELF,
                        // PROT_READ-отображение): ребёнок/эта сторона не должны
                        // получить запись через обработчик COW-fault.
                        (flags | COW_BIT) | RO_BIT
                    };
                    p1p[i1].set_addr(frame, shared);
                    p1c[i1].set_addr(frame, shared);
                    // В карту входят два шарящих, когда лист был приватным
                    // (родитель + ребёнок). Если лист родителя УЖЕ COW (fork от
                    // fork), сторона родителя учтена с первого fork —
                    // регистрируется только новый ребёнок.
                    if !flags.contains(COW_BIT) {
                        crate::memory::pmm::cow_ref(frame.as_u64());
                    }
                    crate::memory::pmm::cow_ref(frame.as_u64());
                }
            }
        }
    }
    // Переставленные PTE родителя могут ещё сидеть в TLB с устаревшими RW-
    // трансляциями; полная перезагрузка их сбрасывает.
    tlb::flush_all();
    Ok(())
}

/// Физический адрес промежуточной таблицы по `idx` таблицы по физическому
/// адресу `parent`, созданной (свежеобнулённой, с промежуточными флагами
/// USER-accessible), если её нет — дерево пользователя нижней половины.
fn fork_child_table_phys(parent: u64, idx: PageTableIndex) -> Result<u64, VmError> {
    let walker = PageTableWalker::new();
    let table = walker.table_mut(parent);
    if !table[idx].flags().contains(PageTableFlags::PRESENT) {
        let frame = crate::memory::pmm::alloc_frame().ok_or(VmError::OutOfMemory)?;
        // SAFETY: `frame` только что выделен и отображён через HHDM, поэтому
        // обнуляется ровно один принадлежащий нам выровненный кадр.
        unsafe {
            ptr::write_bytes(phys_to_virt(frame) as *mut u8, 0, 4096);
        }
        table[idx].set_addr(
            PhysAddr::new(frame),
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE,
        );
    }
    Ok(table[idx].addr().as_u64())
}

/// Обработчик copy-on-write fault: страница по `virt_addr` (АКТИВНОЕ адресное
/// пространство) — COW-разделённый лист (present + `COW_BIT`). Выделить свежий
/// кадр, скопировать разделяемое содержимое, перецелить PTE на приватную копию
/// с восстановленным WRITABLE, отпустить долю этой стороны в старом кадре
/// (освобождая его, если это был последний шарящий) и сбросить TLB. Возвращает
/// `false`, если PTE не COW-лист (применяется путь SIGSEGV вызывающего), если
/// отображение явно запрещает запись ([`RO_BIT`]: `mprotect(PROT_READ)` после
/// fork) или PMM исчерпан.
pub fn cow_copy_page(virt_addr: u64) -> bool {
    let walker = PageTableWalker::new();
    let virt = VirtAddr::new(virt_addr);

    let pml4 = walker.root_mut();
    let Some(pdpt) = walker.next_mut(&pml4[virt.p4_index()]) else {
        return false;
    };
    let Some(pd) = walker.next_mut(&pdpt[virt.p3_index()]) else {
        return false;
    };
    let Some(pt) = walker.next_mut(&pd[virt.p2_index()]) else {
        return false;
    };
    let entry = &mut pt[virt.p1_index()];
    let flags = entry.flags();
    if !flags.contains(PageTableFlags::PRESENT) || !flags.contains(COW_BIT) {
        return false;
    }
    // Само отображение запрещает запись (было read-only до fork, или так его
    // сделал `mprotect`): write fault обязан стать SIGSEGV, никогда — приватной
    // записываемой копией. Это и есть заслон для read-only COW-листьев.
    if flags.contains(RO_BIT) {
        crate::debug!(
            "[VMM] write fault on read-only COW leaf virt=0x{:x} flags={:?} -> SIGSEGV",
            virt_addr,
            flags
        );
        return false;
    }
    let old = entry.addr().as_u64();
    let Some(new) = crate::memory::pmm::alloc_frame() else {
        return false;
    };
    // SAFETY: оба адреса — HHDM-алиасы принадлежащих нам кадров 4 КиБ;
    // копирование не выходит за них.
    unsafe {
        core::ptr::copy_nonoverlapping(
            phys_to_virt(old) as *const u8,
            phys_to_virt(new) as *mut u8,
            4096,
        );
    }
    entry.set_addr(
        PhysAddr::new(new),
        (((flags - COW_BIT) - RO_BIT) | PageTableFlags::WRITABLE) | PageTableFlags::PRESENT,
    );
    tlb::flush(virt);
    if crate::memory::pmm::cow_unref(old) {
        crate::memory::pmm::free_frame(old);
    }
    true
}
