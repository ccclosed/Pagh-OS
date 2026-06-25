// Feature: linux-binary-compat, Property 4: Supported-set membership is exact

use crate::abi::{is_supported, nr};
use proptest::prelude::*;

/// The complete enumerated `Supported_Syscall_Set`.
const SUPPORTED: [u64; 23] = [
    nr::READ,
    nr::WRITE,
    nr::OPEN,
    nr::CLOSE,
    nr::FSTAT,
    nr::LSEEK,
    nr::MMAP,
    nr::MPROTECT,
    nr::MUNMAP,
    nr::BRK,
    nr::IOCTL,
    nr::WRITEV,
    nr::ACCESS,
    nr::GETPID,
    nr::EXIT,
    nr::UNAME,
    nr::ARCH_PRCTL,
    nr::SET_TID_ADDRESS,
    nr::CLOCK_GETTIME,
    nr::EXIT_GROUP,
    nr::OPENAT,
    nr::NEWFSTATAT,
    nr::GETRANDOM,
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any `nr`, `is_supported(nr)` is true iff `nr` is in the enumerated set.
    #[test]
    fn supported_set_membership_is_exact(n in any::<u64>()) {
        let expected = SUPPORTED.contains(&n);
        prop_assert_eq!(is_supported(n), expected,
            "is_supported({}) disagreed with membership in the enumerated set", n);
    }
}

/// Explicitly out-of-scope syscalls must never be supported (R11.4, R11.5).
#[test]
fn out_of_scope_syscalls_are_unsupported() {
    // clone(56), fork(57), vfork(58), futex(202)
    assert!(!is_supported(56), "clone must be unsupported");
    assert!(!is_supported(57), "fork must be unsupported");
    assert!(!is_supported(58), "vfork must be unsupported");
    assert!(!is_supported(202), "futex must be unsupported");

    // A random number not in the set.
    let random_unsupported = 999_999u64;
    assert!(!SUPPORTED.contains(&random_unsupported));
    assert!(!is_supported(random_unsupported));
}
