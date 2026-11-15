// arch/x86_64/acpi.rs — разбор MADT через крейт `acpi` (Requirement 6).
//
// Модуль разбирает MADT ровно один раз, кэширует извлечённые адреса APIC и
// отдаёт кэшированное значение при каждом следующем вызове.
//
// Соответствие требованиям:
//  * 6.1 — получить физическую базу LAPIC, физическую базу I/O APIC и базу
//          глобальных системных прерываний через крейт `acpi`.
//  * 6.2 — разобрать MADT ровно один раз и закэшировать результат (кэш —
//          `Spinlock<Option<ApicAddrs>>`; `ApicAddrs` реализует `Copy`).
//  * 6.3 — если MADT/ACPI отсутствует или разбор не удался, по умолчанию
//          `lapic_phys = 0xFEE0_0000`.
//  * 6.4 — если I/O APIC нет, сообщить `ioapic_phys == 0`.

use core::ptr::NonNull;
use core::sync::atomic::Ordering;

use acpi::madt::Madt;
use acpi::{AcpiHandler, AcpiTables, InterruptModel, PhysicalMapping};

use crate::sync::spinlock::Spinlock;

/// Физические адреса, описывающие топологию APIC платформы, как того требует
/// подсистема APIC.
///
/// Правила: `lapic_phys` по умолчанию `0xFEE0_0000`, если MADT отсутствует;
/// `ioapic_phys == 0` означает «I/O APIC отсутствует».
#[derive(Debug, Clone, Copy)]
pub struct ApicAddrs {
    pub lapic_phys: u64,
    pub ioapic_phys: u64,
    // Оставлено: часть намеренно полной таксономии ApicAddrs по ACPI
    // Requirement 6.1 (база LAPIC, база I/O APIC И база глобальных системных
    // прерываний). Извлекается из MADT и хранится для будущей GSI-маршрутизации
    // I/O APIC; пока не читается ни одним потребителем.
    #[allow(dead_code)]
    pub gsi_base: u32,
}

/// Архитектурная база MMIO LAPIC по умолчанию, когда MADT её не даёт
/// (Requirement 6.3).
pub const DEFAULT_LAPIC_PHYS: u64 = 0xFEE0_0000;

/// Кэш результата разбора (Requirement 6.2). `None` до первого успешного
/// (или дефолтного) разбора, затем хранит значение для всех последующих вызовов.
static CACHE: Spinlock<Option<ApicAddrs>> = Spinlock::new(None);

/// `AcpiHandler`, отображающий физические регионы через HHDM Limine.
///
/// Вся физическая память identity-отображена Limine в верхнюю половину по
/// фиксированному `HHDM_OFFSET`, поэтому виртуальный адрес любого физического
/// региона — просто `phys + HHDM_OFFSET`. Отображать или снимать отображение
/// на регион не нужно.
#[derive(Debug, Clone, Copy)]
struct HhdmAcpiHandler;

impl AcpiHandler for HhdmAcpiHandler {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> PhysicalMapping<Self, T> {
        let hhdm = crate::HHDM_OFFSET.load(Ordering::Relaxed) as usize;
        let virt = physical_address + hhdm;

        // SAFETY: Limine отображает всю физическую память в HHDM, поэтому
        // `phys + HHDM_OFFSET` — всегда валидный отображённый виртуальный адрес
        // для региона в `size` байт. Указатель ненулевой, потому что
        // `HHDM_OFFSET` — база верхней половины (далеко выше нуля), а
        // `physical_address` — реальный адрес таблицы ACPI.
        // `region_length == mapped_length == size`, потому что отображению HHDM
        // не нужен дополнительный паддинг.
        let ptr = NonNull::new(virt as *mut T).expect("HHDM virtual address must be non-null");
        unsafe { PhysicalMapping::new(physical_address, ptr, size, size, *self) }
    }

    fn unmap_physical_region<T>(_region: &PhysicalMapping<Self, T>) {
        // No-op: отображение HHDM постоянно, разбирать нечего.
    }
}

