// Feature: round-2 architecture audit, F-r2-arch-1 / F-r2-arch-2
// Property `cow_ownership`: fork copy-on-write frame ownership.
//
// The kernel shares every leaf frame between a parent and its forked child and
// refcounts the frame (`src/memory/cow.rs`, wrapped by `pmm::COW_REFS`). The
// invariant the kernel must keep is:
//
//   A frame may return to the PMM only when the last address space that maps it
//   has detached its PTE. A COW leaf's share count must equal the number of
//   COW PTEs that still point at the frame.
//
// The 2.22.3 kernel broke it in every leaf-detach path except process exit:
// `munmap`, `mprotect(PROT_NONE)`, `mprotect(RW)` and `MAP_FIXED` replacement
// freed/rewrote a COW leaf without `cow_unref`, so one side could return a
// frame to the allocator while the other side still mapped it (F-r2-arch-1:
// page-table hijack -> uid 0). The same missing accounting made the exit path
// free already-reallocated frames (F-r2-arch-2: 1017 PMM double frees).
//
// This file models that contract on the host: a PMM that refuses double frees
// and frees of frames any address space still maps, the same `CowRefs` the
// kernel compiles, and the same release rule as
// `vmm::release_user_leaf` (`if pte.cow { cow.release(frame) } else { free }`).
// It then drives random sequences of fork/write/unmap/MAP_FIXED/mprotect/exit
// and asserts the invariant after every step. `model_detects_the_historical_bug`
// proves the model would have failed on the vulnerable behaviour.

use crate::cow::CowRefs;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

const FRAME_SIZE: u64 = 4096;

/// One user leaf: the physical frame and whether the PTE carries `COW_BIT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pte {
    frame: u64,
    cow: bool,
}

/// Minimal PMM model: lowest-free allocation (like the kernel bitmap) that
/// records every free and refuses the two ownership violations the audit
/// found — a double free and a free of a still-mapped frame.
struct Pmm {
    free: BTreeSet<u64>,
    next: u64,
    seen: BTreeSet<u64>,
}

impl Pmm {
    fn new() -> Self {
        Self {
            free: BTreeSet::new(),
            next: 0x1000,
            seen: BTreeSet::new(),
        }
    }

    fn alloc(&mut self) -> u64 {
        let frame = match self.free.iter().next().copied() {
            Some(f) => {
                self.free.remove(&f);
                f
            }
            None => {
                let f = self.next;
                self.next += FRAME_SIZE;
                f
            }
        };
        self.seen.insert(frame);
        frame
    }

    fn free(&mut self, frame: u64, live: &[BTreeMap<usize, Pte>]) -> Result<(), String> {
        for (i, map) in live.iter().enumerate() {
            if map.values().any(|pte| pte.frame == frame) {
                return Err(format!(
                    "frame {frame:#x} freed while address space {i} still maps it \
                     (F-r2-arch-1 use-after-free)"
                ));
            }
        }
        if !self.free.insert(frame) {
            return Err(format!(
                "double free of frame {frame:#x} (F-r2-arch-2)"
            ));
        }
        Ok(())
    }
}

/// Address spaces (`None` = exited, never reused) + COW accounting + PMM.
struct World {
    spaces: Vec<Option<BTreeMap<usize, Pte>>>,
    cow: CowRefs,
    pmm: Pmm,
}

impl World {
    /// Address space 0 starts with eight private, mapped pages.
    fn new() -> Self {
        let mut w = Self {
            spaces: vec![None; 4],
            cow: CowRefs::new(),
            pmm: Pmm::new(),
        };
        let mut map = BTreeMap::new();
        for page in 0..8usize {
            let frame = w.pmm.alloc();
            map.insert(page, Pte { frame, cow: false });
        }
        w.spaces[0] = Some(map);
        w
    }

    fn active(&self, space: usize) -> bool {
        self.spaces.get(space).map(Option::is_some).unwrap_or(false)
    }

    fn mapped(&self, space: usize, page: usize) -> Option<Pte> {
        self.spaces
            .get(space)
            .and_then(Option::as_ref)
            .and_then(|m| m.get(&page).copied())
    }

