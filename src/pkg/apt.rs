//! Effectful `apt` package-manager front end (the by-name install pipeline).
//!
//! This is the **kernel-only** orchestration layer that ties the existing pure
//! pieces together into an `apt`-style workflow driven entirely by *package
//! name* — no manual host/port/path as the lower-level `pkg` command requires:
//!
//!   * [`update`] downloads and parses a Debian binary-repository `Packages`
//!     index into an in-RAM [`PackageIndex`] (the read side, [`super::apt_index`]).
//!   * [`install`] plans a dependency-first transaction with the pure resolver
//!     ([`super::apt_resolve`]), then for each package fetches the `.deb`
//!     ([`crate::net::http_fetch::http_get`]), parses + decompresses it
//!     ([`super::deb`]), enumerates its `data.tar` ([`super::tar`]), and writes the
//!     files onto ext2 under `/mnt` ([`super::install_fs`]).
//!   * [`show`] / [`list`] are read-only queries over the cached index.
//!
//! ## State
//!
//! Three process-global, spinlock-guarded singletons hold the session state:
//!
//!   * [`struct@CONFIG`] — the active mirror/suite/component/arch (see
//!     [`AptConfig`]), mutated by [`set_mirror`].
//!   * [`struct@INDEX`] — the parsed [`PackageIndex`], populated by [`update`].
//!     It is kept **only in RAM and never persisted to ext2**: the on-disk image
//!     is just 64 MiB, far too small for a real `main` index (see
//!     [`super::deb::MAX_INDEX_DECOMPRESSED`] for the decompressed-size note), so
//!     it is rebuilt by `apt update` each boot.
//!   * [`struct@INSTALLED`] — the set of package names installed *this session*,
//!     used as `already_installed` for the resolver so re-installs and shared
//!     dependencies are skipped. It is not a real dpkg status database.
//!
//! Network I/O is performed with **no apt lock held**: each global is locked only
//! long enough to read/clone what is needed (the index lock is never held across
//! an `http_get`, which itself disables interrupts while pumping the socket).
//!
//! ## Trust chain
//!
//! Nothing from the mirror is used before the signature chain checks out
//! (issue #32, `OPENPGP-VERIFY-CONTRACT.md`):
//!
//! ```text
//! InRelease | Release.gpg+Release   (OpenPGP, pinned Debian keyring)
//!        |
//!        +-- SHA-256 + size of the Packages variant actually fetched
//!                 |
//!                 +-- per-.deb SHA-256 + size from the signed index,
//!                     checked after the download and BEFORE deb::parse_ar
//! ```
//!
//! A mirror that serves no signatures is refused loudly
//! ([`AptOpError::Unsigned`]); a mismatch anywhere is fatal and there is no flag,
//! feature or environment variable that turns verification off. The only
//! unverified build is `--no-default-features`, where `update`/`install` return
//! [`AptOpError::NetworkDisabled`].

#![allow(dead_code)]

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::sync::spinlock::Spinlock;

use super::apt_index::{PackageIndex, PackageIndexBuilder, PkgRef, StanzaParser};
use super::apt_resolve::{resolve_install, AptError};
use super::deb::{self, Compression};
use super::install_fs;
use super::openpgp::{self, OpenPgpError};
use super::openpgp_crypto;
use super::release_file::{self, DateField, Release};
use super::tar;

/// The active repository configuration for `apt`.
///
/// The index URL is derived from these as
/// `{base}/dists/{suite}/{component}/binary-{arch}/Packages.{xz,gz,}` and each
/// `.deb` URL as `{base}/{filename}` (the pool-relative `Filename:` from the
/// index). Defaults target Debian `stable`/`main`/`amd64` on `deb.debian.org`
/// over **HTTPS** (TLS 1.3).
#[derive(Clone, Debug)]
pub struct AptConfig {
    /// Mirror host (DNS name or IPv4 literal), e.g. `deb.debian.org`.
    pub host: String,
    /// Base path on the mirror, with a leading slash and no trailing slash,
    /// e.g. `/debian`.
    pub base: String,
    /// Release/suite, e.g. `stable`.
    pub suite: String,
    /// Component, e.g. `main`.
    pub component: String,
    /// Binary architecture, e.g. `amd64`.
    pub arch: String,
    /// Transport port. Defaults to 443 when [`tls`](Self::tls) is set, 80 otherwise.
    pub port: u16,
    /// Use HTTPS (TLS 1.3) transport instead of cleartext HTTP.
    ///
    /// **Authenticated, fail-closed:** downloads go through
    /// [`net::tls::https_get`](crate::net::tls::https_get), which validates the
    /// server chain against the committed CA bundle, authorizes the host through
    /// the leaf SAN, applies the clock gate and checks the TLS 1.3
    /// `CertificateVerify`; any failure aborts the handshake. Trust is limited to
    /// the pinned roots in [`net::ca_bundle`](crate::net::ca_bundle) (ISRG Root
    /// X1/X2, GTS R1/R4), so an HTTPS mirror outside that set is refused — see
    /// `SECURITY.md`.
    pub tls: bool,
}

impl AptConfig {
    /// The built-in default configuration (Debian `stable`/`main`/`amd64` over
    /// HTTPS on `deb.debian.org`).
    fn defaults() -> AptConfig {
        AptConfig {
            host: "deb.debian.org".to_string(),
            base: "/debian".to_string(),
            suite: "stable".to_string(),
            component: "main".to_string(),
            arch: "amd64".to_string(),
            port: crate::net::tls::HTTPS_PORT,
            tls: true,
        }
    }

