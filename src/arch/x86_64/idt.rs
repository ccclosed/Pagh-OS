// arch/x86_64/idt.rs — Interrupt Descriptor Table и обработчики исключений.

use core::cell::SyncUnsafeCell;
use x86_64::registers::control::Cr2;
use x86_64::registers::rflags::RFlags;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::VirtAddr;

use crate::arch::x86_64::linux::trap_frame::{
    exception_action, page_fault_action, PageFaultAction, TrapAction,
};

// IDT живёт в `SyncUnsafeCell`, а не в `static mut`, чтобы весь доступ шёл
// через сырой указатель `.get()`, никогда через ссылку на изменяемый static
// (это запрещает lint `static_mut_refs`). `InterruptDescriptorTable` не `Sync`,
// но `SyncUnsafeCell` даёт реализацию `Sync`, требуемую static; корректность
// держится на инварианте «инициализация один раз, однопоточно, до прерываний»,
// описанном в каждом месте доступа.
static IDT: SyncUnsafeCell<InterruptDescriptorTable> =
    SyncUnsafeCell::new(InterruptDescriptorTable::new());

pub fn init() {
    // SAFETY: `init` вызывается ровно один раз при ранней загрузке, на
    // bootstrap-CPU, с ещё выключенными прерываниями и до того, как IDT сможет
    // увидеть другой код. Никакой поток и обработчик прерываний не обращается к
    // `IDT`, пока мы держим этот `&mut`, поэтому построение таблицы через сырой
    // указатель ячейки свободно от алиасинга.
    let idt = unsafe { &mut *IDT.get() };

    idt.divide_error.set_handler_fn(divide_error_handler);
    idt.debug.set_handler_fn(debug_handler);
    idt.non_maskable_interrupt.set_handler_fn(nmi_handler);
    idt.breakpoint.set_handler_fn(breakpoint_handler);
    idt.overflow.set_handler_fn(overflow_handler);
    idt.bound_range_exceeded.set_handler_fn(bound_range_handler);
    idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
    idt.device_not_available
        .set_handler_fn(device_not_available_handler);
    idt.double_fault.set_handler_fn(double_fault_handler);
    idt.invalid_tss.set_handler_fn(invalid_tss_handler);
    idt.segment_not_present
        .set_handler_fn(segment_not_present_handler);
    idt.stack_segment_fault
        .set_handler_fn(stack_segment_handler);
    idt.general_protection_fault
        .set_handler_fn(gp_fault_handler);
    idt.page_fault.set_handler_fn(page_fault_handler);
    idt.x87_floating_point.set_handler_fn(x87_fpu_handler);
    idt.alignment_check.set_handler_fn(alignment_check_handler);
    idt.machine_check.set_handler_fn(machine_check_handler);
    idt.simd_floating_point.set_handler_fn(simd_fpu_handler);
    idt.virtualization.set_handler_fn(virtualization_handler);
    idt.hv_injection_exception
        .set_handler_fn(hv_injection_handler);
    idt.vmm_communication_exception
        .set_handler_fn(vmm_comm_handler);
    idt.security_exception.set_handler_fn(security_handler);

    // Вектор 32: собственный asm-стаб для вытесняющего переключения контекста.
    // SAFETY: адрес — точка входа naked-функции `irq32_stub`, реализующей полный
    // контракт пролога/эпилога прерывания для вектора 32.
    unsafe {
        idt[32].set_handler_addr(VirtAddr::new(
            crate::task::switch::irq32_stub as *const () as u64,
        ));
    }
    // Векторы 33–47: стандартные обработчики IRQ
    idt[33].set_handler_fn(irq33_handler);
    idt[34].set_handler_fn(irq34_handler);
    idt[35].set_handler_fn(irq35_handler);
    idt[36].set_handler_fn(irq36_handler);
    idt[37].set_handler_fn(irq37_handler);
    idt[38].set_handler_fn(irq38_handler);
    idt[39].set_handler_fn(irq39_handler);
    idt[40].set_handler_fn(irq40_handler);
    idt[41].set_handler_fn(irq41_handler);
    idt[42].set_handler_fn(irq42_handler);
    idt[43].set_handler_fn(irq43_handler);
    idt[44].set_handler_fn(irq44_handler);
    idt[45].set_handler_fn(irq45_handler);
    idt[46].set_handler_fn(irq46_handler);
    idt[47].set_handler_fn(irq47_handler);

    // Вектор 0x80: syscall-гейт, вызываемый из ring 3. DPL=3, чтобы
    // пользовательский код мог исполнить `int 0x80`; naked-стаб маршаллит
    // аргументы и диспетчеризует (см. `arch::x86_64::syscall::int80_stub`).
    // Программное прерывание для syscall переиспользует автоматическое
    // переключение стека через RSP0 и чистый возврат `iretq` в ring 3.
    // SAFETY: адрес — точка входа naked-функции `int80_stub`, соблюдающей
    // контракт вызова шлюза прерывания для вектора 0x80.
    unsafe {
        idt[0x80]
            .set_handler_addr(VirtAddr::new(
                crate::arch::x86_64::syscall::int80_stub as *const () as u64,
            ))
            .set_privilege_level(x86_64::PrivilegeLevel::Ring3);
    }

    // SAFETY: `load()` требует `&'static self`. Общая ссылка берётся из
    // указателя `'static`-ячейки и корректна, потому что таблица теперь
    // полностью инициализирована и больше не меняется — весь последующий доступ
    // при диспетчеризации прерываний только читающий. Инвариант инициализации
    // выше гарантирует отсутствие конкурентного `&mut`.
    unsafe { &*IDT.get() }.load();

    crate::debug!("IDT loaded: 32 exceptions + 16 IRQ (vec32=stub)");
}

