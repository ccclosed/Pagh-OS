//! Pure POSIX/ustar reader and writer for the Debian package layer.
//!
//! Pure, `core` + `alloc` only, allocation-bounded, and **panic-free** logic shared
//! with the `host-tests` crate (R11.6). [`read_tar`] enumerates a (decompressed)
//! `data.tar` byte stream into borrowed [`TarEntry`] records — exposing each regular
//! file's content as a zero-copy slice of the input — while validating every 512-byte
//! ustar header's checksum and length consistency without ever reading past the
//! buffer (R9.5, R9.6). [`write_tar`] is the inverse: it emits a valid ustar stream so
//! the round-trip property (R9.7) holds for any set of named entries.
//!
//! This module is intentionally self-contained: it depends on nothing from `deb.rs`
//! (or any other kernel module) so it can be `#[path]`-included by the host test
//! crate and compiled identically by the `#![no_std]` kernel.
#![allow(dead_code)]

use alloc::vec::Vec;

/// Size of a single ustar header/data block.
const BLOCK: usize = 512;

// ustar header field offsets (within a 512-byte block).
const OFF_NAME: usize = 0;
const END_NAME: usize = 100;
const OFF_MODE: usize = 100;
const END_MODE: usize = 108;
const OFF_SIZE: usize = 124;
const END_SIZE: usize = 136;
const OFF_CHKSUM: usize = 148;
const END_CHKSUM: usize = 156;
const OFF_TYPEFLAG: usize = 156;
const OFF_MAGIC: usize = 257;
const OFF_VERSION: usize = 263;
const OFF_PREFIX: usize = 345;
const END_PREFIX: usize = 500;

/// The kind of a tar entry, derived from the ustar `typeflag` byte.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TarType {
    /// A regular file (`typeflag` `'0'` or NUL).
    Regular,
    /// A directory (`typeflag` `'5'`).
    Directory,
    /// Any other entry kind (symlink, device, fifo, hardlink, ...).
    Other,
}

/// A single enumerated ustar entry. `path` and `content` borrow the input buffer.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct TarEntry<'a> {
    /// The entry's path, taken from the NUL-trimmed `name` field.
    pub path: &'a str,
    /// The entry kind, classified from the `typeflag` byte.
    pub kind: TarType,
    /// The octal `mode` field, decoded to a permission bitmask.
    pub mode: u32,
    /// The declared content size in bytes (octal `size` field).
    pub size: u64,
    /// The entry's content as a zero-copy slice of the input (empty for non-files).
    pub content: &'a [u8],
}

/// Reasons a ustar stream is rejected, each naming the field that failed (R9.6).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TarError {
    /// The stored header checksum does not match the recomputed value.
    BadHeaderChecksum,
    /// The octal `size` field could not be parsed.
    BadSizeField,
    /// A content/padding length computation overflowed or is impossible.
    LengthInconsistent,
    /// The declared content or padding extends beyond the end of the buffer.
    Truncated,
}

/// Parse an octal ASCII numeric field.
///
/// ustar numeric fields are octal digits, optionally surrounded by leading spaces
/// and terminated by a NUL or space. An empty/all-blank field parses to `0`. Returns
/// `None` if a non-octal byte appears or if a digit follows a terminator, or on
/// arithmetic overflow. Never panics.
fn parse_octal(field: &[u8]) -> Option<u64> {
    let mut value: u64 = 0;
    let mut started = false;
    let mut ended = false;
    for &b in field {
        match b {
            b' ' => {
                // Leading spaces are ignored; a space after digits terminates.
                if started {
                    ended = true;
                }
            }
            0 => {
                // NUL terminates the field.
                ended = true;
            }
            b'0'..=b'7' => {
                if ended {
                    // A digit after a terminator is malformed.
                    return None;
                }
                value = value.checked_mul(8)?.checked_add((b - b'0') as u64)?;
                started = true;
            }
            _ => return None,
        }
    }
    Some(value)
}

