// arch/x86_64/apic.rs — подсистема APIC: таймер LAPIC, I/O APIC, диспетчеризация IRQ.

use crate::arch::x86_64::acpi::DEFAULT_LAPIC_PHYS;
use crate::sync::spinlock::Spinlock;
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::port::Port;

const LAPIC_EOI: u32 = 0x0B0;
const LAPIC_SPURIOUS: u32 = 0x0F0;
const LAPIC_LVT_TIMER: u32 = 0x320;
const LAPIC_TIMER_INIT: u32 = 0x380;
const LAPIC_TIMER_CUR: u32 = 0x390;
const LAPIC_TIMER_DIV: u32 = 0x3E0;

const SPURIOUS_ENABLE: u32 = 0x100;
const SPURIOUS_VECTOR: u32 = 0xFF;

const IOAPIC_REG_SEL: u32 = 0x00;
const IOAPIC_REG_WIN: u32 = 0x10;
const IOAPIC_REDTBL_BASE: u32 = 0x10;

static LAPIC_BASE: AtomicU64 = AtomicU64::new(0);
static IOAPIC_BASE: AtomicU64 = AtomicU64::new(0);

static IRQ_HANDLERS: Spinlock<[Option<fn()>; 224]> = Spinlock::new([None; 224]);

fn disable_pic() {
    let mut cmd_master: Port<u8> = Port::new(0x20);
    let mut data_master: Port<u8> = Port::new(0x21);
    let mut cmd_slave: Port<u8> = Port::new(0xA0);
    let mut data_slave: Port<u8> = Port::new(0xA1);

    // SAFETY: port I/O к стандартным адресам PIC.
    unsafe {
        cmd_master.write(0x11);
        cmd_slave.write(0x11);
        data_master.write(0x20);
        data_slave.write(0x28);
        data_master.write(0x04);
        data_slave.write(0x02);
        data_master.write(0x01);
        data_slave.write(0x01);
        data_master.write(0xFF);
        data_slave.write(0xFF);
    }
    crate::debug!("PIC 8259 disabled");
}

unsafe fn lapic_write(reg: u32, value: u32) {
    // LAPIC_BASE хранит уже HHDM-отображённую виртуальную базу: `init_apic`
    // сохраняет то, что вернул `vmm::map_mmio`, поэтому HHDM здесь добавлять НЕЛЬЗЯ.
    let addr = (LAPIC_BASE.load(Ordering::Relaxed) + reg as u64) as *mut u32;
    // SAFETY: LAPIC_BASE хранит HHDM-отображённую виртуальную базу MMIO LAPIC,
    // установленную один раз в `init` до любого доступа к регистрам; `addr` —
    // валидный отображённый адрес памяти устройства, и volatile-запись попадает
    // в реальный регистр LAPIC.
    ptr::write_volatile(addr, value);
}

unsafe fn lapic_read(reg: u32) -> u32 {
    // LAPIC_BASE хранит уже HHDM-отображённую виртуальную базу (см. `lapic_write`).
    let addr = (LAPIC_BASE.load(Ordering::Relaxed) + reg as u64) as *const u32;
    // SAFETY: LAPIC_BASE хранит HHDM-отображённую виртуальную базу MMIO LAPIC,
    // установленную один раз в `init` до любого доступа к регистрам; `addr` —
    // валидный отображённый адрес памяти устройства, и volatile-чтение идёт из
    // реального регистра LAPIC.
    ptr::read_volatile(addr)
}

unsafe fn ioapic_write(reg: u32, value: u32) {
    // IOAPIC_BASE должен быть виртуальным (HHDM-отображённым), если ненулевой
    let base = IOAPIC_BASE.load(Ordering::Relaxed);
    let sel = (base + IOAPIC_REG_SEL as u64) as *mut u32;
    let win = (base + IOAPIC_REG_WIN as u64) as *mut u32;
    // SAFETY: вызывающие приходят сюда только когда IOAPIC_BASE установлена в
    // ненулевую HHDM-отображённую базу MMIO I/O APIC; поэтому `sel`/`win`
    // указывают на валидную пару регистров index/data, и volatile-записи идут в
    // реальные регистры.
    ptr::write_volatile(sel, reg);
    ptr::write_volatile(win, value);
}

