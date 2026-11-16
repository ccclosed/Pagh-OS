//! PAT (Page Attribute Table) и write-combined (WC) политика для MMIO.
//!
//! Модуль намеренно `core`-only: `host-tests` подключает его через `#[path]`,
//! поэтому кодирование `IA32_PAT`, разбор CPUID-бита и композиция PTE-флагов
//! проверяются на хосте ровно по тому исходнику, который компилирует ядро.
//! Привилегированная половина (CPUID + `rdmsr`/`wrmsr`) живёт в
//! [`crate::arch::x86_64::pat`] и только вызывает чистые функции отсюда.
//!
//! ## Зачем WC
//!
//! `/dev/fb0` — физический scanout, не RAM под управлением PMM. Раньше его
//! листья отображались `NO_CACHE|WRITE_THROUGH`: каждый пиксель композитора
//! уходил отдельной MMIO-записью (read-modify-write в памяти устройства), и
//! полный кадр 1280×800 стоил десятки миллионов циклов. Write-combining
//! разрешает CPU сливать соседние записи в одну пачку, не давая при этом
//! читать из буфера: для потоковой отрисовки (fill/copy) это правильная
//! политика, а `WRITE_THROUGH`/`NO_CACHE` нужны только для регистров
//! устройств, где важна каждая отдельная транзакция.
//!
//! ## Индекс PAT для 4 КиБ страницы
//!
//! У листа 4 КиБ политика выбирается тремя битами: `PAT << 2 | PWT << 1 |
//! PCD` (см. Intel SDM Vol. 3A, Table 11-11). `PageTableFlags` называет бит 7
//! `HUGE_PAGE` — это верно только для листьев 2 МиБ/1 ГиБ; в 4 КиБ PTE тот же
//! бит и есть PAT. WC — это `PAT=1, PWT=0, PCD=0`, то есть **entry 4**
//! `IA32_PAT`. Поэтому WC-лист несёт только [`PTE_PAT`] и не несёт
//! `NO_CACHE`/`WRITE_THROUGH`.
//!
//! ## Fallback
//!
//! Если CPUID leaf 1 EDX bit 16 не выставлен, PAT-атрибут в PTE игнорируется
//! (или CPU его не поддерживает), и WC недостижим. Тогда `/dev/fb0` остаётся
//! на точной прежней паре `NO_CACHE|WRITE_THROUGH` — корректность сохраняется,
//! теряется только скорость записи. Решение фиксируется один раз на boot в
//! [`set_supported`], чтобы mmap/mprotect не могли разойтись.

use core::sync::atomic::{AtomicBool, Ordering};

/// MSR `IA32_PAT` (64 бита, восемь 8-битных записей).
pub const IA32_PAT: u32 = 0x277;

/// CPUID leaf 1, EDX bit 16: PAT поддерживается процессором.
pub const CPUID_PAT_BIT: u32 = 1 << 16;

/// Лист 4 КиБ, бит 7: индекс PAT (в huge-page листе — `HUGE_PAGE`).
pub const PTE_PAT: u64 = 1 << 7;
/// Лист, бит 3: write-through (`PWT`).
pub const PTE_WRITE_THROUGH: u64 = 1 << 3;
/// Лист, бит 4: cache disable (`PCD`).
pub const PTE_NO_CACHE: u64 = 1 << 4;
/// Лист, бит 0: present.
pub const PTE_PRESENT: u64 = 1 << 0;
/// Лист, бит 1: writable.
pub const PTE_WRITABLE: u64 = 1 << 1;
/// Лист, бит 2: user-accessible.
pub const PTE_USER: u64 = 1 << 2;
/// Лист, бит 52: программный маркер MMIO (совпадает с `vmm::MMIO_BIT`).
pub const PTE_MMIO: u64 = 1 << 52;
/// Лист, бит 63: no-execute.
pub const PTE_NX: u64 = 1 << 63;

