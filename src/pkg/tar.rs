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

use alloc::string::String;
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
const OFF_LINKNAME: usize = 157;
const END_LINKNAME: usize = 257;
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
    /// A symbolic link (`typeflag` `'2'`): [`TarEntry::link_target`] is the target
    /// **verbatim** — the extractor never resolves or normalizes it (issue #18).
    Symlink,
    /// A hard link (`typeflag` `'1'`): [`TarEntry::link_target`] is the *archive*
    /// path of another member this entry shares an inode with.
    ///
    /// Split out of `Symlink` by issue #18: the two need opposite handling (a
    /// symlink is created immediately and may dangle, while a hard link needs its
    /// target to exist first), and dpkg's archives carry both.
    Hardlink,
    /// Any other entry kind (device, fifo, ...).
    Other,
}

/// A single enumerated ustar entry. `path` and `content` borrow the input buffer.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct TarEntry<'a> {
    /// The entry's path, taken from the NUL-trimmed `name` field.
    ///
    /// Long paths arrive either as a GNU `'L'`/"@LongLink" header (the path is then
    /// borrowed from that header's content) or through the ustar `prefix` field;
    /// join the two with [`effective_path`] before using the entry.
    pub path: &'a str,
    /// The ustar `prefix` field, empty for GNU-format archives.
    ///
    /// The canonical path is `prefix + "/" + path` when this is non-empty. It is
    /// kept separate so [`TarEntry`] stays a zero-copy `Copy` view of the input.
    pub prefix: &'a str,
    /// The entry kind, classified from the `typeflag` byte.
    pub kind: TarType,
    /// The octal `mode` field, decoded to a permission bitmask.
    pub mode: u32,
    /// The declared content size in bytes (octal `size` field).
    pub size: u64,
    /// The entry's content as a zero-copy slice of the input (empty for non-files).
    pub content: &'a [u8],
    /// The link target from the `linkname` field (empty for non-link entries).
    pub link_target: &'a str,
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
    /// A pax extension record (`'x'`/`'g'`) is malformed. A stream the parser
    /// cannot interpret is refused as a whole rather than mis-read entry by entry.
    BadExtension,
    /// A symlink/hard link entry carries an empty or unusable link target.
    BadLinkTarget,
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