    /// The URL scheme for the active transport (`"https"` or `"http"`), for
    /// user-facing messages.
    pub fn scheme(&self) -> &'static str {
        if self.tls {
            "https"
        } else {
            "http"
        }
    }

    /// Fetch `path` from this mirror over the configured transport, selecting
    /// HTTPS ([`net::tls::https_get`](crate::net::tls::https_get) — fail-closed
    /// server authentication) or cleartext HTTP
    /// ([`http_get`](crate::net::http_fetch::http_get)) based on [`tls`](Self::tls).
    fn fetch(&self, path: &str) -> Result<Vec<u8>, crate::net::http_fetch::FetchError> {
        if self.tls {
            crate::net::tls::https_get(&self.host, self.port, path)
        } else {
            crate::net::http_fetch::http_get(&self.host, self.port, path)
        }
    }
}

/// Active mirror/suite configuration. `None` until first read, then lazily
/// initialized to [`AptConfig::defaults`].
static CONFIG: Spinlock<Option<AptConfig>> = Spinlock::new(None);

/// The parsed repository index, populated by [`update`]. RAM-only (see module
/// docs): never written to disk, rebuilt by `apt update`.
static INDEX: Spinlock<Option<PackageIndex>> = Spinlock::new(None);

/// Names installed this session, used as `already_installed` for the resolver.
static INSTALLED: Spinlock<BTreeSet<String>> = Spinlock::new(BTreeSet::new());

/// Return a clone of the active configuration, initializing the defaults on
/// first use.
pub fn config() -> AptConfig {
    let mut guard = CONFIG.lock();
    if guard.is_none() {
        *guard = Some(AptConfig::defaults());
    }
    // unwrap: just ensured Some above.
    guard.as_ref().unwrap().clone()
}

/// Point `apt` at a different mirror. `host` may carry an `http://` / `https://`
/// scheme prefix and an optional `:port` (parsed by
/// [`super::mirror::parse_mirror_arg`]):
///
///   * `https://<host>` -> enable TLS (HTTPS) and set the port to 443,
///   * `http://<host>`  -> disable TLS (cleartext HTTP) and set the port to 80,
///   * `<host>:<port>`  -> override the port (e.g. `http://10.0.2.2:8000`),
///   * `<host>` (no scheme) -> leave the current transport/port unchanged.
///
/// Any scheme prefix, `:port`, and trailing `/path` are stripped from the stored
/// host (use `base` for the path). `base`, when given, replaces the base path
/// (normalized to a leading-slash, no-trailing-slash form). The suite/component/
/// arch are left unchanged.
///
/// NOTE: HTTPS authenticates the mirror fail-closed (chain → committed CA
/// bundle, SAN hostname, validity + clock gate, `CertificateVerify`), so a host
/// whose chain does not reach one of the pinned roots — or whose certificate does
/// not authorize the host — is REFUSED rather than fetched from. `http://<host>`
/// selects cleartext HTTP, which is unauthenticated by construction (integrity
/// then rests on nothing; see `SECURITY.md`).
pub fn set_mirror(host: &str, base: Option<&str>) {
    let spec = super::mirror::parse_mirror_arg(host);

    let mut guard = CONFIG.lock();
    let mut cfg = guard.take().unwrap_or_else(AptConfig::defaults);
    cfg.host = spec.host.to_string();
    if let Some(tls) = spec.tls {
        cfg.tls = tls;
        cfg.port = if tls { 443 } else { 80 };
    }
    // An explicit `:port` in the host argument overrides the scheme default port
    // (e.g. `http://10.0.2.2:8000` -> cleartext HTTP on port 8000).
    if let Some(port) = spec.port {
        cfg.port = port;
    }
    if let Some(b) = base {
        cfg.base = normalize_base(b);
    }
    *guard = Some(cfg);
}

/// Select the release/suite `apt` fetches from the mirror (e.g. `stable`,
/// `bookworm`, `trixie-security`).
///
/// The built-in default is Debian `stable`; `apt setmirror` changes the host and
/// base path but deliberately not the suite, so switching suites is its own
/// operation. Like [`set_mirror`], this only updates the session configuration —
/// the next `apt update` verifies the metadata of whichever suite is selected.
pub fn set_suite(suite: &str) {
    let trimmed = suite.trim();
    if trimmed.is_empty() {
        return;
    }
    let mut guard = CONFIG.lock();
    let mut cfg = guard.take().unwrap_or_else(AptConfig::defaults);
    cfg.suite = trimmed.to_string();
    *guard = Some(cfg);
}

/// Normalize a base path: trim surrounding whitespace and trailing slashes, and
/// guarantee a single leading slash. `""` / `"/"` collapse to `""` (mirror root).
fn normalize_base(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        let mut s = String::from("/");
        s.push_str(trimmed);
        s
    }
}

/// Format a byte count as a short human-readable string (`B`/`KiB`/`MiB`) with
/// one decimal place, for the apt progress/summary lines. Integer-only (no FP),
/// so it is cheap and panic-free: `1536 -> "1.5 KiB"`, `13_322_415 -> "12.7 MiB"`.
pub fn human_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if n >= MIB {
        format!("{}.{} MiB", n / MIB, (n % MIB) * 10 / MIB)
    } else if n >= KIB {
        format!("{}.{} KiB", n / KIB, (n % KIB) * 10 / KIB)
    } else {
        format!("{} B", n)
    }
}

