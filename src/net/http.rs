//! Pure HTTP/1.1 request building and response-head parsing.
//!
//! This task (7.1) implements only the **pure** core of the `Package_Fetcher`
//! (design component 7): allocation-aware but socket-free logic that builds a
//! `GET` request and parses an HTTP response head. It is `core` + `alloc` only —
//! no smoltcp sockets, no globals, no hardware — so it compiles into both the
//! kernel (`crate::net::http`) and the `host-tests` crate via `#[path]` include,
//! letting properties P20/P21 exercise the same source (R11.6).
//!
//! The effectful `fetch_deb` shell that pumps a real TCP socket and calls
//! [`parse_http_head`] lands in task 14.1.
//!
//! All parsing is hardened against adversarial input: every index is bounds
//! checked, slicing uses half-open ranges derived from found positions, and no
//! path can panic or read past `buf`.
#![allow(dead_code)]

use alloc::vec::Vec;

/// Outcome of parsing the head (status line + headers) of an HTTP response.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeadParse {
    /// The buffer does not yet contain a complete head (no `CRLFCRLF`
    /// terminator). The caller should read more bytes and retry.
    Need,
    /// A complete, well-formed head was parsed.
    Done {
        /// The numeric HTTP status code from the status line.
        status: u16,
        /// The parsed `Content-Length`, if a well-formed header was present.
        ///
        /// `Some(n)` only when a `Content-Length` header exists and its value
        /// parses as a non-negative integer; `None` when the header is absent or
        /// present but unparseable (R8.3 decides what to do with `None`).
        content_length: Option<u64>,
        /// Byte index just past the `CRLFCRLF` terminator — where the body
        /// begins within the buffer.
        body_off: usize,
    },
    /// A head terminator was present but the status line was not well-formed.
    Malformed,
}

/// The CRLFCRLF sequence that terminates an HTTP message head.
const CRLFCRLF: &[u8] = b"\r\n\r\n";

/// Parse the head of an HTTP/1.x response from `buf` (R8.2, R8.3, R8.5).
///
/// Scans for the `CRLFCRLF` head terminator. If it is absent the head is
/// incomplete and [`HeadParse::Need`] is returned. Otherwise the status line is
/// parsed (`HTTP/1.x <3-digit code> ...`); a status line that does not begin with
/// `HTTP/` or whose code is not exactly three ASCII digits yields
/// [`HeadParse::Malformed`]. Header lines are parsed case-insensitively; a
/// `Content-Length` whose value parses as a non-negative integer sets
/// `content_length = Some(n)`, otherwise it stays `None`. `body_off` is the index
/// immediately following the terminator.
///
/// The function never indexes past `buf` and never panics on any input.
pub fn parse_http_head(buf: &[u8]) -> HeadParse {
    let term = match find_subslice(buf, CRLFCRLF) {
        Some(i) => i,
        None => return HeadParse::Need,
    };
    let body_off = term + CRLFCRLF.len();

    // The head is everything before the terminating CRLFCRLF. Individual lines
    // are separated by CRLF; the head itself contains no trailing blank line.
    let head = &buf[..term];
    let mut lines = SplitCrlf::new(head);

    let status_line = match lines.next() {
        Some(line) => line,
        None => return HeadParse::Malformed,
    };
    let status = match parse_status_code(status_line) {
        Some(code) => code,
        None => return HeadParse::Malformed,
    };

    let mut content_length: Option<u64> = None;
    for line in lines {
        if let Some((name, value)) = split_header(line) {
            if eq_ascii_ci(trim_ascii(name), b"content-length") {
                // Present-but-unparseable leaves content_length as None.
                content_length = parse_u64(trim_ascii(value));
            }
        }
    }

    HeadParse::Done {
        status,
        content_length,
        body_off,
    }
}