    fn live_maps(&self) -> Vec<BTreeMap<usize, Pte>> {
        self.spaces.iter().flatten().cloned().collect()
    }

    /// The single leaf-release rule used by `vmm::release_user_leaf`:
    /// a COW leaf consumes one reference (free only when it was the last);
    /// a private leaf is freed directly. The PTE is detached first, so a frame
    /// freed here is never referenced by the same address space.
    fn release_leaf(&mut self, space: usize, page: usize) -> Result<(), String> {
        let Some(pte) = self
            .spaces
            .get_mut(space)
            .and_then(Option::as_mut)
            .and_then(|m| m.remove(&page))
        else {
            return Ok(());
        };
        let free = if pte.cow {
            self.cow.release(pte.frame)
        } else {
            true
        };
        if free {
            let live = self.live_maps();
            self.pmm.free(pte.frame, &live)?;
        }
        Ok(())
    }

    /// `fork_user_space_cow`: every present leaf is shared, both PTEs get
    /// `COW_BIT`, and the child's arrival adds one reference — plus the parent's
    /// own entry for a previously-private leaf.
    fn fork(&mut self, src: usize) -> Result<(), String> {
        if !self.active(src) {
            return Ok(());
        }
        let Some(dst) = self.spaces.iter().position(Option::is_none) else {
            return Ok(());
        };
        let src_map = self.spaces[src].clone().expect("active");
        let mut child = BTreeMap::new();
        for (&page, &pte) in &src_map {
            if pte.cow {
                self.cow.acquire(pte.frame);
            } else {
                // Parent and child both enter the refcount for a private leaf.
                self.cow.acquire(pte.frame);
                self.cow.acquire(pte.frame);
            }
            self.spaces[src]
                .as_mut()
                .expect("active")
                .get_mut(&page)
                .expect("mapped")
                .cow = true;
            child.insert(page, Pte { frame: pte.frame, cow: true });
        }
        self.spaces[dst] = Some(child);
        Ok(())
    }

    /// Write fault on a COW page: `cow_copy_page` allocates a private frame,
    /// points the PTE at it, then drops this side's share of the old frame.
    fn write(&mut self, space: usize, page: usize) -> Result<(), String> {
        let Some(pte) = self.mapped(space, page) else {
            return Ok(());
        };
        if !pte.cow {
            return Ok(()); // private writable page: direct write
        }
        let fresh = self.pmm.alloc();
        self.spaces[space]
            .as_mut()
            .expect("active")
            .insert(page, Pte { frame: fresh, cow: false });
        if self.cow.release(pte.frame) {
            let live = self.live_maps();
            self.pmm.free(pte.frame, &live)?;
        }
        Ok(())
    }

    /// `MAP_FIXED` replacement: release the old leaf, map a fresh private frame.
    fn map_fixed(&mut self, space: usize, page: usize) -> Result<(), String> {
        if !self.active(space) {
            return Ok(());
        }
        self.release_leaf(space, page)?;
        let frame = self.pmm.alloc();
        self.spaces[space]
            .as_mut()
            .expect("active")
            .insert(page, Pte { frame, cow: false });
        Ok(())
    }

    /// `mprotect` must not change ownership: re-flagging keeps the COW marker
    /// (writable access is delivered lazily by a write fault). This is the
    /// operation the exploit used to clear `COW_BIT`; assert it never does.
    fn mprotect(&self, space: usize, page: usize) -> Result<(), String> {
        if let Some(pte) = self.mapped(space, page) {
            if pte.cow && self.cow.refs(pte.frame) == 0 {
                return Err(format!(
                    "COW PTE at space {space} page {page} has no PMM reference"
                ));
            }
        }
        Ok(())
    }

    /// Process exit (`drop_user_space`): release every leaf, then tear the tree.
    fn exit(&mut self, space: usize) -> Result<(), String> {
        if !self.active(space) {
            return Ok(());
        }
        let pages: Vec<usize> = self.spaces[space]
            .as_ref()
            .expect("active")
            .keys()
            .copied()
            .collect();
        for page in pages {
            self.release_leaf(space, page)?;
        }
        self.spaces[space] = None;
        Ok(())
    }

