// Feature: demand-paged user buffers on the general `read(2)`/`write(2)` path,
// property `user_buffer_paging`: the pure acceptance rule in
// `src/arch/x86_64/linux/validate.rs` that decides, per page, whether a user
// buffer may be copied into or out of.
//
// Why this exists: `sys_read`/`sys_write` used to require PRESENT on every page
// of the whole `count` (`check_user_ptr`), while the #PF handler already serviced
// absent pages of a lazy `brk` heap or anonymous `mmap` region. A legitimate
// buffer — the fresh `Vec`/`Box` a real application passes to `read(2)`, or the
// `mmap` snapshot `pagh-terminal --vt` fills — was therefore rejected with
// `EFAULT`: only the first page ever worked. The fix routes both the syscall
// path and the fault handler through one pure table, `page_acceptance`.
//
// These tests pin the table itself, because it is the part that must not rot:
// absence alone may never be accepted (that would call an unmapped page mapped),
// and `PROT_NONE`/out-of-region may never be populated (that would make
// `mprotect(PROT_NONE)` advisory on this path). The decisive end-to-end proof —
// a real syscall against a real address space — is the in-QEMU routine
// `userbuf::lazy_anon_read_write`.

use crate::validate::{
    check_user_range, page_acceptance, spanned_pages, MappingKind, PageAcceptance, PtrCheck,
};
use proptest::prelude::*;

fn kinds() -> impl Strategy<Value = MappingKind> {
    prop_oneof![
        Just(MappingKind::LazyHeap),
        Just(MappingKind::LazyAnon),
        Just(MappingKind::Eager),
        Just(MappingKind::ProtNone),
        Just(MappingKind::Outside),
    ]
}

fn lazy_kind() -> impl Strategy<Value = MappingKind> {
    prop_oneof![Just(MappingKind::LazyHeap), Just(MappingKind::LazyAnon)]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The acceptance table, exactly as `handle_user_page_fault` consumes it:
    /// a present page is copied as-is in every kind; a missing page is populated
    /// only for the two lazy kinds; `PROT_NONE` and out-of-region refuse whether
    /// or not a PTE happens to exist (a `mprotect(PROT_NONE)` leaf must stay
    /// fatal, never become a silent re-mapping).
    #[test]
    fn acceptance_table_is_exact(present in any::<bool>(), kind in kinds()) {
        let expected = match (kind, present) {
            (MappingKind::LazyHeap | MappingKind::LazyAnon, false) => PageAcceptance::Populate,
            (MappingKind::LazyHeap | MappingKind::LazyAnon, true) => PageAcceptance::Accept,
            (MappingKind::Eager, true) => PageAcceptance::Accept,
            (MappingKind::Eager, false) => PageAcceptance::Refuse,
            (MappingKind::ProtNone | MappingKind::Outside, _) => PageAcceptance::Refuse,
        };
        prop_assert_eq!(page_acceptance(present, kind), expected);
    }

    /// Absence is never accepted on its own: the only non-refusal answer for a
    /// missing page is `Populate`, and it exists only for lazy mappings.
    #[test]
    fn absence_never_passes_unchecked(kind in kinds()) {
        match page_acceptance(false, kind) {
            PageAcceptance::Accept => prop_assert!(false, "absent page accepted for {:?}", kind),
            PageAcceptance::Populate => {
                prop_assert!(matches!(kind, MappingKind::LazyHeap | MappingKind::LazyAnon));
            }
            PageAcceptance::Refuse => {}
        }
    }

    /// A large lazy anon buffer — what `vec![0u8; total]` on `brk` or a fresh
    /// `mmap` gives an application — must never be refused page by page: every
    /// page of the span is `Populate`. This is the bug in one assertion: the old
    /// code answered `EFAULT` from the very first unpresent page.
    #[test]
    fn large_lazy_anon_span_is_fully_populatable(
        start_page in 0u64..(1 << 30),
        pages in 1u64..4096,
    ) {
        let start = start_page * 4096;
        let len = pages * 4096;
        prop_assert_eq!(check_user_range(start, len), PtrCheck::Ok);
        let mut seen = 0u64;
        for page in spanned_pages(start, len) {
            prop_assert_eq!(
                page_acceptance(false, MappingKind::LazyAnon),
                PageAcceptance::Populate,
                "page {:#x} of a {}-page lazy anon buffer refused",
                page,
                pages
            );
            seen += 1;
        }
        prop_assert_eq!(seen, pages, "spanned_pages must enumerate the whole span once");
    }

    /// The same span, but inside a `PROT_NONE` region, stays refused on every
    /// page: populating it would turn a guard page into usable memory.
    #[test]
    fn large_prot_none_span_stays_refused(
        start_page in 0u64..(1 << 30),
        pages in 1u64..64,
    ) {
        let start = start_page * 4096;
        let len = pages * 4096;
        for page in spanned_pages(start, len) {
            let _ = page;
            prop_assert_eq!(
                page_acceptance(false, MappingKind::ProtNone),
                PageAcceptance::Refuse
            );
        }
    }

    /// Once populated, the same lazy span is accepted; the rule is idempotent
    /// across repeated copy attempts (a second `read(2)` into the same buffer
    /// must not re-decide differently).
    #[test]
    fn populated_lazy_span_is_accepted(kind in lazy_kind()) {
        prop_assert_eq!(page_acceptance(true, kind), PageAcceptance::Accept);
        prop_assert_eq!(page_acceptance(false, kind), PageAcceptance::Populate);
    }
}