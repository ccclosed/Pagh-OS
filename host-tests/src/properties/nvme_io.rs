// Feature: NVMe transfer plumbing, Property 57: PRP lists, LBA chunking and the
// polled SQ/CQ rings.
//
// `src/drivers/nvme.rs` cannot run on the host (MMIO, PMM, spinlocks), so the
// arithmetic that decides *which physical pages the controller reads and writes*
// is factored into `src/drivers/nvme_prp.rs` and included here verbatim. The
// properties below are the anti-regression net for the classes of bug that a
// boot smoke test on a one-page transfer cannot see:
//
//   * PRP2 must be a *page address* for exactly two pages and a *PRP-list page
//     address* for three or more, with list entry 0 naming the SECOND page
//     (PRP1 already names the first). A list that starts at the first page
//     silently shifts every page and corrupts DMA memory.
//   * `plan_chunk` must cover the caller's bytes with whole LBAs, never
//     overrun the slot buffer, and partition a long request into consecutive
//     non-overlapping data ranges. An off-by-one here writes the sector after
//     the requested range.
//   * The completion-queue phase tag must invert exactly on a wrap; a stale
//     slot with the previous phase may never be consumed as a new completion.
//   * CAP.MQES clamping must produce a valid (power-of-two, non-zero) queue
//     size, and the driver's create-IOCQ/IOSQ must pass that size minus one.

use crate::nvme_prp::{
    build_prp, plan_chunk, queue_entries, CqRing, PrpError, SqRing, MAX_NLB, PAGE_SIZE,
    PRP_LIST_ENTRIES,
};
use proptest::prelude::*;

/// Largest single-command transfer the driver allocates (16 pages, 64 KiB).
const MAX_PAGES: usize = 16;
const MAX_BYTES: usize = MAX_PAGES * PAGE_SIZE;

fn lba_size() -> impl Strategy<Value = u64> {
    prop_oneof![Just(512u64), Just(4096u64)]
}

/// Page-aligned physical base below 2^40 so `base + MAX_PAGES * 4K` cannot
/// overflow (real DMA addresses are far below that anyway).
fn page_base() -> impl Strategy<Value = u64> {
    (0u64..(1u64 << 28)).prop_map(|page| page * PAGE_SIZE as u64)
}