// ─── Обработчики исключений ──────────────────────────────────────────────

/// Если true, обработчики исключений печатают сырой стек ядра (отладка).
pub static STACK_DUMP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

extern "x86-interrupt" fn divide_error_handler(stack: InterruptStackFrame) {
    crate::error!(
        "[EXC #0] Divide Error RIP: 0x{:016x}",
        stack.instruction_pointer.as_u64()
    );
    user_exception_or_halt(&stack, 0, SIGFPE, "SIGFPE");
}
extern "x86-interrupt" fn debug_handler(stack: InterruptStackFrame) {
    crate::error!(
        "[EXC #1] Debug RIP: 0x{:016x}",
        stack.instruction_pointer.as_u64()
    );
    // RFLAGS.TF записывается из ring 3 (`pushfq/orq $0x100/popfq`), поэтому #DB
    // с установленным trap-флагом — это вызванная userland'ом ловушка
    // пошагового выполнения, даже если сохранённый фрейм описывает ring 0
    // (пользователь поставил TF и сразу выдал syscall; `syscall::init` ещё и
    // маскирует TF в RFLAGS ядра, но оставляем эту классификацию как страховку).
    // Оба случая — ошибка пользователя, а не отказ ядра.
    user_exception_or_halt_flagged(&stack, 1, SIGTRAP, "SIGTRAP");
}
extern "x86-interrupt" fn nmi_handler(stack: InterruptStackFrame) {
    crate::error!(
        "[EXC #2] NMI RIP: 0x{:016x}",
        stack.instruction_pointer.as_u64()
    );
    halt();
}
extern "x86-interrupt" fn breakpoint_handler(stack: InterruptStackFrame) {
    crate::error!(
        "[EXC #3] *** BREAKPOINT HIT *** RIP: 0x{:016x}",
        stack.instruction_pointer.as_u64()
    );
    crate::error!("[EXC #3] IDT is working correctly!");
}
extern "x86-interrupt" fn overflow_handler(stack: InterruptStackFrame) {
    crate::error!(
        "[EXC #4] Overflow RIP: 0x{:016x}",
        stack.instruction_pointer.as_u64()
    );
    // Linux доставляет #OF (INTO/переполнение при into) как SIGSEGV, а не SIGFPE.
    user_exception_or_halt(&stack, 4, SIGSEGV, "SIGSEGV");
}
extern "x86-interrupt" fn bound_range_handler(stack: InterruptStackFrame) {
    crate::error!(
        "[EXC #5] Bound Range RIP: 0x{:016x}",
        stack.instruction_pointer.as_u64()
    );
    user_exception_or_halt(&stack, 5, SIGSEGV, "SIGSEGV");
}
extern "x86-interrupt" fn invalid_opcode_handler(stack: InterruptStackFrame) {
    crate::error!(
        "[EXC #6] Invalid Opcode RIP: 0x{:016x}",
        stack.instruction_pointer.as_u64()
    );
    user_exception_or_halt(&stack, 6, SIGILL, "SIGILL");
}
extern "x86-interrupt" fn device_not_available_handler(stack: InterruptStackFrame) {
    crate::error!(
        "[EXC #7] Device NA RIP: 0x{:016x}",
        stack.instruction_pointer.as_u64()
    );
    user_exception_or_halt(&stack, 7, SIGSEGV, "SIGSEGV");
}
extern "x86-interrupt" fn double_fault_handler(stack: InterruptStackFrame, error_code: u64) -> ! {
    crate::drivers::earlyfb::errorln(format_args!(
        "EXCEPTION #8 double fault err={:#x} RIP={:#x}",
        error_code,
        stack.instruction_pointer.as_u64()
    ));
    crate::error!(
        "[EXC #8] DOUBLE FAULT err=0x{:x} RIP=0x{:016x}",
        error_code,
        stack.instruction_pointer.as_u64()
    );
    crate::arch::cpu::halt_loop()
}
extern "x86-interrupt" fn invalid_tss_handler(_stack: InterruptStackFrame, error_code: u64) {
    crate::error!("[EXC #10] Invalid TSS err=0x{:x}", error_code);
    halt();
}
extern "x86-interrupt" fn segment_not_present_handler(
    _stack: InterruptStackFrame,
    error_code: u64,
) {
    crate::error!("[EXC #11] Segment NP err=0x{:x}", error_code);
    halt();
}
extern "x86-interrupt" fn stack_segment_handler(_stack: InterruptStackFrame, error_code: u64) {
    crate::error!("[EXC #12] Stack Fault err=0x{:x}", error_code);
    halt();
}
/// Последний syscall, замеченный `linux_dispatch`, для контекста постмортема
/// #GP (pid в высокоуровневом смысле, сырой номер syscall). Пишется при каждой
/// диспетчеризации.
pub static LAST_SYSCALL_PID: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(u64::MAX);
pub static LAST_SYSCALL_NR: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(u64::MAX);

