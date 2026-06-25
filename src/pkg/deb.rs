//! Pure `.deb` (`ar` archive) enumeration and member location (design
//! component 8, R9.1/R9.2/R9.4/R9.6).
//!
//! A `.deb` is an `ar` archive (`!<arch>\n` global magic) whose members are, in
//! order, `debian-binary`, `control.tar[.gz|.xz]`, and `data.tar[.gz|.xz]`. This
//! module implements only the **pure** core of the `Deb_Parser`:
//!
//!   * [`parse_ar`] — walk the `ar` member headers and return each member's
//!     trimmed name and bounds-checked content slice (R9.1, R9.6).
//!   * [`locate_members`] — pick out the three `.deb` members (R9.2).
//!   * [`compression_of`] — classify a member name's compression by suffix
//!     (R9.4).
//!
//! It is `core` + `alloc` only — no hardware, no globals — so it compiles into
//! both the kernel (`crate::pkg::deb`) and the `host-tests` crate via a
//! `#[path]` include, letting properties P22/P23 exercise the same source
//! (R11.6). Its external dependencies are all pure-Rust, `no_std`+`alloc`
//! decoders used by [`decompress_data`]: [`miniz_oxide`] for the gzip/DEFLATE
//! path, [`xz4rust`] for `.tar.xz` (LZMA2), and [`ruzstd`] for `.tar.zst`
//! (Zstandard). All three build on both the bare-metal target and the host.
//!
//! All parsing is hardened against adversarial input: every offset is computed
//! with checked arithmetic, every slice is bounds-checked against `buf`, and no
//! path can panic or read past the end of the buffer (R9.6).
#![allow(dead_code)]

use alloc::vec::Vec;

/// The 8-byte global magic that begins every `ar` archive.
const AR_MAGIC: &[u8] = b"!<arch>\n";

/// The fixed size, in bytes, of an `ar` member header.
const AR_HEADER_LEN: usize = 60;

/// Byte range of the 16-byte member-name field within an `ar` header.
const NAME_RANGE: core::ops::Range<usize> = 0..16;

/// Byte range of the 10-byte decimal-ASCII size field within an `ar` header.
const SIZE_RANGE: core::ops::Range<usize> = 48..58;

/// A single member of an `ar` archive: its trimmed name and content bytes.
///
/// `data` is a sub-slice of the original buffer (no copy), bounded to the
/// member's declared size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArMember<'a> {
    /// The member name from the 16-byte name field, trimmed of trailing spaces
    /// and a single trailing `/` (the GNU `ar` convention).
    pub name: &'a str,
    /// The member's content: exactly the declared-size bytes following the
    /// 60-byte header.
    pub data: &'a [u8],
}

/// Errors produced while parsing a `.deb`/`ar` container (R9.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebError {
    /// The buffer does not begin with the `ar` global magic `!<arch>\n`.
    BadArMagic,
    /// An `ar` member header was malformed: truncated, a non-decimal size
    /// field, a non-UTF-8 name, or a member range that exceeds the buffer.
    BadArHeader,
    /// A required `.deb` member (`debian-binary`, `control.tar*`, or
    /// `data.tar*`) was absent, or appeared more than once.
    MissingMember,
    /// A member uses a compression format other than none, gzip, xz, or zstd;
    /// the payload names the offending format (R9.4).
    UnsupportedCompression(&'static str),
    /// Decompression of a `data.tar.{gz,xz,zst}` member failed (task 8.3).
    DecompressFailed,
}

