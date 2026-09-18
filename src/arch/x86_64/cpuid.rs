//! CPUID decoding for `/proc/cpuinfo` (issue #11).
//!
//! This is the only architecture-specific half of procfs: it reads the CPUID
//! leaves into the plain [`CpuInfoData`] struct that the pure formatter in
//! `vfs::procfs_format` renders. Keeping the split this way means every byte of
//! `/proc/cpuinfo` is host-testable while the CPUID instructions themselves stay
//! in one small, auditable place (`docs/procfs.md` §6.6).
//!
//! Every leaf above the CPU's reported maximum is guarded: an unsupported leaf
//! returns unspecified data (typically zero, but the SDM does not promise that),
//! so an unguarded read could print garbage as fact.

use core::arch::x86_64::{__cpuid, __cpuid_count};

use crate::vfs::procfs_format::CpuInfoData;

/// Decode the CPUID information `/proc/cpuinfo` needs.
///
/// Absent leaves leave the corresponding `Option` as `None`, which the formatter
/// turns into an omitted line rather than a fabricated value.
pub fn read() -> CpuInfoData {
    let mut d = CpuInfoData::default();

    // CPUID is an unprivileged architectural query on x86_64 (the intrinsics are
    // safe for exactly that reason); leaf 0 is defined on every x86_64 CPU and
    // returns the vendor string plus the maximum basic leaf.
    let leaf0 = __cpuid(0);
    d.max_basic = leaf0.eax;
    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&leaf0.ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&leaf0.edx.to_le_bytes());
    vendor[8..12].copy_from_slice(&leaf0.ecx.to_le_bytes());
    d.vendor = vendor;

    if d.max_basic >= 1 {
        // Guarded by the reported maximum basic leaf.
        let leaf1 = __cpuid(1);
        d.stepping = leaf1.eax & 0xf;
        d.model = (leaf1.eax >> 4) & 0xf;
        d.family = (leaf1.eax >> 8) & 0xf;
        let ext_model = (leaf1.eax >> 16) & 0xf;
        let ext_family = (leaf1.eax >> 20) & 0xff;
        if d.family == 0xf {
            d.family += ext_family;
        }
        if d.family == 0x6 || d.family == 0xf {
            d.model += ext_model << 4;
        }
        d.leaf1_edx = leaf1.edx;
        d.leaf1_ecx = leaf1.ecx;
    }

    if d.max_basic >= 7 {
        // Guarded by the reported maximum basic leaf; sub-leaf 0 exists whenever
        // leaf 7 does.
        let leaf7 = __cpuid_count(7, 0);
        d.leaf7_ebx = leaf7.ebx;
        d.leaf7_ecx = leaf7.ecx;
    }

    // 0x8000_0000 is the extended-function maximum leaf; like leaf 0 it is defined
    // on every x86_64 CPU.
    let max_ext = __cpuid(0x8000_0000).eax;

    if max_ext >= 0x8000_0001 {
        // Guarded by the reported maximum extended leaf.
        let l = __cpuid(0x8000_0001);
        d.leaf81_edx = l.edx;
        d.leaf81_ecx = l.ecx;
    }

    if max_ext >= 0x8000_0004 {
        let mut brand = [0u8; 48];
        for (i, chunk) in brand.chunks_mut(16).enumerate() {
            // Guarded by the maximum extended leaf; the three brand leaves
            // 0x8000_0002..0x8000_0004 are contiguous by definition.
            let l = __cpuid(0x8000_0002 + i as u32);
            chunk[0..4].copy_from_slice(&l.eax.to_le_bytes());
            chunk[4..8].copy_from_slice(&l.ebx.to_le_bytes());
            chunk[8..12].copy_from_slice(&l.ecx.to_le_bytes());
            chunk[12..16].copy_from_slice(&l.edx.to_le_bytes());
        }
        d.brand = brand;
    }

    if max_ext >= 0x8000_0006 {
        // Guarded by the reported maximum extended leaf.
        let l = __cpuid(0x8000_0006);
        let kb = (l.ecx >> 16) & 0xffff;
        if kb > 0 {
            d.cache_kb = Some(kb);
        }
    }

    if max_ext >= 0x8000_0008 {
        // Guarded by the reported maximum extended leaf.
        let l = __cpuid(0x8000_0008);
        d.phys_bits = (l.eax & 0xff) as u8;
        d.virt_bits = ((l.eax >> 8) & 0xff) as u8;
    }

    if d.max_basic >= 0x16 {
        // Guarded by the reported maximum basic leaf.
        let l = __cpuid(0x16);
        let mhz = l.eax & 0xffff;
        if mhz > 0 {
            d.mhz = Some(mhz);
        }
    }

    d
}
