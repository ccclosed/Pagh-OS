//! Pure `getdents64` record packing (Feature: linux-binary-compat).
//!
//! Pure, `core`+`alloc`-only logic shared with the `host-tests` crate (R11.6).
//! `getdents64` (217) serializes directory entries into the user buffer as a
//! sequence of packed `struct linux_dirent64`:
//!
//! ```c
//! struct linux_dirent64 {
//!     __u64        d_ino;     // 64-bit inode number
//!     __s64        d_off;     // offset to the next dirent
//!     unsigned short d_reclen; // length of this record
//!     unsigned char  d_type;   // file type
//!     char         d_name[];   // NUL-terminated filename
//! };
//! ```
//!
//! The fixed header is 19 bytes (`8 + 8 + 2 + 1`); the record length is the header
//! plus the name plus its NUL terminator, rounded **up to an 8-byte boundary** so
//! each successive record starts 8-byte aligned (every field stays naturally
//! aligned, which Linux relies on). The effectful handler in `io_sys` walks a
//! per-fd directory cursor and appends one of these records per child until the
//! next record would not fit in the caller's buffer.
//!
//! This module carries no kernel dependency, so the packing can be property-tested
//! on the host (sizes, alignment, name NUL-termination, field round-trip).
#![allow(dead_code)]

use alloc::vec;
use alloc::vec::Vec;

/// `d_type` value: unknown type.
pub const DT_UNKNOWN: u8 = 0;
/// `d_type` value: directory.
pub const DT_DIR: u8 = 4;
/// `d_type` value: regular file.
pub const DT_REG: u8 = 8;

/// Fixed-size header of a `linux_dirent64` preceding the variable-length name:
/// `d_ino` (8) + `d_off` (8) + `d_reclen` (2) + `d_type` (1) = 19 bytes.
pub const DIRENT_HEADER: usize = 19;

/// Compute the 8-byte-aligned `d_reclen` for an entry whose name is `name_len`
/// bytes long (the stored name is always NUL-terminated, hence the `+ 1`).
///
/// `reclen = align_up(DIRENT_HEADER + name_len + 1, 8)`. Always a positive
/// multiple of 8 and always `> DIRENT_HEADER`, so the record holds at least one
/// name byte slot plus its terminator.
#[inline]
pub fn dirent_reclen(name_len: usize) -> usize {
    (DIRENT_HEADER + name_len + 1 + 7) & !7
}

/// Pack a single `linux_dirent64` record into a freshly-allocated, fully
/// zero-initialized `Vec<u8>` of length [`dirent_reclen`]`(name.len())`.
///
/// Field layout (little-endian, matching the x86_64 ABI):
///   * `[0..8)`   `d_ino`
///   * `[8..16)`  `d_off`
///   * `[16..18)` `d_reclen`
///   * `[18]`     `d_type`
///   * `[19..]`   `d_name`, copied verbatim, followed by an implicit NUL (the
///                buffer is zero-filled) and trailing alignment padding.
///
/// Because the backing buffer starts zeroed, the byte immediately after the name
/// is guaranteed to be `0`, so the name is always NUL-terminated, and any padding
/// bytes up to `d_reclen` are zero.
pub fn encode_dirent64(d_ino: u64, d_off: i64, d_type: u8, name: &[u8]) -> Vec<u8> {
    let reclen = dirent_reclen(name.len());
    let mut rec = vec![0u8; reclen];
    rec[0..8].copy_from_slice(&d_ino.to_le_bytes());
    rec[8..16].copy_from_slice(&d_off.to_le_bytes());
    rec[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
    rec[18] = d_type;
    rec[DIRENT_HEADER..DIRENT_HEADER + name.len()].copy_from_slice(name);
    // rec[DIRENT_HEADER + name.len()] stays 0 (the NUL terminator).
    rec
}

/// Read back the `d_reclen` field of an encoded record (pure accessor used by the
/// host property tests to recover the record boundary).
#[inline]
pub fn record_reclen(rec: &[u8]) -> u16 {
    u16::from_le_bytes([rec[16], rec[17]])
}