/// Recompute a ustar header checksum: the unsigned sum of all 512 bytes with the
/// 8-byte checksum field (`148..156`) treated as ASCII spaces (`0x20`).
fn header_checksum(block: &[u8]) -> u64 {
    let mut sum: u64 = 0;
    let mut i = 0;
    while i < BLOCK {
        if i >= OFF_CHKSUM && i < END_CHKSUM {
            sum += 0x20;
        } else {
            sum += block[i] as u64;
        }
        i += 1;
    }
    sum
}

/// Trim trailing NULs from a fixed-width string field.
fn nul_trim(field: &[u8]) -> &[u8] {
    let mut end = field.len();
    while end > 0 && field[end - 1] == 0 {
        end -= 1;
    }
    &field[..end]
}

/// Round a byte count up to the next multiple of [`BLOCK`], using checked
/// arithmetic. Returns `None` on overflow.
fn round_to_block(n: u64) -> Option<u64> {
    let blocks = n.checked_add(BLOCK as u64 - 1)? / BLOCK as u64;
    blocks.checked_mul(BLOCK as u64)
}

/// Enumerate the entries of a (decompressed) ustar `data.tar` stream.
///
/// Iterates fixed 512-byte headers, stopping at the end-of-archive marker (a header
/// whose `name` field begins with a NUL byte, which also covers the conventional two
/// trailing zero blocks). For each entry it parses the name, mode, size, and
/// typeflag, validates the header checksum, and exposes regular-file content as a
/// zero-copy slice. Never reads past `buf` and never panics (R9.5, R9.6).
///
/// Errors:
///   * [`TarError::BadHeaderChecksum`] — stored checksum mismatch or unparseable.
///   * [`TarError::BadSizeField`] — the octal `size` field is malformed.
///   * [`TarError::LengthInconsistent`] — a length/padding computation overflows.
///   * [`TarError::Truncated`] — a header or content runs past the buffer end.
///
/// Note on the ustar `prefix` field: when `prefix` (bytes `345..500`) is non-empty,
/// the canonical path is `prefix + "/" + name`. Because [`TarEntry::path`] is a
/// zero-copy borrow of the input, the joined form cannot be materialised without
/// allocation, so the `name` field is used directly. Streams produced by
/// [`write_tar`] (and typical short package paths) never set `prefix`.
pub fn read_tar(buf: &[u8]) -> Result<Vec<TarEntry<'_>>, TarError> {
    let mut entries = Vec::new();
    let mut offset = 0usize;

    loop {
        // A clean end exactly on a block boundary with no trailing zero block.
        if offset == buf.len() {
            break;
        }
        // We need a full header here; anything shorter is truncated.
        if offset + BLOCK > buf.len() {
            return Err(TarError::Truncated);
        }

        let block = &buf[offset..offset + BLOCK];

        // End-of-archive: a zero `name` block (covers the two trailing zero blocks).
        if block[OFF_NAME] == 0 {
            break;
        }

        // Validate the checksum before trusting any other field (R9.6).
        let stored = parse_octal(&block[OFF_CHKSUM..END_CHKSUM])
            .ok_or(TarError::BadHeaderChecksum)?;
        if stored != header_checksum(block) {
            return Err(TarError::BadHeaderChecksum);
        }

        // Size is strict; mode is lenient (defaults to 0 when unparseable).
        let size = parse_octal(&block[OFF_SIZE..END_SIZE]).ok_or(TarError::BadSizeField)?;
        let mode = parse_octal(&block[OFF_MODE..END_MODE]).unwrap_or(0) as u32;

        let kind = match block[OFF_TYPEFLAG] {
            b'0' | 0 => TarType::Regular,
            b'5' => TarType::Directory,
            _ => TarType::Other,
        };

        // Path from the NUL-trimmed name field; must be valid UTF-8 to borrow as &str.
        // (Corruption in the name flips the checksum and is rejected above, so this is
        // only reached for checksum-valid headers.)
        let name_bytes = nul_trim(&block[OFF_NAME..END_NAME]);
        let path = core::str::from_utf8(name_bytes).map_err(|_| TarError::LengthInconsistent)?;

        // Locate the content slice, bounds-checked against the buffer.
        let content_start = offset + BLOCK;
        let size_usize = usize::try_from(size).map_err(|_| TarError::LengthInconsistent)?;
        let content_end = content_start
            .checked_add(size_usize)
            .ok_or(TarError::LengthInconsistent)?;
        if content_end > buf.len() {
            return Err(TarError::Truncated);
        }
        let content = &buf[content_start..content_end];

        // Advance past the content padded up to the next 512-byte boundary.
        let padded = round_to_block(size).ok_or(TarError::LengthInconsistent)?;
        let padded_usize = usize::try_from(padded).map_err(|_| TarError::LengthInconsistent)?;
        let next = content_start
            .checked_add(padded_usize)
            .ok_or(TarError::LengthInconsistent)?;
        if next > buf.len() {
            return Err(TarError::Truncated);
        }

        entries.push(TarEntry {
            path,
            kind,
            mode,
            size,
            content,
        });

        offset = next;
    }

    Ok(entries)
}