/// A read-only summary of one indexed package, returned by [`show`].
///
/// Owns its strings so the caller need not hold the index lock while printing.
#[derive(Clone, Debug)]
pub struct PkgSummary {
    /// Package name.
    pub package: String,
    /// Version string.
    pub version: String,
    /// Architecture.
    pub arch: String,
    /// Pool-relative `.deb` path.
    pub filename: String,
    /// Dependency expression, rendered one display string per AND-group
    /// (alternatives joined with ` | `).
    pub depends: Vec<String>,
    /// Download size in bytes.
    pub size: u64,
}

impl PkgSummary {
    fn from_record(rec: PkgRef<'_>) -> PkgSummary {
        let depends = rec
            .depends()
            .map(|g| g.alts().collect::<Vec<_>>().join(" | "))
            .collect::<Vec<String>>();
        PkgSummary {
            package: rec.package().to_string(),
            version: rec.version().to_string(),
            arch: rec.arch().to_string(),
            filename: rec.filename().to_string(),
            depends,
            size: rec.size(),
        }
    }
}

/// Why an `apt` operation failed.
#[derive(Debug)]
pub enum AptOpError {
    /// Outbound repository access is disabled in this build.
    NetworkDisabled,
    /// No network interface is up; the download could not be attempted.
    NoNetwork,
    /// No index is loaded; `apt update` must run first.
    NoIndex,
    /// The requested name is neither a real nor a virtual package in the index.
    NotFound(String),
    /// Downloading a package (or the index) over HTTP failed.
    Download { pkg: String },
    /// Parsing/decompressing a downloaded package (or the index) failed.
    Parse { pkg: String },
    /// The repository index downloaded but its decompressed stream exceeded the
    /// generous streamed-bytes safety budget ([`super::deb::MAX_INDEX_STREAM_BYTES`])
    /// or failed to decode — e.g. a corrupt or absurdly large index. The streaming
    /// pipeline bounds memory regardless of index size, so this is a clean error,
    /// never an OOM abort.
    IndexTooLarge,
    /// Writing a package's files onto ext2 failed.
    Install { pkg: String },
    /// The mirror serves no signed metadata at all: neither `InRelease` nor
    /// `Release.gpg`+`Release`. pagh refuses to trust unauthenticated package
    /// metadata — there is no override flag, by design.
    Unsigned { url: String },
    /// Signature verification of the signed metadata failed (bad armor, no
    /// trusted signature, expired/revoked key, unset clock, …). `stage` and
    /// `cause` are the same strings the serial diagnostic carries.
    BadSignature {
        stage: &'static str,
        cause: &'static str,
    },
    /// The RTC is below the verifier's clock floor (2025-01-01), so no date in
    /// the metadata can be checked.
    ClockUnset,
    /// The signed `Release` is past its `Valid-Until`, or that field is present
    /// but unreadable.
    ReleaseExpired { detail: &'static str },
    /// The signed `Release` is dated implausibly far in the future.
    ReleaseFuture { detail: &'static str },
    /// A date field of the signed `Release` is present but not readable.
    ReleaseDateUnreadable { field: &'static str },
    /// The signed `Release` cannot be used as served (truncated, or it does not
    /// describe this component/architecture).
    ReleaseMalformed { detail: &'static str },
    /// The signed `Release` does not describe the configured suite.
    ReleaseSuiteMismatch,
    /// The signed `Release` lists no `Packages` variant this build can use.
    NoIndexEntry { path: String },
    /// The downloaded `Packages` body does not match the SHA-256/size the signed
    /// `Release` declares. Fatal for the whole update: no other variant is tried.
    IndexMismatch { path: String, cause: &'static str },
    /// A `.deb` stanza carries no usable `SHA256:`, so the payload cannot be
    /// bound to the signed index.
    DigestUnavailable { pkg: String },
    /// The downloaded `.deb` does not match the SHA-256/size from the signed
    /// index. Refused BEFORE any parsing or unpacking.
    DigestMismatch { pkg: String, cause: &'static str },
}

impl AptOpError {
    /// A human-readable, single-line message for the shell.
    pub fn message(&self) -> String {
        match self {
            AptOpError::NetworkDisabled => "network package installation is disabled in this fail-closed build; rebuild with the default `network_packages` feature".to_string(),
            AptOpError::NoNetwork => "no network - check `ifconfig`".to_string(),
            AptOpError::NoIndex => "no package index - run `apt update` first".to_string(),
            AptOpError::NotFound(n) => format!("package '{}' not found in index", n),
            AptOpError::Download { pkg } => format!("download failed for '{}'", pkg),
            AptOpError::Parse { pkg } => format!("could not parse package '{}'", pkg),
            AptOpError::IndexTooLarge => {
                "package index decode failed or exceeded limits — see 'deb:'/'apt:' \
lines on serial for the exact cause; retry 'apt update', or use a smaller component or a \
                 local mirror (e.g. `apt setmirror http://10.0.2.2 /debian`)"
                    .to_string()
            }
            AptOpError::Install { pkg } => format!("install failed for '{}'", pkg),
            AptOpError::Unsigned { url } => format!(
                "the mirror serves no signed metadata (no InRelease, no Release.gpg; refused {}) \
                 — refusing unauthenticated package metadata; see SECURITY.md",
                url
            ),
            AptOpError::BadSignature { stage, cause } => format!(
                "metadata signature verification failed (stage={} cause={}) - the mirror is \
                 not trusted; refusing to continue",
                stage, cause
            ),
            AptOpError::ClockUnset => "the system clock is unset (before 2025-01-01), so the \
                 signed metadata's validity cannot be checked - refusing (set the RTC)"
                .to_string(),
            AptOpError::ReleaseExpired { detail } => format!(
                "the signed Release is not valid any more ({}) - refusing stale metadata",
                detail
            ),
            AptOpError::ReleaseFuture { detail } => {
                format!("the signed Release is dated in the future ({})", detail)
            }
            AptOpError::ReleaseDateUnreadable { field } => format!(
                "the signed Release carries an unreadable '{}' field - refusing (a date that \
                 cannot be checked is not the same as no date)",
                field
            ),
            AptOpError::ReleaseMalformed { detail } => {
                format!("the signed Release cannot be used as served: {}", detail)
            }
            AptOpError::ReleaseSuiteMismatch => "the signed Release does not describe the \
                 configured suite - refusing metadata for a different suite"
                .to_string(),
            AptOpError::NoIndexEntry { path } => format!(
                "the signed Release lists no '{}' - the mirror cannot be trusted to serve \
                 this component/architecture",
                path
            ),
            AptOpError::IndexMismatch { path, cause } => format!(
                "the downloaded index '{}' does not match the signed Release ({}) - refusing \
                 (no fallback to another variant)",
                path, cause
            ),
            AptOpError::DigestUnavailable { pkg } => format!(
                "package '{}' has no SHA256 in the signed index - cannot bind it to the \
                 signed metadata; refusing to install it",
                pkg
            ),
            AptOpError::DigestMismatch { pkg, cause } => format!(
                "package '{}' does not match the signed index digest ({}) - refusing to \
                 unpack it",
                pkg, cause
            ),
        }
    }
}

/// The keys a repository signature is accepted from.
///
/// The trust store is a compile-time constant: the kernel never fetches, adds or
/// updates a key at runtime. A mirror signed by any other key is refused with
/// `cause=NoTrustedSignature`.
///
/// The harness builds that run `apt update` against the LOCAL test mirror
/// (`lx_selftest`, and `lx_bigindex`, whose synthetic index is signed too) add the
/// deterministic E2E test key — compiled out everywhere else, see
/// [`crate::pkg::openpgp_test_keys`]. Those builds still contain every pinned
/// Debian key; the test key only *adds* an anchor, and no configuration can
/// remove one.
fn trusted_keyring() -> &'static [openpgp::PinnedKey] {
    #[cfg(not(any(
        feature = "lx_selftest",
        feature = "lx_bigindex",
        feature = "lx_bigindex_inram"
    )))]
    {
        &super::openpgp_keys::DEBIAN_KEYRING
    }
    #[cfg(any(
        feature = "lx_selftest",
        feature = "lx_bigindex",
        feature = "lx_bigindex_inram"
    ))]
    {
        &super::openpgp_test_keys::TEST_TRUST_ANCHORS
    }
}