/// Записать syscall, который задача собирается выполнить (зовётся из `linux_dispatch`).
pub fn note_syscall(pid: u64, nr: u64) {
    LAST_SYSCALL_PID.store(pid, core::sync::atomic::Ordering::Relaxed);
    LAST_SYSCALL_NR.store(nr, core::sync::atomic::Ordering::Relaxed);
}

extern "x86-interrupt" fn gp_fault_handler(stack: InterruptStackFrame, error_code: u64) {
    crate::drivers::earlyfb::errorln(format_args!(
        "EXCEPTION #13 general protection err={:#x} RIP={:#x}",
        error_code,
        stack.instruction_pointer.as_u64()
    ));
    crate::error!(
        "[EXC #13] GP Fault err=0x{:x} RIP=0x{:016x} RSP=0x{:016x}",
        error_code,
        stack.instruction_pointer.as_u64(),
        stack.stack_pointer.as_u64()
    );
    let pid = crate::task::scheduler::current_pid();
    crate::error!(
        "[EXC #13] current pid={} last_dispatch: pid={} nr={}",
        pid,
        LAST_SYSCALL_PID.load(core::sync::atomic::Ordering::Relaxed),
        LAST_SYSCALL_NR.load(core::sync::atomic::Ordering::Relaxed)
    );
    // Для отказов на выходе iretq/sysretq: напечатать слова, которые
    // потребляла инструкция возврата (по RSP), и область GPR, снятую перед ней.
    let frsp = stack.stack_pointer.as_u64();
    if frsp >= 0xffff_8000_0000_0000 {
        crate::error!("--- Words around fault RSP 0x{:x} ---", frsp);
        let mut off: i64 = -0x48;
        while off <= 0x28 {
            let addr = (frsp as i64 + off) as u64;
            let sign = if off < 0 { "-" } else { "+" };
            // Никогда не трогать неотображённую память из обработчика #GP:
            // RSP на момент отказа может стоять ровно на вершине стека, и чтение
            // дальше каскадируется в page fault на guard-странице.
            if crate::memory::vmm::virt_to_phys(addr).is_none() {
                crate::error!("  [rsp{}0x{:02x}] <unmapped>", sign, off.unsigned_abs());
            } else {
                let val = unsafe { core::ptr::read_volatile(addr as *const u64) };
                crate::error!("  [rsp{}0x{:02x}] 0x{:016x}", sign, off.unsigned_abs(), val);
            }
            off += 8;
        }
    }
    // Постмортем для отказов на выходе из syscall (iretq/sysretq в стабах): к
    // моменту отказа хвоста выхода оконный фрейм уже снят, но его байты целы на
    // вершине стека ядра задачи. Напечатать их:
    //   [top-0x08] слот user-RSP задачи   [top-0x10] rax  [top-0x18] rbx
    //   [top-0x20] rcx (user RIP для sysretq!)  [top-0x60] r11 (user RFLAGS) ...
    // Полный 24-словный дамп — за флагом (он страшный и шумный для обычного
    // падения userspace); включать позже инструментами вроде `warn on`.
    if !crate::task::scheduler::is_idle(pid)
        && STACK_DUMP.load(core::sync::atomic::Ordering::Relaxed)
    {
        let (_guard, _base, top) = crate::memory::layout::kernel_stack_for_pid(pid);
        crate::error!(
            "--- Top 24 words of kernel stack for pid {} (top=0x{:x}) ---",
            pid,
            top
        );
        let mut i = 24u64;
        while i >= 1 {
            let addr = top - i * 8;
            let val = unsafe { core::ptr::read_volatile(addr as *const u64) };
            crate::error!("  [top-0x{:02x}] 0x{:016x}", i * 8, val);
            i -= 1;
        }
    }
    // #GP, полученный при исполнении кода ring 3, — ошибка пользовательской
    // программы (или нашего compat-слоя), а не ядра. Убить только виновную
    // задачу (код выхода 139 = 128+SIGSEGV), оставив ядро и шелл работать;
    // раньше это останавливало всю машину.
    //
    // MVP S4b: решение принимает та же чистая политика, что у остальных
    // синхронных исключений. Ring-3 отказ убивает задачу и БЕЗ
    // compat-состояния — прежняя проверка `compat_exists_for_exit` отправляла
    // нативную ring-3 задачу в `halt()`; отказ ring 0 остаётся фатальным, кроме
    // заслона «ring 0 на пользовательском RIP у Compat_Process».
    let action = exception_action(
        (stack.code_segment.0 & 3) == 3,
        stack.instruction_pointer.as_u64(),
        crate::arch::x86_64::linux::validate::USER_ADDR_MAX,
        crate::task::compat::compat_exists_for_exit(pid),
    );
    if action == TrapAction::KillProcess {
        crate::task::compat::abandon_held_registry();
        crate::task::compat::with_current_compat(|cs| cs.exit_code = Some(139));
        crate::error!(
            "[EXC #13] killing task pid={} (SIGSEGV) - kernel keeps running",
            pid
        );
        crate::task::scheduler::exit_current();
    }
    halt();
}
extern "x86-interrupt" fn page_fault_handler(
    stack: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    let fault_addr = Cr2::read().unwrap_or(VirtAddr::new(0));
    let rsp = stack.stack_pointer.as_u64();

    // Сначала demand paging: fault not-present на лениво отображённой странице
    // пользователя (анонимный mmap-регион или куча brk) обслуживается
    // отображением нулевого кадра; инструкция с отказом затем повторяется на
    // IRET. Покрывает И отказы ring 3, И отказы режима ядра на пользовательских
    // адресах (copy_in/copy_out по нетронутому буферу). Обработчик отвергает
    // нарушения защиты, адреса ядра, PROT_NONE-регионы и eager-регионы, поэтому
    // всё ниже остаётся настоящим отказом.
    if crate::arch::x86_64::linux::mem_sys::handle_user_page_fault(
        fault_addr.as_u64(),
        error_code.contains(PageFaultErrorCode::PROTECTION_VIOLATION),
        error_code.contains(PageFaultErrorCode::CAUSED_BY_WRITE),
        error_code.contains(PageFaultErrorCode::USER_MODE),
    ) {
        return;
    }

    // На машине без serial-порта экран — единственный свидетель отказа, а отказ
    // выглядит ровно как «загрузка встала после шага N»: обработчик писал в COM1
    // и останавливался. Сообщаем и сюда.
    crate::drivers::earlyfb::errorln(format_args!(
        "EXCEPTION #14 page fault addr={:#x} RIP={:#x}",
        fault_addr.as_u64(),
        stack.instruction_pointer.as_u64()
    ));
    crate::error!(
        "[EXC #14] PAGE FAULT addr=0x{:016x} RIP=0x{:016x} RSP=0x{:016x} P={} W={} U={} I={} ec=0x{:x}",
        fault_addr.as_u64(),
        stack.instruction_pointer.as_u64(),
        rsp,
        error_code.contains(PageFaultErrorCode::PROTECTION_VIOLATION),
        error_code.contains(PageFaultErrorCode::CAUSED_BY_WRITE),
        error_code.contains(PageFaultErrorCode::USER_MODE),
        error_code.contains(PageFaultErrorCode::INSTRUCTION_FETCH),
        error_code.bits(),
    );
    // При отказе в режиме ядра RIP/RBP могут быть мусором (порча потока
    // управления, например RIP=0x1), поэтому печатаем свободное от кучи
    // сканирование стека с адресами возврата в образе ядра — это восстанавливает
    // цепочку вызовов, живую на момент отказа. Затем паркуемся (не возвращаемся:
    // возврат просто повторил бы инструкцию с отказом и зациклился).
    if !error_code.contains(PageFaultErrorCode::USER_MODE) {
        // Заслон: отказ ring 0 на ПОЛЬЗОВАТЕЛЬСКОМ адресе означает, что
        // обработчик syscall читал или писал пользовательский буфер, который
        // соседний поток CLONE_VM снял после проверки на входе. Ring 3 мог бы
        // вызывать это по своему желанию, и раньше это останавливало всю машину;
        // вместо этого убиваем только виновный Compat_Process. Решение чистое и
        // host-тестируемое (`trap_frame::page_fault_action`): отказ ring 0 на
        // адресе ЯДРА по-прежнему идёт фатальным путём ниже, поэтому настоящая
        // ошибка ядра не спрячется за этим заслоном.
        let pid = crate::task::scheduler::current_pid();
        // F-r4-arch-1: kill-путь не имеет права повторно входить в реестр.
        // `compat_exists_for_exit` отвечает без лока, если текущее замыкание
        // `with_current_compat` удерживает COMPAT_STATES; `abandon_held_registry`
        // аварийно отпускает лок, чтобы exit-путь снял состояние процесса и
        // машина осталась жива (вечный спин на нереентерабельном спинлоке с
        // выключенными прерываниями — это отказ всей машины, а не процесса).
        let compat_here = crate::task::compat::compat_exists_for_exit(pid);
        let action = crate::arch::x86_64::linux::trap_frame::page_fault_action(
            false,
            fault_addr.as_u64(),
            crate::arch::x86_64::linux::validate::USER_ADDR_MAX,
            compat_here,
        );
        if action == crate::arch::x86_64::linux::trap_frame::PageFaultAction::KillProcess {
            crate::error!(
                "[EXC #14] kernel-mode fault on USER address addr=0x{:016x} RIP=0x{:016x} pid={} — killing that Compat_Process, machine stays up (F-x86a-4 backstop)",
                fault_addr.as_u64(),
                stack.instruction_pointer.as_u64(),
                pid
            );
            crate::task::compat::abandon_held_registry();
            crate::task::compat::with_current_compat(|cs| cs.exit_code = Some(139));
            crate::task::scheduler::exit_current();
        }
        crate::debug::unwind::stack_scan_backtrace(rsp, 8192);
        crate::arch::cpu::halt_loop();
    }
    // Page fault ring 3 не должен ронять машину: сообщить одну краткую строку
    // SIGSEGV и убить только виновную задачу.
    //
    // MVP S4b: убивать обязан ЛЮБОЙ ring-3 отказ, а не только отказ
    // Compat_Process. Клиент в userspace без compat-состояния (нативная ring-3
    // задача pagh до `install_compat`, любой будущий non-Linux userspace) раньше
    // проходил мимо `compat_exists_for_exit` в `halt()` и уносил машину. Решение
    // принимает чистая host-тестируемая политика `page_fault_action(.., true, ..)`:
    // ring 3 — пользовательский код, поэтому виновная задача есть всегда.
    let pid = crate::task::scheduler::current_pid();
    crate::error!(
        "[EXC #14] SIGSEGV pid={} addr=0x{:016x} RIP=0x{:016x}",
        pid,
        fault_addr.as_u64(),
        stack.instruction_pointer.as_u64()
    );
    let action = page_fault_action(
        error_code.contains(PageFaultErrorCode::USER_MODE),
        fault_addr.as_u64(),
        crate::arch::x86_64::linux::validate::USER_ADDR_MAX,
        crate::task::compat::compat_exists_for_exit(pid),
    );
    if action == PageFaultAction::KillProcess {
        // F-r4-arch-1: не входить в реестр повторно; аварийно отпустить лок,
        // который мог удерживать прерванный `with_current_compat`.
        crate::task::compat::abandon_held_registry();
        crate::task::compat::with_current_compat(|cs| cs.exit_code = Some(139));
        crate::error!(
            "[EXC #14] killing task pid={} (SIGSEGV) - kernel keeps running",
            pid
        );
        crate::task::scheduler::exit_current();
    }
    halt();
}
extern "x86-interrupt" fn x87_fpu_handler(stack: InterruptStackFrame) {
    crate::error!(
        "[EXC #16] x87 FPU RIP=0x{:016x}",
        stack.instruction_pointer.as_u64()
    );
    user_exception_or_halt(&stack, 16, SIGFPE, "SIGFPE");
}
extern "x86-interrupt" fn alignment_check_handler(stack: InterruptStackFrame, error_code: u64) {
    crate::error!(
        "[EXC #17] Alignment err=0x{:x} RIP: 0x{:016x}",
        error_code,
        stack.instruction_pointer.as_u64()
    );
    // Пользовательский код может сам поставить RFLAGS.AC; Linux сообщает #AC как SIGBUS.
    user_exception_or_halt(&stack, 17, SIGBUS, "SIGBUS");
}
extern "x86-interrupt" fn machine_check_handler(_stack: InterruptStackFrame) -> ! {
    crate::error!("[EXC #18] Machine Check — halting");
    crate::arch::cpu::halt_loop()
}
extern "x86-interrupt" fn simd_fpu_handler(stack: InterruptStackFrame) {
    crate::error!(
        "[EXC #19] SIMD FPU RIP=0x{:016x}",
        stack.instruction_pointer.as_u64()
    );
    user_exception_or_halt(&stack, 19, SIGFPE, "SIGFPE");
}
extern "x86-interrupt" fn virtualization_handler(stack: InterruptStackFrame) {
    crate::error!(
        "[EXC #20] Virt RIP=0x{:016x}",
        stack.instruction_pointer.as_u64()
    );
    halt();
}
extern "x86-interrupt" fn hv_injection_handler(stack: InterruptStackFrame) {
    crate::error!(
        "[EXC #28] HV Injection RIP=0x{:016x}",
        stack.instruction_pointer.as_u64()
    );
    halt();
}
extern "x86-interrupt" fn vmm_comm_handler(_stack: InterruptStackFrame, error_code: u64) {
    crate::error!("[EXC #29] VMM Comm err=0x{:x}", error_code);
    halt();
}
extern "x86-interrupt" fn security_handler(_stack: InterruptStackFrame, error_code: u64) {
    crate::error!("[EXC #30] Security err=0x{:x}", error_code);
    halt();
}

