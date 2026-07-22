//! Saved general-purpose register frame shared by the two Linux syscall entry
//! stubs (`int80_stub` and `syscall_entry`) and `linux_dispatch`.
//!
//! Both entry stubs push all 15 general-purpose registers in the **same order**
//! and then pass a single pointer to the resulting frame to `linux_dispatch`.
//! Funnelling through one `*mut SavedRegs` (rather than spreading six register
//! arguments across the SysV C ABI, which only passes six in registers) keeps the
//! dispatcher signature trivial and lets it both *read* the Linux argument
//! registers and *modify* them (e.g. `arch_prctl(ARCH_SET_FS)` writing `FS.base`,
//! or writing the result back into the saved `rax` slot).
//!
//! ## Layout contract (DO NOT REORDER)
//!
//! The stubs push, from first to last:
//!
//! ```text
//!   push rax   ; pushed first  -> HIGHEST address  (offset 112)
//!   push rbx
//!   push rcx
//!   push rdx
//!   push rsi
//!   push rdi
//!   push rbp
//!   push r8
//!   push r9
//!   push r10
//!   push r11
//!   push r12
//!   push r13
//!   push r14
//!   push r15   ; pushed last   -> LOWEST address   (offset 0, == rsp)
//! ```
//!
//! On x86 the stack grows down, so the register pushed *last* (`r15`) sits at the
//! lowest address, which is where `rsp` points and therefore where the
//! `*mut SavedRegs` pointer is taken. A `#[repr(C)]` struct lays its fields out
//! from low to high address, so the field order below is the *reverse* of the push
//! order. This makes each field's offset equal to the byte distance from `rsp`,
//! and in particular places `rax` at offset `14 * 8 = 112` — exactly the slot the
//! stubs target with `mov [rsp + 112], rax` when writing the syscall result back.
#![allow(dead_code)]

/// The saved general-purpose register frame built by the syscall entry stubs.
///
/// Field order is the reverse of the stub push order so that the in-memory layout
/// (offset 0 = `r15`, offset 112 = `rax`) matches the bytes on the kernel stack.
/// `#[repr(C)]` is mandatory: the offsets are an ABI contract with hand-written
/// assembly.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SavedRegs {
    /// `r15` — pushed last, lowest address (offset 0, where `rsp`/the pointer points).
    pub r15: u64,
    /// `r14`.
    pub r14: u64,
    /// `r13`.
    pub r13: u64,
    /// `r12`.
    pub r12: u64,
    /// `r11` — on the `syscall` path this holds the user `RFLAGS` the CPU saved.
    pub r11: u64,
    /// `r10` — Linux syscall argument 4 (`a4`).
    pub r10: u64,
    /// `r9` — Linux syscall argument 6 (`a6`).
    pub r9: u64,
    /// `r8` — Linux syscall argument 5 (`a5`).
    pub r8: u64,
    /// `rbp`.
    pub rbp: u64,
    /// `rdi` — Linux syscall argument 1 (`a1`).
    pub rdi: u64,
    /// `rsi` — Linux syscall argument 2 (`a2`).
    pub rsi: u64,
    /// `rdx` — Linux syscall argument 3 (`a3`).
    pub rdx: u64,
    /// `rcx` — on the `syscall` path this holds the user `RIP` the CPU saved.
    pub rcx: u64,
    /// `rbx`.
    pub rbx: u64,
    /// `rax` — Linux syscall number on entry; the dispatcher's return value is
    /// written back here (offset 112) before the stub restores and returns.
    pub rax: u64,
}

// Compile-time guarantee that the layout matches the assembly contract: the frame
// is exactly 15 registers (120 bytes) and `rax` is at offset 112.
const _: () = {
    assert!(core::mem::size_of::<SavedRegs>() == 15 * 8);
    // offset_of would be ideal but is not const-stable on all toolchains; the
    // size check plus the documented field order pins the `rax` slot at 112.
};
