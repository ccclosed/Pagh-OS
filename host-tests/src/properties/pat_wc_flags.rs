//! Desktop-render §2 A1/A2: PAT entry 4 = WC и флаги write-combined `/dev/fb0`.
//!
//! Ядро выбирает политику кэша 4 КиБ-листа тремя битами (`PAT<<2 | PWT<<1 |
//! PCD`) и записью `IA32_PAT`. Свойства ниже гоняют тот же исходник, который
//! компилирует ядро (`src/memory/pat.rs`), на произвольных значениях MSR и
//! PTE-битов:
//!
//! * `pat_msr_value` меняет ТОЛЬКО записи 4/5 (WC) и сохраняет остальные шесть
//!   байт-в-байт, а повторное применение не меняет значение;
//! * WC-лист всегда `PRESENT|USER|MMIO|PAT|NX`, несёт `WRITABLE`/`NX` строго по
//!   правам `mmap`, и никогда не несёт `NO_CACHE|WRITE_THROUGH`;
//! * fallback-лист — точная прежняя некешируемая композиция без PAT;
//! * `mprotect` на MMIO-листе сохраняет ровно ту политику, которая уже стоит
//!   (WC не откатывается в uncached), а при отсутствии кэш-битов берёт текущую.

use crate::pat::{
    cpuid_supports_pat, fb0_pte_flags, fallback_pte_flags, mmio_cache_bits_from, pat_msr_value,
    wc_pte_flags, FALLBACK_CACHE_BITS, PA_WC, PTE_MMIO, PTE_NO_CACHE, PTE_NX, PTE_PAT,
    PTE_PRESENT, PTE_USER, PTE_WRITABLE, PTE_WRITE_THROUGH, WC_CACHE_BITS,
};
use proptest::prelude::*;

/// Байты, которые `pat_msr_value` обязан сохранить: записи 0–3 и 6–7.
const PAT_PRESERVED_MASK: u64 = 0xFFFF_0000_FFFF_FFFF;

proptest! {
    /// `IA32_PAT`: записи 4/5 = WC, всё остальное — без изменений.
    #[test]
    fn pat_entries_4_and_5_are_wc_and_others_survive(current in any::<u64>()) {
        let value = pat_msr_value(current);
        prop_assert_eq!((value >> 32) & 0xFF, PA_WC, "PAT entry 4 must be WC");
        prop_assert_eq!((value >> 40) & 0xFF, PA_WC, "PAT entry 5 must be WC");
        prop_assert_eq!(
            value & PAT_PRESERVED_MASK,
            current & PAT_PRESERVED_MASK,
            "entries 0-3 and 6-7 must be preserved"
        );
        prop_assert_eq!(pat_msr_value(value), value, "reprogramming is idempotent");
    }

    /// CPUID leaf 1: EDX bit 16 — единственный бит, решающий наличие PAT.
    #[test]
    fn cpuid_pat_is_edx_bit16(edx in any::<u32>()) {
        prop_assert_eq!(cpuid_supports_pat(edx), edx & (1 << 16) != 0);
    }

    /// WC-лист fb0: PAT entry 4, MMIO, без NO_CACHE/WRITE_THROUGH; права — из prot.
    #[test]
    fn wc_leaf_is_pat_entry4(writable in any::<bool>(), nx in any::<bool>()) {
        let flags = wc_pte_flags(writable, nx);
        prop_assert_eq!(flags & PTE_PRESENT, PTE_PRESENT);
        prop_assert_eq!(flags & PTE_USER, PTE_USER);
        prop_assert_eq!(flags & PTE_MMIO, PTE_MMIO);
        prop_assert_eq!(flags & PTE_PAT, PTE_PAT);
        prop_assert_eq!(flags & PTE_NO_CACHE, 0, "WC must not disable the cache");
        prop_assert_eq!(flags & PTE_WRITE_THROUGH, 0, "WC must not be write-through");
        prop_assert_eq!(flags & PTE_WRITABLE != 0, writable);
        prop_assert_eq!(flags & PTE_NX != 0, nx);

        let fallback = fallback_pte_flags(writable, nx);
        prop_assert_eq!(fallback & PTE_PAT, 0, "fallback must not depend on PAT");
        prop_assert_eq!(
            fallback & (PTE_NO_CACHE | PTE_WRITE_THROUGH),
            PTE_NO_CACHE | PTE_WRITE_THROUGH
        );
        prop_assert_eq!(fallback & PTE_MMIO, PTE_MMIO);
        prop_assert_eq!(fallback & PTE_WRITABLE != 0, writable);
        prop_assert_eq!(fallback & PTE_NX != 0, nx);

        prop_assert_eq!(fb0_pte_flags(writable, nx, true), flags);
        prop_assert_eq!(fb0_pte_flags(writable, nx, false), fallback);
    }

    /// `mprotect` сохраняет ровно WC или ровно fallback и не смешивает их.
    #[test]
    fn mprotect_preserves_the_existing_cache_policy(
        old_bits in any::<u64>(),
        wc_supported in any::<bool>(),
    ) {
        let bits = mmio_cache_bits_from(old_bits, wc_supported);
        prop_assert!(
            bits == WC_CACHE_BITS || bits == FALLBACK_CACHE_BITS,
            "cache bits are exactly WC (PAT) or the legacy uncached pair"
        );
        if old_bits & PTE_PAT != 0 {
            prop_assert_eq!(bits, WC_CACHE_BITS, "existing WC must not be downgraded");
        } else if old_bits & (PTE_NO_CACHE | PTE_WRITE_THROUGH) != 0 {
            prop_assert_eq!(
                bits,
                FALLBACK_CACHE_BITS,
                "existing uncached leaf stays on the legacy pair"
            );
        } else if wc_supported {
            prop_assert_eq!(bits, WC_CACHE_BITS);
        } else {
            prop_assert_eq!(bits, FALLBACK_CACHE_BITS);
        }
        // mprotect никогда не добавляет NO_CACHE к WC-листу.
        if old_bits & PTE_PAT != 0 {
            prop_assert_eq!(bits & (PTE_NO_CACHE | PTE_WRITE_THROUGH), 0);
        }
    }
}