/// Enumerate the entries of a (decompressed) ustar/gnu/pax `data.tar` stream.
///
/// Iterates fixed 512-byte headers, stopping at the end-of-archive marker (a header
/// whose `name` field begins with a NUL byte, which also covers the conventional two
/// trailing zero blocks). For each entry it parses the name, mode, size, and
/// typeflag, validates the header checksum, and exposes regular-file content as a
/// zero-copy slice. Never reads past `buf` and never panics (R9.5, R9.6).
///
/// # Long paths and long link targets (issue #18)
///
/// Three encodings exist in the wild and all three are understood:
///
///   * **GNU** — a `'L'` header (`././@LongLink`) whose *content* is the path of the
///     **next** entry, and a `'K'` header the same for its link target. The
///     extension headers are not entries themselves; the borrowed path comes from
///     the extension's content, so [`TarEntry`] stays zero-copy.
///   * **ustar** — the `prefix` field, joined as `prefix + "/" + path` by
///     [`effective_path`].
///   * **pax** — an `'x'` (per-entry) or `'g'` (global) record stream; the `path=`
///     and `linkpath=` keys are applied to the following entries. Other keys are
///     ignored, but a *malformed* record refuses the whole archive
///     ([`TarError::BadExtension`]) rather than risking a mis-read.
///
/// Before issue #18 only the 100-byte `name` field was read, so a long path was
/// silently truncated and the file was installed under the wrong name.
///
/// Errors:
///   * [`TarError::BadHeaderChecksum`] — stored checksum mismatch or unparseable.
///   * [`TarError::BadSizeField`] — the octal `size` field is malformed.
///   * [`TarError::LengthInconsistent`] — a length/padding computation overflows.
///   * [`TarError::Truncated`] — a header or content runs past the buffer end.
///   * [`TarError::BadExtension`] — a pax record is malformed.
///   * [`TarError::BadLinkTarget`] — a link entry has no usable target.
pub fn read_tar(buf: &[u8]) -> Result<Vec<TarEntry<'_>>, TarError> {
    let mut entries = Vec::new();
    let mut offset = 0usize;

    // State carried by the extension headers, all borrowed from `buf`:
    // pending long name (`'L'` or pax `path=`), pending link target (`'K'` or pax
    // `linkpath=`), and — for pax `'g'` — the same values applied to every
    // following entry until the archive ends.
    let mut next_path: Option<&str> = None;
    let mut next_link: Option<&str> = None;
    let mut global_path: Option<&str> = None;
    let mut global_link: Option<&str> = None;

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
        let stored =
            parse_octal(&block[OFF_CHKSUM..END_CHKSUM]).ok_or(TarError::BadHeaderChecksum)?;
        if stored != header_checksum(block) {
            return Err(TarError::BadHeaderChecksum);
        }

        // Size is strict; mode is lenient (defaults to 0 when unparseable).
        let size = parse_octal(&block[OFF_SIZE..END_SIZE]).ok_or(TarError::BadSizeField)?;
        let mode = parse_octal(&block[OFF_MODE..END_MODE]).unwrap_or(0) as u32;

        // Locate the content slice, bounds-checked against the buffer. Every entry
        // (including the extension headers) declares its own size, so this happens
        // before the kind is acted upon.
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

        // Path from the NUL-trimmed name field; must be valid UTF-8 to borrow as &str.
        // (Corruption in the name flips the checksum and is rejected above, so this is
        // only reached for checksum-valid headers.)
        let name_bytes = nul_trim(&block[OFF_NAME..END_NAME]);
        let name = core::str::from_utf8(name_bytes).map_err(|_| TarError::LengthInconsistent)?;
        let prefix_bytes = nul_trim(&block[OFF_PREFIX..END_PREFIX]);
        let prefix =
            core::str::from_utf8(prefix_bytes).map_err(|_| TarError::LengthInconsistent)?;

        match block[OFF_TYPEFLAG] {
            // GNU long name (`L`) / long link target (`K`): the content is the value
            // for the next entry. Neither is an entry of its own.
            b'L' => {
                next_path = Some(str_from_nul_padded(content)?);
                offset = next;
                continue;
            }
            b'K' => {
                next_link = Some(str_from_nul_padded(content)?);
                offset = next;
                continue;
            }
            // pax extended headers: apply `path=`/`linkpath=` to the following entry
            // (`x`) or to every following entry (`g`).
            b'x' | b'g' => {
                let (p_path, p_link) = parse_pax_records(content)?;
                if block[OFF_TYPEFLAG] == b'g' {
                    if p_path.is_some() {
                        global_path = p_path;
                    }
                    if p_link.is_some() {
                        global_link = p_link;
                    }
                } else {
                    if p_path.is_some() {
                        next_path = p_path;
                    }
                    if p_link.is_some() {
                        next_link = p_link;
                    }
                }
                offset = next;
                continue;
            }
            _ => {}
        }

        let kind = match block[OFF_TYPEFLAG] {
            b'0' | 0 => TarType::Regular,
            b'5' => TarType::Directory,
            b'1' => TarType::Hardlink,
            b'2' => TarType::Symlink,
            _ => TarType::Other,
        };

        // The pending extension value wins over the header field; the per-entry one
        // is consumed, the global one persists.
        // A per-entry extension wins over the global one, which wins over the
        // 100-byte `name` field.
        let path = next_path.or(global_path).unwrap_or(name);
        let link_target = if matches!(kind, TarType::Symlink | TarType::Hardlink) {
            let header_link = core::str::from_utf8(nul_trim(&block[OFF_LINKNAME..END_LINKNAME]))
                .map_err(|_| TarError::LengthInconsistent)?;
            next_link.or(global_link).unwrap_or(header_link)
        } else {
            ""
        };
        next_path = None;
        next_link = None;

        entries.push(TarEntry {
            path,
            prefix,
            kind,
            mode,
            size,
            content,
            link_target,
        });

        offset = next;
    }

    Ok(entries)
}

/// A NUL- (and padding-) terminated string from an extension header's content.
///
/// GNU `'L'`/`'K'` contents are NUL-terminated; pax values are not, which is why
/// the trailing NULs are trimmed rather than required.
fn str_from_nul_padded(content: &[u8]) -> Result<&str, TarError> {
    core::str::from_utf8(nul_trim(content)).map_err(|_| TarError::LengthInconsistent)
}