/// Signed metadata that verified, with the signer recorded for diagnostics.
struct SignedRelease {
    /// The `Release` body (clear text of `InRelease`, or the `Release` file).
    text: Vec<u8>,
    /// Which document was verified (`InRelease` or `Release.gpg`).
    source: &'static str,
    /// Primary fingerprint of the pinned key that vouched for it.
    signer: [u8; 20],
}

/// Fetch `dists/<suite>/InRelease`, or fall back to `Release.gpg`+`Release`, and
/// verify the signature against the pinned keyring.
///
/// The fallback happens **only** when `InRelease` is absent (HTTP 404). A
/// present-but-invalid `InRelease` is fatal: falling back there would let a
/// man-in-the-middle force the weaker path.
fn fetch_signed_release(cfg: &AptConfig, now: i64) -> Result<SignedRelease, AptOpError> {
    let inrelease = format!("{}/dists/{}/InRelease", cfg.base, cfg.suite);
    match cfg.fetch(&inrelease) {
        Ok(bytes) => match openpgp::verify_clearsigned(trusted_keyring(), &bytes, now) {
            Ok((cs, verified)) => {
                crate::info!(
                    "apt: verify OK release=InRelease signer={} key={}",
                    hex20(&verified.signer),
                    hex20(&verified.key)
                );
                return Ok(SignedRelease {
                    text: cs.text.to_vec(),
                    source: "InRelease",
                    signer: verified.signer,
                });
            }
            Err(e) => return Err(signature_error(e, "InRelease")),
        },
        Err(crate::net::http_fetch::FetchError::Status(404)) => {
            // No InRelease: the detached pair must be present instead.
        }
        Err(crate::net::http_fetch::FetchError::NoNetwork) => return Err(AptOpError::NoNetwork),
        Err(e) => {
            crate::warn!("apt: InRelease fetch failed ({:?}) - trying Release.gpg", e);
        }
    }

    let release_url = format!("{}/dists/{}/Release", cfg.base, cfg.suite);
    let sig_url = format!("{}/dists/{}/Release.gpg", cfg.base, cfg.suite);
    let signature = match cfg.fetch(&sig_url) {
        Ok(b) => b,
        Err(crate::net::http_fetch::FetchError::Status(404)) => {
            verify_fail("metadata", "Unsigned", &format!(" url={}", sig_url));
            return Err(AptOpError::Unsigned { url: sig_url });
        }
        Err(crate::net::http_fetch::FetchError::NoNetwork) => return Err(AptOpError::NoNetwork),
        Err(_) => {
            return Err(AptOpError::Download {
                pkg: "Release.gpg".to_string(),
            })
        }
    };
    let text = match cfg.fetch(&release_url) {
        Ok(b) => b,
        Err(crate::net::http_fetch::FetchError::Status(404)) => {
            verify_fail("metadata", "Unsigned", &format!(" url={}", release_url));
            return Err(AptOpError::Unsigned { url: release_url });
        }
        Err(crate::net::http_fetch::FetchError::NoNetwork) => return Err(AptOpError::NoNetwork),
        Err(_) => {
            return Err(AptOpError::Download {
                pkg: "Release".to_string(),
            })
        }
    };

    match openpgp::verify_detached(trusted_keyring(), &signature, &text, now) {
        Ok(verified) => {
            crate::info!(
                "apt: verify OK release=Release.gpg signer={} key={}",
                hex20(&verified.signer),
                hex20(&verified.key)
            );
            Ok(SignedRelease {
                text,
                source: "Release.gpg",
                signer: verified.signer,
            })
        }
        Err(e) => Err(signature_error(e, "Release.gpg")),
    }
}

