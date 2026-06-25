//! Minimal static RISC-V (`EM_RISCV`) ELF64 loader.
//!
//! Builds a tiny static `ET_EXEC` test binary in memory (the analogue of the
//! x86_64 kernel's `build_test_elf`), then parses its `PT_LOAD` segments, maps
//! each into user pages (U-bit, per-segment R/W/X), copies the file bytes, and
//! drops to U-mode at the entry point. This replaces the previous hand-mapped
//! raw-bytes path with a real ELF load, generalizing toward the full Linux
//! static-ELF loader.

use alloc::vec::Vec;

const PAGE: usize = 4096;

// ELF constants.
const ET_EXEC: u16 = 2;
const EM_RISCV: u16 = 243;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

fn push_u16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn push_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn push_u64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn rd_u16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}
fn rd_u32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}
fn rd_u64(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// Entry/load VA of the test program (above the identity window, GiB 4).
const TEST_ENTRY: u64 = 0x1_0000_0000;

/// Build a static `ET_EXEC` riscv64 ELF whose single `PT_LOAD` is a tiny program
/// that calls `print_u64(42)` then `exit(0)` via `ecall` (RV64: `li`/`ecall`/`j`).
pub fn build_test_elf() -> Vec<u8> {
    let code: [u32; 7] = [
        0x02a0_0513, // li a0, 42
        0x0020_0893, // li a7, 2  (SYS_PRINT_U64)
        0x0000_0073, // ecall
        0x0000_0513, // li a0, 0
        0x0010_0893, // li a7, 1  (SYS_EXIT)
        0x0000_0073, // ecall
        0x0000_006f, // j .
    ];
    let code_off: u64 = 64 + 56; // ehsize + one phentsize
    let codesz = (code.len() * 4) as u64;

    let mut e = Vec::new();
    // e_ident: magic, ELFCLASS64, ELFDATA2LSB, version, padding.
    e.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    push_u16(&mut e, ET_EXEC); // e_type
    push_u16(&mut e, EM_RISCV); // e_machine
    push_u32(&mut e, 1); // e_version
    push_u64(&mut e, TEST_ENTRY); // e_entry
    push_u64(&mut e, 64); // e_phoff
    push_u64(&mut e, 0); // e_shoff
    push_u32(&mut e, 0); // e_flags
    push_u16(&mut e, 64); // e_ehsize
    push_u16(&mut e, 56); // e_phentsize
    push_u16(&mut e, 1); // e_phnum
    push_u16(&mut e, 0); // e_shentsize
    push_u16(&mut e, 0); // e_shnum
    push_u16(&mut e, 0); // e_shstrndx
    // Program header (PT_LOAD, R+X).
    push_u32(&mut e, PT_LOAD); // p_type
    push_u32(&mut e, PF_X | 0x4); // p_flags R+X
    push_u64(&mut e, code_off); // p_offset
    push_u64(&mut e, TEST_ENTRY); // p_vaddr
    push_u64(&mut e, TEST_ENTRY); // p_paddr
    push_u64(&mut e, codesz); // p_filesz
    push_u64(&mut e, codesz); // p_memsz
    push_u64(&mut e, PAGE as u64); // p_align
    // Code.
    for w in code {
        push_u32(&mut e, w);
    }
    e
}

/// Parse, load, and run a static riscv64 ELF in U-mode. Never returns (the
/// program runs to its `exit` syscall). Panics-free: a malformed ELF prints an
/// error and parks.
pub fn load_and_run(elf: &[u8]) -> ! {
    // Validate the header.
    let ok_magic = elf.get(0..4) == Some(&[0x7f, b'E', b'L', b'F']);
    let class64 = elf.get(4) == Some(&2);
    let machine = rd_u16(elf, 18);
    let etype = rd_u16(elf, 16);
    if !ok_magic || !class64 || machine != Some(EM_RISCV) || etype != Some(ET_EXEC) {
        crate::kprintln!("rv: ELF load failed -- not a static riscv64 ET_EXEC");
        crate::cpu::park();
    }

    let entry = rd_u64(elf, 24).unwrap_or(0) as usize;
    let phoff = rd_u64(elf, 32).unwrap_or(0) as usize;
    let phentsize = rd_u16(elf, 54).unwrap_or(0) as usize;
    let phnum = rd_u16(elf, 56).unwrap_or(0) as usize;

    crate::kprintln!(
        "rv: loading riscv64 ELF -- entry {:#x}, {} program header(s)",
        entry,
        phnum
    );

    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        let p_type = rd_u32(elf, ph).unwrap_or(0);
        if p_type != PT_LOAD {
            continue;
        }
        let p_flags = rd_u32(elf, ph + 4).unwrap_or(0);
        let p_offset = rd_u64(elf, ph + 8).unwrap_or(0) as usize;
        let p_vaddr = rd_u64(elf, ph + 16).unwrap_or(0) as usize;
        let p_filesz = rd_u64(elf, ph + 32).unwrap_or(0) as usize;
        let p_memsz = rd_u64(elf, ph + 40).unwrap_or(0) as usize;

        let exec = p_flags & PF_X != 0;
        let write = p_flags & PF_W != 0;
        let pages = (p_memsz + PAGE - 1) / PAGE;

        for pg in 0..pages {
            let frame = crate::pmm::alloc_frame().expect("elf: out of frames");
            // SAFETY: fresh owned frame, identity-mapped → writable here.
            unsafe { core::ptr::write_bytes(frame as *mut u8, 0, PAGE) };

            // Copy this page's slice of the file image (the rest stays zeroed).
            let seg_off = pg * PAGE;
            if seg_off < p_filesz {
                let n = core::cmp::min(PAGE, p_filesz - seg_off);
                if let Some(src) = elf.get(p_offset + seg_off..p_offset + seg_off + n) {
                    // SAFETY: `frame` is a PAGE-sized owned, identity-mapped buffer.
                    unsafe {
                        core::ptr::copy_nonoverlapping(src.as_ptr(), frame as *mut u8, n);
                    }
                }
            }

            // SAFETY: paging active; VA is above the identity window.
            unsafe { crate::paging::map_user(p_vaddr + seg_off, frame, exec, write) };
        }
        crate::kprintln!(
            "rv:   PT_LOAD vaddr {:#x} memsz {} flags {}{}{}",
            p_vaddr,
            p_memsz,
            if p_flags & 0x4 != 0 { "r" } else { "-" },
            if write { "w" } else { "-" },
            if exec { "x" } else { "-" }
        );
    }

    // Map a one-page user stack just below USER_STACK_TOP.
    let stack = crate::pmm::alloc_frame().expect("elf: user stack frame");
    // SAFETY: as above.
    unsafe { crate::paging::map_user(crate::umode::USER_STACK_TOP - PAGE, stack, false, true) };
    crate::paging::flush();

    let user_sp = crate::umode::USER_STACK_TOP & !0xf;
    crate::kprintln!("rv: entering U-mode at {:#x} (user sp {:#x})...", entry, user_sp);
    // SAFETY: entry + stack are mapped user-accessible; never returns.
    unsafe { crate::umode::enter(entry, user_sp) };
}