/// Enumerate every member of an `ar` archive in order (R9.1, R9.6).
///
/// Requires the 8-byte global magic `!<arch>\n` (else [`DebError::BadArMagic`]),
/// then walks fixed 60-byte member headers. For each header:
///
///   * the name is bytes `[0..16)` trimmed of trailing spaces and a single
///     trailing `/`, interpreted as UTF-8;
///   * the size is the decimal ASCII in bytes `[48..58)`;
///   * the content is the `size` bytes immediately following the header,
///     bounds-checked against `buf`;
///   * the cursor then advances past the content honoring 2-byte even
///     alignment (an odd-sized member is followed by one padding byte).
///
/// Any malformed header field, or a member range that would exceed `buf`,
/// yields [`DebError::BadArHeader`]. The function never indexes past `buf` and
/// never panics on any input.
pub fn parse_ar(buf: &[u8]) -> Result<Vec<ArMember<'_>>, DebError> {
    if buf.len() < AR_MAGIC.len() || &buf[..AR_MAGIC.len()] != AR_MAGIC {
        return Err(DebError::BadArMagic);
    }

    let mut members = Vec::new();
    let mut pos = AR_MAGIC.len();

    while pos < buf.len() {
        // A full 60-byte header must fit within the buffer.
        let header_end = pos.checked_add(AR_HEADER_LEN).ok_or(DebError::BadArHeader)?;
        if header_end > buf.len() {
            return Err(DebError::BadArHeader);
        }
        let header = &buf[pos..header_end];

        let name = parse_name(&header[NAME_RANGE])?;
        let size = parse_decimal(&header[SIZE_RANGE])?;

        // The member content is `size` bytes immediately after the header.
        let data_end = header_end.checked_add(size).ok_or(DebError::BadArHeader)?;
        if data_end > buf.len() {
            return Err(DebError::BadArHeader);
        }
        let data = &buf[header_end..data_end];

        members.push(ArMember { name, data });

        // Advance to the next header, skipping the 2-byte alignment pad byte
        // that follows an odd-sized member. (At end-of-buffer there is no pad
        // byte; the loop condition handles that without reading it.)
        pos = if size % 2 == 1 {
            data_end.checked_add(1).ok_or(DebError::BadArHeader)?
        } else {
            data_end
        };
    }

    Ok(members)
}

/// The three members of a parsed `.deb` container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DebMembers<'a> {
    /// The `debian-binary` format-version member.
    pub debian_binary: ArMember<'a>,
    /// The `control.tar[.gz|.xz]` metadata archive member.
    pub control: ArMember<'a>,
    /// The `data.tar[.gz|.xz]` installable-file-tree member.
    pub data: ArMember<'a>,
}

/// Locate the three required `.deb` members among enumerated `ar` members (R9.2).
///
/// Finds exactly one `debian-binary` member, exactly one member whose name
/// starts with `control.tar`, and exactly one whose name starts with
/// `data.tar`. If any of the three is absent — or any appears more than once —
/// [`DebError::MissingMember`] is returned.
pub fn locate_members<'a>(members: &'a [ArMember<'a>]) -> Result<DebMembers<'a>, DebError> {
    let mut debian_binary: Option<ArMember<'a>> = None;
    let mut control: Option<ArMember<'a>> = None;
    let mut data: Option<ArMember<'a>> = None;

    for member in members {
        if member.name == "debian-binary" {
            if set_once(&mut debian_binary, *member).is_err() {
                return Err(DebError::MissingMember);
            }
        } else if member.name.starts_with("control.tar") {
            if set_once(&mut control, *member).is_err() {
                return Err(DebError::MissingMember);
            }
        } else if member.name.starts_with("data.tar") {
            if set_once(&mut data, *member).is_err() {
                return Err(DebError::MissingMember);
            }
        }
    }

    match (debian_binary, control, data) {
        (Some(debian_binary), Some(control), Some(data)) => Ok(DebMembers {
            debian_binary,
            control,
            data,
        }),
        _ => Err(DebError::MissingMember),
    }
}

/// The compression format of a `.deb` tar member, classified by name suffix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    /// Plain, uncompressed tar (`.tar`).
    None,
    /// Gzip-compressed tar (`.tar.gz`).
    Gzip,
    /// Xz-compressed tar (`.tar.xz`).
    Xz,
    /// Zstandard-compressed tar (`.tar.zst`).
    Zstd,
}

