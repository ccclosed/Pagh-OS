// vfs/elf.rs — загрузчик бинарей ELF64
// 64-битное ядро ОС x86_64 на Rust (#![no_std])

use alloc::vec::Vec;

use crate::memory::vmm;
use core::ptr;
use x86_64::structures::paging::PageTableFlags;

// Реэкспорт чистого host-тестируемого классификатора ELF и выбора bias для
// static-PIE (R11.6). Они живут в соседнем модуле `elf_classify`, поэтому не
// тянут зависимости ядра/пейджинга и включаются в `host-tests` через `#[path]`.
// Эффектный `ElfLoader::load` их использует.
#[allow(unused_imports)]
pub use crate::vfs::elf_classify::{choose_bias, classify_elf, ElfKind, ElfVerdict};

#[repr(C)]
pub struct Elf64Header {
    pub e_ident: [u8; 16],
    pub e_type: u16,
    pub e_machine: u16,
    pub e_version: u32,
    pub e_entry: u64,
    pub e_phoff: u64,
    pub e_shoff: u64,
    pub e_flags: u32,
    pub e_ehsize: u16,
    pub e_phentsize: u16,
    pub e_phnum: u16,
    pub e_shentsize: u16,
    pub e_shnum: u16,
    pub e_shstrndx: u16,
}

#[repr(C)]
pub struct Elf64ProgramHeader {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

/// Читает заголовок ELF-файла из первых байт `data`.
///
/// `&[u8]` начинается с произвольного выравнивания, поэтому `&Elf64Header`
/// поверх него — UB даже при корректной длине, а проверка выравнивания в debug-
/// сборке превращает это UB в панику ядра. Копирование структуры по значению
/// (`ptr::read_unaligned`) выравнивания не требует.
///
/// # Safety
/// Вызывающий проверил `data.len() >= size_of::<Elf64Header>()`.
unsafe fn read_elf_header(data: &[u8]) -> Elf64Header {
    // SAFETY: вызывающий гарантировал, что заголовок в границах;
    // `read_unaligned` принимает любое выравнивание.
    unsafe { core::ptr::read_unaligned(data.as_ptr() as *const Elf64Header) }
}

/// Читает один program header из байтов файла.
///
/// `ph_offset` управляется атакующим (`e_phoff` из файла), поэтому не
/// обязательно выровнен под `Elf64ProgramHeader`. Копирование ниже выравнивания
/// не требует.
///
/// # Safety
/// Вызывающий проверил
/// `ph_offset + size_of::<Elf64ProgramHeader>() <= data.len()`.
unsafe fn read_program_header(data: &[u8], ph_offset: usize) -> Elf64ProgramHeader {
    // SAFETY: в границах по контракту вызывающего; `read_unaligned` принимает
    // любое выравнивание.
    unsafe { core::ptr::read_unaligned(data.as_ptr().add(ph_offset) as *const Elf64ProgramHeader) }
}

const PT_LOAD: u32 = 1;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 0x3E;

const PF_R: u32 = 4;
const PF_W: u32 = 2;
const PF_X: u32 = 1;

const EI_MAG0: usize = 0;
const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;

/// Исключающая верхняя граница нижней половины канонического адресного
/// пространства x86_64 (48-битный VA): валидные пользовательские адреса —
/// `0 .. 0x0000_8000_0000_0000`. Всё выше — либо неканоническая дыра, либо
/// верхняя половина ядра; маппинг такого адреса заалиасил бы пространство ядра
/// или запаниковал бы в `VirtAddr::new` (она отвергает неканонические адреса).
/// Отвергаем их на валидации, чтобы битый program header не дошёл до цикла
/// маппинга.
const USER_ADDR_MAX: u64 = 0x0000_8000_0000_0000;

/// Один замапленный `PT_LOAD`-сегмент, округлённый по страницам, — как нужно
/// `/proc/self/maps`.
///
/// `prot` хранит сырые ELF `p_flags` (`PF_R`/`PF_W`/`PF_X`); procfs-слой
/// переводит их в Linux `PROT_*` при рендере строки. `file_offset` — `p_offset`
/// сегмента, округлённый вниз до границы страницы (Linux показывает
/// выровненный по странице file offset маппинга).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadSegment {
    pub start: u64,
    pub end: u64,
    pub prot: u32,
    pub file_offset: u64,
}

/// Выход загрузчика для успешно замапленного Linux ELF-образа.
///
/// Кроме точки входа и пользовательского PML4, которые использует нативный путь
/// pagh, здесь лежат данные, нужные `Stack_Initializer` для Linux-совместимого
/// auxiliary vector (`AT_PHDR`/`AT_PHENT`/`AT_PHNUM`/`AT_ENTRY`) и для начального
/// program break процесса (R6.4).
pub struct ElfProcess {
    /// Точка входа, уже скорректированная `load_bias` для `ET_DYN` (R5.2).
    pub entry: u64,
    /// Физический адрес свежесозданного пользовательского PML4.
    pub pml4_phys: u64,
    /// Выровненный по странице load bias, применённый ко всем сегментам: `0`
    /// для `ET_EXEC`, выбранное ядром значение для static-PIE `ET_DYN` (R5.1,
    /// R5.2).
    pub load_bias: u64,
    /// `AT_PHDR`: виртуальный адрес таблицы program headers в памяти (с bias,
    /// R6.4).
    pub phdr_vaddr: u64,
    /// `AT_PHENT`: размер записи program header (`e_phentsize`).
    pub phent: u16,
    /// `AT_PHNUM`: число program headers (`e_phnum`).
    pub phnum: u16,
    /// Выровненный по странице верх наивысшего `PT_LOAD` (+bias); начальный
    /// heap break.
    pub initial_brk: u64,
    /// Замапленные `PT_LOAD`-сегменты в порядке program headers. Потребляются
    /// `/proc/self/maps`; пусто у образов без `PT_LOAD`.
    pub segments: Vec<LoadSegment>,
}

