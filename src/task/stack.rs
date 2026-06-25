//! System V x86_64 initial process stack / auxiliary-vector encoder (pure).
//!
//! This module is the **pure layout half** of the `Stack_Initializer` component
//! (design §5). It computes the exact byte image of a Linux-conformant initial
//! user stack — `argc`, the `argv`/`envp` pointer arrays, the ELF auxiliary
//! vector, the 16 `AT_RANDOM` bytes, and the NUL-terminated argument/environment
//! strings — together with the entry `rsp`. It performs **no mapping**; the
//! effectful page mapper is task 13.2 (`run_linux_binary` glue).
//!
//! It is `core` + `alloc` only (uses [`alloc::vec::Vec`]) so it is exercised by
//! the host `proptest` harness (R11.6). The kernel provides a global allocator,
//! and the host-test crate links `std` (which provides `alloc`), so the same
//! source compiles in both places.
//!
//! Requirements covered: R6.1–R6.8 (stack/auxv layout) and R7.5 (argument gate).
#![allow(dead_code)]

use alloc::vec::Vec;

/// ELF auxiliary-vector type tags used by the encoder.
///
/// Values match the Linux x86_64 ABI exactly (design Data Models → `at`).
pub mod at {
    /// End-of-vector marker.
    pub const NULL: u64 = 0;
    /// Address of the program headers in the loaded image.
    pub const PHDR: u64 = 3;
    /// Size in bytes of one program-header entry.
    pub const PHENT: u64 = 4;
    /// Number of program-header entries.
    pub const PHNUM: u64 = 5;
    /// System page size.
    pub const PAGESZ: u64 = 6;
    /// Base address of the interpreter (unused here, defined for completeness).
    pub const BASE: u64 = 7;
    /// Program entry-point address.
    pub const ENTRY: u64 = 9;
    /// Address of 16 bytes of random data.
    pub const RANDOM: u64 = 25;
}

/// Inputs to the auxiliary vector supplied by the ELF loader.
///
/// `random_ptr` is an output, not an input: the encoder *computes* the in-stack
/// address of the 16 random bytes itself (it owns their placement) and ignores
/// whatever value is passed in this field. It is kept in the struct so callers
/// can read the chosen address back if they construct the struct as scratch, but
/// [`build_initial_stack`] never reads it.
pub struct AuxInputs {
    /// `AT_PHDR`: address of the program headers in the user image.
    pub phdr: u64,
    /// `AT_PHENT`: size of one program-header entry.
    pub phent: u64,
    /// `AT_PHNUM`: number of program-header entries.
    pub phnum: u64,
    /// `AT_ENTRY`: program entry-point address.
    pub entry: u64,
    /// `AT_PAGESZ`: system page size.
    pub pagesz: u64,
    /// `AT_RANDOM`: ignored on input; the encoder fills the random block itself.
    pub random_ptr: u64,
}

/// The encoded initial stack: the bytes to copy plus the key user addresses.
///
/// `bytes` is the contiguous image occupying `[initial_rsp, stack_top)`; index 0
/// of `bytes` corresponds to user address `argc_addr` (== `initial_rsp`). A caller
/// (or property test) can locate any in-stack address `a` at `bytes[a - argc_addr]`.
pub struct StackImage {
    /// The raw stack image, lowest address first. `bytes[0]` lives at `argc_addr`.
    pub bytes: Vec<u8>,
    /// User virtual address of the `argc` word (16-byte aligned, R6.5).
    pub argc_addr: u64,
    /// Initial `rsp` for the process — equal to `argc_addr`.
    pub initial_rsp: u64,
}

/// Failure mode of [`build_initial_stack`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StackError {
    /// The encoded image does not fit within `[stack_low, stack_top)` (R6.8).
    TooLarge,
}

/// Number of bytes of random data referenced by `AT_RANDOM` (R6.6).
const RANDOM_LEN: u64 = 16;

/// Round `x` down to the nearest multiple of 16.
#[inline]
fn align_down_16(x: u64) -> u64 {
    x & !15
}

