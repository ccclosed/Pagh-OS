//! Pure parser for a Debian `Release` / `InRelease` body (issue #32).
//!
//! A `Release` file is RFC822-ish: `Key: value` fields, then one or more digest
//! *sections* (`MD5Sum:`, `SHA1:`, `SHA256:`, `SHA512:`) whose entries are
//! continuation lines of the form
//!
//! ```text
//!  <64 hex> <size> <pool-relative path>
//! ```
//!
//! The apt trust chain uses exactly one thing from it: the **SHA-256 and size**
//! of the `Packages` variant it is about to parse. Everything else (suite,
//! codename, dates, `Acquire-By-Hash`) is read for the policy checks in
//! `pkg::apt` and for diagnostics.
//!
//! ## Why the section header matters
//!
//! `MD5Sum` entries are 32 hex characters and `SHA1` entries are 40: a parser
//! that scans the whole file for "hex + size + path" would happily accept an MD5
//! line as the digest of `Packages.gz`. This parser therefore tracks the current
//! section and only collects entries inside `SHA256:`, which is the section the
//! signature actually covers — a deliberate fail-closed choice, since a malformed
//! or unknown section simply yields no entry and the caller refuses.
//!
//! Everything here is `core` + `alloc` only, so the kernel compiles this source
//! for the bare-metal target and `host-tests` `#[path]`-includes it (P54).

#![allow(dead_code)]

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Maximum accepted `Release` body (the live `stable` `InRelease` is ~140 KiB;
/// this leaves room for growth while keeping the parse bounded).
pub const MAX_RELEASE_BYTES: usize = 4 * 1024 * 1024;

/// Maximum digest entries kept from the `SHA256:` section (the live `stable`
/// `Release` lists ~1400).
pub const MAX_RELEASE_ENTRIES: usize = 4096;

/// One `SHA256:` section entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseEntry {
    /// Pool-relative path, e.g. `main/binary-amd64/Packages.gz`.
    pub path: String,
    /// The SHA-256 digest the signature covers for that path.
    pub sha256: [u8; 32],
    /// Declared size in bytes.
    pub size: u64,
}

/// A date-valued field (`Date:`, `Valid-Until:`).
///
/// The three cases are kept apart on purpose: an *absent* field imposes no
/// policy, while a field that is present but unparsable must fail closed — the
/// caller cannot tell "expired" from "garbage" otherwise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DateField {
    /// The field is not present.
    #[default]
    Absent,
    /// The field parsed to Unix seconds.
    Parsed(i64),
    /// The field is present but this parser cannot read it.
    Malformed,
}

impl DateField {
    /// The parsed instant, if any.
    pub fn value(self) -> Option<i64> {
        match self {
            DateField::Parsed(v) => Some(v),
            _ => None,
        }
    }

    /// True if the field exists at all (parsed or malformed).
    pub fn is_present(self) -> bool {
        !matches!(self, DateField::Absent)
    }
}

/// A parsed `Release` body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Release {
    /// `Suite:` (e.g. `stable`).
    pub suite: String,
    /// `Codename:` (e.g. `trixie`).
    pub codename: String,
    /// `Version:` (e.g. `13.7`).
    pub version: String,
    /// `Date:` — when the metadata was generated.
    pub date: DateField,
    /// `Valid-Until:` — when the mirror should stop serving it.
    pub valid_until: DateField,
    /// `Acquire-By-Hash: yes|no`.
    pub acquire_by_hash: bool,
    /// `Components:` as listed.
    pub components: Vec<String>,
    /// `Architectures:` as listed.
    pub architectures: Vec<String>,
    /// `SHA256:` entries, in file order.
    pub sha256: Vec<ReleaseEntry>,
    /// True if the body exceeded [`MAX_RELEASE_BYTES`] and parsing stopped early
    /// (the caller refuses: a partial digest list cannot be trusted).
    pub truncated: bool,
}

impl Release {
    /// The `SHA256:` entry for an exact pool-relative path.
    pub fn sha256_for(&self, path: &str) -> Option<&ReleaseEntry> {
        self.sha256.iter().find(|e| e.path == path)
    }

    /// True if `suite` matches either `Suite:` or `Codename:` (Debian's `stable`
    /// alias and its codename are both legitimate names for the same suite).
    /// An empty suite on either side is not a mismatch.
    pub fn matches_suite(&self, suite: &str) -> bool {
        if self.suite.is_empty() && self.codename.is_empty() {
            return true;
        }
        self.suite == suite || self.codename == suite
    }
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    if !is_hex64(s) {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = (hex_nibble(bytes[2 * i])? << 4) | hex_nibble(bytes[2 * i + 1])?;
    }
    Some(out)
}