/// Parse a pax extension payload into its `path=` and `linkpath=` keys.
///
/// A record is `<decimal length> <key>=<value>\n`, where the length covers the whole
/// record including the length field itself. Unknown keys are ignored; a malformed
/// record is [`TarError::BadExtension`], because guessing here would install files
/// under wrong names.
fn parse_pax_records(content: &[u8]) -> Result<(Option<&str>, Option<&str>), TarError> {
    let mut path = None;
    let mut link = None;
    let mut pos = 0usize;
    while pos < content.len() {
        // Skip the padding NULs that terminate a pax payload.
        if content[pos] == 0 {
            pos += 1;
            continue;
        }
        let space = content[pos..]
            .iter()
            .position(|b| *b == b' ')
            .ok_or(TarError::BadExtension)?;
        let len_str =
            core::str::from_utf8(&content[pos..pos + space]).map_err(|_| TarError::BadExtension)?;
        let rec_len: usize = len_str.trim().parse().map_err(|_| TarError::BadExtension)?;
        if rec_len < space + 2 || rec_len > content.len() - pos {
            return Err(TarError::BadExtension);
        }
        let record = &content[pos..pos + rec_len];
        if record[rec_len - 1] != b'\n' {
            return Err(TarError::BadExtension);
        }
        let body = &record[space + 1..rec_len - 1];
        let eq = body
            .iter()
            .position(|b| *b == b'=')
            .ok_or(TarError::BadExtension)?;
        let key = core::str::from_utf8(&body[..eq]).map_err(|_| TarError::BadExtension)?;
        let value = core::str::from_utf8(&body[eq + 1..]).map_err(|_| TarError::BadExtension)?;
        match key {
            "path" => path = Some(value),
            "linkpath" => link = Some(value),
            _ => {}
        }
        pos += rec_len;
    }
    Ok((path, link))
}

/// The entry's effective path: `prefix + "/" + path` when the ustar `prefix` field
/// is set, else `path` itself.
///
/// The join allocates only for prefixed entries; the common (GNU-format) case stays
/// a borrow of the input buffer.
pub fn effective_path(entry: &TarEntry<'_>) -> String {
    if entry.prefix.is_empty() {
        String::from(entry.path)
    } else {
        let mut s = String::with_capacity(entry.prefix.len() + 1 + entry.path.len());
        s.push_str(entry.prefix);
        s.push('/');
        s.push_str(entry.path);
        s
    }
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

/// How [`write_tar_members`] encodes a path that does not fit the 100-byte `name`
/// field.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TarFormat {
    /// ustar `prefix` (`prefix + "/" + name`), what `--format=ustar` produces.
    /// Paths that fit neither field fall back to a GNU `'L'` header.
    UstarPrefix,
    /// GNU `'L'`/`'K'` headers for every long path/target, what dpkg's `tar`
    /// produces by default.
    GnuLongName,
}

/// One member of a stream built by [`write_tar_members`].
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TarMember<'a> {
    /// A regular file with content.
    File { path: &'a str, content: &'a [u8] },
    /// A directory.
    Directory { path: &'a str },
    /// A symbolic link; `target` is stored verbatim.
    Symlink { path: &'a str, target: &'a str },
    /// A hard link to the archive path `target`.
    Hardlink { path: &'a str, target: &'a str },
}

