// Bug: the disk menu must be asked ONCE per machine — the `#136` fix skipped it
// whenever a pagh volume existed, which on an image built by `tools/mkimage.py`
// (a provisioned volume on the first boot) meant it was never shown at all:
// "why do I have NO mount-selection menu, I asked for it once, not 4334 times".
//
// The decision therefore rests on a recorded CHOICE, not on a volume being
// present (`src/fs/mount_choice.rs`). These properties pin the two halves that
// can go wrong silently:
//
//   * the record round-trips — the sector written on the volume reads back as
//     the same action and device, so the next boot can skip the menu;
//   * nothing else reads as a record. Any single-bit damage, a boot/GPT sector,
//     random bytes or a valid record with a non-zero tail must decode to `None`,
//     because `Some(..)` suppresses the question: a false positive decides a
//     mount the operator never chose, and on a foreign disk that means writing
//     into somebody's filesystem (`boot::init_fs` mounts what the marker names).

use crate::mount_choice::*;
use proptest::prelude::*;

/// Device names the boot path actually produces: `virtio-blk0p1`, `nvme0p1`,
/// `ram0`, `virtio-blk0`. Generated from the same alphabet `Choice::new`
/// validates, so a rejected name in the round-trip test means the writer and the
/// reader disagree about what is storable.
fn device_name() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        proptest::sample::select(
            b"abcdefghijklmnopqrstuvwxyz0123456789-_."
                .iter()
                .copied()
                .collect::<Vec<u8>>(),
        ),
        1..=NAME_MAX,
    )
    .prop_map(|bytes| String::from_utf8(bytes).expect("generated from ASCII"))
}