/// Classify a member name's compression by its suffix (R9.4).
///
/// Returns [`Compression::None`] for `.tar`, [`Compression::Gzip`] for
/// `.tar.gz`, [`Compression::Xz`] for `.tar.xz`, and [`Compression::Zstd`] for
/// `.tar.zst`. Any other suffix yields [`DebError::UnsupportedCompression`]
/// naming the offending format, without reading past the buffer.
pub fn compression_of(name: &str) -> Result<Compression, DebError> {
    if name.ends_with(".tar.gz") {
        Ok(Compression::Gzip)
    } else if name.ends_with(".tar.xz") {
        Ok(Compression::Xz)
    } else if name.ends_with(".tar.zst") {
        Ok(Compression::Zstd)
    } else if name.ends_with(".tar") {
        Ok(Compression::None)
    } else if name.ends_with(".tar.bz2") {
        Err(DebError::UnsupportedCompression("bzip2"))
    } else if name.ends_with(".tar.lzma") {
        Err(DebError::UnsupportedCompression("lzma"))
    } else {
        Err(DebError::UnsupportedCompression("unknown"))
    }
}

/// Classify the compression of a *plain* URL/filename by its trailing extension
/// (not the `.tar.*` double suffix [`compression_of`] keys off).
///
/// This is the apt-index counterpart of [`compression_of`]: a Debian `Packages`
/// index is served as a bare `Packages.xz` / `Packages.gz` / `Packages.zst`, or
/// uncompressed as `Packages`. Maps `.xz` -> [`Compression::Xz`], `.gz` ->
/// [`Compression::Gzip`], `.zst` -> [`Compression::Zstd`]; any other suffix
/// (including a bare `Packages`) is treated as [`Compression::None`]. Unlike
/// [`compression_of`] this never errors — an unknown suffix simply means "no
/// compression", which the caller decodes as a verbatim copy.
pub fn compression_of_filename(name: &str) -> Compression {
    if name.ends_with(".xz") {
        Compression::Xz
    } else if name.ends_with(".gz") {
        Compression::Gzip
    } else if name.ends_with(".zst") {
        Compression::Zstd
    } else {
        Compression::None
    }
}

/// Store `value` into `slot` only if it is empty; error if already occupied.
///
/// Used to enforce the "exactly one" rule for each `.deb` member in
/// [`locate_members`].
fn set_once<'a>(slot: &mut Option<ArMember<'a>>, value: ArMember<'a>) -> Result<(), ()> {
    if slot.is_some() {
        Err(())
    } else {
        *slot = Some(value);
        Ok(())
    }
}

/// Parse a 16-byte `ar` name field into a `&str` (R9.1).
///
/// Trims trailing spaces, then a single trailing `/` (the GNU `ar` convention
/// where names are stored as `name/`). The remaining bytes must be valid UTF-8,
/// else [`DebError::BadArHeader`].
fn parse_name(field: &[u8]) -> Result<&str, DebError> {
    // Trim trailing space padding.
    let mut end = field.len();
    while end > 0 && field[end - 1] == b' ' {
        end -= 1;
    }
    // Trim a single trailing '/' (GNU ar appends one to disambiguate names).
    if end > 0 && field[end - 1] == b'/' {
        end -= 1;
    }
    core::str::from_utf8(&field[..end]).map_err(|_| DebError::BadArHeader)
}

/// Parse a decimal-ASCII size field into a `usize`, overflow-safe (R9.6).
///
/// Leading and trailing spaces are ignored. The field must contain at least one
/// digit and only ASCII digits; a non-digit, an empty field, or a value that
/// overflows `usize` yields [`DebError::BadArHeader`].
fn parse_decimal(field: &[u8]) -> Result<usize, DebError> {
    // Trim surrounding spaces (the field is space-padded on the right).
    let mut start = 0;
    let mut end = field.len();
    while end > 0 && field[end - 1] == b' ' {
        end -= 1;
    }
    while start < end && field[start] == b' ' {
        start += 1;
    }
    let digits = &field[start..end];
    if digits.is_empty() {
        return Err(DebError::BadArHeader);
    }

    let mut value: usize = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            return Err(DebError::BadArHeader);
        }
        let d = (b - b'0') as usize;
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add(d))
            .ok_or(DebError::BadArHeader)?;
    }
    Ok(value)
}

