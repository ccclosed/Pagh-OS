// arch/x86_64/gdt.rs — Global Descriptor Table, TSS, стеки IST.

use core::cell::SyncUnsafeCell;
use core::sync::atomic::{AtomicU16, Ordering};
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

// ─── выделение стеков IST (Interrupt Stack Table) ─────────────────────────

/// Стек IST для Double Fault (16 КиБ, выравнивание 16 байт).
#[repr(C, align(16))]
struct IstStack([u8; 16384]);

// Стеки IST, TSS и GDT живут в `SyncUnsafeCell`, а не в `static mut`, чтобы
// каждый доступ шёл через сырой указатель `.get()` и мы никогда не создавали
// `&`/`&mut` на изменяемый static (это запрещает lint `static_mut_refs`). Ни
// один из типов не реализует `Sync`, но `SyncUnsafeCell` даёт реализацию,
// нужную static; корректность держится на инварианте «инициализация один раз,
// однопоточно, до прерываний», описанном в каждом месте доступа.
static IST1_DOUBLE_FAULT: SyncUnsafeCell<IstStack> = SyncUnsafeCell::new(IstStack([0; 16384]));
static IST2_PAGE_FAULT: SyncUnsafeCell<IstStack> = SyncUnsafeCell::new(IstStack([0; 16384]));

// ─── TSS ────────────────────────────────────────────────────────────────

static TSS: SyncUnsafeCell<TaskStateSegment> = SyncUnsafeCell::new(TaskStateSegment::new());

// ─── GDT ────────────────────────────────────────────────────────────────

static GDT: SyncUnsafeCell<GlobalDescriptorTable> =
    SyncUnsafeCell::new(GlobalDescriptorTable::new());

/// Публичные селекторы сегментов, выставляются после `init()`.
pub struct Selectors;

// Значения селекторов хранятся сырыми `u16` в `AtomicU16`: `SegmentSelector` —
// `#[repr(transparent)]`-обёртка над `u16`, поэтому сохранение `.0` в `init` и
// пересборка обёртки при чтении точно, lock-free и без ссылок заменяют прежние
// `static mut SegmentSelector`.
static KERNEL_CODE_SEL: AtomicU16 = AtomicU16::new(0);
static KERNEL_DATA_SEL: AtomicU16 = AtomicU16::new(0);
static USER_CODE_SEL: AtomicU16 = AtomicU16::new(0);
static USER_DATA_SEL: AtomicU16 = AtomicU16::new(0);
static TSS_SEL: AtomicU16 = AtomicU16::new(0);

impl Selectors {
    pub fn kernel_code() -> SegmentSelector {
        // Ставится один раз в init, дальше только чтение; обёртка собирается из сырого значения.
        SegmentSelector(KERNEL_CODE_SEL.load(Ordering::Relaxed))
    }
    pub fn kernel_data() -> SegmentSelector {
        // Ставится один раз в init, дальше только чтение; обёртка собирается из сырого значения.
        SegmentSelector(KERNEL_DATA_SEL.load(Ordering::Relaxed))
    }
    pub fn user_code() -> SegmentSelector {
        // Ставится один раз в init, дальше только чтение; обёртка собирается из сырого значения.
        SegmentSelector(USER_CODE_SEL.load(Ordering::Relaxed))
    }
    pub fn user_data() -> SegmentSelector {
        // Ставится один раз в init, дальше только чтение; обёртка собирается из сырого значения.
        SegmentSelector(USER_DATA_SEL.load(Ordering::Relaxed))
    }
}

// ─── индексы IST (должны совпадать с записями IDT) ──────────────────────

pub const IST_DOUBLE_FAULT: u16 = 1;
pub const IST_PAGE_FAULT: u16 = 2;

// ─── TSS RSP0 (стек перехода ring-3 → ring-0) ───────────────────────────