fn aligned_list_phys() -> impl Strategy<Value = u64> {
    (0u64..(1u64 << 28)).prop_map(|page| (page | 1) * PAGE_SIZE as u64)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// PRP1/PRP2/list contents agree with an independently written model of
    /// NVMe Base Spec §3.3.3 for every page count 1..=16.
    #[test]
    fn prp_matches_the_spec_model(
        base in page_base(),
        list_phys in aligned_list_phys(),
        pages in 1usize..=MAX_PAGES,
        offset in 0usize..PAGE_SIZE,
        len in 1usize..=(MAX_PAGES * PAGE_SIZE),
    ) {
        prop_assume!(offset + len <= pages * PAGE_SIZE);
        let mut list = vec![u64::MAX; PRP_LIST_ENTRIES];
        let plan = build_prp(base, pages, offset, len, list_phys, &mut list)
            .expect("valid plan");

        let needed = (offset + len + PAGE_SIZE - 1) / PAGE_SIZE;
        prop_assert_eq!(plan.prp1, base + offset as u64);
        match needed {
            1 => {
                prop_assert_eq!(plan.prp2, 0);
                prop_assert_eq!(plan.list_len, 0);
            }
            2 => {
                prop_assert_eq!(plan.prp2, base + PAGE_SIZE as u64);
                prop_assert_eq!(plan.list_len, 0);
                // No list is written for a two-page transfer.
                prop_assert!(list.iter().all(|&entry| entry == u64::MAX));
            }
            n => {
                prop_assert_eq!(plan.prp2, list_phys);
                prop_assert_eq!(plan.list_len, n - 1);
                for i in 0..(n - 1) {
                    // Entry 0 is the SECOND page, not the first.
                    prop_assert_eq!(list[i], base + ((i + 1) * PAGE_SIZE) as u64);
                }
                prop_assert!(list[n - 1..].iter().all(|&entry| entry == u64::MAX));
            }
        }
    }

    /// A list page never needs more entries than it has, and entries are
    /// strictly increasing page addresses with no duplicates.
    #[test]
    fn prp_list_is_strictly_increasing_and_fits(
        base in page_base(),
        list_phys in aligned_list_phys(),
        pages in 3usize..=MAX_PAGES,
        offset in 0usize..PAGE_SIZE,
    ) {
        let len = pages * PAGE_SIZE - offset;
        let mut list = vec![0u64; PRP_LIST_ENTRIES];
        let plan = build_prp(base, pages, offset, len, list_phys, &mut list)
            .expect("valid plan");
        prop_assert!(plan.list_len <= PRP_LIST_ENTRIES);
        prop_assert_eq!(plan.list_len, pages - 1);
        for i in 1..plan.list_len {
            prop_assert!(list[i] > list[i - 1]);
        }
        prop_assert!(list[..plan.list_len]
            .iter()
            .all(|&entry| entry % PAGE_SIZE as u64 == 0));
    }

    /// Every malformed input is refused with a nameable error instead of a
    /// panic or a silently wrong address.
    #[test]
    fn prp_rejects_malformed_plans(
        base in page_base(),
        pages in 1usize..=MAX_PAGES,
        offset in 0usize..PAGE_SIZE,
        len in 1usize..=(MAX_PAGES * PAGE_SIZE),
        noise in 0usize..PAGE_SIZE,
    ) {
        let mut list = vec![0u64; PRP_LIST_ENTRIES];
        prop_assert_eq!(
            build_prp(base, pages, offset, 0, base, &mut list),
            Err(PrpError::EmptyTransfer)
        );
        prop_assert_eq!(
            build_prp(base, pages, PAGE_SIZE + noise, len, base, &mut list),
            Err(PrpError::OffsetOutOfRange)
        );
        prop_assert_eq!(
            build_prp(base + 8, pages, offset, len, base, &mut list),
            Err(PrpError::BaseMisaligned)
        );
        let too_big = pages * PAGE_SIZE - offset + 1;
        if offset + too_big <= usize::MAX / 2 {
            prop_assert_eq!(
                build_prp(base, pages, offset, too_big, base, &mut list),
                Err(PrpError::TransferTooLarge)
            );
        }

        // A list page is only required for >= 3 pages; when one is required it
        // must be aligned and the caller's slice must be large enough.
        let needed = (offset + len + PAGE_SIZE - 1) / PAGE_SIZE;
        if needed >= 3 && needed <= pages {
            prop_assert_eq!(
                build_prp(base, pages, offset, len, base + 8, &mut list),
                Err(PrpError::ListMisaligned)
            );
            let mut short = vec![0u64; needed - 2];
            prop_assert_eq!(
                build_prp(base, pages, offset, len, base, &mut short),
                Err(PrpError::ListTooSmall)
            );
        }
    }

    /// One plan always covers all requested bytes, starts no later than the
    /// requested offset, and leaves `within + command_bytes` inside the slot.
    #[test]
    fn chunk_plan_is_consistent(
        byte_off in 0u64..(1u64 << 40),
        remaining in 1usize..(2 * 1024 * 1024),
        lba in lba_size(),
        max_pages in 1usize..=MAX_PAGES,
    ) {
        let max_bytes = max_pages * PAGE_SIZE;
        prop_assume!(max_bytes >= lba as usize && max_bytes % lba as usize == 0);
        let plan = plan_chunk(byte_off, remaining, lba, max_bytes)
            .expect("a plan must exist for a valid slot configuration");
        let lba_us = lba as usize;
        let command_bytes = plan.lbas as usize * lba_us;

        prop_assert_eq!(plan.within, (byte_off % lba) as usize);
        prop_assert_eq!(plan.lba, byte_off / lba);
        prop_assert!(plan.within < lba_us);
        prop_assert!(plan.lbas >= 1);
        prop_assert!(plan.lbas <= MAX_NLB);
        prop_assert!(plan.data_len >= 1);
        prop_assert!(plan.data_len <= remaining);
        // The command data (LBA-aligned, from slot offset 0) fits the slot, and
        // the caller's bytes at `within` fit inside it.
        prop_assert!(command_bytes <= max_bytes);
        prop_assert!(plan.within + plan.data_len <= max_bytes);

        // The command's LBA span contains the caller's bytes: it starts at or
        // before `byte_off` and ends at or after `byte_off + data_len`.
        let command_start = plan.lba * lba;
        prop_assert!(command_start <= byte_off);
        prop_assert!(byte_off + plan.data_len as u64 <= command_start + command_bytes as u64);

        // It consumes no more than one full slot, and the data range is the
        // largest the slot allows.
        let avail = max_bytes - plan.within;
        prop_assert_eq!(plan.data_len, remaining.min(avail));
    }

    /// Successive plans tile the requested range exactly: every byte is
    /// consumed once, the loop makes progress, and it terminates.
    #[test]
    fn sequential_chunks_partition_the_span(
        byte_off in 0u64..(1u64 << 40),
        remaining in 1usize..(2 * 1024 * 1024),
        lba in lba_size(),
        max_pages in 1usize..=MAX_PAGES,
    ) {
        let max_bytes = max_pages * PAGE_SIZE;
        prop_assume!(max_bytes >= lba as usize && max_bytes % lba as usize == 0);
        let mut off = byte_off;
        let mut left = remaining;
        let mut iterations = 0;
        while left > 0 {
            let plan = plan_chunk(off, left, lba, max_bytes).expect("plan");
            prop_assert!(plan.data_len > 0);
            prop_assert!(plan.data_len <= left);
            off += plan.data_len as u64;
            left -= plan.data_len;
            iterations += 1;
            prop_assert!(iterations <= remaining);
        }
        prop_assert_eq!(off, byte_off + remaining as u64);
    }

    /// The write fast path is only taken when the range is LBA-aligned; in that
    /// case each command transfers exactly its caller bytes (no padding that
    /// could overwrite a neighbour).
    #[test]
    fn aligned_writes_need_no_padding(
        first_lba in 0u64..(1u64 << 30),
        lba in lba_size(),
        lbas in 1u32..=64,
    ) {
        let max_bytes = MAX_BYTES;
        let byte_off = first_lba * lba;
        let remaining = lbas as usize * lba as usize;
        prop_assume!(remaining <= max_bytes);
        let plan = plan_chunk(byte_off, remaining, lba, max_bytes).expect("plan");
        prop_assert_eq!(plan.within, 0);
        prop_assert_eq!(plan.lba, byte_off / lba);
        prop_assert_eq!(plan.data_len, remaining);
        prop_assert_eq!(plan.lbas as usize * lba as usize, remaining);
    }

    /// Configurations the driver must never use are rejected rather than
    /// producing a command whose data cannot fit.
    #[test]
    fn chunk_plan_rejects_impossible_slots(
        byte_off in any::<u64>(),
        remaining in any::<usize>(),
        lba in lba_size(),
        delta in 1usize..PAGE_SIZE,
    ) {
        prop_assert_eq!(plan_chunk(byte_off, 0, lba, MAX_BYTES), None);
        prop_assert_eq!(plan_chunk(byte_off, remaining, 0, MAX_BYTES), None);
        let too_small = (lba as usize).saturating_sub(delta);
        prop_assert_eq!(plan_chunk(byte_off, remaining.max(1), lba, too_small), None);
        // A slot size that is not a whole number of LBAs.
        let misaligned_slot = lba as usize + lba as usize / 2;
        prop_assert_eq!(plan_chunk(byte_off, remaining.max(1), lba, misaligned_slot), None);
        // The NLB field is 16 bits; one command may not ask for more LBAs.
        let too_many_lbas = (MAX_NLB as usize + 1) * lba as usize;
        prop_assert_eq!(
            plan_chunk(byte_off, too_many_lbas, lba, too_many_lbas),
            None
        );
    }

    /// SQ tail wraps exactly at the queue size.
    #[test]
    fn sq_tail_wraps(entries in 1u32..64, steps in 0usize..2000) {
        let mut ring = SqRing::new(entries);
        for i in 0..steps {
            prop_assert_eq!(ring.tail(), (i as u32) % entries);
            ring.advance();
        }
    }

    /// Fresh CQ entries are accepted, stale ones (the previous phase) are not,
    /// and the phase flips exactly when the head wraps.
    #[test]
    fn cq_phase_flips_only_on_wrap(entries in 1u32..64, rounds in 0usize..(256usize)) {
        let mut ring = CqRing::new(entries);
        let mut head = 0u32;
        let mut phase = 1u32;
        for _ in 0..rounds {
            prop_assert_eq!(ring.head(), head);
            prop_assert_eq!(ring.phase(), phase);
            prop_assert!(ring.ready(phase as u16));
            prop_assert!(!ring.ready((phase ^ 1) as u16));
            prop_assert_eq!(ring.consume(), head);
            head += 1;
            if head == entries {
                head = 0;
                phase ^= 1;
            }
        }
    }

    /// After a full pass every slot still carries the old phase; a poller with
    /// the new phase must reject all of them until the controller writes again.
    #[test]
    fn stale_cq_entries_are_not_ready(entries in 1u32..64) {
        let mut ring = CqRing::new(entries);
        for _ in 0..entries {
            prop_assert!(ring.ready(ring.phase() as u16));
            let _ = ring.consume();
        }
        let stale_phase = ring.phase() ^ 1;
        prop_assert!(!ring.ready(stale_phase as u16));
        prop_assert!(ring.ready(ring.phase() as u16));
    }

    /// CAP.MQES clamping yields the largest power of two within both bounds.
    #[test]
    fn queue_size_is_the_largest_valid_power_of_two(
        desired in 0u32..2048,
        supported in 0u32..2048,
    ) {
        let got = queue_entries(desired, supported);
        let bound = desired.min(supported);
        if bound == 0 {
            prop_assert_eq!(got, 0);
        } else {
            prop_assert!(got.is_power_of_two());
            prop_assert!(got <= bound);
            prop_assert!((got as u64) * 2 > bound as u64);
        }
    }
}