/// `IA32_PAT` entry 0: write-back (значение по умолчанию после reset).
pub const PA_WB: u64 = 0x06;
/// `IA32_PAT` entry 1: write-through.
pub const PA_WT: u64 = 0x04;
/// `IA32_PAT` entry 2: uncached-minus (WC на write, UC на read).
pub const PA_UC_MINUS: u64 = 0x07;
/// `IA32_PAT` entry 3: uncached.
pub const PA_UC: u64 = 0x00;
/// `IA32_PAT` entry 4/5: write-combining (единственное значение, которое мы ставим).
pub const PA_WC: u64 = 0x01;

/// Записи `IA32_PAT`, которые ядро программирует: 4 и 5 = WC.
///
/// Entry 4 — та, которую выбирает WC-лист (`PAT=1, PCD=PWT=0`). Entry 5
/// (`PAT=1, PCD=1`) ставится в WC по той же политике из спецификации
/// desktop-render §2 A1, чтобы обе «некешируемые» записи PAT не оставляли
/// дыру UC для будущих пользователей; остальные записи (0–3, 6, 7) не
/// трогаются.
pub const WC_PAT_ENTRIES: [(u32, u64); 2] = [(4, PA_WC), (5, PA_WC)];

/// Кэш-биты WC-листа: только PAT (PCD=PWT=0), без `NO_CACHE`/`WRITE_THROUGH`.
pub const WC_CACHE_BITS: u64 = PTE_PAT;

/// Кэш-биты fallback-листа: ровно прежняя пара некешируемого MMIO.
pub const FALLBACK_CACHE_BITS: u64 = PTE_NO_CACHE | PTE_WRITE_THROUGH;

/// Поддерживает ли CPU PAT (по `CPUID.1:EDX[16]`).
///
/// Чистая функция: CPUID читает `arch::x86_64::pat`, а host-тесты передают
/// произвольное значение EDX.
pub fn cpuid_supports_pat(edx: u32) -> bool {
    edx & CPUID_PAT_BIT != 0
}

/// Значение `IA32_PAT` с WC в записях 4 и 5 и **сохранёнными** остальными.
///
/// Вызывающий передаёт текущее значение MSR; функция не читает железо и не
/// зависит от глобального состояния, поэтому host-тесты доказывают, что ни
/// одна чужая запись не меняется.
pub fn pat_msr_value(current: u64) -> u64 {
    let mut value = current;
    for (index, pa) in WC_PAT_ENTRIES {
        let shift = index * 8;
        value = (value & !(0xFFu64 << shift)) | (pa << shift);
    }
    value
}

/// Листовые флаги WC-отображения `/dev/fb0`.
///
/// Композиция повторяет спецификацию desktop-render §2 A2:
/// `PRESENT | WRITABLE | USER_ACCESSIBLE | MMIO_BIT | PAT(bit 7) | NX`, без
/// `NO_CACHE`/`WRITE_THROUGH`. `writable`/`nx` берутся из `prot` вызывающего
/// (`mmap(PROT_READ)` остаётся read-only — валидация прав не меняется).
pub fn wc_pte_flags(writable: bool, nx: bool) -> u64 {
    base_pte_flags(writable, nx) | PTE_MMIO | WC_CACHE_BITS
}

/// Листовые флаги fallback-отображения `/dev/fb0` (PAT отсутствует).
///
/// Это точная прежняя композиция `leaf_flags | MMIO_BIT | NO_CACHE |
/// WRITE_THROUGH`, вынесенная в чистую функцию, чтобы fallback нельзя было
/// «улучшить» случайно.
pub fn fallback_pte_flags(writable: bool, nx: bool) -> u64 {
    base_pte_flags(writable, nx) | PTE_MMIO | FALLBACK_CACHE_BITS
}

/// Выбрать флаги fb0 по зафиксированной на boot поддержке WC.
pub fn fb0_pte_flags(writable: bool, nx: bool, wc_supported: bool) -> u64 {
    if wc_supported {
        wc_pte_flags(writable, nx)
    } else {
        fallback_pte_flags(writable, nx)
    }
}