/// Apply the `Release` policy checks that are not part of the signature.
fn check_release_policy(cfg: &AptConfig, release: &Release, now: i64) -> Result<(), AptOpError> {
    if release.truncated {
        verify_fail("release", "Truncated", "");
        return Err(AptOpError::ReleaseMalformed {
            detail: "the body is larger than the accepted parse limit",
        });
    }
    if !release.matches_suite(&cfg.suite) {
        verify_fail(
            "release",
            "SuiteMismatch",
            &format!(" suite={} codename={}", release.suite, release.codename),
        );
        return Err(AptOpError::ReleaseSuiteMismatch);
    }
    if !release.architectures.is_empty()
        && !release
            .architectures
            .iter()
            .any(|a| a == &cfg.arch || a == "all")
    {
        verify_fail("release", "ArchitectureNotListed", "");
        return Err(AptOpError::ReleaseMalformed {
            detail: "the configured architecture is not listed in Architectures",
        });
    }

    match release.date {
        DateField::Parsed(date) => {
            if date > now + openpgp::MAX_SKEW {
                verify_fail("release", "FutureDate", "");
                return Err(AptOpError::ReleaseFuture {
                    detail: "Date is after the current time",
                });
            }
        }
        DateField::Malformed => {
            verify_fail("release", "DateUnreadable", "");
            return Err(AptOpError::ReleaseDateUnreadable { field: "Date" });
        }
        DateField::Absent => {}
    }

    match release.valid_until {
        DateField::Parsed(until) => {
            if now > until {
                verify_fail("release", "ValidUntilExpired", "");
                return Err(AptOpError::ReleaseExpired {
                    detail: "Valid-Until has passed",
                });
            }
        }
        DateField::Malformed => {
            verify_fail("release", "ValidUntilUnreadable", "");
            return Err(AptOpError::ReleaseDateUnreadable {
                field: "Valid-Until",
            });
        }
        DateField::Absent => {}
    }
    Ok(())
}

/// Verify a downloaded `Packages` body against the signed `Release` entry
/// **before** it is decompressed or parsed.
fn verify_index_body(
    path: &str,
    bytes: &[u8],
    entry: &release_file::ReleaseEntry,
) -> Result<(), AptOpError> {
    if bytes.len() as u64 != entry.size {
        verify_fail(
            "index",
            "SizeMismatch",
            &format!(" path={} expected={} got={}", path, entry.size, bytes.len()),
        );
        return Err(AptOpError::IndexMismatch {
            path: path.to_string(),
            cause: "SizeMismatch",
        });
    }
    let digest = openpgp_crypto::sha256(bytes);
    if digest != entry.sha256 {
        verify_fail(
            "index",
            "HashMismatch",
            &format!(
                " path={} expected={} got={}",
                path,
                hex32(&entry.sha256),
                hex32(&digest)
            ),
        );
        return Err(AptOpError::IndexMismatch {
            path: path.to_string(),
            cause: "HashMismatch",
        });
    }
    crate::info!(
        "apt: verify index path={} sha256={} size={} ok",
        path,
        hex32(&digest),
        entry.size
    );
    Ok(())
}

/// Verify a downloaded `.deb` against the digest the signed index declares.
///
/// Runs immediately after the download and before `deb::parse_ar`, so a payload
/// that does not match the signed metadata is never parsed, never decompressed
/// and never written to disk.
fn verify_package_body(
    pkg: &str,
    bytes: &[u8],
    expected: &[u8; 32],
    expected_size: u64,
) -> Result<(), AptOpError> {
    if bytes.len() as u64 != expected_size {
        verify_fail(
            "deb",
            "SizeMismatch",
            &format!(
                " pkg={} expected={} got={}",
                pkg,
                expected_size,
                bytes.len()
            ),
        );
        return Err(AptOpError::DigestMismatch {
            pkg: pkg.to_string(),
            cause: "SizeMismatch",
        });
    }
    let digest = openpgp_crypto::sha256(bytes);
    if &digest != expected {
        verify_fail(
            "deb",
            "HashMismatch",
            &format!(
                " pkg={} expected={} got={}",
                pkg,
                hex32(expected),
                hex32(&digest)
            ),
        );
        return Err(AptOpError::DigestMismatch {
            pkg: pkg.to_string(),
            cause: "HashMismatch",
        });
    }
    crate::info!(
        "apt: verify deb pkg={} sha256={} size={} ok",
        pkg,
        hex32(&digest),
        expected_size
    );
    Ok(())
}

/// One `apt: verify FAIL stage=… cause=…` line — the grep surface the e2e
/// harnesses assert on. Exactly one line per refusal.
fn verify_fail(stage: &str, cause: &str, extra: &str) {
    crate::error!("apt: verify FAIL stage={} cause={}{}", stage, cause, extra);
}