// ─── Обработчики IRQ (векторы 33–47) ────────────────────────────────────

macro_rules! irq_handler {
    ($name:ident, $vec:expr) => {
        extern "x86-interrupt" fn $name(_stack: InterruptStackFrame) {
            crate::arch::x86_64::apic::irq_dispatch($vec);
            crate::arch::x86_64::apic::send_eoi();
        }
    };
}

irq_handler!(irq33_handler, 33);
irq_handler!(irq34_handler, 34);
irq_handler!(irq35_handler, 35);
irq_handler!(irq36_handler, 36);
irq_handler!(irq37_handler, 37);
irq_handler!(irq38_handler, 38);
irq_handler!(irq39_handler, 39);
irq_handler!(irq40_handler, 40);
irq_handler!(irq41_handler, 41);
irq_handler!(irq42_handler, 42);
irq_handler!(irq43_handler, 43);
irq_handler!(irq44_handler, 44);
irq_handler!(irq45_handler, 45);
irq_handler!(irq46_handler, 46);
irq_handler!(irq47_handler, 47);

// Номера сигналов Linux для политики пользовательских исключений берутся из
// единой compat-таблицы, чтобы будущая перенумерация не развела их.
const SIGILL: u8 = crate::arch::x86_64::linux::signal_frame::SIGILL as u8;
const SIGTRAP: u8 = crate::arch::x86_64::linux::signal_frame::SIGTRAP as u8;
const SIGBUS: u8 = crate::arch::x86_64::linux::signal_frame::SIGBUS as u8;
const SIGFPE: u8 = crate::arch::x86_64::linux::signal_frame::SIGFPE as u8;
const SIGSEGV: u8 = crate::arch::x86_64::linux::signal_frame::SIGSEGV as u8;