/// Кэш-биты, которые `mprotect` обязан сохранить на MMIO-листе.
///
/// `mprotect` меняет только права (`PROT_*`), но не тип памяти. Раньше он
/// безусловно возвращал `NO_CACHE|WRITE_THROUGH`, из-за чего первый же
/// `mprotect` на fb-регионе тихо отменял WC. Функция читает кэш-атрибут из
/// самого листа:
///
/// * уже WC (`PAT`) — остаётся WC, даже если рядом окажется странная пара;
/// * старая fallback-пара — остаётся fallback;
/// * кэш-битов нет (лист создан не нами) — применяется текущая политика.
pub fn mmio_cache_bits_from(old_bits: u64, wc_supported: bool) -> u64 {
    let old_cache = old_bits & (PTE_PAT | PTE_NO_CACHE | PTE_WRITE_THROUGH);
    if old_cache & PTE_PAT != 0 {
        WC_CACHE_BITS
    } else if old_cache != 0 {
        FALLBACK_CACHE_BITS
    } else if wc_supported {
        WC_CACHE_BITS
    } else {
        FALLBACK_CACHE_BITS
    }
}

/// `PRESENT | USER_ACCESSIBLE` плюс `WRITABLE`/`NO_EXECUTE` по правам.
fn base_pte_flags(writable: bool, nx: bool) -> u64 {
    let mut flags = PTE_PRESENT | PTE_USER;
    if writable {
        flags |= PTE_WRITABLE;
    }
    if nx {
        flags |= PTE_NX;
    }
    flags
}

/// Поддерживается ли WC на этой загрузке (и запрограммирована ли entry 4).
static WC_SUPPORTED: AtomicBool = AtomicBool::new(false);

/// Зафиксировать результат boot-детекта PAT. Вызывается ровно один раз из
/// [`crate::arch::x86_64::pat::init`]; до этого WC считается недоступным, чтобы
/// любое преждевременное отображение получило безопасный fallback.
pub fn set_supported(supported: bool) {
    WC_SUPPORTED.store(supported, Ordering::Relaxed);
}