unsafe fn ioapic_read(reg: u32) -> u32 {
    // IOAPIC_BASE должен быть виртуальным (HHDM-отображённым), если ненулевой
    let base = IOAPIC_BASE.load(Ordering::Relaxed);
    let sel = (base + IOAPIC_REG_SEL as u64) as *mut u32;
    let win = (base + IOAPIC_REG_WIN as u64) as *mut u32;
    // SAFETY: вызывающие приходят сюда только когда IOAPIC_BASE установлена в
    // ненулевую HHDM-отображённую базу MMIO I/O APIC; поэтому `sel`/`win`
    // указывают на валидную пару регистров index/data, и volatile-доступы идут в
    // реальные регистры.
    ptr::write_volatile(sel, reg);
    ptr::read_volatile(win)
}

fn init_lapic() {
    // При необходимости включить LAPIC через MSR (IA32_APIC_BASE, MSR 0x1B).
    // read_msr безопасен (нет эффектов на память); запись ниже остаётся unsafe.
    let mut apic_base_msr = crate::arch::cpu::read_msr(0x1B);
    crate::debug!("LAPIC MSR before: 0x{:016x}", apic_base_msr);

    // x2APIC (бит 10) уводит LAPIC за MSR и отбирает окно регистров,
    // отображённое в память. Прошивка современных машин может передать
    // управление уже с ним, а этот драйвер — memory-mapped: каждая последующая
    // `lapic_write` шла бы в окно, которое CPU больше не обслуживает, то есть
    // загрузка вставала бы здесь без вывода на машине с включённым прошивкой
    // x2APIC. Выключаем его (архитектурный путь назад к xAPIC) и сообщаем об
    // этом, вместо чтения регистров, которых больше нет.
    if apic_base_msr & (1 << 10) != 0 {
        crate::warn!("LAPIC: x2APIC was enabled by firmware; switching to xAPIC (MMIO)");
        apic_base_msr &= !(1 << 10);
        crate::drivers::earlyfb::println(format_args!(
            "note: x2APIC was on, switched to xAPIC MMIO"
        ));
    }

    // Установить бит 11 (глобальное включение)
    apic_base_msr |= 1 << 11;

    // SAFETY: установка бита глобального включения IA32_APIC_BASE (бит 11) с
    // сохранением существующего базового адреса — намеренная корректная
    // реконфигурация LAPIC; мы делаем read-modify-write того же MSR и не
    // программируем новый адрес, поэтому LAPIC остаётся на текущем (уже
    // отображённом) месте.
    unsafe {
        crate::arch::cpu::write_msr(0x1B, apic_base_msr);
    }

    crate::debug!("LAPIC MSR after: 0x{:016x} (enabled)", apic_base_msr);

    // SAFETY: LAPIC_BASE указывает на HHDM-отображённый регион MMIO LAPIC;
    // volatile-доступы к регистрам ниже идут в валидную отображённую память
    // устройства.
    unsafe {
        // Настроить регистры LAPIC
        let spurious = SPURIOUS_VECTOR | SPURIOUS_ENABLE;
        lapic_write(LAPIC_SPURIOUS, spurious);
        let spurious_read = lapic_read(LAPIC_SPURIOUS);
        crate::debug!(
            "Spurious reg: wrote=0x{:x} read=0x{:x}",
            spurious,
            spurious_read
        );

        lapic_write(LAPIC_TIMER_DIV, 3); // Деление на 16

        // ИЗМЕРИТЬ таймер, а не доверять константе.
        //
        // Таймер LAPIC считает на частоте шины платформы, делённой на
        // настроенный делитель — 1 ГГц / 16 под QEMU, десятки МГц на реальном
        // железе. Программирование по константе QEMU делало тик на реальной
        // машине ~1/60 мс: часы шелла уходили на секунду за минуту, а все
        // таймауты были в 60 раз длиннее. PIT — единственные часы, не зависящие
        // от CPU, поэтому он эталон: считаем тики LAPIC за известный интервал PIT.
        //
        // Измерение делается С запрограммированным делителем, то есть это
        // скорость самого счётчика. Повторное деление на делитель (первая
        // версия кода) делало каждый тик в 16 раз длиннее.
        let measured = calibrate_timer_hz();
        let effective_hz = measured.unwrap_or(APIC_TIMER_BASE_HZ / APIC_TIMER_DIVIDER);
        let init_count = (effective_hz / TICK_HZ).clamp(1, u32::MAX as u64);
        match measured {
            Some(hz) => crate::drivers::earlyfb::println(format_args!(
                "lapic: {} Hz effective (PIT), {} counts/tick",
                hz, init_count
            )),
            None => crate::warn!(
                "LAPIC: PIT calibration failed; assuming {} Hz effective",
                effective_hz
            ),
        }
        crate::info!(
            "LAPIC timer: {} Hz effective, divisor {}, {} counts per {} Hz tick{}",
            effective_hz,
            APIC_TIMER_DIVIDER,
            init_count,
            TICK_HZ,
            if measured.is_some() {
                " (measured)"
            } else {
                " (assumed)"
            }
        );
        lapic_write(LAPIC_TIMER_INIT, init_count as u32);

        let timer_cfg = 32 | (1 << 17); // Вектор 32, периодический режим
        lapic_write(LAPIC_LVT_TIMER, timer_cfg);
        let timer_read = lapic_read(LAPIC_LVT_TIMER);
        crate::debug!("Timer LVT: wrote=0x{:x} read=0x{:x}", timer_cfg, timer_read);

        // Проверить, не замаскирован ли таймер
        if (timer_read & (1 << 16)) != 0 {
            crate::warn!("Timer is MASKED! Unmasking...");
            lapic_write(LAPIC_LVT_TIMER, timer_cfg & !(1 << 16));
        }

        lapic_write(LAPIC_EOI, 0);
    }
    crate::debug!("LAPIC fully configured (vector 32, {} Hz)", TICK_HZ);
}

