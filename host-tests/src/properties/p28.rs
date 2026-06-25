// Feature: linux-binary-compat, Property 28: the reported exit code is the low byte of the requested code

use crate::diag::exit_code_byte;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any requested exit code, the normalized code equals `code & 0xFF` and
    /// therefore lies in `0..=255`.
    #[test]
    fn exit_code_is_low_byte(code in any::<u64>()) {
        let byte = exit_code_byte(code);

        // Equals the low 8 bits of the requested code.
        prop_assert_eq!(byte as u64, code & 0xFF);

        // A `u8` is inherently within 0..=255; assert the documented range too.
        prop_assert!((0..=255).contains(&(byte as u16)));
    }
}