/// Parse a `Release` body (the clear text of `InRelease`, or the `Release` file
/// as served).
///
/// Robust and total: unknown fields and unknown sections are ignored, CRLF is
/// tolerated, and a malformed digest line is dropped (leaving *no* entry for that
/// path, which the caller treats as a refusal rather than as "nothing to check").
pub fn parse_release(text: &[u8]) -> Release {
    let mut out = Release::default();
    if text.len() > MAX_RELEASE_BYTES {
        out.truncated = true;
    }
    let limit = core::cmp::min(text.len(), MAX_RELEASE_BYTES);

    // Section state: `None` outside a digest section, `Some(true)` inside
    // `SHA256:`, `Some(false)` inside any other digest section.
    let mut in_sha256: Option<bool> = None;

    for raw_line in text[..limit].split(|&b| b == b'\n') {
        let line = core::str::from_utf8(raw_line).unwrap_or("");
        let line = line.strip_suffix('\r').unwrap_or(line);

        if line.starts_with(' ') || line.starts_with('\t') {
            // Continuation line: a digest entry, or a wrapped field value.
            let entry = line.trim();
            if entry.is_empty() {
                continue;
            }
            if in_sha256 == Some(true) && out.sha256.len() < MAX_RELEASE_ENTRIES {
                // " <hash> <size> <path>"
                let mut parts = entry.split_whitespace();
                if let (Some(hash), Some(size), Some(path)) =
                    (parts.next(), parts.next(), parts.next())
                {
                    if let (Some(digest), Some(size)) = (hex32(hash), size.parse::<u64>().ok()) {
                        out.sha256.push(ReleaseEntry {
                            path: path.to_string(),
                            sha256: digest,
                            size,
                        });
                    }
                }
            }
            continue;
        }

        // A non-continuation line ends any section and is either a section header
        // or a field.
        in_sha256 = None;
        let Some(colon) = line.find(':') else {
            continue;
        };
        let key = line[..colon].trim();
        let value = line[colon + 1..].trim();
        match key {
            "SHA256" => in_sha256 = Some(true),
            "MD5Sum" | "SHA1" | "SHA512" => in_sha256 = Some(false),
            "Suite" => out.suite = value.to_string(),
            "Codename" => out.codename = value.to_string(),
            "Version" => out.version = value.to_string(),
            "Date" => out.date = parse_date(value),
            "Valid-Until" => out.valid_until = parse_date(value),
            "Acquire-By-Hash" => out.acquire_by_hash = value.eq_ignore_ascii_case("yes"),
            "Components" => out.components = split_list(value),
            "Architectures" => out.architectures = split_list(value),
            _ => {}
        }
    }
    out
}

fn split_list(value: &str) -> Vec<String> {
    value.split_whitespace().map(|s| s.to_string()).collect()
}

/// Parse a `Release` timestamp such as `Sat, 12 Sep 2026 07:55:41 UTC`.
///
/// Only UTC (`UTC`/`GMT`) is accepted — a `Release` is served in UTC, and
/// interpreting an unknown zone as local time would silently shift the
/// `Valid-Until` decision.
pub fn parse_date(value: &str) -> DateField {
    if value.trim().is_empty() {
        return DateField::Malformed;
    }
    // Drop the optional weekday ("Sat," / "Thu,").
    let rest = match value.split_once(',') {
        Some((_, rest)) => rest.trim(),
        None => value.trim(),
    };
    let parts: Vec<&str> = rest.split_whitespace().collect();
    if parts.len() != 5 {
        return DateField::Malformed;
    }
    let day: u32 = match parts[0].parse() {
        Ok(d) if (1..=31).contains(&d) => d,
        _ => return DateField::Malformed,
    };
    let month = match parts[1].to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return DateField::Malformed,
    };
    let year: i64 = match parts[2].parse() {
        Ok(y) if (1970..=9999).contains(&y) => y,
        _ => return DateField::Malformed,
    };
    let mut hms = parts[3].split(':');
    let (h, m, s) = match (hms.next(), hms.next(), hms.next(), hms.next()) {
        (Some(h), Some(m), Some(s), None) => (
            h.parse::<i64>().ok(),
            m.parse::<i64>().ok(),
            s.parse::<i64>().ok(),
        ),
        _ => (None, None, None),
    };
    let (h, m, s) = match (h, m, s) {
        (Some(h), Some(m), Some(s))
            if (0..24).contains(&h) && (0..60).contains(&m) && (0..61).contains(&s) =>
        {
            (h, m, s)
        }
        _ => return DateField::Malformed,
    };
    if !parts[4].eq_ignore_ascii_case("UTC") && !parts[4].eq_ignore_ascii_case("GMT") {
        return DateField::Malformed;
    }

    let days = days_from_civil(year, month, day);
    DateField::Parsed(days * 86_400 + h * 3600 + m * 60 + s)
}

/// Days since 1970-01-01 for a proleptic-Gregorian date (Howard Hinnant's
/// `days_from_civil`), valid for the `Release` date range.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = ((m + 9) % 12) as i64; // Mar = 0
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}