/// Измерить частоту счёта таймера LAPIC в Гц, используя PIT как эталонные часы.
///
/// КАК. Канал 2 PIT ставится в режим 0 (interrupt on terminal count) со счётом
/// на ~10 мс; таймер LAPIC запускается с максимума, и при переходе выхода PIT
/// в высокий уровень значения сравниваются. Прошедшее значение LAPIC — число
/// тиков LAPIC за 10 мс, то есть частота с настроенным делителем.
///
/// ПОЧЕМУ PIT. Он идёт от собственного кварца на фиксированных 1.193182 МГц и
/// не зависит от CPU, частоты шины или таблиц ACPI — именно это нужно, чтобы
/// измерить часы с неизвестной частотой. Чтение TSC просто сравнивало бы одну
/// неизвестную с другой.
///
/// `None`, если PIT так и не сообщил о завершении (нет PIT или прошивка
/// перехватывает порты 0x42/0x43) — тогда вызывающий оставляет свою константу.
unsafe fn calibrate_timer_hz() -> Option<u64> {
    use x86_64::instructions::port::Port;

    const PIT_CHANNEL2: u16 = 0x42;
    const PIT_COMMAND: u16 = 0x43;
    const PIT_FREQ: u64 = 1_193_182;
    /// ~10 мс.
    const PIT_COUNT: u16 = 11_932;

    let mut cmd: Port<u8> = Port::new(PIT_COMMAND);
    let mut ch2: Port<u8> = Port::new(PIT_CHANNEL2);
    let mut gate: Port<u8> = Port::new(0x61);

    // SAFETY: порты 0x42/0x43/0x61 — стандартные порты PIT/динамика; записи
    // программируют канал 2 как одноразовый счётчик с выключенным динамиком.
    unsafe {
        // Динамик выключен, gate канала 2 включён (бит 0), счёт разрешён.
        let p61 = gate.read();
        gate.write((p61 & !0x02) | 0x01);
        // Канал 2, lobyte/hibyte, режим 0 (terminal count), двоичный.
        cmd.write(0b1011_0000);
        ch2.write((PIT_COUNT & 0xFF) as u8);
        ch2.write((PIT_COUNT >> 8) as u8);

        // Запустить таймер LAPIC с максимума, чтобы прошедшее значение было
        // счётом, помещающимся в окно.
        lapic_write(LAPIC_TIMER_DIV, 3); // деление на 16, как в реальной конфигурации
        lapic_write(LAPIC_TIMER_INIT, 0xFFFF_FFFF);

        // Ждать перехода выхода PIT в высокий уровень: бит 5 порта 0x61 —
        // состояние выхода канала 2, ставится при достижении нуля счёта.
        let mut spins: u32 = 0;
        while gate.read() & 0x20 == 0 {
            spins += 1;
            if spins > 50_000_000 {
                // Восстановить gate и сдаться, а не вешать загрузку.
                gate.write(p61);
                return None;
            }
            core::hint::spin_loop();
        }

        let elapsed = 0xFFFF_FFFFu32.wrapping_sub(lapic_read(LAPIC_TIMER_CUR));
        gate.write(p61);
        // Остановить таймер до программирования реальной конфигурации.
        lapic_write(LAPIC_TIMER_INIT, 0);

        if elapsed == 0 || elapsed == 0xFFFF_FFFF {
            return None;
        }
        // прошедшие счёты за (PIT_COUNT / PIT_FREQ) секунд.
        let hz = (elapsed as u64 * PIT_FREQ) / PIT_COUNT as u64;
        // Правдоподобная частота LAPIC — от 1 МГц до 4 ГГц; всё прочее означает
        // ошибочное измерение, и вызывающий должен оставить значение по умолчанию.
        if (1_000_000..=4_000_000_000).contains(&hz) {
            Some(hz)
        } else {
            None
        }
    }
}

