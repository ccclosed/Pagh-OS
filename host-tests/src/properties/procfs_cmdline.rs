// Feature: procfs (issue #11), contract `docs/procfs.md` §4.4/§7.2 (property topic
// P54). `/proc/self/cmdline` is the NUL-separated argv: procps `ps`, busybox and
// every "what is this process running" tool parse it. Byte fidelity matters more
// than anywhere else in procfs — argv is `&[u8]` all the way from `execve`, and
// anything that round-trips through `String` would corrupt a non-UTF-8 argument.

use crate::procfs_format::{format_cmdline, CMDLINE_CAP};
use proptest::prelude::*;

proptest! {
    /// The output is exactly the arguments joined by NUL, with a trailing NUL,
    /// and splitting it recovers the input byte-for-byte.
    #[test]
    fn cmdline_round_trips(args in prop::collection::vec(prop::collection::vec(1u8..=0xffu8, 0..24), 0..6)) {
        let refs: Vec<&[u8]> = args.iter().map(|a| a.as_slice()).collect();
        let out = format_cmdline(&refs, CMDLINE_CAP);

        let expected_len: usize = args.iter().map(|a| a.len() + 1).sum();
        prop_assert_eq!(out.len(), expected_len);
        if args.is_empty() {
            prop_assert!(out.is_empty());
        } else {
            prop_assert_eq!(*out.last().unwrap(), 0);
        }

        let mut parts: Vec<&[u8]> = out.split(|b| *b == 0).collect();
        // The trailing NUL yields one empty final element.
        prop_assert_eq!(parts.pop(), Some(&b""[..]));
        prop_assert_eq!(parts.len(), args.len());
        for (got, want) in parts.iter().zip(args.iter()) {
            prop_assert_eq!(got, &want.as_slice());
        }
    }

    /// A cap is respected by dropping whole arguments: no half-written argument,
    /// and a retained one is never truncated.
    #[test]
    fn cmdline_cap_drops_only_whole_arguments(
        args in prop::collection::vec(prop::collection::vec(1u8..=0xffu8, 1..16), 1..8),
        cap in 0usize..64,
    ) {
        let refs: Vec<&[u8]> = args.iter().map(|a| a.as_slice()).collect();
        let out = format_cmdline(&refs, cap);
        let mut parts: Vec<&[u8]> = out.split(|b| *b == 0).collect();
        prop_assert_eq!(parts.pop(), Some(&b""[..]));

        // The cap bounds the retained argument payload; the terminators are extra.
        let retained_payload: usize = parts.iter().map(|p| p.len()).sum();
        prop_assert!(retained_payload <= cap);
        // Every retained argument is a prefix of the input list, complete.
        prop_assert!(parts.len() <= args.len());
        for (i, got) in parts.iter().enumerate() {
            prop_assert_eq!(got, &args[i].as_slice());
        }
        // If the first argument fits the payload cap, at least one is retained.
        if !args.is_empty() && args[0].len() <= cap {
            prop_assert!(!parts.is_empty());
        }
        // The retained prefix is maximal: the first dropped argument is the one
        // that did not fit.
        if parts.len() < args.len() {
            prop_assert!(
                retained_payload + args[parts.len()].len() > cap,
                "arg {} fits but was dropped",
                parts.len()
            );
        }
    }
}

/// Non-UTF-8 and embedded newlines survive unchanged (nothing re-encodes argv).
#[test]
fn cmdline_keeps_arbitrary_bytes() {
    let out = format_cmdline(&[&[0xff, 0xfe, b'\n', 0x00], b"tail"], CMDLINE_CAP);
    assert_eq!(
        out,
        vec![0xff, 0xfe, b'\n', 0x00, 0x00, b't', b'a', b'i', b'l', 0x00]
    );
}

/// The default cap matches the initial-stack argument gate, so a rendered cmdline
/// can always describe the process whose stack was built (`src/task/stack.rs`).
#[test]
fn cmdline_cap_matches_the_argument_gate() {
    assert_eq!(CMDLINE_CAP, 4096);
    let big = vec![b'a'; CMDLINE_CAP];
    let out = format_cmdline(&[big.as_slice()], CMDLINE_CAP);
    // The largest argv the gate admits is still fully representable: payload ==
    // cap plus the one NUL terminator.
    assert_eq!(out.len(), CMDLINE_CAP + 1);
    assert_eq!(*out.last().unwrap(), 0);
    assert_eq!(&out[..CMDLINE_CAP], big.as_slice());
}

/// An empty argv renders an empty file, exactly like Linux for a process with no
/// arguments.
#[test]
fn empty_argv_is_an_empty_file() {
    assert!(format_cmdline(&[], CMDLINE_CAP).is_empty());
}
