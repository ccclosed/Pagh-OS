// Feature: ext2 links (issue #18) — host properties for the pure symlink
// layout + hard-link accounting core (`src/fs/ext2/symlink.rs`), the rules the
// kernel writer must follow exactly (contract `EXT2-LINKS.md` §2, §3, §8).
//
// The properties are deliberately format-level, not example-level:
//
//   * the fast/slow boundary is `target.len() + 1 <= 60` (the NUL must fit in
//     the inode's `i_block`), so 59 bytes stay inline and 60 bytes need a data
//     block — e2fsprogs 1.47.3 and Linux agree, and getting it wrong silently
//     loses the last character of a 60-byte target on a Linux mount;
//   * the inline image is the target verbatim plus NUL padding, and the
//     byte<->`u32` reshape into `i_block` is lossless for *any* 60 bytes (a link
//     target is not necessarily UTF-8: it comes from an untrusted archive);
//   * `unlink` frees an inode's blocks only when the removed name was the last
//     one — otherwise data a surviving name still reads would be released;
//   * `i_links_count` is authoritative on this filesystem (there is no fsck, and
//     neither `recover()` nor `reconcile_free_counts` repairs it), so the count
//     must equal the number of names pointing at the inode after every
//     operation, and a free inode must never be linkable again.

use crate::ext2_symlink::*;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Layout selection over target lengths 0..=300: only an empty target is
    /// rejected; `1..=59` is fast, `60..` is slow. A fast plan carries the
    /// target verbatim followed by a NUL and zero padding, `size` is the target
    /// length, and reading the image back at `size` returns exactly the target.
    #[test]
    fn plan_picks_layout_and_renders_inline(len in 0usize..=300, fill in any::<u8>()) {
        let target: Vec<u8> = vec![fill; len];
        match plan(&target) {
            Err(SymlinkError::Empty) => prop_assert_eq!(len, 0),
            Err(SymlinkError::TooBig) => prop_assert!(false, "a 300-byte target is not too big"),
            Ok(p) => {
                prop_assert!(len > 0);
                prop_assert_eq!(p.size, len);
                if len <= FAST_MAX_TARGET {
                    prop_assert_eq!(p.layout, SymlinkLayout::Fast);
                    let inline = p.inline.expect("fast plan carries an inline image");
                    prop_assert_eq!(&inline[..len], &target[..]);
                    prop_assert_eq!(inline[len], 0, "inline image is NUL terminated");
                    for b in &inline[len + 1..] {
                        prop_assert_eq!(*b, 0, "padding after the NUL stays zero");
                    }
                    prop_assert_eq!(fast_target(&inline, len as u32), Some(&target[..]));
                    prop_assert!(is_fast(0, len as u32));
                } else {
                    prop_assert_eq!(p.layout, SymlinkLayout::Slow);
                    prop_assert!(p.inline.is_none());
                    prop_assert!(!is_fast(8, len as u32));
                }
            }
        }
    }

    /// The byte<->`u32` reshape of the inline image is lossless for arbitrary
    /// 60-byte contents, and a planned fast link survives the trip the kernel
    /// takes it through (`inline_words` into `i_block`, `inline_bytes` back).
    #[test]
    fn inline_words_roundtrip(bytes in prop::collection::vec(any::<u8>(), INLINE_CAPACITY)) {
        let mut inline = [0u8; INLINE_CAPACITY];
        inline.copy_from_slice(&bytes);

        let words = inline_words(&inline);
        prop_assert_eq!(words.len(), INLINE_WORDS);
        prop_assert_eq!(inline_bytes(&words), inline);

        let target = &inline[..FAST_MAX_TARGET];
        let p = plan(target).expect("59 bytes is a valid target");
        let image = p.inline.expect("fast plan");
        let restored = inline_bytes(&inline_words(&image));
        prop_assert_eq!(restored, image);
        prop_assert_eq!(fast_target(&restored, p.size as u32), Some(target));
    }

    /// `is_fast` is exactly `i_blocks == 0 && i_size <= 60`, and the reader never
    /// hands out more than `i_size` bytes (or more than the 60 inline bytes).
    #[test]
    fn fast_read_is_bounded(i_blocks in 0u32..4, i_size in 0u32..80, seed in any::<u8>()) {
        let inline = [seed; INLINE_CAPACITY];
        prop_assert_eq!(is_fast(i_blocks, i_size), i_blocks == 0 && i_size <= 60);
        match fast_target(&inline, i_size) {
            Some(t) => prop_assert_eq!(t.len(), i_size as usize),
            None => prop_assert!(i_size > 60),
        }
    }

    /// A crafted symlink inode can never drive an unbounded allocation or a
    /// block-map interpretation (security batch after 2.22.0):
    ///
    ///   * F-x86v-1: `read_symlink` sized its output from `i_size` alone, so a
    ///     slow link with `i_size = 0xFFFFFFF0` asked for a 4 GiB `Vec` and
    ///     aborted the kernel. The shape classifier bounds every accepted
    ///     target: fast ≤ 60 inline bytes, slow ≤ one 4096-byte block;
    ///   * F-x86v-4: `unlink` picked the slow branch from `i_size`, so an
    ///     `i_blocks == 0, i_size > 60` inode had its target text read as a
    ///     block number and freed that block. `owns_data_block` decides from
    ///     `i_blocks` only, so an inline inode owns nothing to free.
    #[test]
    fn crafted_symlink_shape_is_bounded(i_blocks in any::<u32>(), i_size in any::<u32>()) {
        match target_shape(i_blocks, i_size) {
            TargetShape::Fast(len) => {
                prop_assert_eq!(i_blocks, 0);
                prop_assert!(len <= INLINE_CAPACITY);
                prop_assert!(is_fast(i_blocks, i_size));
            }
            TargetShape::Slow(len) => {
                prop_assert!(i_blocks != 0);
                prop_assert!(!is_fast(i_blocks, i_size));
                prop_assert!(len <= SLOW_MAX_TARGET);
            }
            TargetShape::Corrupt => {
                // Exactly the two inconsistent shapes: inline text claiming to
                // be longer than the inline capacity, or a slow target larger
                // than a block.
                prop_assert!(
                    (i_blocks == 0 && i_size as usize > INLINE_CAPACITY)
                        || (i_blocks != 0 && i_size as usize > SLOW_MAX_TARGET)
                );
            }
        }
        // The delete decision depends on `i_blocks` alone, never on `i_size`.
        prop_assert_eq!(owns_data_block(i_blocks), i_blocks != 0);
    }

    /// Hard-link accounting: after any sequence of link/unlink operations the
    /// count equals the number of names, an inode is freed exactly when its last
    /// name disappears, and `i_links_count` never wraps its 16-bit field.
    #[test]
    fn link_count_tracks_names(ops in prop::collection::vec((any::<bool>(), 0u8..6), 1..80)) {
        // Model of one inode: `count` names currently point at it.
        let mut count: u16 = 1; // created by its first name
        let mut names: Vec<u8> = vec![0];
        let mut freed = false;

        for (is_link, name) in ops {
            if is_link {
                // A link to a free inode, or a duplicate name, is refused and
                // must not change the count.
                if freed || names.contains(&name) {
                    continue;
                }
                let next = link_bump(count).expect("a live inode can take one more link");
                prop_assert_eq!(next, count + 1);
                count = next;
                names.push(name);
            } else {
                // No such name (or the inode is already gone): nothing happens.
                if freed || !names.contains(&name) {
                    continue;
                }
                match unlink_action(count) {
                    Some(UnlinkAction::DropLink) => {
                        prop_assert!(count > 1);
                        count -= 1;
                    }
                    Some(UnlinkAction::FreeInode) => {
                        prop_assert_eq!(count, 1);
                        // The writer releases the blocks/inode and zeroes the
                        // count (so a host tool never sees "1 link, no entry").
                        count = 0;
                        freed = true;
                    }
                    None => prop_assert!(false, "a live inode never has count 0"),
                }
                names.retain(|n| *n != name);
            }

            // Invariant I3: the count is exactly the number of names, and the
            // inode is free exactly when no name is left.
            prop_assert_eq!(count as usize, names.len());
            prop_assert_eq!(freed, names.is_empty());
            if freed {
                prop_assert_eq!(count, 0);
            }
        }

        // A free inode is never linkable or unlinkable again (the writer reports
        // `FsError::Corrupt` instead of touching a reallocated inode).
        prop_assert!(unlink_action(0).is_none());
        prop_assert!(link_bump(0).is_none());
        // The 16-bit field saturates instead of wrapping.
        prop_assert!(link_bump(u16::MAX).is_none());
        prop_assert_eq!(link_bump(u16::MAX - 1), Some(u16::MAX));
    }

    /// The unlink decision is a total function of the count: 1 frees, > 1 only
    /// drops a name, 0 is corruption.
    #[test]
    fn unlink_action_is_total(count in any::<u16>()) {
        match unlink_action(count) {
            None => prop_assert_eq!(count, 0),
            Some(UnlinkAction::DropLink) => prop_assert!(count > 1),
            Some(UnlinkAction::FreeInode) => prop_assert_eq!(count, 1),
        }
    }
}

