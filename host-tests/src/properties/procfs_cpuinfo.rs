// Feature: procfs (issue #11), contract `docs/procfs.md` §4.2/§7.2 (property topic
// P52). `/proc/cpuinfo` is parsed by libuv's `uv_cpu_info` with byte-exact
// formats — `fscanf(fp, "processor\t: %u\n", &cpu)` and the literal
// `"model name\t: "` — by Python's `platform` module, and by busybox applets.
//
// The two things this property protects are (a) those exact separators and
// (b) honesty: a flag name may appear only when its CPUID bit is set, so the file
// can never advertise a feature (AVX, RDSEED, …) the CPU does not have.

use crate::procfs_format::{
    cpu_flags, format_cpuinfo, CpuInfoData, FlagWord, CPU_FLAG_TABLE, UNKNOWN_MODEL,
};
use proptest::prelude::*;

fn cpu_data() -> impl Strategy<Value = CpuInfoData> {
    (
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        prop::option::of(1u32..(1 << 20)),
        prop::option::of(1u32..10_000u32),
        prop::collection::vec(any::<u8>(), 12),
        prop::collection::vec(any::<u8>(), 48),
    )
        .prop_map(|(m0, m1, m2, m3, m4, m5, m6, m7, cache, mhz, v, b)| {
            let mut vendor = [0u8; 12];
            vendor.copy_from_slice(&v);
            let mut brand = [0u8; 48];
            brand.copy_from_slice(&b);
            CpuInfoData {
                vendor,
                brand,
                family: m0 & 0xffff,
                model: m1 & 0xfff,
                stepping: m2 & 0xf,
                max_basic: m3,
                cache_kb: cache,
                mhz,
                phys_bits: (m4 & 0xff) as u8,
                virt_bits: (m5 & 0xff) as u8,
                leaf1_edx: m6,
                leaf1_ecx: m7,
                leaf7_ebx: m0,
                leaf7_ecx: m1,
                leaf81_edx: m2,
                leaf81_ecx: m3,
            }
        })
}

fn word(c: &CpuInfoData, w: FlagWord) -> u32 {
    match w {
        FlagWord::Leaf1Edx => c.leaf1_edx,
        FlagWord::Leaf1Ecx => c.leaf1_ecx,
        FlagWord::Leaf7Ebx => c.leaf7_ebx,
        FlagWord::Leaf7Ecx => c.leaf7_ecx,
        FlagWord::Leaf81Edx => c.leaf81_edx,
        FlagWord::Leaf81Ecx => c.leaf81_ecx,
    }
}

/// Exact expectation: the table entries whose bit is set, in table order.
fn expected_flags(c: &CpuInfoData) -> Vec<&'static str> {
    CPU_FLAG_TABLE
        .iter()
        .copied()
        .filter(|(_, w, bit)| word(c, *w) & (1u32 << *bit) != 0)
        .map(|(name, _, _)| name)
        .collect()
}

fn text(out: &[u8]) -> &str {
    core::str::from_utf8(out).expect("cpuinfo is ASCII")
}

proptest! {
    /// One block per CPU, each terminated by a blank line, each starting with the
    /// exact `processor\t: N\n` line libuv's `fscanf` requires.
    #[test]
    fn cpuinfo_blocks_and_processor_lines(cpus in prop::collection::vec(cpu_data(), 1..4)) {
        let out = format_cpuinfo(&cpus);
        let t = text(&out);
        prop_assert!(t.ends_with("\n\n"), "last block must end with a blank line");

        let blocks: Vec<&str> = t.trim_end_matches('\n').split("\n\n").collect();
        prop_assert_eq!(blocks.len(), cpus.len());
        for (i, block) in blocks.iter().enumerate() {
            let first = block.lines().next().unwrap();
            prop_assert_eq!(first, format!("processor\t: {}", i));
            prop_assert!(block.contains("model name\t: "), "libuv literal missing");
            prop_assert!(block.contains("flags\t\t: "));
            prop_assert!(block.contains("bugs\t\t:"));
        }
    }

    /// Flags are exactly the CPUID-supported set — no more, no less.
    #[test]
    fn cpuinfo_flags_are_truthful(c in cpu_data()) {
        prop_assert_eq!(cpu_flags(&c), expected_flags(&c));
        let names = cpu_flags(&c);
        for name in &names {
            prop_assert!(CPU_FLAG_TABLE.iter().any(|(n, _, _)| n == name));
        }
    }

    /// A CPU with no CPUID bits set advertises no flags at all.
    #[test]
    fn all_zero_words_declare_no_flags(_dummy in any::<u8>()) {
        let c = CpuInfoData::default();
        prop_assert!(cpu_flags(&c).is_empty());
        let out = format_cpuinfo(&[c]);
        prop_assert!(text(&out).contains("flags\t\t: \n"));
    }

    /// `model name` always carries a value, and the optional leaves are present
    /// exactly when they were decoded.
    #[test]
    fn cpuinfo_optional_lines_follow_the_input(c in cpu_data()) {
        let out = format_cpuinfo(&[c]);
        let t = text(&out);

        let name_line = t
            .lines()
            .find(|l| l.starts_with("model name\t: "))
            .expect("model name line");
        prop_assert!(!name_line["model name\t: ".len()..].trim().is_empty());

        prop_assert_eq!(t.contains("cpu MHz\t\t: "), c.mhz.is_some());
        prop_assert_eq!(t.contains("cache size\t: "), c.cache_kb.is_some());
        prop_assert_eq!(t.contains("bogomips\t: "), c.mhz.is_some());
        prop_assert_eq!(
            t.contains("address sizes\t: "),
            c.phys_bits > 0 && c.virt_bits > 0
        );
    }
}

/// An all-NUL brand string never yields an empty `model name` value: parsers get
/// the documented placeholder instead of nothing.
#[test]
fn empty_brand_uses_the_placeholder() {
    let out = format_cpuinfo(&[CpuInfoData::default()]);
    let t = text(&out);
    let want = format!("model name\t: {}\n", UNKNOWN_MODEL);
    assert!(
        t.contains(&want),
        "expected the unknown-model placeholder, got:\n{t}"
    );
    // A vendor of all NULs is omitted rather than printed as empty.
    assert!(!t.contains("vendor_id"), "no vendor_id for an empty vendor");
}

/// The brand string is trimmed and inner whitespace collapsed, so the line stays
/// parseable even if the CPU pads it.
#[test]
fn brand_is_cleaned() {
    let mut brand = [0u8; 48];
    brand[..10].copy_from_slice(b"  Intel  X");
    let c = CpuInfoData {
        brand,
        ..Default::default()
    };
    let out = format_cpuinfo(&[c]);
    let t = text(&out);
    assert!(t.contains("model name\t: Intel X\n"), "got:\n{t}");
}

/// Single-CPU honesty: `siblings`/`cpu cores` report 1, matching what
/// `sched_getaffinity` tells the same program.
#[test]
fn single_cpu_geometry_is_consistent() {
    let out = format_cpuinfo(&[CpuInfoData::default()]);
    let t = text(&out);
    for line in [
        "siblings\t: 1",
        "cpu cores\t: 1",
        "physical id\t: 0",
        "core id\t\t: 0",
    ] {
        assert!(t.contains(line), "missing {line}");
    }
}