pub struct ElfLoader;

/// Кадры, замапленные одним проходом загрузчика, в порядке маппинга — для
/// отката при ошибке.
///
/// Загрузчик мапит образ постранично через `vmm::map`; guard делает откат
/// безусловным при *любом* возврате, включая добавленные позже. Порядок важен:
/// сначала unmap виртуальной страницы, затем возврат кадра в PMM, чтобы живая PTE
/// никогда не указывала на память, которую аллокатор может выдать снова.
/// Пользовательский PML4 и промежуточные таблицы страниц `vmm::map` **не**
/// освобождаются — teardown-API у VMM нет (то же на выходе процесса), — поэтому
/// guard владеет *data*-кадрами; это осознанное ограничение.
pub(crate) struct MappedFrames {
    entries: Vec<(u64, u64)>,
}

impl MappedFrames {
    pub(crate) fn new() -> Self {
        MappedFrames {
            entries: Vec::new(),
        }
    }

    /// Мапит `frame` по `addr`. При ошибке кадр возвращается сразу (он не был
    /// замаплен), а всё замапленное до него освобождает [`Drop`].
    ///
    /// `pub(crate)` вместе с самим типом: in-QEMU регрессия
    /// (`elf::failed map rolls back …`) гоняет guard на настоящих кадрах — только
    /// так поздний отказ проверяется детерминированно.
    pub(crate) fn map(
        &mut self,
        frame: u64,
        addr: u64,
        flags: PageTableFlags,
    ) -> Result<(), &'static str> {
        if vmm::map(frame, addr, flags).is_err() {
            crate::memory::pmm::free_frame(frame);
            return Err("ELF: VMM map failed");
        }
        self.entries.push((frame, addr));
        Ok(())
    }

    /// Маппинг удался: кадры теперь принадлежат загруженному образу.
    fn commit(&mut self) {
        self.entries.clear();
    }
}

impl Drop for MappedFrames {
    fn drop(&mut self) {
        while let Some((frame, addr)) = self.entries.pop() {
            let _ = vmm::unmap(addr);
            crate::memory::pmm::free_frame(frame);
        }
    }
}

impl ElfLoader {
    pub fn load(data: &[u8]) -> Result<ElfProcess, &'static str> {
        if data.len() < core::mem::size_of::<Elf64Header>() {
            return Err("ELF: data too small for header");
        }

        // SAFETY: срез длиннее заголовка (проверено выше); байты выровнены
        // только побайтно, поэтому заголовок копируется, а не берётся по ссылке.
        let header_owned = unsafe { read_elf_header(data) };
        let header: &Elf64Header = &header_owned;

        if header.e_ident[EI_MAG0] != 0x7F || &header.e_ident[1..4] != b"ELF" {
            return Err("ELF: invalid magic");
        }
        if header.e_ident[EI_CLASS] != ELFCLASS64 {
            return Err("ELF: not 64-bit");
        }
        if header.e_ident[EI_DATA] != ELFDATA2LSB {
            return Err("ELF: not little-endian");
        }
        if header.e_type != ET_EXEC {
            return Err("ELF: not ET_EXEC");
        }
        if header.e_machine != EM_X86_64 {
            return Err("ELF: not x86_64");
        }
        if header.e_version != 1 {
            return Err("ELF: unsupported version");
        }

        // Валидируем таблицу program headers и каждый PT_LOAD overflow-safe
        // арифметикой *до* аллокации пользовательского PML4 и любого маппинга.
        // Так любой отказ битому бинарю возвращает Err, не трогая память, и —
        // что важно — цикл маппинга позже не выйдет за границы, не скопирует за
        // вход и не подсунет неканонический адрес в `VirtAddr::new` (это была бы
        // паника).
        Self::validate_program_headers(data, header)?;

        crate::debug!(
            "Valid ELF64, entry=0x{:x}, {} phdrs",
            header.e_entry,
            header.e_phnum
        );

        let pml4_phys = vmm::new_user_pml4().map_err(|_| "ELF: failed to create PML4")?;