/// The maximum size, in bytes, of decompressed output this layer will produce
/// for any single member (R9.6).
///
/// `.deb` input is untrusted, and both xz and zstd can expand a tiny payload
/// into an enormous stream (a "decompression bomb"). Every decoder here is
/// streamed into a `Vec` and aborted with [`DebError::DecompressFailed`] the
/// moment the running output would exceed this cap, so a hostile member can
/// never drive unbounded allocation. 64 MiB comfortably covers a real
/// `data.tar` while staying within kernel memory bounds.
const MAX_DECOMPRESSED: usize = 64 * 1024 * 1024;

/// A higher decompression cap for whole-buffer (non-streaming) decompression.
///
/// **Superseded for the apt index path:** `pkg::apt::update` no longer
/// decompresses the index into one buffer — it streams via [`decompress_stream`]
/// (bounded by [`MAX_INDEX_STREAM_BYTES`]) so memory stays bounded regardless of
/// index size. This cap is retained for any caller that still wants a single
/// whole-buffer decode with a higher-than-`.deb` ceiling; it is sized to stay
/// within the kernel heap (256 MiB, see `memory::layout::HEAP_INITIAL_PAGES`).
/// Callers that decompress a `.deb` member keep the smaller [`MAX_DECOMPRESSED`].
pub const MAX_INDEX_DECOMPRESSED: usize = 96 * 1024 * 1024;

/// Decompress a `data.tar` (or `control.tar`) member into a plain, uncompressed
/// tar byte stream (R9.3, R9.4).
///
/// The behavior is selected by `c`, the compression previously classified from
/// the member name by [`compression_of`]:
///
///   * [`Compression::None`] — the member is already a plain tar; its bytes are
///     copied verbatim into a fresh `Vec`.
///   * [`Compression::Gzip`] — the gzip wrapper (RFC 1952 header + trailer) is
///     stripped and the embedded DEFLATE stream (RFC 1951) is inflated to the
///     original tar bytes via [`miniz_oxide`].
///   * [`Compression::Xz`] — the `.xz` container (LZMA2, optionally with BCJ /
///     delta filters) is decoded with [`xz4rust`], a pure-Rust, `no_std` port of
///     xz-embedded. The decoder is driven incrementally into an output `Vec`.
///   * [`Compression::Zstd`] — the Zstandard frame is decoded with [`ruzstd`]'s
///     `StreamingDecoder`, read incrementally into an output `Vec`.
///
/// Robustness (R9.6): the gzip header is parsed with checked arithmetic and is
/// fully bounds-checked against `member.data`. The xz and zstd paths stream into
/// a `Vec` that is capped at [`MAX_DECOMPRESSED`]; the moment the running output
/// would exceed the cap — or the decoder stops making progress, or any decode
/// error occurs — the function returns [`DebError::DecompressFailed`] rather than
/// allocating without bound. Any decoder error is mapped to
/// [`DebError::DecompressFailed`].
pub fn decompress_data(member: &ArMember<'_>, c: Compression) -> Result<Vec<u8>, DebError> {
    decompress_bytes(member.data, c)
}

/// Decompress a raw byte buffer under compression `c` into a plain byte stream,
/// capped at [`MAX_DECOMPRESSED`] (R9.3, R9.4).
///
/// This is the buffer-level core that [`decompress_data`] delegates to (passing
/// `member.data`). It is exposed so callers holding raw bytes that are *not* an
/// `ar` member — notably the apt-index path, which fetches a bare `Packages.xz`/
/// `.gz`/`.zst` over HTTP — can reuse the exact same gzip/xz/zstd decoders. See
/// [`decompress_data`] for the per-format behavior and robustness guarantees.
pub fn decompress_bytes(data: &[u8], c: Compression) -> Result<Vec<u8>, DebError> {
    decompress_bytes_capped(data, c, MAX_DECOMPRESSED)
}

