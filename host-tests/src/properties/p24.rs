// Feature: linux-binary-compat, Property 24: tar enumeration parses fields correctly and rejects corrupt headers

use crate::tar::{read_tar, write_tar, TarError, TarType};
use proptest::prelude::*;

// ustar header field offsets within a 512-byte block (must match tar.rs).
const OFF_SIZE: usize = 124;
const OFF_CHKSUM: usize = 148;
const END_CHKSUM: usize = 156;

/// Recompute and write a valid ustar header checksum into `block[148..156)`,
/// matching `tar.rs`: the unsigned sum of all 512 bytes with the checksum field
/// counted as ASCII spaces, encoded as 6 octal digits, a NUL, then a space.
fn set_checksum(block: &mut [u8]) {
    let mut sum: u64 = 0;
    for (i, &b) in block.iter().enumerate().take(512) {
        if (OFF_CHKSUM..END_CHKSUM).contains(&i) {
            sum += 0x20;
        } else {
            sum += b as u64;
        }
    }
    block[154] = 0;
    block[155] = b' ';
    let mut v = sum;
    let mut pos = 154;
    while pos > OFF_CHKSUM {
        pos -= 1;
        block[pos] = b'0' + (v & 0o7) as u8;
        v >>= 3;
    }
}

/// Generate a list of `(name, content)` entries with unique, ustar-valid names.
fn entries_strategy() -> impl Strategy<Value = Vec<(String, Vec<u8>)>> {
    prop::collection::vec(
        (
            "[A-Za-z0-9_./-]{1,30}",
            prop::collection::vec(any::<u8>(), 0..256),
        ),
        1..6,
    )
    .prop_map(|mut v| {
        // Names must be unique so the read-back can be compared positionally.
        let mut seen = std::collections::HashSet::new();
        v.retain(|(name, _)| seen.insert(name.clone()));
        v
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// A valid ustar stream parses every field correctly, and an otherwise-valid
    /// stream with a single corrupted header field is rejected with the matching
    /// `Err` (never a panic, never an out-of-bounds read).
    #[test]
    fn tar_parses_fields_and_rejects_corruption(
        entries in entries_strategy(),
        corruption in 0u8..2,
    ) {
        let refs: Vec<(&str, &[u8])> =
            entries.iter().map(|(n, c)| (n.as_str(), c.as_slice())).collect();
        let buf = write_tar(&refs);

        // --- Valid stream: fields parse correctly ----------------------------
        let parsed = read_tar(&buf).expect("a write_tar stream must read back");
        prop_assert_eq!(parsed.len(), entries.len());
        for (got, (name, content)) in parsed.iter().zip(entries.iter()) {
            prop_assert_eq!(got.path, name.as_str());
            prop_assert_eq!(got.kind, TarType::Regular);
            prop_assert_eq!(got.mode, 0o644);
            prop_assert_eq!(got.size, content.len() as u64);
            prop_assert_eq!(got.content, content.as_slice());
        }

        // --- Single corrupted field in the first header -> matching Err -------
        let mut bad = buf.clone();
        let expected = match corruption {
            0 => {
                // Bad checksum: zero the stored checksum so it no longer matches.
                for b in bad[OFF_CHKSUM..END_CHKSUM].iter_mut() {
                    *b = b'0';
                }
                bad[154] = 0;
                bad[155] = b' ';
                TarError::BadHeaderChecksum
            }
            _ => {
                // Bad octal size: inject a non-octal digit, then re-sign the
                // header so the checksum passes and the size field is reached.
                bad[OFF_SIZE] = b'9';
                set_checksum(&mut bad[0..512]);
                TarError::BadSizeField
            }
        };

        prop_assert_eq!(read_tar(&bad), Err(expected));
    }
}
