//! Pure ELF classification and static-PIE load-bias selection.
//!
//! Pure, allocation-free, `core`-only logic shared with the `host-tests` crate
//! (R11.6). This module decides — *without mapping anything, allocating page
//! tables, or touching global state* — whether a byte buffer is a Linux static
//! `ET_EXEC` or static-PIE `ET_DYN` x86_64 ELF this kernel can load, and (for
//! `ET_DYN`) picks a page-aligned load bias that keeps every segment inside the
//! lower-half user address space.
//!
//! The effectful loader (`ElfLoader::load`, task 13.1) consumes [`classify_elf`]
//! and [`choose_bias`] before it allocates a user PML4 or maps any segment, so a
//! malformed/ineligible binary is rejected with a descriptive `&'static str`
//! before any memory is touched (R5.5, R5.6, R5.9, R11.3, R12.1).
//!
//! Parsing is done with explicit little-endian byte reads off the slice (rather
//! than reinterpreting the buffer as a `#[repr(C)]` struct) so the logic is fully
//! portable, alignment-safe, and identical on the host and the target.
#![allow(dead_code)]

/// x86_64 page size (4 KiB).
const PAGE_SIZE: u64 = 4096;

/// Exclusive upper bound of the lower-half canonical user address range
/// (`User_Addr_Max`). A segment is acceptable only when its page-rounded range
/// lies strictly below this.
///
/// NOTE: this is intentionally a standalone copy of the same constant in
/// `src/vfs/elf.rs`. That module pulls in kernel-only dependencies (the VMM/PMM
/// and the `x86_64` paging crate), so importing it would make this pure module —
/// and the `host-tests` crate that `#[path]`-includes it — impossible to build on
/// the host. The value is fixed by the architecture.
pub const USER_ADDR_MAX: u64 = 0x0000_8000_0000_0000;

/// Default page-aligned base used for static-PIE (`ET_DYN`) load bias.
///
/// Deterministic so two runs of the same binary load identically. Any
/// page-aligned value below `USER_ADDR_MAX` works for a self-relocating
/// static-PIE; `0x1_0000` (64 KiB) keeps the NULL page unmapped while leaving the
/// whole user half available above it.
pub const PIE_BASE: u64 = 0x1_0000;

// ── ELF64 identification ────────────────────────────────────────────────────
const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;

// ── e_type / e_machine ──────────────────────────────────────────────────────
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 0x3E;

// ── p_type ──────────────────────────────────────────────────────────────────
const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;

// ── ELF64 structure sizes ───────────────────────────────────────────────────
const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;

// ── ELF64 header field offsets ──────────────────────────────────────────────
const E_TYPE_OFF: usize = 16;
const E_MACHINE_OFF: usize = 18;
const E_PHOFF_OFF: usize = 32;
const E_PHENTSIZE_OFF: usize = 54;
const E_PHNUM_OFF: usize = 56;

// ── ELF64 program-header field offsets (relative to the phdr) ───────────────
const P_TYPE_OFF: usize = 0;
const P_OFFSET_OFF: usize = 8;
const P_VADDR_OFF: usize = 16;
const P_FILESZ_OFF: usize = 32;
const P_MEMSZ_OFF: usize = 40;

/// The kind of ELF image accepted by the loader.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ElfKind {
    /// A classic static executable (`ET_EXEC`); loaded at its absolute `p_vaddr`
    /// (bias 0). (R5.1)
    Exec,
    /// A static position-independent executable (`ET_DYN`, no `PT_INTERP`);
    /// loaded at `p_vaddr + bias` for a kernel-chosen bias. (R5.2)
    Dyn,
}

/// Result of classifying a candidate ELF buffer.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ElfVerdict {
    /// The buffer is ineligible; the contained `&'static str` names the cause
    /// (used for the loader's single rejection diagnostic — R12.1).
    Reject(&'static str),
    /// The buffer is a loadable static binary.
    Load {
        /// Whether this is an `ET_EXEC` or a static-PIE `ET_DYN`.
        kind: ElfKind,
        /// `true` for `ET_DYN` (a load bias must be chosen via [`choose_bias`]),
        /// `false` for `ET_EXEC` (bias is always 0).
        bias_required: bool,
    },
}