/// Частота тика планировщика (периодический таймер LAPIC). От этой константы
/// выводится каждое преобразование тиков в реальное время в ядре:
/// гранулярность `clock_gettime`/`times`, монотонные часы сетевого стека и
/// сетевые таймауты в тиках.
///
/// Практический потолок: каждый тик стоит одну доставку IRQ (~тысячи циклов),
/// поэтому частоты выше нескольких кГц морят всё остальное; 1000 Гц совпадает с
/// классической настройкой Linux `CONFIG_HZ=1000`. Выше — только
/// one-shot/TSC-deadline (tickless) таймер вместо периодического режима.
pub const TICK_HZ: u64 = 1000;

/// Частота шины платформы, тактирующей таймер LAPIC под QEMU (`-cpu max`: 1 ГГц).
const APIC_TIMER_BASE_HZ: u64 = 1_000_000_000;

/// Делитель таймера LAPIC, настроенный в `init_apic` (значение записи 3 = деление на 16).
const APIC_TIMER_DIVIDER: u64 = 16;

/// Перевести миллисекунды в тики планировщика с округлением вверх.
pub const fn ms_to_ticks(ms: u64) -> u64 {
    (ms * TICK_HZ + 999) / 1000
}

fn init_ioapic() {
    if IOAPIC_BASE.load(Ordering::Relaxed) == 0 {
        return;
    }
    // SAFETY: здесь IOAPIC_BASE ненулевая, то есть хранит HHDM-отображённую базу
    // MMIO I/O APIC; хелперы ioapic_read/write трогают только валидные
    // отображённые регистры.
    unsafe {
        let ver = ioapic_read(0x01);
        let max_redir = ((ver >> 16) & 0xFF) + 1;
        crate::debug!("I/O APIC: {} redir entries", max_redir);
        for i in 0..max_redir {
            ioapic_write(IOAPIC_REDTBL_BASE + i * 2 + 1, 1 << 16);
        }
    }
}