/// Диспозиция синхронного исключения CPU без собственного обработчика ядра
/// (#DE/#DB/#UD/#OF/#BR/#NM/#AC/#MF/#XM).
///
/// Одна инструкция ring 3 (`1/0`, `ud2`, `popfq` с TF) раньше парковала всю
/// машину в `halt()`: политика «пользовательский отказ убивает процесс, ядро
/// живёт» была только у #GP и #PF. Этот хелпер применяет тот же контракт
/// (`trap_frame::exception_action`) ко всем таким векторам: Compat_Process,
/// найденный в ring 3 (или с отказом на user RIP), получает
/// `exit_code = 128 + signal` и завершается; всё остальное по-прежнему
/// останавливается, чтобы настоящая ошибка ring 0 не спряталась за политикой.
fn user_exception_or_halt(
    stack: &InterruptStackFrame,
    vector: u8,
    signal: u8,
    signal_name: &str,
) -> ! {
    user_exception_or_halt_inner(stack, vector, signal, signal_name, false)
}
/// [`user_exception_or_halt`] для #DB: сохранённый RFLAGS.TF тоже помечает
/// ловушку как вызванную пользователем, даже если сохранённый фрейм — ring 0
/// (см. `debug_handler`).
fn user_exception_or_halt_flagged(
    stack: &InterruptStackFrame,
    vector: u8,
    signal: u8,
    signal_name: &str,
) -> ! {
    let user_tf = stack.cpu_flags.contains(RFlags::TRAP_FLAG);
    user_exception_or_halt_inner(stack, vector, signal, signal_name, user_tf)
}

