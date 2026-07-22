// Feature: linux-binary-compat, Property 21: the GET request line is well-formed for any host and path

use crate::http::build_get_request;
use proptest::prelude::*;

/// Return `true` if `haystack` contains `needle` as a contiguous byte subslice.
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return needle.is_empty();
    }
    haystack
        .windows(needle.len())
        .any(|w| w == needle)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any printable `(host, path)`, the request begins with
    /// `GET <path> HTTP/1.1\r\n`, contains `Host: <host>\r\n`, and ends with a
    /// blank line (`\r\n\r\n`).
    #[test]
    fn get_request_line_is_well_formed(
        // Printable ASCII (0x20..=0x7e), excluding CR/LF so the generated values
        // cannot themselves inject header structure.
        host in "[ -~]{0,40}",
        path in "[ -~]{0,40}",
    ) {
        let req = build_get_request(&host, &path);

        let prefix = format!("GET {path} HTTP/1.1\r\n");
        prop_assert!(
            req.starts_with(prefix.as_bytes()),
            "request did not start with the expected request line"
        );

        let host_header = format!("Host: {host}\r\n");
        prop_assert!(
            contains_subslice(&req, host_header.as_bytes()),
            "request did not contain the expected Host header"
        );

        prop_assert!(
            req.ends_with(b"\r\n\r\n"),
            "request did not end with a blank line"
        );
    }
}