/// Помощник `memory::pat::wc_supported()`: можно ли отображать fb0 как WC.
pub fn wc_supported() -> bool {
    WC_SUPPORTED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpuid_bit_is_leaf1_edx_bit16() {
        assert!(cpuid_supports_pat(CPUID_PAT_BIT));
        assert!(cpuid_supports_pat(u32::MAX));
        assert!(!cpuid_supports_pat(0));
        assert!(!cpuid_supports_pat(CPUID_PAT_BIT << 1));
        assert!(!cpuid_supports_pat(!CPUID_PAT_BIT));
    }

    #[test]
    fn pat_msr_value_sets_wc_entries_and_preserves_the_rest() {
        // Reset-value Intel SDM: PA0=WB, PA1=WT, PA2=UC-, PA3=UC,
        // PA4=WB, PA5=WT, PA6=UC-, PA7=UC.
        let reset: u64 = 0x0007_0406_0007_0406;
        let v = pat_msr_value(reset);
        for i in 0..8u32 {
            let entry = (v >> (i * 8)) & 0xFF;
            if i == 4 || i == 5 {
                assert_eq!(entry, PA_WC, "entry {i} must be WC");
            } else {
                assert_eq!(entry, (reset >> (i * 8)) & 0xFF, "entry {i} preserved");
            }
        }
        // Идемпотентность: повторный вызов на уже записанном значении — no-op.
        assert_eq!(pat_msr_value(v), v);
    }

    #[test]
    fn pat_msr_value_preserves_arbitrary_other_entries() {
        let current: u64 = 0x1122_3344_5566_7788;
        let v = pat_msr_value(current);
        assert_eq!(v & 0xFFFF_0000_FFFF_FFFF, current & 0xFFFF_0000_FFFF_FFFF);
        assert_eq!(v >> 48, current >> 48);
        assert_eq!((v >> 32) & 0xFF, PA_WC);
        assert_eq!((v >> 40) & 0xFF, PA_WC);
    }

    #[test]
    fn wc_flags_match_the_spec_mask() {
        let flags = wc_pte_flags(true, true);
        assert_eq!(
            flags,
            PTE_PRESENT | PTE_WRITABLE | PTE_USER | PTE_MMIO | PTE_PAT | PTE_NX,
            "fb0 WC leaf = PRESENT|WRITABLE|USER|MMIO|PAT|NX"
        );
        assert_eq!(flags & PTE_NO_CACHE, 0, "WC leaf must not disable cache");
        assert_eq!(
            flags & PTE_WRITE_THROUGH,
            0,
            "WC leaf must not be write-through"
        );
        assert_ne!(flags & PTE_PAT, 0);
        assert_eq!(flags & PTE_MMIO, PTE_MMIO);
    }

    #[test]
    fn wc_flags_honour_protection() {
        let ro = wc_pte_flags(false, true);
        assert_eq!(ro & PTE_WRITABLE, 0, "PROT_READ stays read-only");
        assert_eq!(ro & PTE_NX, PTE_NX);
        assert_eq!(ro & PTE_MMIO, PTE_MMIO);
        assert_eq!(ro & PTE_PAT, PTE_PAT);

        let exec = wc_pte_flags(true, false);
        assert_eq!(exec & PTE_NX, 0, "PROT_EXEC clears NX");
        assert_eq!(exec & PTE_WRITABLE, PTE_WRITABLE);
    }

    #[test]
    fn fallback_flags_are_the_exact_legacy_pair() {
        let flags = fallback_pte_flags(true, true);
        assert_eq!(
            flags,
            PTE_PRESENT
                | PTE_WRITABLE
                | PTE_USER
                | PTE_MMIO
                | PTE_NO_CACHE
                | PTE_WRITE_THROUGH
                | PTE_NX
        );
        assert_eq!(flags & PTE_PAT, 0, "fallback must not depend on PAT");
    }

    #[test]
    fn fb0_flags_pick_wc_only_when_supported() {
        assert_eq!(fb0_pte_flags(true, true, true), wc_pte_flags(true, true));
        assert_eq!(
            fb0_pte_flags(true, true, false),
            fallback_pte_flags(true, true)
        );
    }

    #[test]
    fn mmio_cache_bits_keep_wc_and_never_downgrade() {
        // Уже WC → остаётся WC.
        assert_eq!(
            mmio_cache_bits_from(
                PTE_PRESENT | PTE_WRITABLE | PTE_MMIO | PTE_PAT | PTE_NX,
                false
            ),
            WC_CACHE_BITS
        );
        // Старый fallback → остаётся fallback, даже если PAT появился.
        assert_eq!(
            mmio_cache_bits_from(PTE_NO_CACHE | PTE_WRITE_THROUGH | PTE_MMIO, true),
            FALLBACK_CACHE_BITS
        );
        // Кэш-битов нет → текущая политика.
        assert_eq!(mmio_cache_bits_from(PTE_MMIO, true), WC_CACHE_BITS);
        assert_eq!(mmio_cache_bits_from(PTE_MMIO, false), FALLBACK_CACHE_BITS);
        // Противоречивая пара PAT+NC не откатывает WC.
        assert_eq!(
            mmio_cache_bits_from(PTE_PAT | PTE_NO_CACHE | PTE_WRITE_THROUGH, false),
            WC_CACHE_BITS
        );
    }

    #[test]
    fn wc_supported_defaults_to_false() {
        // Тесты процесса делят глобальное состояние; проверяем безопасное
        // значение по умолчанию, не полагаясь на порядок запуска остальных
        // тестов (они не зовут set_supported).
        set_supported(false);
        assert!(!wc_supported());
        set_supported(true);
        assert!(wc_supported());
        set_supported(false);
    }
}
