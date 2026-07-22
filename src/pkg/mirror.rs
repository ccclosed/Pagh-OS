//! Pure parsing of an `apt setmirror` host argument.
//!
//! `setmirror` accepts an optional URL scheme prefix on the host so the user can
//! pick the transport inline, e.g.:
//!
//! ```text
//! apt setmirror https://deb.debian.org /debian   # TLS (HTTPS, port 443)
//! apt setmirror http://deb.debian.org  /debian   # cleartext (HTTP, port 80)
//! apt setmirror deb.debian.org         /debian   # leave transport unchanged
//! ```
//!
//! This module is `core`-only and self-contained (no kernel deps, no globals), so
//! the same source is exercised by the host property tests (`#[path]`-included)
//! and compiled into the kernel — matching how `http`/`dns`/`deb` are shared.

/// The parsed result of a mirror host argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MirrorSpec<'a> {
    /// The bare host (DNS name or IPv4 literal), with any scheme prefix, any
    /// `:port`, and any trailing `/path` removed. The path, if any, belongs in the
    /// separate `base` argument.
    pub host: &'a str,
    /// `Some(true)` if an `https://` scheme was given, `Some(false)` for
    /// `http://`, `None` if no scheme prefix was present (caller keeps the current
    /// transport setting).
    pub tls: Option<bool>,
    /// `Some(port)` if an explicit `:port` followed the host (e.g.
    /// `http://10.0.2.2:8000`), `None` otherwise (caller keeps the scheme-default
    /// or current port).
    pub port: Option<u16>,
}

/// Parse a mirror host argument that may carry an `http://` / `https://` prefix
/// and an optional `:port`.
///
/// Scheme matching is ASCII case-insensitive. After any scheme is stripped, the
/// host is everything up to (but not including) the first `/`; any trailing path
/// is dropped (it should be supplied via the `base` argument instead). Within
/// that host token, a trailing `:<digits>` that parses as a `u16` is taken as the
/// port and removed from the host; a `:` not followed by a valid port number is
/// left as part of the host and yields `port = None`.
pub fn parse_mirror_arg(arg: &str) -> MirrorSpec<'_> {
    let (tls, rest) = if let Some(r) = strip_prefix_ci(arg, "https://") {
        (Some(true), r)
    } else if let Some(r) = strip_prefix_ci(arg, "http://") {
        (Some(false), r)
    } else {
        (None, arg)
    };

    // Drop any trailing path: the host (with optional :port) is up to the first '/'.
    let host_port = match rest.find('/') {
        Some(i) => &rest[..i],
        None => rest,
    };

    // Split off a trailing ":<port>" if the part after the last ':' is a valid u16.
    let (host, port) = match host_port.rfind(':') {
        Some(i) => match host_port[i + 1..].parse::<u16>() {
            Ok(p) => (&host_port[..i], Some(p)),
            Err(_) => (host_port, None),
        },
        None => (host_port, None),
    };

    MirrorSpec { host, tls, port }
}

/// ASCII case-insensitive prefix strip. Returns the remainder after `prefix` when
/// `s` starts with it (ignoring ASCII case), else `None`. Operates on bytes to
/// avoid any UTF-8 boundary panic; `prefix` is always an ASCII scheme here, so the
/// split index is a valid `str` boundary on a match.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let pb = prefix.as_bytes();
    let sb = s.as_bytes();
    if sb.len() >= pb.len() && sb[..pb.len()].eq_ignore_ascii_case(pb) {
        Some(&s[pb.len()..])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_scheme_sets_tls_and_strips() {
        let s = parse_mirror_arg("https://deb.debian.org");
        assert_eq!(s.host, "deb.debian.org");
        assert_eq!(s.tls, Some(true));
    }

    #[test]
    fn http_scheme_clears_tls_and_strips() {
        let s = parse_mirror_arg("http://example.com");
        assert_eq!(s.host, "example.com");
        assert_eq!(s.tls, Some(false));
    }

    #[test]
    fn no_scheme_leaves_tls_unset() {
        let s = parse_mirror_arg("deb.debian.org");
        assert_eq!(s.host, "deb.debian.org");
        assert_eq!(s.tls, None);
    }

    #[test]
    fn scheme_is_case_insensitive() {
        assert_eq!(parse_mirror_arg("HTTPS://h").tls, Some(true));
        assert_eq!(parse_mirror_arg("HtTp://h").tls, Some(false));
    }

    #[test]
    fn trailing_path_is_dropped_from_host() {
        let s = parse_mirror_arg("https://deb.debian.org/debian");
        assert_eq!(s.host, "deb.debian.org");
        assert_eq!(s.tls, Some(true));
        assert_eq!(s.port, None);
    }

    #[test]
    fn explicit_port_is_parsed_and_stripped() {
        let s = parse_mirror_arg("http://10.0.2.2:8000");
        assert_eq!(s.host, "10.0.2.2");
        assert_eq!(s.tls, Some(false));
        assert_eq!(s.port, Some(8000));
    }

    #[test]
    fn port_with_path_is_parsed() {
        let s = parse_mirror_arg("http://10.0.2.2:8000/debian");
        assert_eq!(s.host, "10.0.2.2");
        assert_eq!(s.tls, Some(false));
        assert_eq!(s.port, Some(8000));
    }

    #[test]
    fn port_without_scheme_is_parsed() {
        let s = parse_mirror_arg("example.com:1234");
        assert_eq!(s.host, "example.com");
        assert_eq!(s.tls, None);
        assert_eq!(s.port, Some(1234));
    }

    #[test]
    fn no_port_leaves_port_unset() {
        let s = parse_mirror_arg("https://deb.debian.org");
        assert_eq!(s.port, None);
    }

    #[test]
    fn invalid_port_is_left_in_host() {
        // A ':' not followed by a valid u16 stays part of the host, port = None.
        let s = parse_mirror_arg("http://host:notaport");
        assert_eq!(s.host, "host:notaport");
        assert_eq!(s.port, None);
        // Out-of-range port (> 65535) is not a valid u16 either.
        let s = parse_mirror_arg("http://host:70000");
        assert_eq!(s.host, "host:70000");
        assert_eq!(s.port, None);
    }
}