        // Мапим PT_LOAD В свежий пользовательский PML4. `vmm::map`/
        // `virt_to_phys` работают с *активным* CR3, поэтому на время цикла
        // сегментов ставим пользовательский PML4, затем восстанавливаем CR3
        // ядра. Верхняя половина ядра (код, стек, куча, битмап PMM и HHDM,
        // используемый для копирования данных) была клонирована в
        // пользовательский PML4 внутри `new_user_pml4`, так что ядро продолжает
        // работать при переключённом CR3.
        //
        // Вызывающий (`create_user_process`) запускает это с выключенными
        // прерываниями, поэтому тик таймера не увидит временно установленный
        // пользовательский CR3.
        let kernel_cr3 = vmm::current_pml4_phys();
        // SAFETY: `pml4_phys` — валидный PML4, содержащий верхнюю половину ядра.
        unsafe {
            vmm::load_cr3(pml4_phys);
        }
        let load_result = Self::map_segments(data, header);
        // SAFETY: восстанавливаем PML4 ядра независимо от успеха/отказа, чтобы
        // не вернуться к вызывающему с чужим адресным пространством.
        unsafe {
            vmm::load_cr3(kernel_cr3);
        }
        let brk = load_result?;

        crate::debug!("Loaded: entry=0x{:x} brk=0x{:x}", header.e_entry, brk);

