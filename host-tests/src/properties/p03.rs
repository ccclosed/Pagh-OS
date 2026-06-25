// Feature: linux-binary-compat, Property 3: Errno encoding round-trips into the Linux negated-errno range

use crate::errno::{encode_errno, Errno};
use proptest::prelude::*;

/// The seven `Errno` variants the compatibility layer can report.
const ALL_ERRNOS: [Errno; 7] = [
    Errno::EPERM,
    Errno::ENOENT,
    Errno::EBADF,
    Errno::ENOMEM,
    Errno::EFAULT,
    Errno::EINVAL,
    Errno::ENOSYS,
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any `Errno` variant, encoding it for `rax` as `(-(e as i64)) as u64`
    /// reinterprets back to an `i64` in `[-4095, -1]` and decodes to the original.
    #[test]
    fn errno_encoding_round_trips(idx in 0usize..ALL_ERRNOS.len()) {
        let e = ALL_ERRNOS[idx];

        let encoded: u64 = encode_errno(e);
        let as_signed = encoded as i64;

        // Lies within the Linux negated-errno range.
        prop_assert!((-4095..=-1).contains(&as_signed),
            "encoded errno {as_signed} out of range [-4095, -1]");

        // Decodes back to the original variant's discriminant.
        let decoded = -as_signed;
        prop_assert_eq!(decoded, e as i64);
    }
}