fn any_action() -> impl Strategy<Value = Action> {
    proptest::sample::select(vec![Action::Mount, Action::Format])
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// A recorded choice survives the write/read trip, and `menu_required` is
    /// exactly "no record": `Some` skips the menu, `None` asks.
    #[test]
    fn a_recorded_choice_round_trips(action in any_action(), name in device_name()) {
        let choice = Choice::new(action, &name).expect("valid device name is storable");
        let mut sector = [0u8; SECTOR_BYTES];
        prop_assert!(choice.encode(&mut sector));

        let read = Choice::decode(&sector).expect("a sector we wrote decodes");
        prop_assert_eq!(read.action(), action);
        prop_assert_eq!(read.device(), name.as_str());
        prop_assert!(!menu_required(Some(&read)));
        prop_assert!(menu_required(None));

        // Only the marker sector is meaningful; a truncated buffer is not a
        // marker even when its first bytes would match.
        prop_assert!(Choice::decode(&sector[..SECTOR_BYTES - 1]).is_none());
    }

    /// Writing twice is idempotent: the same choice encodes to the same bytes.
    /// `boot_menu::record_choice` calls `write_once` semantics on this, so an
    /// encoder that changed bytes between calls would rewrite the history.
    #[test]
    fn encoding_the_same_choice_is_stable(action in any_action(), name in device_name()) {
        let choice = Choice::new(action, &name).expect("valid device name is storable");
        let mut first = [0u8; SECTOR_BYTES];
        let mut second = [0xFFu8; SECTOR_BYTES];
        prop_assert!(choice.encode(&mut first));
        prop_assert!(choice.encode(&mut second));
        prop_assert_eq!(first, second);
    }

    /// Any single-bit damage to the record is rejected, never silently read as a
    /// (possibly different) choice. The checksum covers every byte the reader
    /// interprets besides the version/action bytes checked explicitly.
    #[test]
    fn one_flipped_bit_never_reads_as_a_choice(
        action in any_action(),
        name in device_name(),
        byte in 0usize..SECTOR_BYTES,
        bit in 0u8..8,
    ) {
        let choice = Choice::new(action, &name).expect("valid device name is storable");
        let mut sector = [0u8; SECTOR_BYTES];
        prop_assert!(choice.encode(&mut sector));
        sector[byte] ^= 1u8 << bit;
        // Flipping a bit in the zeroed tail is rejected by the tail rule, a flip
        // in the record by magic/version/action/charset/checksum.
        prop_assert!(Choice::decode(&sector).is_none());
    }

    /// A boot sector, a GPT header or arbitrary data is not a recorded choice:
    /// the menu is asked again rather than suppressed.
    #[test]
    fn foreign_sectors_are_not_choices(seed in any::<u64>(), len in 0usize..=SECTOR_BYTES) {
        let mut state = seed | 1;
        let random: Vec<u8> = (0..SECTOR_BYTES)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state & 0xFF) as u8
            })
            .collect();
        prop_assert!(Choice::decode(&random).is_none());

        let mut gpt = [0u8; SECTOR_BYTES];
        gpt[..8].copy_from_slice(b"EFI PART");
        prop_assert!(Choice::decode(&gpt).is_none());
        prop_assert!(Choice::decode(&gpt[..len]).is_none());
    }

    /// A valid record followed by anything in the part of the sector the marker
    /// leaves zeroed is not our marker: the writer owns the whole 512 bytes, so
    /// leftovers there mean a foreign or half-written sector.
    #[test]
    fn a_dirty_tail_is_not_a_choice(action in any_action(), name in device_name(), junk in 1u8..=255) {
        let choice = Choice::new(action, &name).expect("valid device name is storable");
        let mut sector = [0u8; SECTOR_BYTES];
        prop_assert!(choice.encode(&mut sector));
        sector[SECTOR_BYTES - 1] = junk;
        prop_assert!(Choice::decode(&sector).is_none());
    }

    /// Names that cannot be stored never become a record — the caller must fail
    /// the boot's marker write (and keep asking) instead of writing a sector the
    /// reader would reject.
    #[test]
    fn unstorable_names_are_refused(name in ".{0,80}") {
        let storable = !name.is_empty()
            && name.len() <= NAME_MAX
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        match Choice::new(Action::Mount, &name) {
            Some(choice) => {
                prop_assert!(storable);
                let mut sector = [0u8; SECTOR_BYTES];
                prop_assert!(choice.encode(&mut sector));
                prop_assert_eq!(
                    Choice::decode(&sector).map(|c| c.device().to_string()),
                    Some(name.clone())
                );
            }
            None => prop_assert!(!storable),
        }
    }

    /// An unknown action byte is a corrupt record, not a guess: the reader must
    /// not invent `Mount` for it. Only `1`/`2` are actions, and the checksum
    /// covers the action byte too, so a patched record is rejected even when the
    /// patched value happens to be the other valid action (a silent reversal of
    /// the operator's answer would otherwise mount/format the wrong thing).
    #[test]
    fn unknown_actions_are_not_guesses(name in device_name(), byte in 0u8..=255) {
        prop_assert_eq!(Action::from_byte(byte).is_some(), byte == 1 || byte == 2);
        let choice = Choice::new(Action::Mount, &name).expect("valid device name is storable");
        let mut sector = [0u8; SECTOR_BYTES];
        prop_assert!(choice.encode(&mut sector));
        // Byte 17 is the action field of the documented layout (magic 0..16,
        // version 16, action 17, name length 18..20, name 20..52, checksum 52..56).
        sector[17] = byte;
        if byte == Action::Mount.as_byte() {
            prop_assert!(Choice::decode(&sector).is_some());
        } else {
            prop_assert!(Choice::decode(&sector).is_none());
        }
    }
}

/// The action byte offsets and name length are part of the on-disk layout: a
/// change here silently reinterprets markers written by an older kernel.
#[test]
fn layout_constants_are_the_documented_ones() {
    assert_eq!(NAME_MAX, 32);
    assert_eq!(MARKER_SECTOR, 1);
    assert_eq!(SECTOR_BYTES, 512);
    assert_eq!(Action::Mount.as_byte(), 1);
    assert_eq!(Action::Format.as_byte(), 2);
    assert_eq!(Action::Mount.name(), "mount");
    assert_eq!(Action::Format.name(), "format");
}