/// Like [`decompress_bytes`] but with an explicit output cap `max`.
///
/// The apt-index path passes [`MAX_INDEX_DECOMPRESSED`] (256 MiB) so a real
/// `main` `Packages` decompresses, while `.deb` members keep the tighter default.
/// Output exceeding `max` (or any decoder error / stall) yields
/// [`DebError::DecompressFailed`]; no path reads past `data`.
pub fn decompress_bytes_capped(
    data: &[u8],
    c: Compression,
    max: usize,
) -> Result<Vec<u8>, DebError> {
    match c {
        // Already plain: hand back an owned copy of the bytes (still capped).
        Compression::None => {
            if data.len() > max {
                return Err(DebError::DecompressFailed);
            }
            Ok(data.to_vec())
        }

        // Strip the gzip wrapper, then inflate the raw DEFLATE payload, bounding
        // the inflated output at `max`.
        Compression::Gzip => {
            let deflate_start = gzip_payload_offset(data)?;
            // The slice from `deflate_start` includes the 8-byte gzip trailer
            // (CRC32 + ISIZE); the DEFLATE decoder stops at the stream's final
            // block and ignores those trailing bytes, so passing them is safe.
            let deflate = &data[deflate_start..];
            miniz_oxide::inflate::decompress_to_vec_with_limit(deflate, max)
                .map_err(|_| DebError::DecompressFailed)
        }

        // Decode the `.xz` container (LZMA2 + optional filters) via xz4rust.
        Compression::Xz => decompress_xz(data, max),

        // Decode the Zstandard frame via ruzstd.
        Compression::Zstd => decompress_zstd(data, max),
    }
}

/// Decode a complete `.xz` stream (`input`) into its uncompressed bytes,
/// capped at `max` (R9.4, R9.6).
///
/// Uses [`xz4rust::XzDecoder`] with a heap-allocated dictionary bounded to 64
/// MiB (the largest xz preset's window), driven block-by-block through a small
/// fixed scratch buffer. Decoding stops at the stream's `EndOfStream`. Any
/// decoder error, a stall (no input consumed and no output produced), or output
/// exceeding the cap maps to [`DebError::DecompressFailed`]. No path reads past
/// `input`.
fn decompress_xz(input: &[u8], max: usize) -> Result<Vec<u8>, DebError> {
    use xz4rust::{XzDecoder, XzNextBlockResult};

    // Bound the dictionary the decoder may allocate so a hostile header cannot
    // request the crate's 3 GiB maximum. The dictionary need only cover the LZMA2
    // window, so it stays bounded by the smaller `.deb` member cap regardless of
    // the (larger) index output cap `max`.
    let dict = core::cmp::min(max, MAX_DECOMPRESSED);
    let mut decoder = XzDecoder::in_heap_with_alloc_dict_size(xz4rust::DICT_SIZE_MIN, dict);

    let mut out: Vec<u8> = Vec::new();
    let mut scratch = [0u8; 8192];
    let mut pos: usize = 0;

    loop {
        let result = decoder
            .decode(&input[pos..], &mut scratch)
            .map_err(|_| DebError::DecompressFailed)?;
        match result {
            XzNextBlockResult::NeedMoreData(consumed, produced) => {
                pos = pos.checked_add(consumed).ok_or(DebError::DecompressFailed)?;
                push_capped(&mut out, &scratch[..produced], max)?;
                // Guard against a non-progressing decoder (e.g. truncated input
                // that yields neither consumption nor output) to avoid spinning.
                if consumed == 0 && produced == 0 {
                    return Err(DebError::DecompressFailed);
                }
            }
            XzNextBlockResult::EndOfStream(_, produced) => {
                push_capped(&mut out, &scratch[..produced], max)?;
                return Ok(out);
            }
        }
    }
}

/// Decode a complete Zstandard frame (`input`) into its uncompressed bytes,
/// capped at `max` (R9.4, R9.6).
///
/// Uses [`ruzstd`]'s `StreamingDecoder` over the crate's own `no_std`
/// `ruzstd::io::Read` trait (implemented for `&[u8]`), reading through a small
/// fixed scratch buffer until the decoder reports end-of-stream (a zero-length
/// read). Any decoder error or output exceeding `max` maps to
/// [`DebError::DecompressFailed`]. No path reads past `input`.
fn decompress_zstd(input: &[u8], max: usize) -> Result<Vec<u8>, DebError> {
    use ruzstd::decoding::StreamingDecoder;
    use ruzstd::io::Read;

    let mut src: &[u8] = input;
    let mut decoder = StreamingDecoder::new(&mut src).map_err(|_| DebError::DecompressFailed)?;

    let mut out: Vec<u8> = Vec::new();
    let mut scratch = [0u8; 8192];

    loop {
        let n = decoder
            .read(&mut scratch)
            .map_err(|_| DebError::DecompressFailed)?;
        if n == 0 {
            return Ok(out);
        }
        push_capped(&mut out, &scratch[..n], max)?;
    }
}

