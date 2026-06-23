// arch/x86_64/acpi.rs — ACPI MADT parsing via the `acpi` crate (Requirement 6)
// 64-bit x86_64 Limine kernel in Rust (#![no_std])
//
// This module replaces the hand-rolled RSDP/RSDT/MADT parser (still living in
// `apic.rs` until task 9.2) with the `acpi` crate. It parses the MADT exactly
// once, caches the extracted APIC addresses, and serves the cached value on
// every subsequent call.
//
// Requirement mapping:
//  * 6.1 — obtain the LAPIC physical base, I/O APIC physical base, and the
//          global system interrupt base via the `acpi` crate.
//  * 6.2 — parse the MADT exactly once and cache the result (the cache is a
//          `Spinlock<Option<ApicAddrs>>`; `ApicAddrs` is `Copy`).
//  * 6.3 — if the MADT/ACPI is absent or parsing fails, default
//          `lapic_phys = 0xFEE0_0000`.
//  * 6.4 — if no I/O APIC is present, report `ioapic_phys == 0`.

use core::ptr::NonNull;
use core::sync::atomic::Ordering;

use acpi::madt::Madt;
use acpi::{AcpiHandler, AcpiTables, InterruptModel, PhysicalMapping};

use crate::sync::spinlock::Spinlock;

/// Physical addresses describing the platform's APIC topology, as required by
/// the APIC subsystem.
///
/// Validation rules (see design "Data Models"):
///  * `lapic_phys` defaults to `0xFEE0_0000` when the MADT is absent.
///  * `ioapic_phys == 0` means "no I/O APIC present".
#[derive(Debug, Clone, Copy)]
pub struct ApicAddrs {
    pub lapic_phys: u64,
    pub ioapic_phys: u64,
    pub gsi_base: u32,
}

/// Architectural default LAPIC MMIO base used when the MADT does not provide
/// one (Requirement 6.3).
const DEFAULT_LAPIC_PHYS: u64 = 0xFEE0_0000;

/// Cached parse result (Requirement 6.2). `None` until the first successful
/// (or defaulted) parse, then holds the value served on every later call.
static CACHE: Spinlock<Option<ApicAddrs>> = Spinlock::new(None);

/// `AcpiHandler` that maps physical regions through Limine's HHDM.
///
/// All physical memory is identity-mapped into the higher half by Limine at a
/// fixed `HHDM_OFFSET`, so the virtual address of any physical region is simply
/// `phys + HHDM_OFFSET`. There is therefore nothing to map or unmap per region.
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

        // SAFETY: Limine maps all physical memory into the HHDM, so
        // `phys + HHDM_OFFSET` is always a valid, mapped virtual address for a
        // region of `size` bytes. The pointer is non-null because `HHDM_OFFSET`
        // is a high-half base (far above zero) and `physical_address` is a real
        // ACPI table address. `region_length == mapped_length == size` because
        // the HHDM mapping needs no extra padding.
        let ptr = NonNull::new(virt as *mut T).expect("HHDM virtual address must be non-null");
        unsafe { PhysicalMapping::new(physical_address, ptr, size, size, *self) }
    }

    fn unmap_physical_region<T>(_region: &PhysicalMapping<Self, T>) {
        // No-op: the HHDM mapping is permanent, so there is nothing to tear down.
    }
}

/// Return the platform's APIC addresses, parsing the MADT exactly once.
///
/// The first call parses the MADT via the `acpi` crate and caches the result;
/// every subsequent call returns the cached value (Requirement 6.2). On any
/// failure (no RSDP, parse error, or absent MADT) the defaults are cached and
/// returned (Requirements 6.3, 6.4).
pub fn apic_addresses() -> ApicAddrs {
    // Hold the cache lock across the parse so the MADT is parsed exactly once
    // even if two callers race. The critical section is short and acquires no
    // other instance of this lock, so there is no deadlock risk.
    let mut guard = CACHE.lock();
    if let Some(cached) = *guard {
        return cached;
    }

    let addrs = parse();
    *guard = Some(addrs);
    addrs
}

/// Defaults used whenever ACPI/MADT information is unavailable (Requirements
/// 6.3, 6.4): the architectural LAPIC base, no I/O APIC, GSI base 0.
const fn defaults() -> ApicAddrs {
    ApicAddrs {
        lapic_phys: DEFAULT_LAPIC_PHYS,
        ioapic_phys: 0,
        gsi_base: 0,
    }
}

/// Resolve the RSDP physical address from Limine's RSDP response.
///
/// Under base revision 2 (this kernel's revision), Limine reports the RSDP as a
/// virtual HHDM pointer; the `acpi` crate wants the physical address, so we
/// subtract `HHDM_OFFSET`. If the value is already below the HHDM base it is
/// treated as a physical address as-is (defensive).
fn rsdp_phys_addr() -> Option<usize> {
    let resp = crate::RSDP_REQUEST.response()?;
    let rsdp_virt = resp.address as u64;
    if rsdp_virt == 0 {
        return None;
    }
    let hhdm = crate::HHDM_OFFSET.load(Ordering::Relaxed);
    let phys = if rsdp_virt >= hhdm { rsdp_virt - hhdm } else { rsdp_virt };
    Some(phys as usize)
}

/// Parse the MADT once via the `acpi` crate and extract the APIC addresses.
///
/// Returns [`defaults`] on any failure so the caller always gets a usable value
/// (Requirements 6.3, 6.4).
fn parse() -> ApicAddrs {
    let rsdp_phys = match rsdp_phys_addr() {
        Some(p) => p,
        None => {
            crate::warn!("acpi: RSDP unavailable, using defaults");
            return defaults();
        }
    };

    // SAFETY: `rsdp_phys` is the physical RSDP address reported by Limine; the
    // HHDM handler maps it (and the tables it references) into valid mapped
    // virtual memory. The `acpi` crate validates the RSDP signature/checksum.
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

    // `parse_interrupt_model_in` allocates its result slices via the global
    // allocator (the heap is initialized before APIC in the boot order).
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

            // Requirement 6.4: `ioapic_phys == 0` when no I/O APIC is present.
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
        // No APIC interrupt model described -> defaults (Requirement 6.3).
        _ => defaults(),
    }
}
