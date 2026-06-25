// Feature: linux-binary-compat, Property 17: the initial stack image is a well-formed System V layout

use crate::stack::{at, build_initial_stack, AuxInputs};
use proptest::prelude::*;
use std::collections::BTreeMap;

// A generous, 16-byte-aligned stack window that comfortably fits the small
// argv/envp the generator produces.
const STACK_TOP: u64 = 0x0000_7fff_0000_0000;
const STACK_LOW: u64 = STACK_TOP - 0x1_0000; // 64 KiB

/// Generate a single argument/environment string: non-empty-safe bytes with NO
/// interior NUL (real argv/envp never contain NUL), so each NUL-terminated copy
/// round-trips byte-for-byte.
fn token() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(1u8..=255u8, 0..12)
}

fn tokens() -> impl Strategy<Value = Vec<Vec<u8>>> {
    prop::collection::vec(token(), 0..6)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Decoding the `StackImage` recovers a well-formed System V layout: `argc`,
    /// the argv pointers + one NULL, the envp pointers + one NULL, an auxv with
    /// each tag once + one AT_NULL, a 16-byte-aligned `&argc`, an in-range
    /// AT_RANDOM block, and pointers that recover the original strings.
    #[test]
    fn stack_image_is_well_formed_sysv(
        argv in tokens(),
        envp in tokens(),
        phdr in any::<u64>(),
        phent in any::<u64>(),
        phnum in any::<u64>(),
        entry in any::<u64>(),
        pagesz in any::<u64>(),
        random16 in any::<[u8; 16]>(),
    ) {
        let argv_refs: Vec<&[u8]> = argv.iter().map(|v| v.as_slice()).collect();
        let envp_refs: Vec<&[u8]> = envp.iter().map(|v| v.as_slice()).collect();
        let aux = AuxInputs { phdr, phent, phnum, entry, pagesz, random_ptr: 0 };

        let img = build_initial_stack(STACK_TOP, STACK_LOW, &argv_refs, &envp_refs, &aux, random16)
            .expect("stack must fit in the generous window");

        let argc_addr = img.argc_addr;
        prop_assert_eq!(img.initial_rsp, argc_addr);
        // R6.5: &argc is 16-byte aligned.
        prop_assert_eq!(argc_addr % 16, 0);

        let bytes = &img.bytes;
        // Helper: read a little-endian u64 at absolute user address `addr`.
        let read_u64 = |addr: u64| -> Option<u64> {
            let off = addr.checked_sub(argc_addr)? as usize;
            let slice = bytes.get(off..off + 8)?;
            Some(u64::from_le_bytes(slice.try_into().unwrap()))
        };
        // Helper: read the NUL-terminated bytes pointed at by `ptr`.
        let read_cstr = |ptr: u64| -> Option<Vec<u8>> {
            let start = ptr.checked_sub(argc_addr)? as usize;
            let mut out = Vec::new();
            for &b in bytes.get(start..)? {
                if b == 0 {
                    return Some(out);
                }
                out.push(b);
            }
            None // no terminator found
        };

        let mut cur = argc_addr;

        // argc == argv.len()  (R6.2)
        let argc = read_u64(cur).unwrap();
        prop_assert_eq!(argc, argv.len() as u64);
        cur += 8;

        // argv pointers, each recovering the original string  (R6.1, R6.7)
        for original in &argv {
            let ptr = read_u64(cur).unwrap();
            cur += 8;
            let recovered = read_cstr(ptr).expect("argv ptr must reference a NUL-terminated string");
            prop_assert_eq!(&recovered, original);
        }
        // exactly one NULL terminating argv
        prop_assert_eq!(read_u64(cur).unwrap(), 0);
        cur += 8;

        // envp pointers, each recovering the original string  (R6.1, R6.7)
        for original in &envp {
            let ptr = read_u64(cur).unwrap();
            cur += 8;
            let recovered = read_cstr(ptr).expect("envp ptr must reference a NUL-terminated string");
            prop_assert_eq!(&recovered, original);
        }
        // exactly one NULL terminating envp
        prop_assert_eq!(read_u64(cur).unwrap(), 0);
        cur += 8;

        // auxv: read (tag, val) pairs until AT_NULL  (R6.3, R6.4)
        let mut seen: BTreeMap<u64, u64> = BTreeMap::new();
        let mut at_null_count = 0u32;
        for _ in 0..64 {
            let tag = read_u64(cur).unwrap();
            cur += 8;
            let val = read_u64(cur).unwrap();
            cur += 8;
            if tag == at::NULL {
                at_null_count += 1;
                break;
            }
            // each non-null tag appears at most once
            prop_assert!(seen.insert(tag, val).is_none(), "duplicate auxv tag {}", tag);
        }
        prop_assert_eq!(at_null_count, 1);

        // exactly the six required tags, once each
        let required = [at::PHDR, at::PHENT, at::PHNUM, at::ENTRY, at::PAGESZ, at::RANDOM];
        prop_assert_eq!(seen.len(), required.len());
        for tag in required {
            prop_assert!(seen.contains_key(&tag), "missing auxv tag {}", tag);
        }

        // values that must equal the AuxInputs
        prop_assert_eq!(seen[&at::PHENT], phent);
        prop_assert_eq!(seen[&at::PHNUM], phnum);
        prop_assert_eq!(seen[&at::ENTRY], entry);
        prop_assert_eq!(seen[&at::PHDR], phdr);
        prop_assert_eq!(seen[&at::PAGESZ], pagesz);

        // AT_RANDOM points at 16 bytes within [stack_low, stack_top)  (R6.6)
        let rand_ptr = seen[&at::RANDOM];
        prop_assert!(rand_ptr >= STACK_LOW);
        prop_assert!(rand_ptr + 16 <= STACK_TOP);
        let rand_off = (rand_ptr - argc_addr) as usize;
        prop_assert_eq!(&bytes[rand_off..rand_off + 16], &random16[..]);
    }
}