/// The two crafted inodes from the audit report, verbatim, plus the valid
/// shapes on both sides of each boundary. These are the exact values the guest
/// PoCs patch into the ext2 image (F-x86v-1: `i_blocks = 8`,
/// `i_size = 0xFFFFFFF0`; F-x86v-4: `i_blocks = 0`, `i_size = 61`).
#[test]
fn audit_crafted_symlink_inodes_are_corrupt_not_allocations() {
    // F-x86v-1: used to reach `vec![0u8; 4_294_967_280]` and abort the kernel.
    assert_eq!(target_shape(8, 0xFFFF_FFF0), TargetShape::Corrupt);
    // F-x86v-4: used to free the block number spelled by this target text.
    assert_eq!(target_shape(0, 61), TargetShape::Corrupt);
    assert!(!owns_data_block(0));

    // Valid shapes: 60 bytes still fit inline (a foreign image may have it);
    // the writer itself only creates fast links up to 59 bytes.
    assert_eq!(target_shape(0, 0), TargetShape::Fast(0));
    assert_eq!(target_shape(0, 59), TargetShape::Fast(59));
    assert_eq!(target_shape(0, 60), TargetShape::Fast(60));
    assert_eq!(target_shape(8, 60), TargetShape::Slow(60));
    assert_eq!(
        target_shape(8, SLOW_MAX_TARGET as u32),
        TargetShape::Slow(SLOW_MAX_TARGET)
    );
    assert_eq!(
        target_shape(8, SLOW_MAX_TARGET as u32 + 1),
        TargetShape::Corrupt
    );
    // A slow inode owns exactly one block, whatever its target length.
    assert!(owns_data_block(8));
    assert!(owns_data_block(u32::MAX));
}
