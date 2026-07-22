// Feature: linux-binary-compat, Property 20: HTTP response-head parsing extracts status and content length

use crate::http::{parse_http_head, HeadParse};
use proptest::prelude::*;

/// Strategy producing a `Content-Length` scenario: the header line to embed in the
/// head (if any) paired with the value `parse_http_head` should report.
///
///   * Absent       -> no header line, expected `None`.
///   * Valid(n)     -> `Content-Length: n`, expected `Some(n)`.
///   * Unparseable  -> `Content-Length: <letters>`, expected `None` (present but bad).
fn content_length_strategy() -> impl Strategy<Value = (Option<String>, Option<u64>)> {
    prop_oneof![
        Just((None, None)),
        any::<u64>().prop_map(|n| (Some(format!("Content-Length: {n}")), Some(n))),
        "[A-Za-z]{1,6}".prop_map(|s| (Some(format!("Content-Length: {s}")), None)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any response head, once the terminating blank line is present
    /// `parse_http_head` returns `Done` with the parsed status, the content length
    /// (`Some(n)` iff a `Content-Length` parses as a non-negative integer, else
    /// `None`), and `body_off` just past the `CRLFCRLF`. A head missing the blank
    /// line yields `Need`; a malformed status line yields `Malformed`.
    #[test]
    fn http_head_parsing_extracts_status_and_length(
        status in 100u16..=599,
        (cl_header, expected_len) in content_length_strategy(),
        body in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        // --- Complete, well-formed head -> Done -------------------------------
        let mut head = format!("HTTP/1.1 {status} OK\r\n");
        if let Some(h) = &cl_header {
            head.push_str(h);
            head.push_str("\r\n");
        }
        head.push_str("Connection: close\r\n");
        head.push_str("\r\n");

        let mut buf = head.into_bytes();
        let body_off = buf.len();
        buf.extend_from_slice(&body);

        prop_assert_eq!(
            parse_http_head(&buf),
            HeadParse::Done { status, content_length: expected_len, body_off }
        );

        // --- Incomplete head (no terminating blank line) -> Need --------------
        let partial = format!("HTTP/1.1 {status} OK\r\nConnection: close\r\n").into_bytes();
        prop_assert_eq!(parse_http_head(&partial), HeadParse::Need);

        // --- Malformed status line (does not begin with HTTP/) -> Malformed ----
        let bad_prefix = format!("NOTHTTP {status} OK\r\n\r\n").into_bytes();
        prop_assert_eq!(parse_http_head(&bad_prefix), HeadParse::Malformed);

        // --- Malformed status line (status code is not three digits) ----------
        let bad_code = b"HTTP/1.1 20 OK\r\n\r\n";
        prop_assert_eq!(parse_http_head(bad_code), HeadParse::Malformed);
    }
}
