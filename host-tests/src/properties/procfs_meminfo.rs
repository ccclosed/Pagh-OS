// Feature: procfs (issue #11), contract `docs/procfs.md` §4.1/§7.2 (property topic
// P51). `/proc/meminfo` is read by libuv (`uv_get_total_memory` /
// `uv_get_free_memory`: `strstr(buf, "MemTotal:")` then `sscanf("%llu kB")`),
// busybox `free`, htop and psutil. Those parsers split on whitespace but require
// the `Key:` + value + `" kB"` shape exactly, so the formatter — not the kernel
// state behind it — is what this property pins down.

use crate::procfs_format::{format_meminfo, MemInfoKb, PROC_INO_BASE};
use proptest::prelude::*;

/// Keys the contract requires, in order (`docs/procfs.md` §4.1).
const REQUIRED_KEYS: [&str; 18] = [
    "MemTotal:",
    "MemFree:",
    "MemAvailable:",
    "Buffers:",
    "Cached:",
    "SwapCached:",
    "Active:",
    "Inactive:",
    "SwapTotal:",
    "SwapFree:",
    "Dirty:",
    "Writeback:",
    "AnonPages:",
    "Mapped:",
    "Shmem:",
    "Slab:",
    "SReclaimable:",
    "SUnreclaim:",
];

/// Split one `meminfo` line into `(key_with_colon, kb)`, or `None` if it does not
/// have the exact `Key:<spaces><digits> kB` shape.
fn parse_meminfo_line(line: &str) -> Option<(&str, u64)> {
    let (key, rest) = line.split_once(':')?;
    let rest = rest.strip_suffix(" kB")?;
    if !rest.starts_with(' ') {
        return None;
    }
    let digits = rest.trim_start_matches(' ');
    // No tabs, no double spaces inside the number, digits only.
    if digits.is_empty()
        || !digits.bytes().all(|b| b.is_ascii_digit())
        || rest.len() - digits.len() != rest.len() - rest.trim_start_matches(' ').len()
    {
        return None;
    }
    Some((key, digits.parse::<u64>().ok()?))
}

fn lines(out: &[u8]) -> Vec<&str> {
    core::str::from_utf8(out)
        .expect("meminfo is ASCII")
        .trim_end_matches('\n')
        .split('\n')
        .collect()
}

proptest! {
    /// Every line parses, every required key appears exactly once in order, and
    /// the values obey the documented identities.
    #[test]
    fn meminfo_shape_and_values(total in 0u64..(1 << 40), free in 0u64..(1 << 40)) {
        let free = core::cmp::min(free, total);
        let out = format_meminfo(&MemInfoKb { total_kb: total, free_kb: free });
        let ls = lines(&out);

        prop_assert_eq!(ls.len(), REQUIRED_KEYS.len());
        prop_assert!(out.len() <= 4096, "meminfo must fit libuv's 4096-byte buffer");

        let mut values = Vec::new();
        for (line, key) in ls.iter().zip(REQUIRED_KEYS.iter()) {
            let (got_key, kb) = parse_meminfo_line(line)
                .unwrap_or_else(|| panic!("unparseable meminfo line {line:?}"));
            prop_assert_eq!(format!("{got_key}:"), *key);
            values.push(kb);
        }

        prop_assert_eq!(values[0], total);
        prop_assert_eq!(values[1], free);
        prop_assert_eq!(values[2], free, "MemAvailable mirrors MemFree (no cache to reclaim)");
        prop_assert!(values[0] >= values[1]);
        prop_assert_eq!(values[8], 0, "SwapTotal");
        prop_assert_eq!(values[9], 0, "SwapFree");
    }

    /// The exact column rule Linux uses: label padded to 16, value right-aligned
    /// in 8, then `" kB"`.
    #[test]
    fn meminfo_columns_match_linux(total in 0u64..(1 << 40), free in 0u64..(1 << 40)) {
        let free = core::cmp::min(free, total);
        let out = format_meminfo(&MemInfoKb { total_kb: total, free_kb: free });
        let ls = lines(&out);
        prop_assert_eq!(ls[0], format!("{:<16}{:>8} kB", "MemTotal:", total));
        prop_assert_eq!(ls[1], format!("{:<16}{:>8} kB", "MemFree:", free));
        prop_assert_eq!(ls[2], format!("{:<16}{:>8} kB", "MemAvailable:", free));
        prop_assert_eq!(ls[3], format!("{:<16}{:>8} kB", "Buffers:", 0));
    }
}

/// The substrings libuv searches for must be present verbatim, including the
/// single space before the unit (its `sscanf` literal is `" kB"`).
#[test]
fn libuv_substrings_are_verbatim() {
    let out = format_meminfo(&MemInfoKb {
        total_kb: 4_194_304,
        free_kb: 1_048_576,
    });
    let text = core::str::from_utf8(&out).unwrap();
    for needle in [
        "MemTotal:",
        "MemAvailable:",
        "MemFree:",
        "Buffers:",
        "Cached:",
    ] {
        assert!(text.contains(needle), "missing {needle}");
    }
    assert!(text.contains(" kB\n"), "unit must be ' kB' + newline");
    assert!(text.ends_with('\n'));
    assert!(!text.contains('\t'), "meminfo uses spaces, never tabs");
}

/// Inode numbers must stay clear of the synthetic FNV range (bit 63) and above
/// the ramfs range, so `(st_dev, st_ino)` pairs never collide (`docs/procfs.md`
/// §2.3).
#[test]
fn proc_inode_base_is_unambiguous() {
    assert_eq!(PROC_INO_BASE & (1 << 63), 0);
    assert!(PROC_INO_BASE > 0x0054_0000, "above the ramfs inode range");
}