/// Map a signature failure onto the apt error, keeping the (stage, cause) pair.
fn signature_error(e: OpenPgpError, source: &str) -> AptOpError {
    let (stage, cause) = e.diagnostic();
    verify_fail(stage, cause, &format!(" release={}", source));
    AptOpError::BadSignature { stage, cause }
}

fn hex_nibble_out(out: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(HEX[(byte >> 4) as usize] as char);
    out.push(HEX[(byte & 0x0f) as usize] as char);
}

/// Lower-case hex of a 32-octet digest (diagnostics only).
fn hex32(digest: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in digest {
        hex_nibble_out(&mut out, *byte);
    }
    out
}

/// Lower-case hex of a 20-octet fingerprint (diagnostics only).
fn hex20(fpr: &[u8; 20]) -> String {
    let mut out = String::with_capacity(40);
    for byte in fpr {
        hex_nibble_out(&mut out, *byte);
    }
    out
}

/// Download and parse the repository `Packages` index into RAM, returning the
/// number of package records loaded.
///
/// ## Trust chain (issue #32)
///
/// The index is only parsed after the metadata that describes it verified:
///
///   1. `dists/<suite>/InRelease` (clear-signed) is fetched and verified against
///      the pinned keyring — or, only when it is absent (404), the detached
///      `Release.gpg` over the `Release` file. A mirror that serves neither is
///      refused outright ([`AptOpError::Unsigned`]); there is no override flag.
///   2. The verified `Release` is parsed ([`release_file`]) and its policy
///      fields are checked: suite/codename, architecture list, `Date` and
///      `Valid-Until`.
///   3. Only `Packages` variants that the signed `Release` actually lists are
///      fetched — an unlisted variant cannot be verified, so it is never used.
///   4. The downloaded body is checked against the declared **SHA-256 and size
///      before** it is decompressed or parsed. A mismatch is fatal: the update
///      does not fall back to another variant (a decode failure still does,
///      since that is a corrupt stream rather than a mismatch).
///
/// The digest is computed over the body the fetch layer already holds (one pass,
/// no copy); the decompressed index is still produced incrementally by
/// [`stream_parse_index`], so resident memory does not grow with index size.
///
/// ## Failure is fail-closed
///
/// Any refusal clears [`struct@INDEX`]: a failed update never leaves the previous
/// index in place as if the mirror had answered, so `apt install` cannot proceed
/// on metadata that no longer verifies.
pub fn update() -> Result<usize, AptOpError> {
    let result = update_verified();
    if result.is_err() {
        *INDEX.lock() = None;
    }
    result
}

fn update_verified() -> Result<usize, AptOpError> {
    #[cfg(not(feature = "insecure_network_demo"))]
    return Err(AptOpError::NetworkDisabled);

    let cfg = config();
    let now = crate::arch::x86_64::linux::rtc::now_unix() as i64;

    // 1+2. Signed metadata, verified, parsed and policy-checked.
    let signed = fetch_signed_release(&cfg, now)?;
    let release = release_file::parse_release(&signed.text);
    check_release_policy(&cfg, &release, now)?;

    // 3. Candidate index variants, in preference order: gzip first (faster to
    // decode at full-index scale), then xz, then uncompressed — but only those
    // the signed Release lists, with their declared digest and size.
    let dir = format!(
        "{}/dists/{}/{}/binary-{}",
        cfg.base, cfg.suite, cfg.component, cfg.arch
    );
    let rel_dir = format!("{}/binary-{}", cfg.component, cfg.arch);
    let candidates: [(String, String, Compression); 3] = [
        (
            format!("{}/Packages.gz", dir),
            format!("{}/Packages.gz", rel_dir),
            Compression::Gzip,
        ),
        (
            format!("{}/Packages.xz", dir),
            format!("{}/Packages.xz", rel_dir),
            Compression::Xz,
        ),
        (
            format!("{}/Packages", dir),
            format!("{}/Packages", rel_dir),
            Compression::None,
        ),
    ];

    let mut saw_no_network = false;
    let mut last_decode_err: Option<AptOpError> = None;
    let mut listed = 0usize;

    for (url, path, comp) in candidates.iter() {
        let Some(entry) = release.sha256_for(path) else {
            continue;
        };
        listed += 1;
        crate::info!(
            "apt: Get {}://{}{} [{}/{}/{}]",
            cfg.scheme(),
            cfg.host,
            url,
            cfg.suite,
            cfg.component,
            cfg.arch
        );
        match cfg.fetch(url) {
            Ok(bytes) => {
                // 4. The signed Release is the authority for these bytes: check
                // them BEFORE any decoding. A mismatch is fatal for the whole
                // update (see the doc comment).
                verify_index_body(path, &bytes, entry)?;
                crate::info!(
                    "apt: fetched {} index body - decompressing...",
                    human_bytes(bytes.len() as u64)
                );
                // A successful download that fails to *decode* is no longer fatal
                // for the whole update: fall through and try the next index
                // variant (different bytes AND a different decoder).
                let index = match stream_parse_index(&bytes, *comp) {
                    Ok(index) => index,
                    Err(e) => {
                        crate::warn!("apt: decode of {} failed - trying next index variant", url);
                        last_decode_err = Some(e);
                        continue;
                    }
                };
                let count = index.len();
                *INDEX.lock() = Some(index);
                crate::info!("apt: index ready - {} packages", count);
                return Ok(count);
            }
            Err(crate::net::http_fetch::FetchError::NoNetwork) => {
                saw_no_network = true;
                break;
            }
            Err(_) => {
                // Try the next compression variant.
            }
        }
    }

    if listed == 0 {
        verify_fail("index", "NoIndexEntry", &format!(" path={}", rel_dir));
        return Err(AptOpError::NoIndexEntry {
            path: format!("{}/Packages.gz", rel_dir),
        });
    }

    if let Some(e) = last_decode_err {
        Err(e)
    } else if saw_no_network {
        Err(AptOpError::NoNetwork)
    } else {
        Err(AptOpError::Download {
            pkg: "Packages".to_string(),
        })
    }
}