/// Emit a valid ustar/GNU stream for `members`, using `format` for long paths.
///
/// This is the fixture builder for the link tests (issue #18): it can produce
/// symlinks, hard links, GNU `'L'`/`'K'` extension headers and ustar `prefix`
/// paths, so the parser's handling of all of them is exercised by properties
/// instead of hand-assembled byte buffers. Pure and panic-free; a member whose
/// path or target exceeds the representable length is dropped rather than
/// truncated (a truncated path would be a wrong entry, which is exactly the bug
/// this writer exists to catch).
pub fn write_tar_members(members: &[TarMember<'_>], format: TarFormat) -> Vec<u8> {
    let mut out = Vec::new();

    for member in members {
        let (path, typeflag, content, link) = match *member {
            TarMember::File { path, content } => (path, b'0', content, ""),
            TarMember::Directory { path } => (path, b'5', &[][..], ""),
            TarMember::Symlink { path, target } => (path, b'2', &[][..], target),
            TarMember::Hardlink { path, target } => (path, b'1', &[][..], target),
        };

        // Long path first: a GNU `'L'` header unless the ustar prefix can carry it.
        let mut prefix = "";
        let mut name = path;
        if path.len() > END_NAME - OFF_NAME {
            match long_path_split(path, format) {
                Some((p, n)) => {
                    prefix = p;
                    name = n;
                }
                None => {
                    // GNU `'L'`: the path goes into an extension entry.
                    let mut ext = [0u8; BLOCK];
                    write_common(&mut ext, "././@LongLink", 0, b'L', "", "");
                    write_octal(&mut ext[OFF_SIZE..END_SIZE], (path.len() + 1) as u64);
                    sign_header(&mut ext);
                    out.extend_from_slice(&ext);
                    let mut body = Vec::with_capacity(path.len() + 1);
                    body.extend_from_slice(path.as_bytes());
                    body.push(0);
                    append_padded(&mut out, &body);
                }
            }
        }
        // Long link target: GNU `'K'`.
        if !link.is_empty() && link.len() > END_LINKNAME - OFF_LINKNAME {
            if format == TarFormat::UstarPrefix && link.len() <= END_LINKNAME - OFF_LINKNAME {
                // unreachable today (the field cannot hold it); kept explicit
            }
            let mut ext = [0u8; BLOCK];
            write_common(&mut ext, "././@LongLink", 0, b'K', "", "");
            write_octal(&mut ext[OFF_SIZE..END_SIZE], (link.len() + 1) as u64);
            sign_header(&mut ext);
            out.extend_from_slice(&ext);
            let mut body = Vec::with_capacity(link.len() + 1);
            body.extend_from_slice(link.as_bytes());
            body.push(0);
            append_padded(&mut out, &body);
        }

        let mut header = [0u8; BLOCK];
        write_common(&mut header, name, 0o644, typeflag, link, prefix);
        write_octal(&mut header[OFF_SIZE..END_SIZE], content.len() as u64);
        sign_header(&mut header);
        out.extend_from_slice(&header);
        append_padded(&mut out, content);
    }

    // Two trailing zero blocks terminate the archive.
    out.resize(out.len() + 2 * BLOCK, 0);
    out
}

/// Split a long path into the ustar `prefix` + `name` pair, when it fits.
fn long_path_split(path: &str, format: TarFormat) -> Option<(&str, &str)> {
    if format == TarFormat::GnuLongName {
        return None;
    }
    let name_max = END_NAME - OFF_NAME;
    let prefix_max = END_PREFIX - OFF_PREFIX;
    let split = path.rfind('/')?;
    let (prefix, name) = (&path[..split], &path[split + 1..]);
    if !name.is_empty() && name.len() <= name_max && prefix.len() <= prefix_max {
        Some((prefix, name))
    } else {
        None
    }
}

/// Fill the common header fields (everything except size and checksum).
fn write_common(
    header: &mut [u8; BLOCK],
    name: &str,
    mode: u64,
    typeflag: u8,
    link: &str,
    prefix: &str,
) {
    let nb = name.as_bytes();
    let n = core::cmp::min(nb.len(), END_NAME - OFF_NAME);
    header[OFF_NAME..OFF_NAME + n].copy_from_slice(&nb[..n]);
    write_octal(&mut header[OFF_MODE..END_MODE], mode);
    header[OFF_TYPEFLAG] = typeflag;
    let lb = link.as_bytes();
    let l = core::cmp::min(lb.len(), END_LINKNAME - OFF_LINKNAME);
    header[OFF_LINKNAME..OFF_LINKNAME + l].copy_from_slice(&lb[..l]);
    header[OFF_MAGIC..OFF_MAGIC + 6].copy_from_slice(b"ustar\0");
    header[OFF_VERSION..OFF_VERSION + 2].copy_from_slice(b"00");
    let pb = prefix.as_bytes();
    let p = core::cmp::min(pb.len(), END_PREFIX - OFF_PREFIX);
    header[OFF_PREFIX..OFF_PREFIX + p].copy_from_slice(&pb[..p]);
}

/// Encode the checksum over an already-filled header.
fn sign_header(header: &mut [u8; BLOCK]) {
    for b in header[OFF_CHKSUM..END_CHKSUM].iter_mut() {
        *b = b' ';
    }
    let sum = header_checksum(header);
    write_chksum(&mut header[OFF_CHKSUM..END_CHKSUM], sum);
}

/// Append `data` followed by NUL padding to the next 512-byte boundary.
fn append_padded(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(data);
    let rem = data.len() % BLOCK;
    if rem != 0 {
        out.resize(out.len() + (BLOCK - rem), 0);
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
    let members: Vec<TarMember<'_>> = entries
        .iter()
        .map(|(path, content)| TarMember::File { path, content })
        .collect();
    write_tar_members(&members, TarFormat::UstarPrefix)
}
