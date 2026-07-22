// Feature: linux-binary-compat, Property 18: stack construction fails cleanly when inputs do not fit

use crate::stack::{build_initial_stack, AuxInputs, StackError};
use proptest::prelude::*;

// A deliberately tiny stack window. The fixed pointer/auxv table alone is
// (1 + N + 1 + M + 1 + 14) * 8 bytes — at minimum 136 bytes — plus 16 random
// bytes, so any window this small cannot hold the image.
const STACK_TOP: u64 = 0x10_0000;

fn token() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(1u8..=255u8, 1..16)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// When the encoded image cannot fit in `[stack_low, stack_top)`,
    /// `build_initial_stack` returns `Err(StackError::TooLarge)` and no image.
    #[test]
    fn stack_too_large_is_rejected(
        window in 0u64..=64,
        argv in prop::collection::vec(token(), 1..8),
        envp in prop::collection::vec(token(), 0..8),
        phdr in any::<u64>(),
        phent in any::<u64>(),
        phnum in any::<u64>(),
        entry in any::<u64>(),
        pagesz in any::<u64>(),
        random16 in any::<[u8; 16]>(),
    ) {
        let stack_low = STACK_TOP - window;
        let argv_refs: Vec<&[u8]> = argv.iter().map(|v| v.as_slice()).collect();
        let envp_refs: Vec<&[u8]> = envp.iter().map(|v| v.as_slice()).collect();
        let aux = AuxInputs { phdr, phent, phnum, entry, pagesz, random_ptr: 0 };

        let result =
            build_initial_stack(STACK_TOP, stack_low, &argv_refs, &envp_refs, &aux, random16);

        prop_assert_eq!(result.err(), Some(StackError::TooLarge));
    }
}
