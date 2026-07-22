// Feature: linux-binary-compat, Property 1: Syscall argument marshalling is the Linux ABI permutation

use crate::abi::marshal_args;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any six register values placed in `(rax, rdi, rsi, rdx, r10, r8, r9)`,
    /// marshalling yields `nr == rax` and `args == [rdi, rsi, rdx, r10, r8, r9]`
    /// in that exact order.
    #[test]
    fn marshal_is_linux_abi_permutation(
        rax in any::<u64>(),
        rdi in any::<u64>(),
        rsi in any::<u64>(),
        rdx in any::<u64>(),
        r10 in any::<u64>(),
        r8 in any::<u64>(),
        r9 in any::<u64>(),
    ) {
        let (nr, args) = marshal_args(rax, rdi, rsi, rdx, r10, r8, r9);

        prop_assert_eq!(nr, rax);
        prop_assert_eq!(args, [rdi, rsi, rdx, r10, r8, r9]);
    }
}
