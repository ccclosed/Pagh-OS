// Feature: linux-binary-compat, Property 25: tar write/read round-trip preserves names and content

use crate::tar::{read_tar, write_tar};
use proptest::prelude::*;
use std::collections::BTreeMap;

/// Generate `(name, content)` entries with unique, ustar-valid names
/// (non-empty, <= 100 bytes, no NUL).
fn entries_strategy() -> impl Strategy<Value = Vec<(String, Vec<u8>)>> {
    prop::collection::vec(
        (
            "[A-Za-z0-9_./-]{1,40}",
            prop::collection::vec(any::<u8>(), 0..300),
        ),
        0..8,
    )
    .prop_map(|mut v| {
        let mut seen = std::collections::HashSet::new();
        v.retain(|(name, _)| seen.insert(name.clone()));
        v
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any set of named regular-file entries, `read_tar(write_tar(entries))`
    /// yields the same set of names with byte-for-byte identical content.
    #[test]
    fn tar_write_read_round_trips(entries in entries_strategy()) {
        let refs: Vec<(&str, &[u8])> =
            entries.iter().map(|(n, c)| (n.as_str(), c.as_slice())).collect();

        let stream = write_tar(&refs);
        let parsed = read_tar(&stream).expect("a write_tar stream must read back");

        let expected: BTreeMap<String, Vec<u8>> =
            entries.iter().map(|(n, c)| (n.clone(), c.clone())).collect();
        let actual: BTreeMap<String, Vec<u8>> = parsed
            .iter()
            .map(|e| (e.path.to_string(), e.content.to_vec()))
            .collect();

        prop_assert_eq!(actual, expected);
    }
}