// ── little-endian field readers (bounds-checked, total) ─────────────────────

#[inline]
fn rd_u16(data: &[u8], off: usize) -> Option<u16> {
    let b = data.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

#[inline]
fn rd_u32(data: &[u8], off: usize) -> Option<u32> {
    let b = data.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

#[inline]
fn rd_u64(data: &[u8], off: usize) -> Option<u64> {
    let b = data.get(off..off.checked_add(8)?)?;
    Some(u64::from_le_bytes([
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
    ]))
}

/// Round `x` up to the next page boundary, returning `None` on overflow.
#[inline]
fn page_up(x: u64) -> Option<u64> {
    x.checked_add(PAGE_SIZE - 1).map(|v| v & !(PAGE_SIZE - 1))
}

/// Pure classification of a candidate ELF buffer (no mapping, no allocation).
///
/// Returns [`ElfVerdict::Load`] **iff** the buffer is a 64-bit little-endian
/// `EM_X86_64` ELF of type `ET_EXEC` or `ET_DYN` that contains no `PT_INTERP`
/// segment, and every `PT_LOAD` segment satisfies all of:
///   * `p_filesz <= p_memsz` (R5.9),
///   * `p_offset + p_filesz <= data.len()` (R5.9), and
///   * the page-rounded range `[p_vaddr, p_vaddr + p_memsz)` lies strictly below
///     [`USER_ADDR_MAX`] (R5.5). For `ET_DYN` this is evaluated at a zero bias as
///     a lower bound; the actual bias is chosen later by [`choose_bias`].
///
/// Any other buffer yields [`ElfVerdict::Reject`] with a descriptive cause and
/// nothing is mapped (R5.3, R5.6, R11.3). All arithmetic is overflow-safe.
pub fn classify_elf(data: &[u8]) -> ElfVerdict {
    // ── header presence & identity (R5.6) ──
    if data.len() < EHDR_SIZE {
        return ElfVerdict::Reject("ELF: data too small for header");
    }
    if data[0] != 0x7F || &data[1..4] != b"ELF" {
        return ElfVerdict::Reject("ELF: invalid magic");
    }
    if data[EI_CLASS] != ELFCLASS64 {
        return ElfVerdict::Reject("ELF: not 64-bit");
    }
    if data[EI_DATA] != ELFDATA2LSB {
        return ElfVerdict::Reject("ELF: not little-endian");
    }

    let e_machine = match rd_u16(data, E_MACHINE_OFF) {
        Some(v) => v,
        None => return ElfVerdict::Reject("ELF: truncated header"),
    };
    if e_machine != EM_X86_64 {
        return ElfVerdict::Reject("ELF: not x86_64");
    }

    let e_type = match rd_u16(data, E_TYPE_OFF) {
        Some(v) => v,
        None => return ElfVerdict::Reject("ELF: truncated header"),
    };
    let kind = match e_type {
        ET_EXEC => ElfKind::Exec,
        ET_DYN => ElfKind::Dyn,
        _ => return ElfVerdict::Reject("ELF: not ET_EXEC or ET_DYN"),
    };

    // ── program-header table bounds ──
    let phnum = match rd_u16(data, E_PHNUM_OFF) {
        Some(v) => v as usize,
        None => return ElfVerdict::Reject("ELF: truncated header"),
    };
    if phnum == 0 {
        // No segments to load. Nothing can violate the segment invariants; the
        // image is classified purely by its type.
        return ElfVerdict::Load {
            kind,
            bias_required: kind == ElfKind::Dyn,
        };
    }

    let phentsize = match rd_u16(data, E_PHENTSIZE_OFF) {
        Some(v) => v as usize,
        None => return ElfVerdict::Reject("ELF: truncated header"),
    };
    if phentsize < PHDR_SIZE {
        return ElfVerdict::Reject("ELF: invalid program header size");
    }
    let phoff = match rd_u64(data, E_PHOFF_OFF) {
        Some(v) => v as usize,
        None => return ElfVerdict::Reject("ELF: truncated header"),
    };

    let table_bytes = match phnum.checked_mul(phentsize) {
        Some(v) => v,
        None => return ElfVerdict::Reject("ELF: program header table size overflow"),
    };
    let table_end = match phoff.checked_add(table_bytes) {
        Some(v) => v,
        None => return ElfVerdict::Reject("ELF: program header table offset overflow"),
    };
    if table_end > data.len() {
        return ElfVerdict::Reject("ELF: program header table beyond data");
    }

    // ── per-segment validation ──
    for i in 0..phnum {
        // In range: i < phnum and phnum*phentsize did not overflow.
        let ph = phoff + i * phentsize;

        let p_type = match rd_u32(data, ph + P_TYPE_OFF) {
            Some(v) => v,
            None => return ElfVerdict::Reject("ELF: truncated program header"),
        };

        // A PT_INTERP segment means dynamic linking, which is unsupported.
        if p_type == PT_INTERP {
            return ElfVerdict::Reject("ELF: dynamically linked, unsupported");
        }

        if p_type != PT_LOAD {
            continue;
        }

        let p_offset = match rd_u64(data, ph + P_OFFSET_OFF) {
            Some(v) => v,
            None => return ElfVerdict::Reject("ELF: truncated program header"),
        };
        let p_vaddr = match rd_u64(data, ph + P_VADDR_OFF) {
            Some(v) => v,
            None => return ElfVerdict::Reject("ELF: truncated program header"),
        };
        let p_filesz = match rd_u64(data, ph + P_FILESZ_OFF) {
            Some(v) => v,
            None => return ElfVerdict::Reject("ELF: truncated program header"),
        };
        let p_memsz = match rd_u64(data, ph + P_MEMSZ_OFF) {
            Some(v) => v,
            None => return ElfVerdict::Reject("ELF: truncated program header"),
        };

        // filesz must not exceed memsz (a negative bss tail is impossible). (R5.9)
        if p_filesz > p_memsz {
            return ElfVerdict::Reject("ELF: p_filesz exceeds p_memsz");
        }

        // File data range must lie within the supplied buffer. (R5.9)
        let file_end = match p_offset.checked_add(p_filesz) {
            Some(v) => v,
            None => return ElfVerdict::Reject("ELF: segment file range overflow"),
        };
        if file_end > data.len() as u64 {
            return ElfVerdict::Reject("ELF: file data beyond input");
        }

        // Virtual range must be canonical lower-half and not overflow. For
        // ET_DYN this is the zero-bias lower bound; choose_bias re-checks the
        // biased range. (R5.5)
        let vaddr_end = match p_vaddr.checked_add(p_memsz) {
            Some(v) => v,
            None => return ElfVerdict::Reject("ELF: vaddr range overflow"),
        };
        let page_end = match page_up(vaddr_end) {
            Some(v) => v,
            None => return ElfVerdict::Reject("ELF: vaddr page-round overflow"),
        };
        if page_end >= USER_ADDR_MAX {
            return ElfVerdict::Reject("ELF: segment outside user address space");
        }
    }

    ElfVerdict::Load {
        kind,
        bias_required: kind == ElfKind::Dyn,
    }
}

/// Choose a page-aligned load bias for a static-PIE (`ET_DYN`) image.
///
/// `max_vaddr_end` is the maximum `p_vaddr + p_memsz` across all `PT_LOAD`
/// segments (the highest un-biased virtual address the image touches). Returns
/// a 4096-aligned `bias` such that
/// `bias + page_up(max_vaddr_end) < USER_ADDR_MAX`, so every biased segment stays
/// strictly inside the lower-half user range (R5.2 / Property 16). Returns `None`
/// when no such bias at the deterministic [`PIE_BASE`] fits. All arithmetic is
/// overflow-safe.
pub fn choose_bias(max_vaddr_end: u64) -> Option<u64> {
    let bias = PIE_BASE; // already 4096-aligned
    let end = page_up(max_vaddr_end)?;
    let top = bias.checked_add(end)?;
    if top < USER_ADDR_MAX {
        Some(bias)
    } else {
        None
    }
}