/// Запрограммировать привилегированный указатель стека `TSS.privilege_stack_table[0]` (RSP0).
///
/// Когда CPU принимает прерывание (например, тик вытеснения) или исполняет
/// `int 0x80` *в ring 3*, он переключает уровень привилегий и загружает
/// указатель стека ядра из RSP0 в TSS. Если RSP0 не задан (ноль), первый
/// переход ring-3 → ring-0 положит данные по адресу 0 и упадёт. Поэтому
/// ring-3 задача не может выполняться, пока RSP0 не указывает на валидный стек
/// ядра.
///
/// `rsp0` должен быть (исключающей) вершиной отображённого стека ядра,
/// выровненного на 16 байт. CPU перечитывает это поле из памяти TSS при каждом
/// переключении привилегий, поэтому обновление после `load_tss` (мы делаем это
/// для каждой ring-3 задачи в `task::process::create_user_process`) действует
/// немедленно.
///
/// ОГРАНИЧЕНИЕ (одна задача): слот RSP0 ровно один, поэтому дизайн поддерживает
/// только ОДНУ ring-3 задачу одновременно. При нескольких пользовательских
/// задачах RSP0 пришлось бы перенастраивать при каждом переключении в ring-3;
/// это вне рамок текущего однопользовательского запуска.
pub fn set_kernel_stack(rsp0: u64) {
    // SAFETY: инвариант «инициализация один раз, однопоточно, до прерываний» —
    // TSS принадлежит `SyncUnsafeCell` этого модуля и меняется только здесь и в
    // `init`, без конкурентного доступа. RSP0 пишется через сырой указатель
    // ячейки одной выровненной 64-битной записью; CPU перечитает его при
    // следующем переключении привилегий, поэтому запись через `.get()` (никогда
    // не `&mut` на static) действует сразу.
    unsafe {
        (*TSS.get()).privilege_stack_table[0] = VirtAddr::new(rsp0);
    }
}

// ─── init() ─────────────────────────────────────────────────────────────