/// Вернуть адреса APIC платформы, разобрав MADT ровно один раз.
///
/// Первый вызов разбирает MADT через крейт `acpi` и кэширует результат; каждый
/// следующий вызов возвращает кэш (Requirement 6.2). При любой ошибке (нет
/// RSDP, ошибка разбора, нет MADT) кэшируются и возвращаются значения по
/// умолчанию (Requirements 6.3, 6.4).
pub fn apic_addresses() -> ApicAddrs {
    // Держать лок кэша на время разбора, чтобы MADT разобрался ровно один раз
    // даже при гонке двух вызывающих. Критическая секция короткая и не берёт
    // других экземпляров этого лока, поэтому дедлока нет.
    let mut guard = CACHE.lock();
    if let Some(cached) = *guard {
        return cached;
    }

    let addrs = parse();
    *guard = Some(addrs);
    addrs
}

/// Значения по умолчанию, когда информация ACPI/MADT недоступна
/// (Requirements 6.3, 6.4): архитектурная база LAPIC, нет I/O APIC, GSI base 0.
const fn defaults() -> ApicAddrs {
    ApicAddrs {
        lapic_phys: DEFAULT_LAPIC_PHYS,
        ioapic_phys: 0,
        gsi_base: 0,
    }
}

/// Определить физический адрес RSDP из ответа Limine с RSDP.
///
/// При базовой ревизии 2 (ревизия этого ядра) Limine сообщает RSDP как
/// виртуальный HHDM-указатель; крейт `acpi` хочет физический адрес, поэтому
/// вычитаем `HHDM_OFFSET`. Значение ниже базы HHDM считается физическим адресом
/// как есть (защитно).
fn rsdp_phys_addr() -> Option<usize> {
    let resp = crate::RSDP_REQUEST.response()?;
    let rsdp_virt = resp.address as u64;
    if rsdp_virt == 0 {
        return None;
    }
    let hhdm = crate::HHDM_OFFSET.load(Ordering::Relaxed);
    let phys = if rsdp_virt >= hhdm {
        rsdp_virt - hhdm
    } else {
        rsdp_virt
    };
    Some(phys as usize)
}

/// Разобрать MADT один раз через крейт `acpi` и извлечь адреса APIC.
///
/// При любой ошибке возвращает [`defaults`], чтобы вызывающий всегда получал
/// пригодное значение (Requirements 6.3, 6.4).
fn parse() -> ApicAddrs {
    let rsdp_phys = match rsdp_phys_addr() {
        Some(p) => p,
        None => {
            crate::warn!("acpi: RSDP unavailable, using defaults");
            return defaults();
        }
    };

    // SAFETY: `rsdp_phys` — физический адрес RSDP, сообщённый Limine; HHDM-
    // обработчик отображает его (и таблицы, на которые он ссылается) в валидную
    // отображённую виртуальную память. Крейт `acpi` проверяет сигнатуру и
    // контрольную сумму RSDP.
    let tables = match unsafe { AcpiTables::from_rsdp(HhdmAcpiHandler, rsdp_phys) } {
        Ok(t) => t,
        Err(_) => {
            crate::warn!("acpi: ACPI table parse failed, using defaults");
            return defaults();
        }
    };

    let madt = match tables.find_table::<Madt>() {
        Ok(m) => m,
        Err(_) => {
            crate::warn!("acpi: MADT absent, using defaults");
            return defaults();
        }
    };

    // `parse_interrupt_model_in` выделяет срезы результата через глобальный
    // аллокатор (куча инициализируется до APIC в порядке загрузки).
    let model = match madt.get().parse_interrupt_model_in(alloc::alloc::Global) {
        Ok((model, _processor_info)) => model,
        Err(_) => {
            crate::warn!("acpi: MADT parse failed, using defaults");
            return defaults();
        }
    };

    match model {
        InterruptModel::Apic(apic) => {
            let lapic_phys = if apic.local_apic_address != 0 {
                apic.local_apic_address
            } else {
                DEFAULT_LAPIC_PHYS
            };

            // Requirement 6.4: `ioapic_phys == 0`, если I/O APIC отсутствует.
            let (ioapic_phys, gsi_base) = match apic.io_apics.first() {
                Some(io) => (io.address as u64, io.global_system_interrupt_base),
                None => (0, 0),
            };

            ApicAddrs {
                lapic_phys,
                ioapic_phys,
                gsi_base,
            }
        }
        // Модель прерываний APIC не описана -> значения по умолчанию (Requirement 6.3).
        _ => defaults(),
    }
}
