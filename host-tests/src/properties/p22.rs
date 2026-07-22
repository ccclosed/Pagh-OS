// Feature: linux-binary-compat, Property 22: gzip decompression inverts gzip compression

use crate::deb::{decompress_data, ArMember, Compression};
use proptest::prelude::*;

/// Wrap a raw DEFLATE stream in a minimal RFC 1952 gzip container.
///
/// A fixed 10-byte header with no optional fields (`FLG == 0`, `CM == 8`), the
/// DEFLATE payload, then an 8-byte trailer (CRC32 + ISIZE). `decompress_data`'s
/// inflater stops at the final DEFLATE block and ignores the trailer, so its
/// exact bytes are immaterial here.
fn gzip_wrap(payload: &[u8]) -> Vec<u8> {
    let deflate = miniz_oxide::deflate::compress_to_vec(payload, 6);

    let mut out = Vec::new();
    // ID1 ID2 CM FLG MTIME(4) XFL OS
    out.extend_from_slice(&[0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff]);
    out.extend_from_slice(&deflate);
    // CRC32 (4) + ISIZE (4); ignored by the decoder.
    let isize_le = (payload.len() as u32).to_le_bytes();
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&isize_le);
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any payload, gzip-compressing it on the host and then decompressing the
    /// gzip-wrapped `ArMember` via `decompress_data(.., Gzip)` reproduces the
    /// original bytes exactly.
    #[test]
    fn gzip_decompression_round_trips(
        payload in prop::collection::vec(any::<u8>(), 0..1024),
    ) {
        let gz = gzip_wrap(&payload);
        let member = ArMember { name: "data.tar.gz", data: &gz };

        let decoded = decompress_data(&member, Compression::Gzip)
            .expect("gzip-wrapped payload must decompress");

        prop_assert_eq!(decoded, payload);
    }
}