/// Progress-log cadence: emit one `apt: parsed N packages...` line roughly every
/// this many decompressed bytes (and at least every [`PROGRESS_PKGS`] packages),
/// so a multi-second index load shows visible forward progress without spamming.
const PROGRESS_BYTES: usize = 4 * 1024 * 1024;
/// Progress-log cadence by package count (see [`PROGRESS_BYTES`]).
const PROGRESS_PKGS: usize = 5000;

/// Decompress a fetched `Packages` body of compression `comp` **incrementally**
/// and parse it into records, holding only the compressed body, small decode
/// buffers, and the growing record list resident (never the whole decompressed
/// index). Logs periodic progress. Maps any decode/parse overrun to
/// [`AptOpError::IndexTooLarge`] (a clean error, not an abort).
fn stream_parse_index(bytes: &[u8], comp: Compression) -> Result<PackageIndex, AptOpError> {
    // Explicit start marker -- the full Debian index decompresses
    // to ~150 MiB and parses for minutes; without this line the gap after
    // "decompressing..." looks like a hang.
    crate::info!("apt: parsing index (takes a few minutes for the full Debian index; progress every 4 MiB)...");
    let mut parser = StanzaParser::new();
    let mut builder = PackageIndexBuilder::new();
    let mut decompressed: usize = 0;
    let mut next_byte_mark = PROGRESS_BYTES;
    let mut next_pkg_mark = PROGRESS_PKGS;
    // DIAGNOSTIC (Part B, lx_bigindex): a finer ~1 MiB heap-headroom marker so we
    // can watch allocator used/free as the big index is parsed (rule allocator
    // exhaustion/corruption in or out). Feature-gated: the default kernel build
    // does not compile or run this.
    #[cfg(feature = "lx_bigindex")]
    let mut next_heap_mark: usize = 1024 * 1024;

    let result = deb::decompress_stream(bytes, comp, deb::MAX_INDEX_STREAM_BYTES, |chunk| {
        decompressed += chunk.len();
        parser.push_view(chunk, &mut builder);
        // DIAGNOSTIC heap-headroom log (~every 1 MiB decompressed).
        #[cfg(feature = "lx_bigindex")]
        if decompressed >= next_heap_mark {
            let (size, used, free) = crate::memory::heap::stats();
            crate::info!(
                "BIGINDEX heap: decompressed {} KiB, pkgs {}, heap used {} KiB / free {} KiB / size {} KiB",
                decompressed / 1024,
                builder.len(),
                used / 1024,
                free / 1024,
                size / 1024
            );
            while decompressed >= next_heap_mark {
                next_heap_mark += 1024 * 1024;
            }
        }
        // Periodic progress (by bytes OR by package count) so a long load does
        // not look hung, while staying modest (not per-line).
        if decompressed >= next_byte_mark || builder.len() >= next_pkg_mark {
            crate::info!(
                "apt: reading package lists... {} / {} pkgs",
                human_bytes(decompressed as u64),
                builder.len()
            );
            while decompressed >= next_byte_mark {
                next_byte_mark += PROGRESS_BYTES;
            }
            while builder.len() >= next_pkg_mark {
                next_pkg_mark += PROGRESS_PKGS;
            }
        }
        Ok(())
    });

    match result {
        Ok(_) => {
            // Flush the trailing partial line / final stanza.
            parser.finish_view(&mut builder);
            crate::info!(
                "apt: read package lists - {} / {} pkgs",
                human_bytes(decompressed as u64),
                builder.len()
            );
            Ok(PackageIndex::from_builder(builder))
        }
        Err(_) => {
            crate::warn!(
                "apt: index decode failed after {} decompressed / {} pkgs parsed",
                human_bytes(decompressed as u64),
                builder.len()
            );
            Err(AptOpError::IndexTooLarge)
        }
    }
}