/// Append `chunk` to `out`, returning [`DebError::DecompressFailed`] if the
/// total would exceed `max` (R9.6).
fn push_capped(out: &mut Vec<u8>, chunk: &[u8], max: usize) -> Result<(), DebError> {
    let new_len = out.len().checked_add(chunk.len()).ok_or(DebError::DecompressFailed)?;
    if new_len > max {
        return Err(DebError::DecompressFailed);
    }
    out.extend_from_slice(chunk);
    Ok(())
}

/// A generous streamed-bytes safety budget for the apt **index** path
/// ([`decompress_stream`] called from `pkg::apt::update`).
///
/// Unlike [`MAX_INDEX_DECOMPRESSED`], this is **not** the size of any single
/// resident buffer: the streaming pipeline consumes each decompressed chunk and
/// drops it, so the decompressed index is never held whole in RAM. This cap is
/// only a runaway-stream stop — a decompression bomb or an absurdly large index
/// is aborted cleanly with [`DebError::DecompressFailed`] (surfaced by `apt` as a
/// clear error) instead of streaming forever. 512 MiB comfortably covers the full
/// Debian `main` `Packages` (~150 MiB decompressed) while still bounding work.
pub const MAX_INDEX_STREAM_BYTES: usize = 512 * 1024 * 1024;

/// The fixed output-chunk size used by [`decompress_stream`]. Each decoded chunk
/// is at most this many bytes; the caller's `sink` sees the output in pieces of
/// this size (the last piece may be shorter), so resident decode buffers stay
/// small regardless of the total decompressed size.
///
/// Kept at 8 KiB to match the original (proven-safe) decoder scratch size: these
/// buffers live on the (modest) kernel-thread stack, so a larger array here would
/// overflow it. Chunk size affects only how finely output is delivered, never
/// correctness.
const STREAM_CHUNK: usize = 8 * 1024;

/// Decompress `data` under compression `c`, delivering the uncompressed output to
/// `sink` in fixed-size chunks (at most [`STREAM_CHUNK`] bytes each) **without**
/// ever materializing the whole decompressed stream in memory.
///
/// This is the bounded-memory core behind `apt update`: the compressed body
/// (~10 MiB for a real `Packages.xz`) stays resident, but its decompressed form
/// (which can be ~150 MiB) is produced incrementally — each chunk is handed to
/// `sink` (which feeds it to the streaming [`crate::pkg::apt_index::StanzaParser`]
/// and then drops it) before the next chunk is decoded. Returns the total number
/// of output bytes produced.
///
/// `sink` is called for every non-empty output chunk; if it returns `Err`, the
/// decode stops and that error is propagated. Output exceeding `max` total bytes,
/// any decoder error, or a non-progressing decoder (truncated input) yields
/// [`DebError::DecompressFailed`]. No path reads past `data`.
pub fn decompress_stream<F>(
    data: &[u8],
    c: Compression,
    max: usize,
    mut sink: F,
) -> Result<usize, DebError>
where
    F: FnMut(&[u8]) -> Result<(), DebError>,
{
    let mut total: usize = 0;
    let mut emit = |chunk: &[u8], total: &mut usize| -> Result<(), DebError> {
        if chunk.is_empty() {
            return Ok(());
        }
        let new_total = total.checked_add(chunk.len()).ok_or(DebError::DecompressFailed)?;
        if new_total > max {
            return Err(DebError::DecompressFailed);
        }
        *total = new_total;
        sink(chunk)
    };

    match c {
        // Already plain: hand the bytes to the sink in fixed-size slices.
        Compression::None => {
            for chunk in data.chunks(STREAM_CHUNK) {
                emit(chunk, &mut total)?;
            }
            Ok(total)
        }

        // Strip the gzip wrapper, then stream raw DEFLATE through miniz_oxide.
        Compression::Gzip => {
            let deflate_start = gzip_payload_offset(data)?;
            stream_inflate(&data[deflate_start..], max, |chunk| emit(chunk, &mut total))?;
            Ok(total)
        }

        // Drive the xz4rust decoder block by block.
        Compression::Xz => {
            stream_xz(data, max, |chunk| emit(chunk, &mut total))?;
            Ok(total)
        }

        // Drive the ruzstd streaming decoder read by read.
        Compression::Zstd => {
            stream_zstd(data, max, |chunk| emit(chunk, &mut total))?;
            Ok(total)
        }
    }
}

