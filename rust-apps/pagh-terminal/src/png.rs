//! Minimal PNG encoder/decoder (8-bit RGBA, no filtering, stored DEFLATE).
//!
//! Avoiding an image crate keeps the terminal a single dependency-free binary
//! that cross-compiles to musl.  The encoder produces valid PNGs for any RGBA
//! canvas; the decoder intentionally implements only the subset this crate
//! writes (used by tests to prove round-trip integrity).  It still validates
//! lengths, CRCs, zlib framing and the Adler-32 checksum, so a corrupted
//! screenshot cannot silently become a "passing" test fixture.

use std::io;
use std::path::Path;

use crate::render::Canvas;

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

#[derive(Debug)]
pub enum PngError {
    BadSignature,
    Truncated(&'static str),
    BadChunk(&'static str),
    BadCrc(&'static str),
    Unsupported(&'static str),
    BadZlib(&'static str),
    TooLarge,
}

impl std::fmt::Display for PngError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PngError::BadSignature => write!(f, "not a PNG file"),
            PngError::Truncated(what) => write!(f, "truncated PNG ({what})"),
            PngError::BadChunk(what) => write!(f, "invalid PNG chunk ({what})"),
            PngError::BadCrc(what) => write!(f, "PNG CRC mismatch ({what})"),
            PngError::Unsupported(what) => write!(f, "unsupported PNG feature ({what})"),
            PngError::BadZlib(what) => write!(f, "invalid zlib stream ({what})"),
            PngError::TooLarge => write!(f, "PNG dimensions exceed the supported limit"),
        }
    }
}

impl std::error::Error for PngError {}

/// Encode an RGBA canvas as PNG.
pub fn encode(canvas: &Canvas) -> Vec<u8> {
    let mut raw = Vec::with_capacity((canvas.width * 4 + 1) * canvas.height);
    for y in 0..canvas.height {
        raw.push(0); // filter type: none
        let start = y * canvas.width * 4;
        raw.extend_from_slice(&canvas.pixels[start..start + canvas.width * 4]);
    }

    let mut out = Vec::with_capacity(raw.len() + 128);
    out.extend_from_slice(&SIGNATURE);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(canvas.width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(canvas.height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // RGBA8, deflate, no filter, no interlace
    write_chunk(&mut out, b"IHDR", &ihdr);

    let zlib = zlib_stored(&raw);
    write_chunk(&mut out, b"IDAT", &zlib);
    write_chunk(&mut out, b"IEND", &[]);
    out
}

/// Save a canvas as a PNG file.
pub fn save(canvas: &Canvas, path: impl AsRef<Path>) -> io::Result<()> {
    std::fs::write(path, encode(canvas))
}

/// Decode the PNG subset written by [`encode`].
pub fn decode(bytes: &[u8]) -> Result<Canvas, PngError> {
    if bytes.len() < SIGNATURE.len() || bytes[..8] != SIGNATURE {
        return Err(PngError::BadSignature);
    }
    let mut pos = 8;
    let mut width = 0usize;
    let mut height = 0usize;
    let mut idat = Vec::new();
    let mut saw_ihdr = false;
    let mut saw_iend = false;
    while pos + 12 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let kind = &bytes[pos + 4..pos + 8];
        if pos + 12 + len > bytes.len() {
            return Err(PngError::Truncated("chunk data"));
        }
        let data = &bytes[pos + 8..pos + 8 + len];
        let crc_expected =
            u32::from_be_bytes(bytes[pos + 8 + len..pos + 12 + len].try_into().unwrap());
        let mut crc_data = Vec::with_capacity(4 + len);
        crc_data.extend_from_slice(kind);
        crc_data.extend_from_slice(data);
        if crc32(&crc_data) != crc_expected {
            return Err(PngError::BadCrc("chunk"));
        }
        match kind {
            b"IHDR" => {
                if saw_ihdr || len != 13 {
                    return Err(PngError::BadChunk("IHDR"));
                }
                width = u32::from_be_bytes(data[0..4].try_into().unwrap()) as usize;
                height = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
                if width == 0 || height == 0 || width > 16384 || height > 16384 {
                    return Err(PngError::TooLarge);
                }
                if data[8] != 8 || data[9] != 6 || data[10] != 0 || data[11] != 0 || data[12] != 0 {
                    return Err(PngError::Unsupported("IHDR bit depth/color type/interlace"));
                }
                saw_ihdr = true;
            }
            b"IDAT" => {
                if !saw_ihdr {
                    return Err(PngError::BadChunk("IDAT before IHDR"));
                }
                idat.extend_from_slice(data);
            }
            b"IEND" => {
                saw_iend = true;
                break;
            }
            _ => {}
        }
        pos += 12 + len;
    }
    if !saw_ihdr {
        return Err(PngError::Truncated("IHDR"));
    }
    if !saw_iend {
        return Err(PngError::Truncated("IEND"));
    }
    let raw = zlib_inflate_stored(&idat)?;
    let stride = width.checked_mul(4).ok_or(PngError::TooLarge)?;
    let want = (stride + 1).checked_mul(height).ok_or(PngError::TooLarge)?;
    if raw.len() != want {
        return Err(PngError::BadZlib("unexpected decompressed size"));
    }
    let mut pixels = Vec::with_capacity(stride * height);
    for y in 0..height {
        let row = &raw[y * (stride + 1)..(y + 1) * (stride + 1)];
        if row[0] != 0 {
            return Err(PngError::Unsupported("row filter"));
        }
        pixels.extend_from_slice(&row[1..]);
    }
    Ok(Canvas {
        width,
        height,
        pixels,
    })
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_data = Vec::with_capacity(4 + data.len());
    crc_data.extend_from_slice(kind);
    crc_data.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_data).to_be_bytes());
}

fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 16);
    out.extend_from_slice(&[0x78, 0x01]); // CMF/FLG: deflate, 32K window, no dict
    if raw.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    } else {
        let mut chunks = raw.chunks(65_535).peekable();
        while let Some(chunk) = chunks.next() {
            let final_block = chunks.peek().is_none();
            out.push(if final_block { 1 } else { 0 });
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

fn zlib_inflate_stored(data: &[u8]) -> Result<Vec<u8>, PngError> {
    if data.len() < 6 {
        return Err(PngError::BadZlib("short header"));
    }
    let cmf = data[0];
    let flg = data[1];
    if cmf & 0x0F != 8 || (u16::from(cmf) * 256 + u16::from(flg)) % 31 != 0 {
        return Err(PngError::BadZlib("header check"));
    }
    if flg & 0x20 != 0 {
        return Err(PngError::BadZlib("preset dictionary"));
    }
    let mut pos = 2;
    let mut out = Vec::new();
    loop {
        if pos >= data.len() {
            return Err(PngError::Truncated("deflate block"));
        }
        let header = data[pos];
        pos += 1;
        let final_block = header & 1 != 0;
        let btype = (header >> 1) & 0b11;
        if btype != 0 {
            return Err(PngError::Unsupported("compressed deflate block"));
        }
        if pos + 4 > data.len() {
            return Err(PngError::Truncated("deflate LEN/NLEN"));
        }
        let len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        let nlen = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        if nlen != !(len as u16) {
            return Err(PngError::BadZlib("LEN/NLEN mismatch"));
        }
        pos += 4;
        if pos + len > data.len() {
            return Err(PngError::Truncated("deflate payload"));
        }
        out.extend_from_slice(&data[pos..pos + len]);
        pos += len;
        if final_block {
            break;
        }
    }
    if data.len() < pos + 4 {
        return Err(PngError::Truncated("adler32"));
    }
    let expected = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap());
    if adler32(&out) != expected {
        return Err(PngError::BadZlib("adler32"));
    }
    Ok(out)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::Rgb;

    #[test]
    fn roundtrip_small_canvas() {
        let mut canvas = Canvas::new(3, 2, Rgb(10, 20, 30));
        canvas.fill_rect(1, 0, 2, 1, Rgb(200, 100, 50));
        let png = encode(&canvas);
        assert_eq!(&png[..8], &SIGNATURE);
        let decoded = decode(&png).unwrap();
        assert_eq!(decoded.width, 3);
        assert_eq!(decoded.height, 2);
        assert_eq!(decoded.pixels, canvas.pixels);
    }

    #[test]
    fn detects_corruption() {
        let canvas = Canvas::new(2, 2, Rgb(1, 2, 3));
        let mut png = encode(&canvas);
        let last = png.len() - 5;
        png[last] ^= 0xFF;
        assert!(matches!(
            decode(&png),
            Err(PngError::BadCrc(_)) | Err(PngError::BadZlib(_))
        ));
    }

    #[test]
    fn stored_zlib_roundtrip() {
        let data = vec![7u8; 200_000];
        let compressed = zlib_stored(&data);
        assert_eq!(zlib_inflate_stored(&compressed).unwrap(), data);
    }
}