/// Build the System V x86_64 initial process stack image (pure; R6.1–R6.8).
///
/// Layout, from the lowest address (`argc_addr`, where `rsp` points at entry)
/// upward toward `stack_top`:
///
/// ```text
/// argc_addr -> [ argc (== argv.len()) ]
///              [ argv[0] .. argv[N-1] ][ NULL ]
///              [ envp[0] .. envp[M-1] ][ NULL ]
///              [ (AT_PHDR,..)(AT_PHENT,..)(AT_PHNUM,..)
///                (AT_ENTRY,..)(AT_PAGESZ,..)(AT_RANDOM,&rand)(AT_NULL,0) ]
///              [ ... zero padding ... ]
///              [ 16 random bytes ]   <- AT_RANDOM points here
///              [ argv/envp strings, each NUL-terminated ]   (ends at stack_top)
/// ```
///
/// Every `argv`/`envp` pointer references the first byte of its NUL-terminated
/// copy stored inside this image (R6.7), and `&argc` is 16-byte aligned (R6.5).
/// Returns [`StackError::TooLarge`] if the image cannot fit in
/// `[stack_low, stack_top)` (R6.8).
pub fn build_initial_stack(
    stack_top: u64,
    stack_low: u64,
    argv: &[&[u8]],
    envp: &[&[u8]],
    aux: &AuxInputs,
    random16: [u8; 16],
) -> Result<StackImage, StackError> {
    // ── Fixed pointer/auxv table size (in bytes), all 8-byte words ───────────
    //   argc (1) + argv ptrs (N) + NULL (1) + envp ptrs (M) + NULL (1)
    //   + auxv: 6 real entries + 1 AT_NULL = 7 pairs = 14 words.
    let n = argv.len() as u64;
    let m = envp.len() as u64;
    let table_words = 1u64
        .checked_add(n)
        .and_then(|w| w.checked_add(1)) // argv NULL
        .and_then(|w| w.checked_add(m))
        .and_then(|w| w.checked_add(1)) // envp NULL
        .and_then(|w| w.checked_add(14)) // 7 auxv pairs
        .ok_or(StackError::TooLarge)?;
    let table_size = table_words.checked_mul(8).ok_or(StackError::TooLarge)?;

    // ── String blob: argv strings then envp strings, each NUL-terminated ─────
    // Record each string's offset within the blob so we can compute its final
    // absolute user address once `argc_addr` is fixed.
    let mut str_blob: Vec<u8> = Vec::new();
    let mut argv_str_off: Vec<u64> = Vec::with_capacity(argv.len());
    let mut envp_str_off: Vec<u64> = Vec::with_capacity(envp.len());
    for s in argv {
        argv_str_off.push(str_blob.len() as u64);
        str_blob.extend_from_slice(s);
        str_blob.push(0);
    }
    for s in envp {
        envp_str_off.push(str_blob.len() as u64);
        str_blob.extend_from_slice(s);
        str_blob.push(0);
    }
    let str_total = str_blob.len() as u64;

    // ── Address computation (high → low) ─────────────────────────────────────
    // Strings sit flush against stack_top; the 16 random bytes sit just below
    // them; the fixed table sits below that with argc 16-byte aligned. Any slack
    // becomes zero padding between the table and the random bytes.
    let str_start = stack_top.checked_sub(str_total).ok_or(StackError::TooLarge)?;
    let rand_start = str_start.checked_sub(RANDOM_LEN).ok_or(StackError::TooLarge)?;
    let table_floor = rand_start.checked_sub(table_size).ok_or(StackError::TooLarge)?;
    let argc_addr = align_down_16(table_floor);

    // Fit check (R6.8): must stay within the mapped stack region.
    if argc_addr < stack_low || stack_top < argc_addr {
        return Err(StackError::TooLarge);
    }

    // ── Materialize the contiguous image [argc_addr, stack_top) ──────────────
    let image_len = (stack_top - argc_addr) as usize;
    let mut bytes = alloc::vec![0u8; image_len];

    // Helper: write a u64 (LE) at the byte offset for absolute address `addr`.
    let put_u64 = |buf: &mut [u8], addr: u64, val: u64| {
        let off = (addr - argc_addr) as usize;
        buf[off..off + 8].copy_from_slice(&val.to_le_bytes());
    };

    // Walk the table writing words at successive 8-byte slots from argc_addr.
    let mut cur = argc_addr;
    // argc (R6.2)
    put_u64(&mut bytes, cur, n);
    cur += 8;
    // argv pointers, then a single NULL (R6.1)
    for off in &argv_str_off {
        put_u64(&mut bytes, cur, str_start + off);
        cur += 8;
    }
    put_u64(&mut bytes, cur, 0);
    cur += 8;
    // envp pointers, then a single NULL (R6.1)
    for off in &envp_str_off {
        put_u64(&mut bytes, cur, str_start + off);
        cur += 8;
    }
    put_u64(&mut bytes, cur, 0);
    cur += 8;
    // auxv: each tag once (R6.3), with PHENT/PHNUM/ENTRY from inputs (R6.4),
    // AT_RANDOM pointing at the in-stack random block (R6.6), AT_NULL last (R6.1).
    let auxv: [(u64, u64); 7] = [
        (at::PHDR, aux.phdr),
        (at::PHENT, aux.phent),
        (at::PHNUM, aux.phnum),
        (at::ENTRY, aux.entry),
        (at::PAGESZ, aux.pagesz),
        (at::RANDOM, rand_start),
        (at::NULL, 0),
    ];
    for (tag, val) in auxv {
        put_u64(&mut bytes, cur, tag);
        cur += 8;
        put_u64(&mut bytes, cur, val);
        cur += 8;
    }
    // `cur` now equals argc_addr + table_size; the gap up to rand_start stays zero.

    // ── Random block (R6.6) ──────────────────────────────────────────────────
    let rand_off = (rand_start - argc_addr) as usize;
    bytes[rand_off..rand_off + 16].copy_from_slice(&random16);

    // ── String bytes (R6.7) ──────────────────────────────────────────────────
    let str_off = (str_start - argc_addr) as usize;
    bytes[str_off..str_off + str_total as usize].copy_from_slice(&str_blob);

    Ok(StackImage {
        bytes,
        argc_addr,
        initial_rsp: argc_addr,
    })
}

/// Run-request argument gate (R7.5).
///
/// Returns `true` iff `argv` has at most 256 entries **and** the combined byte
/// length of all arguments is at most 4096 bytes.
pub fn arg_gate(argv: &[&[u8]]) -> bool {
    if argv.len() > 256 {
        return false;
    }
    let mut total: usize = 0;
    for a in argv {
        total = match total.checked_add(a.len()) {
            Some(t) => t,
            None => return false,
        };
        if total > 4096 {
            return false;
        }
    }
    total <= 4096
}
