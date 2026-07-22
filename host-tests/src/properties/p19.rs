// Feature: linux-binary-compat, Property 19: the run-request argument gate enforces the count and byte limits

use crate::stack::arg_gate;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// `arg_gate(argv)` is true iff `argv` has at most 256 entries and the combined
    /// byte length of all arguments is at most 4096.
    #[test]
    fn arg_gate_enforces_count_and_byte_limits(
        // Generate up to 300 argument lengths (each up to 40 bytes) so cases land
        // on both sides of the 256-count and 4096-byte boundaries.
        lens in prop::collection::vec(0usize..40, 0..300),
    ) {
        let args: Vec<Vec<u8>> = lens.iter().map(|&n| vec![b'a'; n]).collect();
        let arg_refs: Vec<&[u8]> = args.iter().map(|v| v.as_slice()).collect();

        let total: usize = lens.iter().sum();
        let expected = lens.len() <= 256 && total <= 4096;

        prop_assert_eq!(arg_gate(&arg_refs), expected);
    }
}