fn user_exception_or_halt_inner(
    stack: &InterruptStackFrame,
    vector: u8,
    signal: u8,
    signal_name: &str,
    force_user: bool,
) -> ! {
    let rip = stack.instruction_pointer.as_u64();
    let user_mode = force_user || (stack.code_segment.0 & 3) == 3;
    let pid = crate::task::scheduler::current_pid();
    // F-r4-arch-1: тот же нереентерабельный реестр, что и в #PF. Отказ внутри
    // `with_current_compat` (kernel store в RO user-страницу — например
    // sigreturn-путь) раньше звал `compat_exists` и вешал CPU вторичным
    // `lock()`; теперь решение принимается без лока, а удерживаемый лок
    // аварийно отпускается перед завершением задачи.
    let action = exception_action(
        user_mode,
        rip,
        crate::arch::x86_64::linux::validate::USER_ADDR_MAX,
        crate::task::compat::compat_exists_for_exit(pid),
    );
    if action == TrapAction::KillProcess {
        crate::task::compat::abandon_held_registry();
        crate::task::compat::with_current_compat(|cs| cs.exit_code = Some(128 + signal));
        crate::error!(
            "[EXC #{}] killing Compat_Process pid={} ({}) - kernel keeps running",
            vector,
            pid,
            signal_name
        );
        crate::task::scheduler::exit_current();
    }
    // Сюда доходит только отказ ring 0: ring-3 вектор убивает задачу всегда
    // (политика `exception_action` выше), поэтому оставшийся случай — отказ
    // ring 0 на ядерном RIP или без compat-состояния, чтобы кого-то винить.
    // Это настоящий отказ ядра/железа; обработчик уже напечатал вектор и RIP.
    halt();
}

fn halt() -> ! {
    crate::arch::cpu::halt_loop()
}