/// Stream a raw DEFLATE payload (gzip wrapper already stripped) through
/// `miniz_oxide`'s incremental [`InflateState`], pushing each produced output
/// chunk to `sink`. The internal LZ77 dictionary lives inside the boxed
/// `InflateState` (~32 KiB) and the output scratch is one [`STREAM_CHUNK`] buffer,
/// so memory stays bounded irrespective of the inflated size.
fn stream_inflate<F>(mut input: &[u8], _max: usize, mut sink: F) -> Result<(), DebError>
where
    F: FnMut(&[u8]) -> Result<(), DebError>,
{
    use miniz_oxide::inflate::stream::{inflate, InflateState};
    use miniz_oxide::{DataFormat, MZFlush, MZStatus};

    let mut state = InflateState::new_boxed(DataFormat::Raw);
    let mut out = [0u8; STREAM_CHUNK];

    loop {
        let res = inflate(&mut state, input, &mut out, MZFlush::None);
        let consumed = res.bytes_consumed;
        let written = res.bytes_written;
        input = &input[consumed..];

        if written > 0 {
            sink(&out[..written])?;
        }

        match res.status {
            Ok(MZStatus::StreamEnd) => return Ok(()),
            Ok(_) => {
                // No progress with input still pending means a truncated/corrupt
                // stream that can never complete: fail rather than spin.
                if consumed == 0 && written == 0 {
                    return Err(DebError::DecompressFailed);
                }
            }
            Err(_) => return Err(DebError::DecompressFailed),
        }
    }
}

/// Stream an `.xz` container through [`xz4rust::XzDecoder`], pushing each produced
/// output chunk to `sink`. Mirrors [`decompress_xz`] but consumes output
/// incrementally instead of accumulating it.
fn stream_xz<F>(input: &[u8], max: usize, mut sink: F) -> Result<(), DebError>
where
    F: FnMut(&[u8]) -> Result<(), DebError>,
{
    use xz4rust::{XzDecoder, XzNextBlockResult};

    let dict = core::cmp::min(max, MAX_DECOMPRESSED);
    let mut decoder = XzDecoder::in_heap_with_alloc_dict_size(xz4rust::DICT_SIZE_MIN, dict);

    let mut scratch = [0u8; STREAM_CHUNK];
    let mut pos: usize = 0;

    loop {
        let result = decoder
            .decode(&input[pos..], &mut scratch)
            .map_err(|_| DebError::DecompressFailed)?;
        match result {
            XzNextBlockResult::NeedMoreData(consumed, produced) => {
                pos = pos.checked_add(consumed).ok_or(DebError::DecompressFailed)?;
                if produced > 0 {
                    sink(&scratch[..produced])?;
                }
                if consumed == 0 && produced == 0 {
                    return Err(DebError::DecompressFailed);
                }
            }
            XzNextBlockResult::EndOfStream(_, produced) => {
                if produced > 0 {
                    sink(&scratch[..produced])?;
                }
                return Ok(());
            }
        }
    }
}