/// Политика доступа к портам: в TSS нет I/O-битмапа (`iomap_base` указывает за
/// предел сегмента), поэтому при `IOPL=0` любой `in`/`out`/PCI-config из ring 3 —
/// это #GP. Так задумано как эшелонированная защита: исправление
/// `rt_sigreturn` вычищает IOPL из восстановленных RFLAGS, а отсутствие битмапа
/// означает, что даже будущий путь, поднявший IOPL, должен менять этот TSS,
/// чтобы получить порты.
///
/// Инициализировать GDT, TSS со стеками IST и загрузить их в CPU.
///
/// SAFETY: должен вызываться один раз при ранней загрузке, до включения
/// прерываний.
pub fn init() {
    // Вершины стеков IST. `.get()` даёт `*mut IstStack` на ячейку; приведение
    // указателя к числу даёт базу стека, а + 16384 — его (исключающую) вершину.
    // Ссылка на static не создаётся.
    let df_stack_top = IST1_DOUBLE_FAULT.get() as u64 + 16384;
    let pf_stack_top = IST2_PAGE_FAULT.get() as u64 + 16384;

    // SAFETY: инвариант «инициализация один раз, однопоточно, до прерываний» —
    // эксклюзивный доступ к TSS через сырой указатель ячейки; никакой другой код
    // не наблюдает и не меняет TSS во время ранней загрузки.
    unsafe {
        let tss = TSS.get();
        (*tss).interrupt_stack_table[IST_DOUBLE_FAULT as usize] = VirtAddr::new(df_stack_top);
        (*tss).interrupt_stack_table[IST_PAGE_FAULT as usize] = VirtAddr::new(pf_stack_top);
    }

    // Построить дескрипторы GDT.
    // SAFETY: инвариант «инициализация один раз, однопоточно, до прерываний» —
    // эксклюзивный доступ к GDT через сырой указатель ячейки. `&'static TSS`,
    // передаваемый в `Descriptor::tss_segment`, берётся из ячейки TSS, которая
    // полностью инициализирована выше и не меняется конкурентно; ссылка должна
    // жить только для дескриптора, а TSS живёт всю программу.
    unsafe {
        let gdt = &mut *GDT.get();
        KERNEL_CODE_SEL.store(
            gdt.append(Descriptor::kernel_code_segment()).0,
            Ordering::Relaxed,
        );
        KERNEL_DATA_SEL.store(
            gdt.append(Descriptor::kernel_data_segment()).0,
            Ordering::Relaxed,
        );
        // ПОРЯДОК ВАЖЕН: `sysretq` загружает CS.sel = STAR[63:48]+16 и
        // SS.sel = STAR[63:48]+8, не обращаясь к GDT, поэтому дескриптор DATA
        // пользователя обязан лежать ровно на 8 байт НИЖЕ дескриптора CODE
        // пользователя. При прежнем порядке [user_code, user_data] валидного
        // значения STAR не существует: sysret выдал бы ring 3 SS = kernel_data|3,
        // и первое же ring-3 прерывание упало бы с #GP на восстановлении этого SS
        // через iretq. От сырых значений селекторов ничего не зависит: все
        // пользователи идут через `Selectors`.
        USER_DATA_SEL.store(
            gdt.append(Descriptor::user_data_segment()).0,
            Ordering::Relaxed,
        );
        USER_CODE_SEL.store(
            gdt.append(Descriptor::user_code_segment()).0,
            Ordering::Relaxed,
        );
        let tss_ref: &'static TaskStateSegment = &*TSS.get();
        TSS_SEL.store(
            gdt.append(Descriptor::tss_segment(tss_ref)).0,
            Ordering::Relaxed,
        );
    }

    // Загрузить GDT и TSS.
    // SAFETY: инвариант «инициализация один раз, однопоточно, до прерываний».
    // GDT содержит валидные дескрипторы, построенные выше, TSS инициализирован;
    // доступ идёт через сырой указатель ячейки GDT. `&'static`, требуемый
    // `load()`, обеспечен: GDT живёт всю программу и после этой точки не
    // меняется. Перезагрузка селекторов использует только что сохранённые
    // атомарные значения.
    unsafe {
        let gdt: &'static GlobalDescriptorTable = &*GDT.get();
        gdt.load();
        // Перезагрузить сегмент кода дальним переходом
        core::arch::asm!(
            "push {sel}",
            "lea {tmp}, [2f + rip]",
            "push {tmp}",
            "retfq",
            "2:",
            sel = in(reg) KERNEL_CODE_SEL.load(Ordering::Relaxed) as u64,
            tmp = lateout(reg) _,
            options(preserves_flags),
        );
        // Перезагрузить сегменты данных — ВКЛЮЧАЯ SS.
        //
        // SS нужно перезагрузить здесь, а не только DS/ES/FS/GS. На входе CPU
        // работает с устаревшим селектором стека, загруженным загрузчиком
        // (Limine) — его 64-битный селектор данных оказался 0x30. После того как
        // `GDT.load()` подставит НАШУ GDT, селектор 0x30 — старшая половина
        // 16-байтного системного дескриптора TSS (индекс 6 GDT), то есть НЕ
        // валидный сегмент данных. Если оставить SS = 0x30, первый тик
        // вытеснения положит SS=0x30 во фрейм прерывания, и соответствующий
        // `iretq` (irq32_stub) перезагрузит его против нашей GDT, упав с
        // #GP e=0x30. Загрузка SS = KERNEL_DATA_SEL (0x10) делает текущий
        // селектор стека валидным present DPL0-сегментом данных, и путь
        // восстановления iret корректен (Requirements 11.1, 11.2). `mov ss`
        // естественно подавляет IRQ на следующей инструкции, поэтому RSP
        // остаётся согласованным на время загрузки.
        core::arch::asm!(
            "mov ds, {sel:x}",
            "mov es, {sel:x}",
            "mov fs, {sel:x}",
            "mov gs, {sel:x}",
            "mov ss, {sel:x}",
            sel = in(reg) KERNEL_DATA_SEL.load(Ordering::Relaxed),
            options(nomem, nostack),
        );
        // Загрузить TSS
        x86_64::instructions::tables::load_tss(SegmentSelector(TSS_SEL.load(Ordering::Relaxed)));
    }

    crate::debug!(
        "GDT loaded: kernel CS={:#x}, DS={:#x}, user CS={:#x}, DS={:#x}, TSS={:#x}",
        KERNEL_CODE_SEL.load(Ordering::Relaxed),
        KERNEL_DATA_SEL.load(Ordering::Relaxed),
        USER_CODE_SEL.load(Ordering::Relaxed),
        USER_DATA_SEL.load(Ordering::Relaxed),
        TSS_SEL.load(Ordering::Relaxed),
    );
}