/// Build an HTTP/1.1 `GET` request for `path` on `host` (R8.1, Property 21).
///
/// Produces exactly:
///
/// ```text
/// GET <path> HTTP/1.1\r\n
/// Host: <host>\r\n
/// Connection: close\r\n
/// \r\n
/// ```
///
/// The result always begins with `GET <path> HTTP/1.1\r\n`, contains
/// `Host: <host>\r\n`, and ends with a blank line.
pub fn build_get_request(host: &str, path: &str) -> Vec<u8> {
    let mut req = Vec::new();
    req.extend_from_slice(b"GET ");
    req.extend_from_slice(path.as_bytes());
    req.extend_from_slice(b" HTTP/1.1\r\n");
    req.extend_from_slice(b"Host: ");
    req.extend_from_slice(host.as_bytes());
    req.extend_from_slice(b"\r\n");
    req.extend_from_slice(b"Connection: close\r\n");
    req.extend_from_slice(b"\r\n");
    req
}

// ---------------------------------------------------------------------------
// Pure parsing helpers (bounds-checked, panic-free)
// ---------------------------------------------------------------------------

/// Return the index of the first occurrence of `needle` within `haystack`, or
/// `None`. Empty `needle` is treated as not found.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let n = needle.len();
    if n == 0 || haystack.len() < n {
        return None;
    }
    let mut i = 0;
    while i + n <= haystack.len() {
        if &haystack[i..i + n] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Iterator over CRLF-separated lines of a head slice. A bare trailing CRLF (no
/// following bytes) yields no extra empty line; intermediate empty lines are
/// preserved but never occur in a well-formed head before the terminator.
struct SplitCrlf<'a> {
    rest: &'a [u8],
    done: bool,
}

impl<'a> SplitCrlf<'a> {
    fn new(s: &'a [u8]) -> Self {
        SplitCrlf {
            rest: s,
            done: s.is_empty(),
        }
    }
}

impl<'a> Iterator for SplitCrlf<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.done {
            return None;
        }
        match find_subslice(self.rest, b"\r\n") {
            Some(i) => {
                let line = &self.rest[..i];
                self.rest = &self.rest[i + 2..];
                if self.rest.is_empty() {
                    self.done = true;
                }
                Some(line)
            }
            None => {
                self.done = true;
                Some(self.rest)
            }
        }
    }
}

/// Parse the numeric status code from a status line of the form
/// `HTTP/1.x <code> [reason]`. Returns `None` unless the line begins with
/// `HTTP/` and the code is exactly three ASCII digits.
fn parse_status_code(line: &[u8]) -> Option<u16> {
    if !line.starts_with(b"HTTP/") {
        return None;
    }
    // The status code follows the first space.
    let sp = line.iter().position(|&b| b == b' ')?;
    let rest = &line[sp + 1..];
    // The code token ends at the next space or end of line.
    let code_end = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
    let code = &rest[..code_end];
    if code.len() != 3 || !code.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value = (code[0] - b'0') as u16 * 100
        + (code[1] - b'0') as u16 * 10
        + (code[2] - b'0') as u16;
    Some(value)
}

/// Split a header line into `(name, value)` at the first colon, or `None` if the
/// line has no colon.
fn split_header(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let colon = line.iter().position(|&b| b == b':')?;
    let name = &line[..colon];
    let value = &line[colon + 1..];
    Some((name, value))
}

/// Trim leading and trailing ASCII spaces and tabs from a byte slice.
fn trim_ascii(mut s: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = s {
        if *first == b' ' || *first == b'\t' {
            s = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = s {
        if *last == b' ' || *last == b'\t' {
            s = rest;
        } else {
            break;
        }
    }
    s
}

/// Case-insensitive ASCII byte-slice equality.
fn eq_ascii_ci(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// Parse a non-negative decimal integer. Returns `None` for an empty slice, any
/// non-digit byte (including sign characters), or on `u64` overflow.
fn parse_u64(s: &[u8]) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut acc: u64 = 0;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add((b - b'0') as u64)?;
    }
    Some(acc)
}