/// Write a zero-padded octal numeric field of width `field.len()`: `width - 1`
/// octal digits followed by a trailing NUL. High bits beyond the field width are
/// dropped (callers only pass values that fit).
fn write_octal(field: &mut [u8], mut value: u64) {
    let last = field.len() - 1;
    field[last] = 0;
    let mut pos = last;
    while pos > 0 {
        pos -= 1;
        field[pos] = b'0' + (value & 0o7) as u8;
        value >>= 3;
    }
}

/// Write the 8-byte checksum field as 6 octal digits, a NUL, then a space — the
/// conventional ustar encoding that [`header_checksum`] / [`parse_octal`] accept.
fn write_chksum(field: &mut [u8], mut value: u64) {
    // field is exactly 8 bytes.
    field[6] = 0;
    field[7] = b' ';
    let mut pos = 6;
    while pos > 0 {
        pos -= 1;
        field[pos] = b'0' + (value & 0o7) as u8;
        value >>= 3;
    }
}

/// Emit a valid ustar stream for the given `(name, content)` entries.
///
/// Each entry is written as a 512-byte regular-file (`typeflag '0'`) header with
/// mode `0644`, the correct octal size, a valid `ustar\0`/`00` magic, and a correct
/// checksum, followed by the content padded up to a 512-byte boundary. The archive is
/// terminated by two zero blocks. This is the inverse of [`read_tar`], enabling the
/// round-trip property (R9.7). Pure and panic-free.
pub fn write_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();

    for (name, content) in entries {
        let mut header = [0u8; BLOCK];

        // name (0..100), truncated to the field width.
        let nb = name.as_bytes();
        let n = core::cmp::min(nb.len(), END_NAME - OFF_NAME);
        header[OFF_NAME..OFF_NAME + n].copy_from_slice(&nb[..n]);

        // mode 0644 (100..108).
        write_octal(&mut header[OFF_MODE..END_MODE], 0o644);

        // size (124..136).
        write_octal(&mut header[OFF_SIZE..END_SIZE], content.len() as u64);

        // typeflag '0' = regular file (156).
        header[OFF_TYPEFLAG] = b'0';

        // ustar magic + version.
        header[OFF_MAGIC..OFF_MAGIC + 6].copy_from_slice(b"ustar\0");
        header[OFF_VERSION..OFF_VERSION + 2].copy_from_slice(b"00");

        // Checksum: fill the field with spaces, sum, then encode it.
        for b in header[OFF_CHKSUM..END_CHKSUM].iter_mut() {
            *b = b' ';
        }
        let sum = header_checksum(&header);
        write_chksum(&mut header[OFF_CHKSUM..END_CHKSUM], sum);

        out.extend_from_slice(&header);

        // content + padding to the next 512-byte boundary.
        out.extend_from_slice(content);
        let rem = content.len() % BLOCK;
        if rem != 0 {
            out.resize(out.len() + (BLOCK - rem), 0);
        }
    }

    // Two trailing zero blocks terminate the archive.
    out.resize(out.len() + 2 * BLOCK, 0);

    out
}
