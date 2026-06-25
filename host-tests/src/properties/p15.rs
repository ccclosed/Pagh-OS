// Feature: linux-binary-compat, Property 15: ELF classification accepts static executables and rejects everything ineligible

use crate::elf_classify::{classify_elf, ElfKind, ElfVerdict, USER_ADDR_MAX};
use proptest::prelude::*;

// ELF64 structure sizes / field offsets mirrored from the classifier.
const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;

fn put16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

#[derive(Clone, Copy, Debug)]
struct SegSpec {
    p_type: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_filesz: u64,
    p_memsz: u64,
}

/// Build a well-formed ELF64 image: 64-byte header, a program-header table at
/// offset 64 with `phentsize == 56`, and a buffer large enough to contain every
/// segment's file range.
fn build_elf(e_type: u16, segs: &[SegSpec]) -> Vec<u8> {
    let phnum = segs.len();
    let phoff = EHDR_SIZE;
    let table_end = phoff + phnum * PHDR_SIZE;
    let mut len = table_end;
    for s in segs {
        let file_end = (s.p_offset + s.p_filesz) as usize;
        if file_end > len {
            len = file_end;
        }
    }
    let mut b = vec![0u8; len];
    b[0] = 0x7F;
    b[1] = b'E';
    b[2] = b'L';
    b[3] = b'F';
    b[4] = 2; // ELFCLASS64
    b[5] = 1; // ELFDATA2LSB
    put16(&mut b, 16, e_type);
    put16(&mut b, 18, 0x3E); // EM_X86_64
    put64(&mut b, 32, phoff as u64); // e_phoff
    put16(&mut b, 54, PHDR_SIZE as u16); // e_phentsize
    put16(&mut b, 56, phnum as u16); // e_phnum
    for (i, s) in segs.iter().enumerate() {
        let ph = phoff + i * PHDR_SIZE;
        put32(&mut b, ph, s.p_type);
        put64(&mut b, ph + 8, s.p_offset);
        put64(&mut b, ph + 16, s.p_vaddr);
        put64(&mut b, ph + 32, s.p_filesz);
        put64(&mut b, ph + 40, s.p_memsz);
    }
    b
}

/// A reject-inducing mutation applied to an otherwise-valid image. `Valid` keeps
/// it loadable.
#[derive(Clone, Copy, Debug)]
enum Mutation {
    Valid,
    BadMagic,
    BadClass,
    BadData,
    BadMachine,
    BadType,
    InterpPresent,
    FileszGtMemsz,
    RangeOverMax,
    ShortBuffer,
    FileRangeBeyond,
}

fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        Just(Mutation::Valid),
        Just(Mutation::BadMagic),
        Just(Mutation::BadClass),
        Just(Mutation::BadData),
        Just(Mutation::BadMachine),
        Just(Mutation::BadType),
        Just(Mutation::InterpPresent),
        Just(Mutation::FileszGtMemsz),
        Just(Mutation::RangeOverMax),
        Just(Mutation::ShortBuffer),
        Just(Mutation::FileRangeBeyond),
    ]
}

/// Generate a valid PT_LOAD segment: `p_filesz <= p_memsz`, file range inside the
/// buffer (`p_offset == 0`), page-rounded virtual range well below USER_ADDR_MAX.
fn seg() -> impl Strategy<Value = SegSpec> {
    (0u64..0x4000_0000_0000u64, 0u64..0x2_0000u64, any::<u64>()).prop_map(
        |(p_vaddr, p_memsz, fr)| {
            let p_filesz = if p_memsz == 0 { 0 } else { fr % (p_memsz + 1) };
            SegSpec {
                p_type: PT_LOAD,
                p_offset: 0,
                p_vaddr,
                p_filesz,
                p_memsz,
            }
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// `classify_elf` returns `Load` exactly for a 64-bit LE x86_64 `ET_EXEC`/`ET_DYN`
    /// with no `PT_INTERP` and all `PT_LOAD` valid; every reject-inducing mutation
    /// yields `Reject`.
    #[test]
    fn classify_accepts_valid_rejects_ineligible(
        is_dyn in any::<bool>(),
        segs in prop::collection::vec(seg(), 1..4),
        mutation in mutation(),
    ) {
        let e_type = if is_dyn { ET_DYN } else { ET_EXEC };
        let mut bytes = build_elf(e_type, &segs);

        match mutation {
            Mutation::Valid => {
                let verdict = classify_elf(&bytes);
                let expected_kind = if is_dyn { ElfKind::Dyn } else { ElfKind::Exec };
                prop_assert_eq!(
                    verdict,
                    ElfVerdict::Load { kind: expected_kind, bias_required: is_dyn }
                );
            }
            Mutation::BadMagic => {
                bytes[1] = 0x00;
                prop_assert!(matches!(classify_elf(&bytes), ElfVerdict::Reject(_)));
            }
            Mutation::BadClass => {
                bytes[4] = 1; // not ELFCLASS64
                prop_assert!(matches!(classify_elf(&bytes), ElfVerdict::Reject(_)));
            }
            Mutation::BadData => {
                bytes[5] = 2; // not ELFDATA2LSB
                prop_assert!(matches!(classify_elf(&bytes), ElfVerdict::Reject(_)));
            }
            Mutation::BadMachine => {
                put16(&mut bytes, 18, 0x28); // EM_ARM
                prop_assert!(matches!(classify_elf(&bytes), ElfVerdict::Reject(_)));
            }
            Mutation::BadType => {
                put16(&mut bytes, 16, 1); // ET_REL
                prop_assert!(matches!(classify_elf(&bytes), ElfVerdict::Reject(_)));
            }
            Mutation::InterpPresent => {
                // Mark the first program header as PT_INTERP.
                let ph = EHDR_SIZE;
                put32(&mut bytes, ph, PT_INTERP);
                prop_assert!(matches!(classify_elf(&bytes), ElfVerdict::Reject(_)));
            }
            Mutation::FileszGtMemsz => {
                let ph = EHDR_SIZE;
                put32(&mut bytes, ph, PT_LOAD);
                put64(&mut bytes, ph + 32, 5); // p_filesz
                put64(&mut bytes, ph + 40, 4); // p_memsz < p_filesz
                prop_assert!(matches!(classify_elf(&bytes), ElfVerdict::Reject(_)));
            }
            Mutation::RangeOverMax => {
                let ph = EHDR_SIZE;
                put32(&mut bytes, ph, PT_LOAD);
                put64(&mut bytes, ph + 16, USER_ADDR_MAX); // p_vaddr at the boundary
                put64(&mut bytes, ph + 32, 0); // p_filesz
                put64(&mut bytes, ph + 40, 0); // p_memsz
                prop_assert!(matches!(classify_elf(&bytes), ElfVerdict::Reject(_)));
            }
            Mutation::ShortBuffer => {
                bytes.truncate(32); // below EHDR_SIZE
                prop_assert!(matches!(classify_elf(&bytes), ElfVerdict::Reject(_)));
            }
            Mutation::FileRangeBeyond => {
                let len = bytes.len() as u64;
                let ph = EHDR_SIZE;
                put32(&mut bytes, ph, PT_LOAD);
                put64(&mut bytes, ph + 8, len + 100); // p_offset past the buffer
                put64(&mut bytes, ph + 32, 1); // p_filesz
                put64(&mut bytes, ph + 40, 1); // p_memsz
                prop_assert!(matches!(classify_elf(&bytes), ElfVerdict::Reject(_)));
            }
        }

        // `segs` produced the baseline image; mutations operate on raw bytes.
        let _ = &segs;
    }
}