/// Stream a Zstandard frame through [`ruzstd`]'s `StreamingDecoder`, pushing each
/// produced output chunk to `sink`. Mirrors [`decompress_zstd`] but consumes
/// output incrementally instead of accumulating it.
fn stream_zstd<F>(input: &[u8], _max: usize, mut sink: F) -> Result<(), DebError>
where
    F: FnMut(&[u8]) -> Result<(), DebError>,
{
    use ruzstd::decoding::StreamingDecoder;
    use ruzstd::io::Read;

    let mut src: &[u8] = input;
    let mut decoder = StreamingDecoder::new(&mut src).map_err(|_| DebError::DecompressFailed)?;

    let mut scratch = [0u8; STREAM_CHUNK];

    loop {
        let n = decoder
            .read(&mut scratch)
            .map_err(|_| DebError::DecompressFailed)?;
        if n == 0 {
            return Ok(());
        }
        sink(&scratch[..n])?;
    }
}

/// Bit flags in the gzip `FLG` byte (RFC 1952 §2.3.1).
const GZIP_FHCRC: u8 = 1 << 1;
const GZIP_FEXTRA: u8 = 1 << 2;
const GZIP_FNAME: u8 = 1 << 3;
const GZIP_FCOMMENT: u8 = 1 << 4;

/// Validate a gzip header and return the byte offset at which the embedded
/// DEFLATE stream begins (RFC 1952).
///
/// Checks the two magic bytes (`0x1f 0x8b`) and the deflate compression method
/// (`CM == 8`), then skips the fixed 10-byte header and any optional
/// `FEXTRA`/`FNAME`/`FCOMMENT`/`FHCRC` fields indicated by the `FLG` byte. Every
/// read is bounds-checked with checked arithmetic against `data`; a truncated or
/// otherwise malformed header yields [`DebError::DecompressFailed`] without
/// reading past the buffer.
fn gzip_payload_offset(data: &[u8]) -> Result<usize, DebError> {
    // Fixed header: ID1 ID2 CM FLG MTIME(4) XFL OS = 10 bytes.
    if data.len() < 10 {
        return Err(DebError::DecompressFailed);
    }
    if data[0] != 0x1f || data[1] != 0x8b {
        return Err(DebError::DecompressFailed);
    }
    if data[2] != 8 {
        // Only the DEFLATE compression method is defined/supported.
        return Err(DebError::DecompressFailed);
    }
    let flags = data[3];
    let mut pos: usize = 10;

    // FEXTRA: 2-byte little-endian length, then that many bytes.
    if flags & GZIP_FEXTRA != 0 {
        let len_end = pos.checked_add(2).ok_or(DebError::DecompressFailed)?;
        if len_end > data.len() {
            return Err(DebError::DecompressFailed);
        }
        let xlen = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos = len_end
            .checked_add(xlen)
            .ok_or(DebError::DecompressFailed)?;
        if pos > data.len() {
            return Err(DebError::DecompressFailed);
        }
    }

    // FNAME: NUL-terminated original file name.
    if flags & GZIP_FNAME != 0 {
        pos = skip_cstring(data, pos)?;
    }

    // FCOMMENT: NUL-terminated comment.
    if flags & GZIP_FCOMMENT != 0 {
        pos = skip_cstring(data, pos)?;
    }

    // FHCRC: 2-byte header CRC16.
    if flags & GZIP_FHCRC != 0 {
        pos = pos.checked_add(2).ok_or(DebError::DecompressFailed)?;
        if pos > data.len() {
            return Err(DebError::DecompressFailed);
        }
    }

    // The DEFLATE payload must be non-empty (at least one block byte) and must
    // leave room for the 8-byte trailer; require at least one byte past `pos`.
    if pos >= data.len() {
        return Err(DebError::DecompressFailed);
    }
    Ok(pos)
}

/// Advance `pos` past a NUL-terminated byte string in `data`, returning the
/// offset immediately after the terminating NUL.
///
/// Returns [`DebError::DecompressFailed`] if the string is not terminated before
/// the end of `data` (no read past the buffer).
fn skip_cstring(data: &[u8], mut pos: usize) -> Result<usize, DebError> {
    loop {
        if pos >= data.len() {
            return Err(DebError::DecompressFailed);
        }
        let byte = data[pos];
        pos = pos.checked_add(1).ok_or(DebError::DecompressFailed)?;
        if byte == 0 {
            return Ok(pos);
        }
    }
}