pub fn init() {
    crate::debug!("Initializing...");
    disable_pic();
    let addrs = crate::arch::x86_64::acpi::apic_addresses();
    let lapic_phys = addrs.lapic_phys;
    let ioapic_phys = addrs.ioapic_phys;

    // APIC владеет собственным MMIO-отображением (Requirement 7.4): создать
    // отображения LAPIC/IOAPIC здесь через `map_mmio` из VMM до любого доступа к
    // регистрам. `map_mmio` отображает NO_CACHE|NO_EXECUTE|PRESENT|WRITABLE через
    // окно HHDM и возвращает виртуальную базу, равную `phys + hhdm` — тому же
    // значению, которое раньше считалось вручную, — поэтому LAPIC_BASE/
    // IOAPIC_BASE не меняются.
    // Сказать, что сообщила прошивка, ДО обращения к ней: на реальной машине
    // единственное свидетельство плохой записи MADT — сам адрес, а загрузка
    // умирает раньше, чем что-то ещё успеет его напечатать.
    crate::drivers::earlyfb::println(format_args!(
        "acpi: lapic_phys={:#x} ioapic_phys={:#x}",
        lapic_phys, ioapic_phys
    ));

    // Ошибка отображения сообщается, но не фатальна: откат к архитектурной базе
    // LAPIC. Прошивка с неверным адресом должна стоить одной строки
    // предупреждения, а не всей загрузки.
    let lapic_virt = match crate::memory::vmm::map_mmio(lapic_phys, 0x1000) {
        Ok(v) => v,
        Err(e) => {
            crate::error!(
                "apic: cannot map LAPIC at {:#x} ({}); falling back to {:#x}",
                lapic_phys,
                e,
                DEFAULT_LAPIC_PHYS
            );
            crate::drivers::earlyfb::println(format_args!(
                "apic: LAPIC {:#x} unusable, trying {:#x}",
                lapic_phys, DEFAULT_LAPIC_PHYS
            ));
            crate::memory::vmm::map_mmio(DEFAULT_LAPIC_PHYS, 0x1000).expect("map LAPIC MMIO")
        }
    };
    let ioapic_virt = if ioapic_phys != 0 {
        match crate::memory::vmm::map_mmio(ioapic_phys, 0x1000) {
            Ok(v) => v,
            Err(e) => {
                crate::error!(
                    "apic: cannot map I/O APIC at {:#x} ({}); no IRQ routing",
                    ioapic_phys,
                    e
                );
                crate::drivers::earlyfb::println(format_args!(
                    "apic: I/O APIC {:#x} unusable; no IRQ routing",
                    ioapic_phys
                ));
                0
            }
        }
    } else {
        0
    };
    LAPIC_BASE.store(lapic_virt, Ordering::Relaxed);
    IOAPIC_BASE.store(ioapic_virt, Ordering::Relaxed);

    crate::debug!(
        "LAPIC virt=0x{:x}, IOAPIC virt=0x{:x}",
        LAPIC_BASE.load(Ordering::Relaxed),
        IOAPIC_BASE.load(Ordering::Relaxed)
    );

    init_lapic();
    init_ioapic();
}

pub fn register_irq(vector: u8, handler: fn()) {
    IRQ_HANDLERS.lock()[(vector - 32) as usize] = Some(handler);
}

pub fn irq_dispatch(vector: u8) {
    if vector < 32 {
        return;
    }
    if let Some(h) = IRQ_HANDLERS.lock()[(vector - 32) as usize] {
        h();
    }
}

pub fn send_eoi() {
    unsafe {
        lapic_write(LAPIC_EOI, 0);
    }
}

pub fn route_irq(isa_irq: u8, vector: u8) {
    if IOAPIC_BASE.load(Ordering::Relaxed) == 0 {
        return;
    }
    // SAFETY: здесь IOAPIC_BASE ненулевая, то есть хранит HHDM-отображённую базу
    // MMIO I/O APIC; ioapic_write трогает только валидные отображённые регистры
    // перенаправления.
    unsafe {
        let reg = IOAPIC_REDTBL_BASE + (isa_irq as u32) * 2;
        ioapic_write(reg, vector as u32);
        ioapic_write(reg + 1, 0);
    }
    crate::debug!("IRQ {} routed to vector {}", isa_irq, vector);
}