        Ok(ElfProcess {
            entry: header.e_entry,
            pml4_phys,
            // Нативный путь pagh грузит только абсолютные ET_EXEC, поэтому bias
            // здесь всегда 0, а адреса entry/phdr абсолютные.
            load_bias: 0,
            phdr_vaddr: Self::compute_phdr_vaddr(data, header, 0),
            phent: header.e_phentsize,
            phnum: header.e_phnum,
            initial_brk: brk,
            // У нативного пути нет потребителя maps; сегменты собираются только
            // для Linux-образов (`load_linux`).
            segments: Vec::new(),
        })
    }

    /// Грузит статически слинкованный Linux `ET_EXEC` или static-PIE `ET_DYN` и
    /// отдаёт выход загрузчика, нужный `Stack_Initializer` для auxv
    /// (R5.1/R5.2/R5.3/R5.4/R5.7/R5.8/R7.6/R11.3/R12.1).
    ///
    /// Это *аддитивный* сосед [`ElfLoader::load`]: нативный путь остаётся
    /// ограничен абсолютными `ET_EXEC` (его используют `create_user_process` /
    /// `spawn_test_user_process`), а этот дополнительно принимает static-PIE
    /// `ET_DYN` через чистый классификатор [`classify_elf`]/[`choose_bias`] и
    /// возвращает полный [`ElfProcess`] с заполненными `load_bias`,
    /// `phdr_vaddr`, `phent`, `phnum` и `initial_brk`.
    ///
    /// Шаги:
    ///   1. [`classify_elf`] — при [`ElfVerdict::Reject`] одна диагностика с
    ///      причиной + идентификатором бинаря и `Err` **без** аллокации PML4 и
    ///      маппинга (R5.3/R5.5/R5.6/R5.9/R11.3/R12.1).
    ///   2. `ET_DYN` → [`choose_bias`] по `max(p_vaddr + p_memsz)`; `ET_EXEC` →
    ///      bias `0` (R5.1/R5.2).
    ///   3. Свежий пользовательский PML4; каждый `PT_LOAD` мапится по
    ///      `p_vaddr + bias` с `PF_W`→WRITABLE, `!PF_X`→NO_EXECUTE, всегда
    ///      `USER_ACCESSIBLE`; копируется ровно `p_filesz` байт, а
    ///      `[p_filesz, p_memsz)` обнуляется (R5.4/R5.7/R5.8/R7.6).
    ///   4. Вычисляются `phdr_vaddr`, `phent`, `phnum`, `initial_brk`.
    pub fn load_linux(data: &[u8]) -> Result<ElfProcess, &'static str> {
        // 1. Чистая классификация — отказ *до* касания памяти (R12.1).
        let kind = match classify_elf(data) {
            ElfVerdict::Reject(msg) => {
                // Одна диагностика с причиной отказа + идентификатором бинаря
                // (здесь есть только байтовый буфер, так что идентификатор —
                // его длина). Severity — Error, R12.5.
                crate::error!(
                    "Linux ELF loader: rejecting binary (id=<{}-byte image>): {}",
                    data.len(),
                    msg
                );
                return Err(msg);
            }
            ElfVerdict::Load { kind, .. } => kind,
        };

        // `classify_elf` гарантирует, что буфер не меньше заголовка ELF64, имеет
        // валидные magic / ELFCLASS64 / ELFDATA2LSB / EM_X86_64 и что каждый
        // PT_LOAD в границах с округлённым по страницам (при нулевом bias)
        // диапазоном ниже USER_ADDR_MAX. Значит, чтение заголовка в границах.
        // SAFETY: длину проверил `classify_elf`; из-за побайтного выравнивания
        // заголовок копируется, а не берётся по ссылке.
        let header_owned = unsafe { read_elf_header(data) };
        let header: &Elf64Header = &header_owned;

        // 2. Выбираем load bias.
        let bias = match kind {
            ElfKind::Exec => 0u64,
            ElfKind::Dyn => {
                let max_end = Self::max_load_vaddr_end(data, header)?;
                choose_bias(max_end).ok_or("ELF: no load bias fits in user address space")?
            }
        };

        // Точка входа с поправкой на bias (0 для ET_EXEC). Overflow-safe.
        let entry = header
            .e_entry
            .checked_add(bias)
            .ok_or("ELF: entry point overflow under bias")?;

        crate::debug!(
            "Linux ELF: kind={:?} bias=0x{:x} entry=0x{:x} {} phdrs",
            kind,
            bias,
            entry,
            header.e_phnum
        );

        // 3. Свежий пользовательский PML4, затем маппинг смещённых PT_LOAD. Как
        // и в нативном пути, на время цикла ставим пользовательский CR3 (чтобы
        // `vmm::map`/`virt_to_phys` работали с ним) и восстанавливаем CR3 ядра
        // независимо от результата. Вызывающий идёт с выключенными прерываниями.
        let pml4_phys = vmm::new_user_pml4().map_err(|_| "ELF: failed to create PML4")?;

        let kernel_cr3 = vmm::current_pml4_phys();
        // SAFETY: `pml4_phys` — валидный PML4, содержащий верхнюю половину ядра.
        unsafe {
            vmm::load_cr3(pml4_phys);
        }
        let mut segments: Vec<LoadSegment> = Vec::new();
        let map_result = Self::map_segments_biased(data, header, bias, &mut segments);
        // SAFETY: PML4 ядра восстанавливается до любого возврата вызывающему.
        unsafe {
            vmm::load_cr3(kernel_cr3);
        }
        let initial_brk = map_result?;

        // 4. Выход загрузчика для auxv.
        let phdr_vaddr = Self::compute_phdr_vaddr(data, header, bias);

        crate::debug!(
            "Linux ELF loaded: entry=0x{:x} bias=0x{:x} phdr=0x{:x} phent={} phnum={} brk=0x{:x}",
            entry,
            bias,
            phdr_vaddr,
            header.e_phentsize,
            header.e_phnum,
            initial_brk
        );

        Ok(ElfProcess {
            entry,
            pml4_phys,
            load_bias: bias,
            phdr_vaddr,
            phent: header.e_phentsize,
            phnum: header.e_phnum,
            initial_brk,
            segments,
        })
    }

    /// Мапит образ ET_DYN PT_INTERP в существующее пользовательское адресное
    /// пространство.
    pub fn map_interpreter(
        pml4_phys: u64,
        data: &[u8],
        bias: u64,
        segments: &mut Vec<LoadSegment>,
    ) -> Result<u64, &'static str> {
        match classify_elf(data) {
            ElfVerdict::Load {
                kind: ElfKind::Dyn, ..
            } => {}
            ElfVerdict::Load { .. } => return Err("ELF: interpreter is not ET_DYN"),
            ElfVerdict::Reject(msg) => return Err(msg),
        }
        // SAFETY: `classify_elf` проверил длину; буфер выровнен только побайтно,
        // поэтому заголовок копируется из него.
        let header_owned = unsafe { read_elf_header(data) };
        let header: &Elf64Header = &header_owned;
        let max_end = Self::max_load_vaddr_end(data, header)?;
        let biased_end = bias
            .checked_add(max_end)
            .ok_or("ELF: interpreter bias overflow")?;
        if biased_end >= USER_ADDR_MAX {
            return Err("ELF: interpreter does not fit in user address space");
        }
        let entry = header
            .e_entry
            .checked_add(bias)
            .ok_or("ELF: interpreter entry overflow")?;
        let kernel_cr3 = vmm::current_pml4_phys();
        unsafe {
            vmm::load_cr3(pml4_phys);
        }
        let result = Self::map_segments_biased(data, header, bias, segments);
        unsafe {
            vmm::load_cr3(kernel_cr3);
        }
        result?;
        Ok(entry)
    }

    /// Максимум `p_vaddr + p_memsz` по всем `PT_LOAD` (наивысший несмещённый
    /// виртуальный адрес образа) — вход [`choose_bias`] для static-PIE `ET_DYN`.
    /// Overflow-safe; `0`, если загружаемых сегментов нет.
    ///
    /// PRECONDITION: `classify_elf(data)` уже успешно отработал, поэтому таблица
    /// phdr целиком в границах и каждая читаемая ниже структура program header
    /// лежит внутри `data`.
    fn max_load_vaddr_end(data: &[u8], header: &Elf64Header) -> Result<u64, &'static str> {
        let phoff = header.e_phoff as usize;
        let phentsize = header.e_phentsize as usize;
        let phnum = header.e_phnum as usize;
        let mut max_end: u64 = 0;

        for i in 0..phnum {
            // В границах: classify_elf проверил phoff + phnum*phentsize <= len.
            let ph_offset = phoff + i * phentsize;
            // SAFETY: `ph_offset + phentsize <= table_end <= data.len()`.
            // `ph_offset` управляется атакующим, поэтому может быть не выровнен
            // под `Elf64ProgramHeader`: копируем заголовок, а не ссылаемся на
            // него.
            let ph = unsafe { read_program_header(data, ph_offset) };
            if ph.p_type != PT_LOAD {
                continue;
            }
            let end = ph
                .p_vaddr
                .checked_add(ph.p_memsz)
                .ok_or("ELF: vaddr range overflow")?;
            if end > max_end {
                max_end = end;
            }
        }
        Ok(max_end)
    }

    /// Вычисляет `AT_PHDR`: виртуальный адрес таблицы program headers в памяти
    /// после загрузки образа с `bias`.
    ///
    /// Стандартное соглашение SysV: заголовки замаплены в составе того
    /// `PT_LOAD`, чей файловый диапазон `[p_offset, p_offset + p_filesz)`
    /// покрывает `e_phoff`; для него таблица лежит по
    /// `p_vaddr + bias + (e_phoff - p_offset)`. Если ни один сегмент явно не
    /// покрывает `e_phoff` (редкость), откат к «заголовки в начале первого
    /// `PT_LOAD`», то есть `first_load.p_vaddr + bias + e_phoff`, и наконец к
    /// `bias + e_phoff`, если загружаемых сегментов нет вовсе.
    ///
    /// PRECONDITION: `classify_elf(data)` уже успешно отработал.
    fn compute_phdr_vaddr(data: &[u8], header: &Elf64Header, bias: u64) -> u64 {
        let phoff = header.e_phoff;
        let phentsize = header.e_phentsize as usize;
        let phnum = header.e_phnum as usize;
        let table_off = header.e_phoff as usize;
        let mut first_load_vaddr: Option<u64> = None;

        for i in 0..phnum {
            // В границах: classify_elf проверил, что таблица phdr помещается в
            // `data`.
            let ph_offset = table_off + i * phentsize;
            // SAFETY: границы гарантированы classify_elf. `ph_offset`
            // управляется атакующим, поэтому может быть не выровнен под
            // `Elf64ProgramHeader`: копируем заголовок, а не ссылаемся на него.
            let ph = unsafe { read_program_header(data, ph_offset) };
            if ph.p_type != PT_LOAD {
                continue;
            }
            if first_load_vaddr.is_none() {
                first_load_vaddr = Some(ph.p_vaddr);
            }
            // Покрывает ли файловый диапазон этого сегмента смещение таблицы
            // phdr?
            let seg_file_end = ph.p_offset.saturating_add(ph.p_filesz);
            if ph.p_offset <= phoff && phoff < seg_file_end {
                return ph
                    .p_vaddr
                    .wrapping_add(bias)
                    .wrapping_add(phoff - ph.p_offset);
            }
        }

        if let Some(v) = first_load_vaddr {
            return v.wrapping_add(bias).wrapping_add(phoff);
        }
        bias.wrapping_add(phoff)
    }

    /// Мапит все `PT_LOAD` по `p_vaddr + bias` в **текущий активный** PML4
    /// (вызывающий сначала ставит пользовательский PML4) и возвращает выровненный
    /// по странице program break (округлённый верх наивысшего сегмента).
    ///
    /// Структурно совпадает с [`ElfLoader::map_segments`], но применяет load bias
    /// (для static-PIE) к виртуальному адресу каждого сегмента. Флаги на
    /// сегмент: `PF_W`→WRITABLE, `!PF_X`→NO_EXECUTE, всегда `USER_ACCESSIBLE`
    /// (R5.4/R7.6). Копируется ровно `p_filesz` байт с `p_offset`, а
    /// `[p_filesz, p_memsz)` обнуляется (R5.7/R5.8). Вся арифметика
    /// overflow-safe, смещённый диапазон защитно перепроверяется по
    /// `USER_ADDR_MAX`.
    ///
    /// PRECONDITION: `classify_elf(data)` уже успешно отработал для этого образа.
    fn map_segments_biased(
        data: &[u8],
        header: &Elf64Header,
        bias: u64,
        segments: &mut Vec<LoadSegment>,
    ) -> Result<u64, &'static str> {
        // В отличие от нативного пути этот не проходит через
        // `validate_program_headers` (смещённый vaddr-диапазон проверяется только
        // при известном bias), поэтому граница eager-маппинга применяется здесь —
        // до первой аллокации.
        if data.len() as u64 > Self::MAX_IMAGE_BYTES {
            return Err("ELF: image file too large for the loader");
        }
        let mut total_memsz: u64 = 0;

        let mut brk: u64 = 0;
        // Кадры, замапленные этим проходом; фиксируются только при полном
        // успехе (см. `MappedFrames`). Один guard на весь образ: отказ в позднем
        // сегменте обязан освободить и ранние.
        let mut mapped = MappedFrames::new();
        let phoff = header.e_phoff as usize;
        let phentsize = header.e_phentsize as usize;
        let phnum = header.e_phnum as usize;

        if phnum == 0 {
            return Ok(0);
        }
        if phentsize < core::mem::size_of::<Elf64ProgramHeader>() {
            return Err("ELF: invalid program header size");
        }

        for i in 0..phnum {
            let ph_offset = phoff
                .checked_add(i.checked_mul(phentsize).ok_or("ELF: phdr index overflow")?)
                .ok_or("ELF: phdr offset overflow")?;
            let ph_end = ph_offset
                .checked_add(phentsize)
                .ok_or("ELF: phdr end overflow")?;
            if ph_end > data.len() {
                return Err("ELF: program header beyond data");
            }

            // SAFETY: границы проверены выше. `ph_offset` управляется
            // атакующим, поэтому может быть не выровнен под
            // `Elf64ProgramHeader`: копируем заголовок, а не ссылаемся на него.
            let ph = unsafe { read_program_header(data, ph_offset) };

            if ph.p_type != PT_LOAD {
                continue;
            }

            let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
            if ph.p_flags & PF_W != 0 {
                flags |= PageTableFlags::WRITABLE;
            }
            if ph.p_flags & PF_X == 0 {
                flags |= PageTableFlags::NO_EXECUTE;
            }

            // Применяем load bias к виртуальному адресу сегмента (R5.2).
            let vaddr_start = ph
                .p_vaddr
                .checked_add(bias)
                .ok_or("ELF: vaddr+bias overflow")?;
            let vaddr_end = vaddr_start
                .checked_add(ph.p_memsz)
                .ok_or("ELF: vaddr range overflow")?;
            if vaddr_start >= USER_ADDR_MAX || vaddr_end > USER_ADDR_MAX {
                return Err("ELF: segment outside user address space");
            }
            total_memsz = total_memsz
                .checked_add(ph.p_memsz)
                .ok_or("ELF: segment sizes overflow")?;
            if total_memsz > Self::MAX_IMAGE_BYTES {
                return Err("ELF: image too large for the loader");
            }
            let page_start = (vaddr_start / 4096) * 4096;
            let page_end = vaddr_end
                .checked_add(4095)
                .ok_or("ELF: vaddr page-round overflow")?
                / 4096
                * 4096;

            crate::debug!(
                "PT_LOAD@bias: 0x{:x}..0x{:x} fsz={} msz={} fl=0x{:x}",
                vaddr_start,
                vaddr_end,
                ph.p_filesz,
                ph.p_memsz,
                ph.p_flags
            );

            segments.push(LoadSegment {
                start: page_start,
                end: page_end,
                prot: ph.p_flags & (PF_R | PF_W | PF_X),
                file_offset: ph.p_offset & !4095,
            });

            let mut addr = page_start;
            while addr < page_end {
                let frame = crate::memory::pmm::alloc_frame().ok_or("ELF: PMM OOM for LOAD")?;
                // Страницы PT_LOAD — независимые кадры PMM, не физически
                // непрерывная аллокация. Обнуляем каждый кадр через его HHDM-алиас.
                unsafe {
                    ptr::write_bytes(vmm::phys_to_virt(frame) as *mut u8, 0, 4096);
                }
                // Каждый кадр регистрируется сразу после маппинга: любой поздний
                // отказ (OOM, плохой перевод адреса, ошибка копирования)
                // освобождает всё, а не оставляет страницы живыми.
                mapped.map(frame, addr, flags)?;
                addr += 4096;
            }

            if ph.p_filesz > 0 {
                let file_off = ph.p_offset as usize;
                let file_sz = ph.p_filesz as usize;
                let file_end = file_off
                    .checked_add(file_sz)
                    .ok_or("ELF: segment file range overflow")?;
                if file_end > data.len() {
                    return Err("ELF: file data beyond input");
                }
                // Копируем постранично: кадры PMM под соседними виртуальными
                // страницами не гарантированно физически непрерывны. Один HHDM
                // memcpy на `file_sz` испортил бы постороннюю физическую память.
                let mut copied: usize = 0;
                while copied < file_sz {
                    let dst_vaddr = vaddr_start + copied as u64;
                    let page_left = 4096usize - (dst_vaddr as usize & 4095);
                    let chunk = core::cmp::min(page_left, file_sz - copied);
                    let dst_phys = vmm::virt_to_phys(dst_vaddr)
                        .ok_or("ELF: failed to translate segment page")?;
                    let dst = vmm::phys_to_virt(dst_phys) as *mut u8;
                    unsafe {
                        ptr::copy_nonoverlapping(data.as_ptr().add(file_off + copied), dst, chunk);
                    }
                    copied += chunk;
                }
            }

            if ph.p_memsz > ph.p_filesz {
                let bss_start = vaddr_start + ph.p_filesz;
                let bss_end = vaddr_end;
                let mut cursor = bss_start;
                while cursor < bss_end {
                    let page_left = 4096u64 - (cursor & 4095);
                    let chunk = core::cmp::min(page_left, bss_end - cursor);
                    let phys =
                        vmm::virt_to_phys(cursor).ok_or("ELF: failed to translate bss page")?;
                    unsafe {
                        ptr::write_bytes(vmm::phys_to_virt(phys) as *mut u8, 0, chunk as usize);
                    }
                    cursor += chunk;
                }
            }

            if vaddr_end > brk {
                brk = vaddr_end;
            }
        }

        brk = (brk + 4095) & !4095;
        mapped.commit();
        Ok(brk)
    }

    /// Наибольший суммарный `PT_LOAD p_memsz`, который может запросить один
    /// образ (64 MiB).
    ///
    /// Каждая страница сегмента мапится **eagerly**, поэтому это жёсткая граница
    /// того, сколько PMM может съесть один образ до любого другого отказа.
    /// Реальный юзерленд этого ядра (`hello-pagh`, `ld.so`/`libc`, `python3`,
    /// `nvim`) требует единицы MiB; 64 MiB дают запас и остаются 1/16 гостя
    /// 1 GiB, которого бутит E2E-стенд.
    pub const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

    /// Валидирует таблицу program headers и все `PT_LOAD` только overflow-safe
    /// арифметикой. `Ok(())`, если каждый сегмент в границах и адресуем, иначе
    /// `Err` с первой найденной проблемой.
    ///
    /// Не делает ни аллокаций, ни маппинга: загрузчик отвергает битые бинари до
    /// создания пользовательского PML4, а цикл маппинга в `map_segments` не может
    /// паниковать или выйти за входной буфер.
    ///
    /// Образ с нулевым `e_phnum` принимается (мапить нечего); вызывающий всё
    /// равно получает валидную точку входа и сам обрабатывает отказ запуска.
    fn validate_program_headers(data: &[u8], header: &Elf64Header) -> Result<(), &'static str> {
        // Сам файл не может быть больше описываемого образа: отказ до обхода.
        if data.len() as u64 > Self::MAX_IMAGE_BYTES {
            return Err("ELF: image file too large for the loader");
        }
        // Сумма загружаемых размеров с границей `MAX_IMAGE_BYTES` ниже — именно
        // эта проверка не пускает подделанный `p_memsz` в цикл маппинга.
        let mut total_memsz: u64 = 0;

        let phnum = header.e_phnum as usize;
        if phnum == 0 {
            // Program headers нет: грузить нечего. Принимаем.
            return Ok(());
        }

        let phentsize = header.e_phentsize as usize;
        if phentsize < core::mem::size_of::<Elf64ProgramHeader>() {
            return Err("ELF: invalid program header size");
        }

        let phoff = header.e_phoff as usize;

        // Ограничиваем всю таблицу phdr: phoff + phnum*phentsize должен
        // помещаться в `data`.
        let table_bytes = phnum
            .checked_mul(phentsize)
            .ok_or("ELF: program header table size overflow")?;
        let table_end = phoff
            .checked_add(table_bytes)
            .ok_or("ELF: program header table offset overflow")?;
        if table_end > data.len() {
            return Err("ELF: program header table beyond data");
        }

        for i in 0..phnum {
            // Безопасно: i < phnum, phnum*phentsize не переполнилось, значит
            // значение <= table_bytes и в диапазоне usize.
            let ph_offset = phoff + i * phentsize;

            // SAFETY: `ph_offset + phentsize <= table_end <= data.len()` и
            // `phentsize >= size_of::<Elf64ProgramHeader>()`, поэтому вся
            // структура program header лежит внутри `data`. `ph_offset`
            // управляется атакующим, поэтому может быть не выровнен под
            // `Elf64ProgramHeader`: копируем заголовок, а не ссылаемся на него.
            let ph = unsafe { read_program_header(data, ph_offset) };

            if ph.p_type != PT_LOAD {
                continue;
            }

            // Диапазон данных файла обязан лежать во входном буфере.
            if ph.p_filesz > 0 {
                let file_off = ph.p_offset as usize;
                let file_sz = ph.p_filesz as usize;
                let file_end = file_off
                    .checked_add(file_sz)
                    .ok_or("ELF: segment file range overflow")?;
                if file_end > data.len() {
                    return Err("ELF: file data beyond input");
                }
            }

            // filesz не больше memsz (иначе bss-хвост был бы отрицательным).
            if ph.p_filesz > ph.p_memsz {
                return Err("ELF: p_filesz exceeds p_memsz");
            }

            // Виртуальный диапазон обязан быть канонической нижней половиной
            // (user) и не переполняться.
            let vaddr_end = ph
                .p_vaddr
                .checked_add(ph.p_memsz)
                .ok_or("ELF: vaddr range overflow")?;
            if ph.p_vaddr >= USER_ADDR_MAX || vaddr_end > USER_ADDR_MAX {
                return Err("ELF: segment outside user address space");
            }

            // Eager-маппинг: ограничиваем сумму, чтобы правдоподобный, но
            // огромный `p_memsz` отвергался здесь, а не в цикле маппинга.
            total_memsz = total_memsz
                .checked_add(ph.p_memsz)
                .ok_or("ELF: segment sizes overflow")?;
            if total_memsz > Self::MAX_IMAGE_BYTES {
                return Err("ELF: image too large for the loader");
            }

            // Округлённый по странице конец тоже не должен переполняться (его
            // использует цикл маппинга).
            vaddr_end
                .checked_add(4095)
                .ok_or("ELF: vaddr page-round overflow")?;
        }

        Ok(())
    }

    /// Мапит все `PT_LOAD` из `header`/`data` в **текущий активный** PML4
    /// (вызывающий сначала ставит пользовательский PML4) и возвращает
    /// вычисленный program break (`brk`).
    ///
    /// PRECONDITION: `validate_program_headers` уже успешно отработал для этих
    /// `data`/`header`, поэтому все смещения/размеры в границах, а виртуальные
    /// адреса — каноническая нижняя половина. Тем не менее проверки границ
    /// защитно повторяются checked-арифметикой, чтобы функция не паниковала и не
    /// выходила за границы даже при независимом вызове.
    fn map_segments(data: &[u8], header: &Elf64Header) -> Result<u64, &'static str> {
        let mut brk: u64 = 0;
        // Кадры, замапленные этим проходом; фиксируются только при полном
        // успехе (см. `MappedFrames`). Один guard на весь образ: отказ в позднем
        // сегменте обязан освободить и ранние.
        let mut mapped = MappedFrames::new();
        let phoff = header.e_phoff as usize;
        let phentsize = header.e_phentsize as usize;
        let phnum = header.e_phnum as usize;

        if phnum == 0 {
            return Ok(0);
        }

        if phentsize < core::mem::size_of::<Elf64ProgramHeader>() {
            return Err("ELF: invalid program header size");
        }

        for i in 0..phnum {
            let ph_offset = phoff
                .checked_add(i.checked_mul(phentsize).ok_or("ELF: phdr index overflow")?)
                .ok_or("ELF: phdr offset overflow")?;
            let ph_end = ph_offset
                .checked_add(phentsize)
                .ok_or("ELF: phdr end overflow")?;
            if ph_end > data.len() {
                return Err("ELF: program header beyond data");
            }

            // SAFETY: границы проверены выше. `ph_offset` управляется
            // атакующим, поэтому может быть не выровнен под
            // `Elf64ProgramHeader`: копируем заголовок, а не ссылаемся на него.
            let ph = unsafe { read_program_header(data, ph_offset) };

            if ph.p_type != PT_LOAD {
                continue;
            }

            let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
            if ph.p_flags & PF_W != 0 {
                flags |= PageTableFlags::WRITABLE;
            }
            if ph.p_flags & PF_X == 0 {
                flags |= PageTableFlags::NO_EXECUTE;
            }

            let vaddr_start = ph.p_vaddr;
            let vaddr_end = vaddr_start
                .checked_add(ph.p_memsz)
                .ok_or("ELF: vaddr range overflow")?;
            if vaddr_start >= USER_ADDR_MAX || vaddr_end > USER_ADDR_MAX {
                return Err("ELF: segment outside user address space");
            }
            let page_start = (vaddr_start / 4096) * 4096;
            let page_end = vaddr_end
                .checked_add(4095)
                .ok_or("ELF: vaddr page-round overflow")?
                / 4096
                * 4096;

            crate::debug!(
                "PT_LOAD: 0x{:x}..0x{:x} fsz={} msz={} fl=0x{:x}",
                vaddr_start,
                vaddr_end,
                ph.p_filesz,
                ph.p_memsz,
                ph.p_flags
            );

            let mut addr = page_start;
            while addr < page_end {
                let frame = crate::memory::pmm::alloc_frame().ok_or("ELF: PMM OOM for LOAD")?;
                // Страницы PT_LOAD — независимые кадры PMM, не физически
                // непрерывная аллокация. Обнуляем каждый кадр через его HHDM-алиас.
                unsafe {
                    ptr::write_bytes(vmm::phys_to_virt(frame) as *mut u8, 0, 4096);
                }
                // Каждый кадр регистрируется сразу после маппинга: любой поздний
                // отказ (OOM, плохой перевод адреса, ошибка копирования)
                // освобождает всё, а не оставляет страницы живыми.
                mapped.map(frame, addr, flags)?;
                addr += 4096;
            }

            if ph.p_filesz > 0 {
                let file_off = ph.p_offset as usize;
                let file_sz = ph.p_filesz as usize;
                let file_end = file_off
                    .checked_add(file_sz)
                    .ok_or("ELF: segment file range overflow")?;
                if file_end > data.len() {
                    return Err("ELF: file data beyond input");
                }
                // Копируем постранично: кадры PMM под соседними виртуальными
                // страницами не гарантированно физически непрерывны. Один HHDM
                // memcpy на `file_sz` испортил бы постороннюю физическую память.
                let mut copied: usize = 0;
                while copied < file_sz {
                    let dst_vaddr = vaddr_start + copied as u64;
                    let page_left = 4096usize - (dst_vaddr as usize & 4095);
                    let chunk = core::cmp::min(page_left, file_sz - copied);
                    let dst_phys = vmm::virt_to_phys(dst_vaddr)
                        .ok_or("ELF: failed to translate segment page")?;
                    let dst = vmm::phys_to_virt(dst_phys) as *mut u8;
                    unsafe {
                        ptr::copy_nonoverlapping(data.as_ptr().add(file_off + copied), dst, chunk);
                    }
                    copied += chunk;
                }
            }

            if ph.p_memsz > ph.p_filesz {
                let bss_start = vaddr_start + ph.p_filesz;
                let bss_end = vaddr_end;
                let mut cursor = bss_start;
                while cursor < bss_end {
                    let page_left = 4096u64 - (cursor & 4095);
                    let chunk = core::cmp::min(page_left, bss_end - cursor);
                    let phys =
                        vmm::virt_to_phys(cursor).ok_or("ELF: failed to translate bss page")?;
                    unsafe {
                        ptr::write_bytes(vmm::phys_to_virt(phys) as *mut u8, 0, chunk as usize);
                    }
                    cursor += chunk;
                }
            }

            if vaddr_end > brk {
                brk = vaddr_end;
            }
        }

        brk = (brk + 4095) & !4095;
        mapped.commit();
        Ok(brk)
    }
}