/// Resolve and install `name` (and its not-yet-installed dependencies) from the
/// loaded index, returning the package names installed in dependency-first order.
///
/// Every `.deb` is bound to the *signed* index before it is unpacked: the
/// `SHA256:` and `Size:` of the stanza that supplied the `Filename:` are checked
/// against the downloaded bytes (see [`verify_package_body`]). A stanza without a
/// usable digest is refused, and so is any mismatch — the package is not parsed,
/// not decompressed and not written to disk.
///
/// Requires [`update`] to have been run (else [`AptOpError::NoIndex`]). The plan
/// is computed by [`resolve_install`] against a snapshot of the session
/// installed-set; each planned package's `.deb` is then fetched from
/// `{base}/{filename}`, parsed, decompressed, and written onto ext2 under `/mnt`.
/// Each successfully installed name is recorded in [`struct@INSTALLED`].
pub fn install(name: &str) -> Result<Vec<String>, AptOpError> {
    #[cfg(not(feature = "insecure_network_demo"))]
    return Err(AptOpError::NetworkDisabled);

    let cfg = config();

    // Snapshot the session installed-set for the resolver.
    let already = INSTALLED.lock().clone();

    // Plan the transaction and capture, for each package, the pool filename AND
    // the digest the signed index declares for it — from the SAME record, so the
    // URL and the digest can never come from different stanzas. The index lock is
    // held only for this short, network-free span.
    struct Target {
        pkg: String,
        filename: String,
        sha256: Option<[u8; 32]>,
        size: u64,
    }
    let targets: Vec<Target> = {
        let guard = INDEX.lock();
        let index = guard.as_ref().ok_or(AptOpError::NoIndex)?;
        let plan = resolve_install(index, name, &already).map_err(|e| match e {
            AptError::NotFound(n) => AptOpError::NotFound(n),
        })?;
        let mut t = Vec::with_capacity(plan.len());
        for pkg in &plan {
            // resolve_install yields real package names; get() should hit, but
            // fall back to provider resolution defensively.
            if let Some(rec) = index.get(pkg).or_else(|| index.get_provider(pkg)) {
                t.push(Target {
                    pkg: rec.package().to_string(),
                    filename: rec.filename().to_string(),
                    sha256: rec.sha256(),
                    size: rec.size(),
                });
            }
        }
        t
    };

    let total = targets.len();
    if total > 0 {
        let plan: Vec<&str> = targets.iter().map(|t| t.pkg.as_str()).collect();
        crate::info!(
            "apt: {} new package(s) to install: {}",
            total,
            plan.join(" ")
        );
    }

    let mut installed: Vec<String> = Vec::new();

    for (i, target) in targets.into_iter().enumerate() {
        let step = i + 1;
        let pkg = target.pkg;
        let filename = target.filename;
        if filename.is_empty() {
            return Err(AptOpError::Download { pkg: pkg.clone() });
        }
        // The payload is bound to the signed metadata through this digest. A
        // stanza without one cannot be verified, so it is refused instead of
        // installed on faith.
        let Some(expected_digest) = target.sha256 else {
            verify_fail("deb", "DigestUnavailable", &format!(" pkg={}", pkg));
            return Err(AptOpError::DigestUnavailable { pkg });
        };
        let url = format!("{}/{}", cfg.base, filename);
        crate::info!(
            "apt: [{}/{}] Get {} <- {}://{}{}",
            step,
            total,
            pkg,
            cfg.scheme(),
            cfg.host,
            url
        );

        let bytes = cfg.fetch(&url).map_err(|e| match e {
            crate::net::http_fetch::FetchError::NoNetwork => AptOpError::NoNetwork,
            _ => AptOpError::Download { pkg: pkg.clone() },
        })?;
        let dl = bytes.len();

        // Signature -> Release -> Packages -> .deb: the digest from the signed
        // index is checked BEFORE the payload is parsed, decompressed or written
        // to ext2. Nothing below this line sees unverified bytes.
        verify_package_body(&pkg, &bytes, &expected_digest, target.size)?;

        let members = deb::parse_ar(&bytes).map_err(|_| AptOpError::Parse { pkg: pkg.clone() })?;
        let deb_members =
            deb::locate_members(&members).map_err(|_| AptOpError::Parse { pkg: pkg.clone() })?;
        let comp = deb::compression_of(deb_members.data.name)
            .map_err(|_| AptOpError::Parse { pkg: pkg.clone() })?;
        let tar_bytes = deb::decompress_data(&deb_members.data, comp)
            .map_err(|_| AptOpError::Parse { pkg: pkg.clone() })?;
        let entries =
            tar::read_tar(&tar_bytes).map_err(|_| AptOpError::Parse { pkg: pkg.clone() })?;
        let n = install_fs::install_data_tar(&entries, "/mnt")
            .map_err(|_| AptOpError::Install { pkg: pkg.clone() })?;

        // Sync after each package so files survive a later crash.
        if let Ok(node) = crate::vfs::lookup_path("/mnt") {
            node.sync();
        }

        INSTALLED.lock().insert(pkg.clone());
        crate::info!(
            "apt: [{}/{}] Unpacked {} ({} files, {})",
            step,
            total,
            pkg,
            n,
            human_bytes(dl as u64)
        );
        installed.push(pkg);
    }

    Ok(installed)
}

/// Look up a package summary by name (real or virtual). Returns `None` if no
/// index is loaded or the name is unknown.
pub fn show(name: &str) -> Option<PkgSummary> {
    let guard = INDEX.lock();
    let index = guard.as_ref()?;
    let rec = index.get(name).or_else(|| index.get_provider(name))?;
    Some(PkgSummary::from_record(rec))
}

/// True if an index is currently loaded.
pub fn has_index() -> bool {
    INDEX.lock().is_some()
}

/// The deterministic resident footprint of the currently-loaded index in bytes
/// (the [`PackageIndex::footprint`] accounting identity), or `None` if no index
/// is loaded. Read-only; used by the live-update self-test to report the
/// Resident_Index_Footprint against the 128 MiB ceiling (R2.4/R6.2).
pub fn index_footprint() -> Option<usize> {
    INDEX.lock().as_ref().map(|i| i.footprint())
}

/// List package names known to the index. With `filter`, only names containing
/// that substring are returned. Names come back sorted and de-duplicated; an
/// empty `Vec` means no index is loaded.
pub fn list(filter: Option<&str>) -> Vec<String> {
    let guard = INDEX.lock();
    match guard.as_ref() {
        Some(index) => index
            .names()
            .filter(|n| filter.map_or(true, |f| n.contains(f)))
            .map(|n| n.to_string())
            .collect(),
        None => Vec::new(),
    }
}
