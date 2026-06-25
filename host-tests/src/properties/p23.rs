// Feature: linux-binary-compat, Property 23: ar enumeration locates the three .deb members within bounds

use crate::deb::{locate_members, parse_ar};
use proptest::prelude::*;

/// Build a single 60-byte `ar` member header for `name` with content length
/// `size`. Layout: name `[0..16)`, mtime `[16..28)`, uid `[28..34)`, gid
/// `[34..40)`, mode `[40..48)`, size (decimal ASCII) `[48..58)`, magic `` `\n ``
/// `[58..60)`. All numeric fields are space-padded; only name and size are read
/// by `parse_ar`.
fn ar_header(name: &str, size: usize) -> [u8; 60] {
    let mut h = [b' '; 60];
    let nb = name.as_bytes();
    h[..nb.len()].copy_from_slice(nb);

    let size_str = size.to_string();
    // Decimal ASCII, left-justified within the 10-byte size field [48..58).
    h[48..48 + size_str.len()].copy_from_slice(size_str.as_bytes());

    // Header terminator magic.
    h[58] = b'`';
    h[59] = b'\n';
    h
}

/// Append a member (header + content + even-alignment pad byte) to `buf`.
fn push_member(buf: &mut Vec<u8>, name: &str, content: &[u8]) {
    buf.extend_from_slice(&ar_header(name, content.len()));
    buf.extend_from_slice(content);
    if content.len() % 2 == 1 {
        buf.push(b'\n');
    }
}

/// Assemble a synthetic `.deb` `ar` archive containing the three canonical
/// members, in order: `debian-binary`, `control.tar.gz`, `data.tar.gz`.
fn build_deb(debian: &[u8], control: &[u8], data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"!<arch>\n");
    push_member(&mut buf, "debian-binary", debian);
    push_member(&mut buf, "control.tar.gz", control);
    push_member(&mut buf, "data.tar.gz", data);
    buf
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any synthesized `.deb`, `parse_ar` enumerates the three members in
    /// order with trimmed names and in-bounds content slices equal to the input,
    /// and `locate_members` returns the three members with the correct ranges.
    #[test]
    fn ar_enumeration_locates_three_members(
        debian in prop::collection::vec(any::<u8>(), 0..64),
        control in prop::collection::vec(any::<u8>(), 0..200),
        data in prop::collection::vec(any::<u8>(), 0..200),
    ) {
        let buf = build_deb(&debian, &control, &data);

        let members = parse_ar(&buf).expect("synthetic ar archive must parse");

        // Three members, enumerated in order with names trimmed of padding.
        prop_assert_eq!(members.len(), 3);
        prop_assert_eq!(members[0].name, "debian-binary");
        prop_assert_eq!(members[1].name, "control.tar.gz");
        prop_assert_eq!(members[2].name, "data.tar.gz");

        // Content slices match the inputs (and are therefore within `buf`).
        prop_assert_eq!(members[0].data, &debian[..]);
        prop_assert_eq!(members[1].data, &control[..]);
        prop_assert_eq!(members[2].data, &data[..]);

        // Every returned range lies within the backing buffer.
        let base = buf.as_ptr() as usize;
        let end = base + buf.len();
        for m in &members {
            let start = m.data.as_ptr() as usize;
            prop_assert!(start >= base && start + m.data.len() <= end);
        }

        // locate_members picks out exactly the three members with correct bytes.
        let located = locate_members(&members).expect("three members must be located");
        prop_assert_eq!(located.debian_binary.name, "debian-binary");
        prop_assert_eq!(located.debian_binary.data, &debian[..]);
        prop_assert_eq!(located.control.name, "control.tar.gz");
        prop_assert_eq!(located.control.data, &control[..]);
        prop_assert_eq!(located.data.name, "data.tar.gz");
        prop_assert_eq!(located.data.data, &data[..]);
    }
}