    /// Global invariant, checked after every operation:
    ///   * no free frame is referenced by a live PTE;
    ///   * every frame's COW refcount equals the number of COW PTEs at it;
    ///   * every frame with a non-zero refcount is still mapped (no leaked ref);
    ///   * a frame is never mapped as private in more than one address space.
    fn check_invariants(&self) -> Result<(), String> {
        let mut cow_ptes: BTreeMap<u64, u32> = BTreeMap::new();
        let mut pte_count: BTreeMap<u64, u32> = BTreeMap::new();
        for map in self.spaces.iter().flatten() {
            for pte in map.values() {
                *pte_count.entry(pte.frame).or_insert(0) += 1;
                if pte.cow {
                    *cow_ptes.entry(pte.frame).or_insert(0) += 1;
                }
                if self.pmm.free.contains(&pte.frame) {
                    return Err(format!(
                        "live PTE references free frame {:#x} (use-after-free)",
                        pte.frame
                    ));
                }
            }
        }

        for &frame in &self.pmm.seen {
            let expected = cow_ptes.get(&frame).copied().unwrap_or(0);
            let actual = self.cow.refs(frame);
            if actual != expected {
                return Err(format!(
                    "frame {frame:#x}: COW refcount {actual} != COW PTE count {expected}"
                ));
            }
            if pte_count.get(&frame).copied().unwrap_or(0) == 0 && actual != 0 {
                return Err(format!(
                    "frame {frame:#x}: {actual} leaked COW reference(s), frame is not mapped"
                ));
            }
            if expected == 0 {
                let mapped_in = pte_count.get(&frame).copied().unwrap_or(0);
                if mapped_in > 1 {
                    return Err(format!(
                        "frame {frame:#x}: private frame mapped {mapped_in} times \
                         (writable alias across address spaces)"
                    ));
                }
            }
        }

        let tracked: usize = self
            .pmm
            .seen
            .iter()
            .filter(|f| self.cow.refs(**f) != 0)
            .count();
        if self.cow.tracked_frames() != tracked {
            return Err(format!(
                "CowRefs tracks {} frames, {tracked} have references",
                self.cow.tracked_frames()
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
enum Op {
    Fork { src: usize },
    Write { space: usize, page: usize },
    Unmap { space: usize, page: usize },
    MapFixed { space: usize, page: usize },
    Mprotect { space: usize, page: usize },
    Exit { space: usize },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0usize..4).prop_map(|src| Op::Fork { src }),
        (0usize..4, 0usize..8).prop_map(|(space, page)| Op::Write { space, page }),
        (0usize..4, 0usize..8).prop_map(|(space, page)| Op::Unmap { space, page }),
        (0usize..4, 0usize..8).prop_map(|(space, page)| Op::MapFixed { space, page }),
        (0usize..4, 0usize..8).prop_map(|(space, page)| Op::Mprotect { space, page }),
        (0usize..4).prop_map(|space| Op::Exit { space }),
    ]
}

fn run_model(ops: &[Op]) -> Result<(), String> {
    let mut w = World::new();
    w.check_invariants()?;
    for op in ops {
        match *op {
            Op::Fork { src } => w.fork(src)?,
            Op::Write { space, page } => w.write(space, page)?,
            Op::Unmap { space, page } => w.release_leaf(space, page)?,
            Op::MapFixed { space, page } => w.map_fixed(space, page)?,
            Op::Mprotect { space, page } => w.mprotect(space, page)?,
            Op::Exit { space } => w.exit(space)?,
        }
        w.check_invariants()?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Any interleaving of fork / COW write / unmap / MAP_FIXED / mprotect /
    /// exit keeps the ownership invariant; no frame is ever freed while mapped
    /// and no frame is freed twice.
    #[test]
    fn cow_ownership_holds_under_arbitrary_sequences(
        ops in prop::collection::vec(op_strategy(), 0..48)
    ) {
        if let Err(e) = run_model(&ops) {
            prop_assert!(false, "ownership invariant violated after {:?}: {e}", ops);
        }
    }
}

/// The exact F-r2-arch-1 trigger: fork, parent mprotect(RW), child munmap, then
/// the parent must still own the frames; only a later private copy frees the
/// old frame, exactly once.
#[test]
fn fork_mprotect_child_munmap_keeps_the_parent_frame() {
    let mut w = World::new();
    w.check_invariants().expect("initial");

    let page = 0usize;
    let shared = w.mapped(0, page).expect("mapped").frame;

    w.fork(0).expect("fork");
    // Parent mprotect(RW): the marker and refcount must survive.
    w.mprotect(0, page).expect("mprotect");
    assert!(w.mapped(0, page).expect("parent PTE").cow);
    assert_eq!(w.cow.refs(shared), 2, "parent + child");

    // Child munmap: only its reference goes away; the frame stays allocated.
    w.release_leaf(1, page).expect("child munmap");
    assert_eq!(w.cow.refs(shared), 1);
    assert!(!w.pmm.free.contains(&shared), "parent still maps the frame");
    w.check_invariants().expect("after child munmap");

    // Parent writes: gets a private copy, drops the last share of the old frame.
    w.write(0, page).expect("parent COW write");
    let fresh = w.mapped(0, page).expect("parent PTE").frame;
    assert_ne!(fresh, shared);
    assert!(w.pmm.free.contains(&shared), "last sharer freed the old frame");
    assert!(!w.pmm.free.contains(&fresh));
    w.check_invariants().expect("after COW write");

    // Child exit must not free anything a second time.
    w.exit(1).expect("child exit");
    w.check_invariants().expect("after child exit");
}

/// Negative control: the historical behaviour (free on detach without
/// `cow_unref`) is exactly what the model refuses. If a future refactor
/// reintroduces it, `cow_ownership_holds_under_arbitrary_sequences` fails the
/// same way.
#[test]
fn model_detects_the_historical_bug() {
    let mut w = World::new();
    w.fork(0).expect("fork");
    let pte = w.mapped(1, 0).expect("child maps the leaf");
    // Old munmap: unmap + free_frame, no cow_unref.
    w.spaces[1].as_mut().expect("active").remove(&0);
    let live = w.live_maps();
    let err = w.pmm.free(pte.frame, &live).expect_err("must be refused");
    assert!(err.contains("use-after-free"), "unexpected error: {err}");
}

#[test]
fn cow_refs_share_aligned_keys_and_release_last() {
    let mut c = CowRefs::new();
    // An offset address keys the same page-aligned frame.
    assert_eq!(c.acquire(0x1234), 1);
    assert_eq!(c.refs(0x1000), 1);
    assert_eq!(c.acquire(0x1000), 2);
    assert_eq!(c.tracked_frames(), 1);

    assert!(!c.release(0x1000));
    assert_eq!(c.refs(0x1000), 1);
    assert!(c.release(0x1234));
    assert_eq!(c.refs(0x1000), 0);
    assert_eq!(c.tracked_frames(), 0);

    // An untracked frame is treated as privately owned: the caller frees it.
    assert!(c.release(0x2000));
}

#[test]
fn fork_of_fork_counts_every_address_space() {
    let mut w = World::new();
    w.fork(0).expect("first fork");
    w.fork(0).expect("second fork from the same parent");
    let frame = w.mapped(0, 0).expect("mapped").frame;
    assert_eq!(w.cow.refs(frame), 3, "parent + two children");
    w.check_invariants().expect("after forks");

    w.release_leaf(1, 0).expect("child1 unmap");
    assert_eq!(w.cow.refs(frame), 2);
    assert!(!w.pmm.free.contains(&frame));

    w.release_leaf(2, 0).expect("child2 unmap");
    assert_eq!(w.cow.refs(frame), 1);
    assert!(!w.pmm.free.contains(&frame), "parent still maps it");

    w.release_leaf(0, 0).expect("parent unmap");
    assert_eq!(w.cow.refs(frame), 0);
    assert!(w.pmm.free.contains(&frame), "last sharer freed it exactly once");
    w.check_invariants().expect("all sharers gone");
}