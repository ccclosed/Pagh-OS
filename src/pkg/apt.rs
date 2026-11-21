//! Эффектный фронтенд пакетного менеджера `apt` (конвейер установки по имени).
//!
//! Это **kernel-only** слой оркестрации, связывающий существующие чистые части в
//! apt-подобный рабочий процесс, ведомый целиком *именем пакета* — без ручных
//! host/port/path, которых требует низкоуровневая команда `pkg`:
//!
//!   * [`update`] скачивает и разбирает индекс `Packages` бинарного Debian-
//!     репозитория в in-RAM [`PackageIndex`] (сторона чтения, [`super::apt_index`]).
//!   * [`install`] планирует dependency-first транзакцию чистым резолвером
//!     ([`super::apt_resolve`]), затем для каждого пакета качает `.deb`
//!     ([`crate::net::http_fetch::http_get`]), разбирает и распаковывает
//!     ([`super::deb`]), перечисляет его `data.tar` ([`super::tar`]) и пишет файлы
//!     на ext2 под `/mnt` ([`super::install_fs`]).
//!   * [`show`] / [`list`] — read-only запросы к кэшированному индексу.
//!
//! ## Состояние
//!
//! Три process-global, защищённых спинлоком синглтона держат состояние сессии:
//!
//!   * [`struct@CONFIG`] — активные mirror/suite/component/arch (см. [`AptConfig`]),
//!     меняются [`set_mirror`].
//!   * [`struct@INDEX`] — разобранный [`PackageIndex`], заполняется [`update`].
//!     Хранится **только в RAM и никогда не персистится на ext2**: диск всего 64 MiB,
//!     слишком мало для реального индекса `main` (см.
//!     [`super::deb::MAX_INDEX_DECOMPRESSED`]), так что индекс пересобирается
//!     `apt update` каждую загрузку.
//!   * [`struct@INSTALLED`] — множество имён пакетов, установленных *в этой сессии*,
//!     используемое как `already_installed` резолвера, чтобы пропускать повторные
//!     установки и общие зависимости. Это не настоящая dpkg status database.
//!
//! Сетевой I/O выполняется **без удержания лока apt**: каждый глобал лочится лишь
//! настолько, чтобы прочитать/склонировать нужное (лок индекса никогда не держится
//! через `http_get`, который сам выключает прерывания при прокачке сокета).
//!
//! ## Цепочка доверия
//!
//! Ничто с зеркала не используется, пока не прошла проверка цепочки подписей
//! (issue #32, `OPENPGP-VERIFY-CONTRACT.md`):
//!
//! ```text
//! InRelease | Release.gpg+Release   (OpenPGP, закреплённый Debian-keyring)
//!        |
//!        +-- SHA-256 + размер реально скачанного варианта Packages
//!                 |
//!                 +-- SHA-256 + размер каждого .deb из подписанного индекса,
//!                     проверяются после скачивания и ДО deb::parse_ar
//! ```
//!
//! Зеркало, не отдающее подписи, отвергается громко ([`AptOpError::Unsigned`]);
//! несовпадение где угодно фатально, и нет флага, фичи или переменной окружения,
//! отключающей проверку. Единственная непроверяемая сборка — `--no-default-features`,
//! где `update`/`install` возвращают [`AptOpError::NetworkDisabled`].

#![allow(dead_code)]

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::sync::spinlock::Spinlock;

use super::apt_index::{PackageIndex, PackageIndexBuilder, PkgRef, StanzaParser};
use super::apt_resolve::{resolve_install, AptError};
use super::deb::{self, Compression};
use super::install::plan_install_meta;
use super::install_fs;
use super::openpgp::{self, OpenPgpError};
use super::openpgp_crypto;
use super::release_file::{self, DateField, Release};
use super::tar;

/// Host of the built-in live mirror (the official Debian archive), over HTTPS.
///
/// SINGLE SOURCE OF TRUTH: the default [`AptConfig`], the shell's mirror
/// examples and the live E2E harnesses all name the mirror through this
/// constant and [`DEFAULT_MIRROR_BASE`] instead of re-spelling host and base
/// (issue #19 cleanup: the live harness used to point at a *second*,
/// cleartext-HTTP mirror of the same host, so "which mirror does the guest
/// actually use" had two answers).
pub const DEFAULT_MIRROR_HOST: &str = "deb.debian.org";
/// Base path of the built-in live mirror (`https://<host>/debian`). See
/// [`DEFAULT_MIRROR_HOST`].
pub const DEFAULT_MIRROR_BASE: &str = "/debian";

/// The active repository configuration for `apt`.
///
/// The index URL is derived from these as
/// `{base}/dists/{suite}/{component}/binary-{arch}/Packages.{xz,gz,}` and each
/// `.deb` URL as `{base}/{filename}` (the pool-relative `Filename:` from the
/// index). Defaults target Debian `stable`/`main`/`amd64` on
/// [`DEFAULT_MIRROR_HOST`] over **HTTPS** (TLS 1.3).
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
    /// HTTPS on [`DEFAULT_MIRROR_HOST`]).
    fn defaults() -> AptConfig {
        AptConfig {
            host: DEFAULT_MIRROR_HOST.to_string(),
            base: DEFAULT_MIRROR_BASE.to_string(),
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

// ───────────────────────── phase timings (W0) ─────────────────────────

/// The scheduler's millisecond counter (`TICK_HZ` = 1000, so one tick is ~1 ms of
/// guest time). This is the timing source for every `apt: timings` line.
///
/// It is monotonic, cheap (one relaxed load), and advances during a network fetch
/// because [`crate::net::http_fetch::http_get`] waits with
/// [`crate::task::scheduler::sleep_ticks`] between TCP pump steps rather than
/// spinning with interrupts off.
fn now_ms() -> u64 {
    crate::task::scheduler::ticks()
}

/// Kernel-heap bytes currently accounted as used, sampled at phase boundaries.
///
/// `apt` reports the **maximum of these samples** as `heap_peak_kib`: a phase
/// boundary always follows the allocation a phase makes (the decompressed
/// `data.tar`, the parsed entry list, the installed-file buffers), so the high
/// water mark over those boundaries captures the peak that matters for the
/// streaming comparison without adding an allocator hot-path hook.
fn heap_used_bytes() -> usize {
    crate::memory::heap::stats().1
}

/// Total time of the last `apt update`/`apt install`, in scheduler milliseconds.
///
/// The in-guest self-test prints this next to its verdict so a benchmark log
/// carries a machine-readable total without scraping the phase breakdown.
static LAST_TOTAL_MS: Spinlock<Option<u64>> = Spinlock::new(None);

/// Total scheduler-ms of the most recent [`update`] or [`install`], or `None`
/// before the first one. Used by the `apt_e2e`/benchmark self-tests.
pub fn last_total_ms() -> Option<u64> {
    *LAST_TOTAL_MS.lock()
}

/// One `apt` operation's phase timers.
///
/// Each `phase(name)` call charges the time since the previous mark to `name`
/// (accumulating repeats, so a per-package phase shows the sum across packages)
/// and samples the heap. [`Timings::finish`] emits exactly one
/// `apt: timings cmd=… <phase>=<ms>… total_ms=… heap_peak_kib=…` line — the grep
/// surface the benchmarks and the e2e harness read. Timing never changes what the
/// operation does: it only reads two counters.
struct Timings {
    cmd: &'static str,
    start: u64,
    last: u64,
    phases: Vec<(&'static str, u64)>,
    counts: Vec<(&'static str, u64)>,
    heap_peak: usize,
}

impl Timings {
    /// Start timing `cmd`; the first phase includes everything since here.
    fn begin(cmd: &'static str) -> Timings {
        let now = now_ms();
        Timings {
            cmd,
            start: now,
            last: now,
            phases: Vec::new(),
            counts: Vec::new(),
            heap_peak: heap_used_bytes(),
        }
    }

    /// Sample the heap high-water mark (no-op when below the current peak).
    fn sample_heap(&mut self) {
        let used = heap_used_bytes();
        if used > self.heap_peak {
            self.heap_peak = used;
        }
    }

    /// Charge the time since the previous mark to `name` and sample the heap.
    fn phase(&mut self, name: &'static str) {
        let now = now_ms();
        let elapsed = now.saturating_sub(self.last);
        self.last = now;
        match self.phases.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = slot.1.saturating_add(elapsed),
            None => self.phases.push((name, elapsed)),
        }
        self.sample_heap();
    }

    /// Record a non-time counter (bytes, packages, files) for the log line.
    fn count(&mut self, name: &'static str, value: u64) {
        match self.counts.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = slot.1.saturating_add(value),
            None => self.counts.push((name, value)),
        }
    }

    /// Emit the single `apt: timings …` line and record the total.
    fn finish(mut self) {
        let total = now_ms().saturating_sub(self.start);
        *LAST_TOTAL_MS.lock() = Some(total);
        self.sample_heap();
        let mut line = format!("ppkg: timings cmd={}", self.cmd);
        for (name, ms) in self.phases.iter() {
            line.push(' ');
            line.push_str(name);
            line.push('=');
            line.push_str(&format!("{}", ms));
        }
        for (name, value) in self.counts.iter() {
            line.push(' ');
            line.push_str(name);
            line.push('=');
            line.push_str(&format!("{}", value));
        }
        line.push_str(&format!(
            " total_ms={} heap_peak_kib={}",
            total,
            self.heap_peak / 1024
        ));
        crate::info!("{}", line);
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
        feature = "lx_bigindex_inram",
        feature = "lx_apt_bench"
    )))]
    {
        &super::openpgp_keys::DEBIAN_KEYRING
    }
    #[cfg(any(
        feature = "lx_selftest",
        feature = "lx_bigindex",
        feature = "lx_bigindex_inram",
        feature = "lx_apt_bench"
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
fn fetch_signed_release(
    cfg: &AptConfig,
    now: i64,
    t: &mut Timings,
) -> Result<SignedRelease, AptOpError> {
    let inrelease = format!("{}/dists/{}/InRelease", cfg.base, cfg.suite);
    let fetched = cfg.fetch(&inrelease);
    t.phase("release_fetch_ms");
    match fetched {
        Ok(bytes) => match openpgp::verify_clearsigned(trusted_keyring(), &bytes, now) {
            Ok((cs, verified)) => {
                crate::info!(
                    "ppkg: verify OK release=InRelease signer={} key={}",
                    hex20(&verified.signer),
                    hex20(&verified.key)
                );
                t.phase("release_verify_ms");
                return Ok(SignedRelease {
                    text: cs.text.to_vec(),
                    source: "InRelease",
                    signer: verified.signer,
                });
            }
            Err(e) => {
                t.phase("release_verify_ms");
                return Err(signature_error(e, "InRelease"));
            }
        },
        Err(crate::net::http_fetch::FetchError::Status(404)) => {
            // No InRelease: the detached pair must be present instead.
        }
        Err(crate::net::http_fetch::FetchError::NoNetwork) => return Err(AptOpError::NoNetwork),
        Err(e) => {
            crate::warn!(
                "ppkg: InRelease fetch failed ({:?}) - trying Release.gpg",
                e
            );
        }
    }

    let release_url = format!("{}/dists/{}/Release", cfg.base, cfg.suite);
    let sig_url = format!("{}/dists/{}/Release.gpg", cfg.base, cfg.suite);
    let signature = match cfg.fetch(&sig_url) {
        Ok(b) => {
            t.phase("release_fetch_ms");
            b
        }
        Err(crate::net::http_fetch::FetchError::Status(404)) => {
            t.phase("release_fetch_ms");
            verify_fail("metadata", "Unsigned", &format!(" url={}", sig_url));
            return Err(AptOpError::Unsigned { url: sig_url });
        }
        Err(crate::net::http_fetch::FetchError::NoNetwork) => {
            t.phase("release_fetch_ms");
            return Err(AptOpError::NoNetwork);
        }
        Err(_) => {
            t.phase("release_fetch_ms");
            return Err(AptOpError::Download {
                pkg: "Release.gpg".to_string(),
            });
        }
    };
    let text = match cfg.fetch(&release_url) {
        Ok(b) => {
            t.phase("release_fetch_ms");
            b
        }
        Err(crate::net::http_fetch::FetchError::Status(404)) => {
            t.phase("release_fetch_ms");
            verify_fail("metadata", "Unsigned", &format!(" url={}", release_url));
            return Err(AptOpError::Unsigned { url: release_url });
        }
        Err(crate::net::http_fetch::FetchError::NoNetwork) => {
            t.phase("release_fetch_ms");
            return Err(AptOpError::NoNetwork);
        }
        Err(_) => {
            t.phase("release_fetch_ms");
            return Err(AptOpError::Download {
                pkg: "Release".to_string(),
            });
        }
    };

    match openpgp::verify_detached(trusted_keyring(), &signature, &text, now) {
        Ok(verified) => {
            t.phase("release_verify_ms");
            crate::info!(
                "ppkg: verify OK release=Release.gpg signer={} key={}",
                hex20(&verified.signer),
                hex20(&verified.key)
            );
            Ok(SignedRelease {
                text,
                source: "Release.gpg",
                signer: verified.signer,
            })
        }
        Err(e) => {
            t.phase("release_verify_ms");
            Err(signature_error(e, "Release.gpg"))
        }
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
        "ppkg: verify index path={} sha256={} size={} ok",
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
        "ppkg: verify deb pkg={} sha256={} size={} ok",
        pkg,
        hex32(&digest),
        expected_size
    );
    Ok(())
}

/// One `apt: verify FAIL stage=… cause=…` line — the grep surface the e2e
/// harnesses assert on. Exactly one line per refusal.
fn verify_fail(stage: &str, cause: &str, extra: &str) {
    crate::error!("ppkg: verify FAIL stage={} cause={}{}", stage, cause, extra);
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

// ───────────────────── persistent compact index cache (W3) ─────────────────────
//
// `apt update` normally re-downloads, re-decompresses and re-parses the whole
// `Packages` index every boot. The indices this project cares about are large
// (the full Debian `main` decompresses to ~150 MiB), so that is the most
// expensive part of a boot-time package refresh. The cache below stores the
// parsed index in its compact arena form (see
// [`PackageIndex::to_compact_bytes`](super::apt_index::PackageIndex::to_compact_bytes))
// under the ext2 install root and revalidates it against the **signed** `Release`
// the current update just verified.
//
// ## Trust model
//
// The signed `Release` is ALWAYS fetched and verified first; the cache never
// replaces that step and is only consulted after it succeeds. The cache key is
// `(SHA-256 of the verified Release body, suite, component, arch, format
// version)`, so a mirror rollback/rotation or a configuration change invalidates
// it. The payload is length-delimited, versioned and carries a SHA-256 of its own
// bytes, and every offset is validated before use; anything inconsistent is
// treated as absent and the normal verified network path runs.
//
// The cache file lives on `/mnt`, i.e. at the same trust level as everything
// else installed there (packages are signature-verified before installation, and
// an actor who can rewrite `/mnt` can already rewrite the userland and boot
// files this kernel executes). Its checksum protects against corruption and
// accidental clobbering by an installed file; it is not a substitute for the
// signed `Release` check, which still gates every run.

/// Directory of the persistent apt index cache on the install root.
const INDEX_CACHE_DIR: &str = "/mnt/var/cache/pagh/apt";
/// File name of the single cached index (keyed by suite/component/arch + Release).
const INDEX_CACHE_FILE: &str = "index-v1.bin";
/// Cache file magic (16 bytes, fixed).
const INDEX_CACHE_MAGIC: [u8; 16] = *b"PAGHAPT-CACHE-v1";
/// Cache container format version; bump on any layout change.
const INDEX_CACHE_VERSION: u32 = 1;
/// Refuse to read or write a cache larger than this. The guest disk is 64 MiB
/// and the compact encoding is several times smaller than the text index; a
/// "cache" beyond this bound is corrupt or from an incompatible future format, so
/// it is ignored (fallback) rather than trusted.
const MAX_INDEX_CACHE_BYTES: usize = 48 * 1024 * 1024;
/// Fixed part of the cache header:
/// magic(16) + version(4) + release digest(32) + three u16 lengths(6) +
/// payload length(8) + payload SHA-256(32).
const INDEX_CACHE_HEADER_FIXED: usize = 16 + 4 + 32 + 6 + 8 + 32;

/// Whether the persistent index cache is active in this build.
///
/// The cache is a production optimization. The diagnostic harnesses that exist to
/// exercise the REAL download+decompress+parse path (`lx_bigindex`,
/// `lx_bigindex_inram`, and the live-mirror `lx_livetest`) must never be short-
/// circuited by a cache left on a reused disk, so it is compiled out there.
fn index_cache_enabled() -> bool {
    !cfg!(any(
        feature = "lx_bigindex",
        feature = "lx_bigindex_inram",
        feature = "lx_livetest"
    ))
}

/// Create (or look up) every component of an absolute directory path.
fn ensure_dir_path(path: &str) -> Result<Arc<dyn crate::vfs::VfsNode>, crate::vfs::VfsError> {
    use crate::vfs::VfsError;
    if let Ok(node) = crate::vfs::lookup_path(path) {
        if node.is_directory() {
            return Ok(node);
        }
        return Err(VfsError::InvalidArgument);
    }
    let split = path.rfind('/').ok_or(VfsError::InvalidArgument)?;
    let parent_path = if split == 0 { "/" } else { &path[..split] };
    let name = &path[split + 1..];
    if name.is_empty() {
        return Err(VfsError::InvalidArgument);
    }
    let parent = ensure_dir_path(parent_path)?;
    match parent.create_dir(name) {
        Ok(node) => Ok(node),
        Err(VfsError::AlreadyExists) => match parent.lookup(name) {
            Ok(node) if node.is_directory() => Ok(node),
            _ => Err(VfsError::InvalidArgument),
        },
        Err(e) => Err(e),
    }
}

/// Write `bytes` at `offset`, looping on short writes (`write_all` for a
/// [`VfsNode`]).
fn write_all_at(
    node: &Arc<dyn crate::vfs::VfsNode>,
    offset: u64,
    bytes: &[u8],
) -> Result<(), crate::vfs::VfsError> {
    let mut written = 0usize;
    while written < bytes.len() {
        let n = node.write(offset + written as u64, &bytes[written..])?;
        if n == 0 {
            return Err(crate::vfs::VfsError::IoError);
        }
        written += n;
    }
    Ok(())
}

/// Read exactly `buf.len()` bytes at `offset`; a short read is an I/O error.
fn read_exact_at(
    node: &Arc<dyn crate::vfs::VfsNode>,
    offset: u64,
    buf: &mut [u8],
) -> Result<(), crate::vfs::VfsError> {
    let mut read = 0usize;
    while read < buf.len() {
        let n = node.read(offset + read as u64, &mut buf[read..])?;
        if n == 0 {
            return Err(crate::vfs::VfsError::IoError);
        }
        read += n;
    }
    Ok(())
}

fn cache_push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn cache_push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn cache_push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Header cursor for the cache container (strict, bounds-checked).
struct CacheCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> CacheCursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }
    fn u16(&mut self) -> Option<u16> {
        let mut b = [0u8; 2];
        b.copy_from_slice(self.take(2)?);
        Some(u16::from_le_bytes(b))
    }
    fn u32(&mut self) -> Option<u32> {
        let mut b = [0u8; 4];
        b.copy_from_slice(self.take(4)?);
        Some(u32::from_le_bytes(b))
    }
    fn u64(&mut self) -> Option<u64> {
        let mut b = [0u8; 8];
        b.copy_from_slice(self.take(8)?);
        Some(u64::from_le_bytes(b))
    }
    fn digest(&mut self) -> Option<[u8; 32]> {
        let mut d = [0u8; 32];
        d.copy_from_slice(self.take(32)?);
        Some(d)
    }
    fn str(&mut self, len: usize) -> Option<&'a str> {
        core::str::from_utf8(self.take(len)?).ok()
    }
}

/// Serialize `index` plus its cache key and write it to [`INDEX_CACHE_DIR`].
///
/// Returns the number of bytes written. Any failure (no space, unsupported
/// filesystem, encoding larger than [`MAX_INDEX_CACHE_BYTES`]) is a logged
/// warning, not an update failure: the cache is an optimization only.
fn store_index_cache(
    cfg: &AptConfig,
    release_digest: &[u8; 32],
    index: &PackageIndex,
) -> Result<usize, ()> {
    let payload = index.to_compact_bytes();
    if payload.len() > MAX_INDEX_CACHE_BYTES {
        crate::warn!(
            "ppkg: index cache skipped ({} > {} byte cap)",
            payload.len(),
            MAX_INDEX_CACHE_BYTES
        );
        return Err(());
    }
    let suite = cfg.suite.as_bytes();
    let component = cfg.component.as_bytes();
    let arch = cfg.arch.as_bytes();
    if suite.len() > u16::MAX as usize
        || component.len() > u16::MAX as usize
        || arch.len() > u16::MAX as usize
    {
        return Err(());
    }
    let payload_digest = openpgp_crypto::sha256(&payload);

    let mut header: Vec<u8> =
        Vec::with_capacity(INDEX_CACHE_HEADER_FIXED + suite.len() + component.len() + arch.len());
    header.extend_from_slice(&INDEX_CACHE_MAGIC);
    cache_push_u32(&mut header, INDEX_CACHE_VERSION);
    header.extend_from_slice(release_digest);
    cache_push_u16(&mut header, suite.len() as u16);
    cache_push_u16(&mut header, component.len() as u16);
    cache_push_u16(&mut header, arch.len() as u16);
    header.extend_from_slice(suite);
    header.extend_from_slice(component);
    header.extend_from_slice(arch);
    cache_push_u64(&mut header, payload.len() as u64);
    header.extend_from_slice(&payload_digest);

    let dir = ensure_dir_path(INDEX_CACHE_DIR).map_err(|e| {
        crate::warn!("ppkg: index cache dir unavailable ({:?}); not caching", e);
    })?;
    // Replace any previous cache atomically-enough for a single-writer kernel:
    // remove first, then create the new file (a crash in between just yields a
    // cache miss next boot).
    let _ = dir.remove(INDEX_CACHE_FILE);
    let file = dir.create_file(INDEX_CACHE_FILE).map_err(|e| {
        crate::warn!("ppkg: index cache create failed ({:?}); not caching", e);
    })?;
    let mut offset = 0u64;
    if let Err(e) = write_all_at(&file, offset, &header) {
        crate::warn!("ppkg: index cache write failed ({:?})", e);
        let _ = dir.remove(INDEX_CACHE_FILE);
        return Err(());
    }
    offset += header.len() as u64;
    if let Err(e) = write_all_at(&file, offset, &payload) {
        crate::warn!("ppkg: index cache write failed ({:?})", e);
        let _ = dir.remove(INDEX_CACHE_FILE);
        return Err(());
    }
    offset += payload.len() as u64;
    file.sync();
    if file.size() != offset {
        crate::warn!(
            "ppkg: index cache size mismatch after write ({} != {})",
            file.size(),
            offset
        );
        let _ = dir.remove(INDEX_CACHE_FILE);
        return Err(());
    }
    Ok(offset as usize)
}

/// Try to load the cached index for the verified `Release` whose body has digest
/// `release_digest`.
///
/// Returns the index and the cache file size on a validated hit, `None` on a
/// miss or ANY inconsistency (corrupt header, checksum mismatch, foreign key,
/// undecodable payload, oversized file). The caller then runs the normal
/// verified download+parse path. Never panics and never trusts a partial file.
fn load_index_cache(cfg: &AptConfig, release_digest: &[u8; 32]) -> Option<(PackageIndex, usize)> {
    let dir = crate::vfs::lookup_path(INDEX_CACHE_DIR).ok()?;
    let file = dir.lookup(INDEX_CACHE_FILE).ok()?;
    if file.is_directory() {
        return None;
    }
    let size = file.size() as usize;
    if size <= INDEX_CACHE_HEADER_FIXED || size > MAX_INDEX_CACHE_BYTES {
        crate::info!("ppkg: index cache MISS (implausible size {})", size);
        return None;
    }
    let mut buf = alloc::vec![0u8; size];
    if read_exact_at(&file, 0, &mut buf).is_err() {
        crate::warn!("ppkg: index cache read failed; ignoring it");
        return None;
    }

    let mut c = CacheCursor {
        bytes: &buf,
        pos: 0,
    };
    let parsed = (|| -> Option<(PackageIndex, usize)> {
        if c.take(INDEX_CACHE_MAGIC.len())? != INDEX_CACHE_MAGIC {
            return None;
        }
        if c.u32()? != INDEX_CACHE_VERSION {
            return None;
        }
        if c.digest()? != *release_digest {
            return None;
        }
        let suite_len = c.u16()? as usize;
        let component_len = c.u16()? as usize;
        let arch_len = c.u16()? as usize;
        let suite = c.str(suite_len)?;
        let component = c.str(component_len)?;
        let arch = c.str(arch_len)?;
        if suite != cfg.suite || component != cfg.component || arch != cfg.arch {
            return None;
        }
        let payload_len = c.u64()? as usize;
        let payload_digest = c.digest()?;
        let header_len = c.pos;
        if payload_len != size.checked_sub(header_len)? {
            return None;
        }
        let payload = buf.get(header_len..)?;
        if openpgp_crypto::sha256(payload) != payload_digest {
            return None;
        }
        let index = PackageIndex::from_compact_bytes(payload)?;
        Some((index, size))
    })();

    match parsed {
        Some((index, bytes)) => {
            crate::info!(
                "ppkg: index cache HIT ({} packages, {} bytes, key=sha256:{})",
                index.len(),
                bytes,
                hex32(release_digest)
            );
            Some((index, bytes))
        }
        None => {
            crate::info!("ppkg: index cache MISS (key/format/checksum mismatch)");
            None
        }
    }
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

    // W0: phase timers around the whole update. The result is unchanged; the
    // one `apt: timings cmd=update …` line is emitted on success and failure.
    let mut t = Timings::begin("update");
    let result = update_timed(&mut t);
    if let Ok(count) = result {
        t.count("pkgs", count as u64);
    }
    t.finish();
    result
}

/// The body of [`update_verified`], taking the phase timer so every early return
/// still reports what it measured.
fn update_timed(t: &mut Timings) -> Result<usize, AptOpError> {
    let cfg = config();
    let now = crate::arch::x86_64::linux::rtc::now_unix() as i64;

    // 1+2. Signed metadata, verified, parsed and policy-checked.
    let signed = fetch_signed_release(&cfg, now, t)?;
    let release = release_file::parse_release(&signed.text);
    check_release_policy(&cfg, &release, now)?;
    t.phase("release_policy_ms");

    // W3: if the signed Release is byte-identical to the one the cache was built
    // from, the parsed index can be read back instead of downloading,
    // decompressing and parsing `Packages`. This runs only AFTER the signature and
    // Release policy checks above, so the cache can never bypass them; a miss or
    // any inconsistency falls through to the normal verified path below.
    let release_digest = openpgp_crypto::sha256(&signed.text);
    let cache_enabled = index_cache_enabled();
    if cache_enabled {
        if let Some((index, bytes)) = load_index_cache(&cfg, &release_digest) {
            t.phase("cache_load_ms");
            t.count("cache_bytes", bytes as u64);
            let count = index.len();
            *INDEX.lock() = Some(index);
            crate::info!("ppkg: index ready - {} packages (cache)", count);
            return Ok(count);
        }
    }
    t.phase("cache_load_ms");

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
            "ppkg: Get {}://{}{} [{}/{}/{}]",
            cfg.scheme(),
            cfg.host,
            url,
            cfg.suite,
            cfg.component,
            cfg.arch
        );
        let fetched = cfg.fetch(url);
        t.phase("index_fetch_ms");
        match fetched {
            Ok(bytes) => {
                // 4. The signed Release is the authority for these bytes: check
                // them BEFORE any decoding. A mismatch is fatal for the whole
                // update (see the doc comment).
                verify_index_body(path, &bytes, entry)?;
                t.phase("index_verify_ms");
                crate::info!(
                    "ppkg: fetched {} index body - decompressing...",
                    human_bytes(bytes.len() as u64)
                );
                // A successful download that fails to *decode* is no longer fatal
                // for the whole update: fall through and try the next index
                // variant (different bytes AND a different decoder).
                let index = match stream_parse_index(&bytes, *comp) {
                    Ok(index) => index,
                    Err(e) => {
                        t.phase("index_parse_ms");
                        crate::warn!("ppkg: decode of {} failed - trying next index variant", url);
                        last_decode_err = Some(e);
                        continue;
                    }
                };
                t.phase("index_parse_ms");
                // W3: persist the parsed index for the next update, keyed by the
                // signed Release digest + this configuration. Failure to cache is
                // only a warning: the update itself already succeeded.
                if cache_enabled {
                    if let Ok(bytes) = store_index_cache(&cfg, &release_digest, &index) {
                        t.count("cache_store_bytes", bytes as u64);
                    }
                }
                t.phase("cache_store_ms");
                let count = index.len();
                *INDEX.lock() = Some(index);
                crate::info!("ppkg: index ready - {} packages", count);
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
    crate::info!("ppkg: parsing index (takes a few minutes for the full Debian index; progress every 4 MiB)...");
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
                "ppkg: reading package lists... {} / {} pkgs",
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
                "ppkg: read package lists - {} / {} pkgs",
                human_bytes(decompressed as u64),
                builder.len()
            );
            Ok(PackageIndex::from_builder(builder))
        }
        Err(_) => {
            crate::warn!(
                "ppkg: index decode failed after {} decompressed / {} pkgs parsed",
                human_bytes(decompressed as u64),
                builder.len()
            );
            Err(AptOpError::IndexTooLarge)
        }
    }
}

/// Pass 1 of the streaming `.deb` install: decompress `data.tar` **once** and
/// collect member metadata only (no content, no `Vec<TarEntry>`).
///
/// Bounded by [`deb::MAX_DECOMPRESSED`] like any other `.deb` member decode; every
/// member is validated by the same panic-free [`tar::TarWalker`] rules as
/// [`tar::read_tar`] (checksum, size, truncation, GNU/pax extensions).
fn scan_deb_metadata(
    member: &deb::ArMember<'_>,
    comp: Compression,
    pkg: &str,
    observer: &mut dyn InstallObserver,
) -> Result<Vec<tar::TarMeta>, AptOpError> {
    struct Collect(Vec<tar::TarMeta>);
    impl tar::TarVisitor for Collect {
        fn on_entry(&mut self, meta: &tar::TarMeta) -> Result<(), tar::TarError> {
            self.0.push(meta.clone());
            Ok(())
        }
        fn on_data(&mut self, _chunk: &[u8]) -> Result<(), tar::TarError> {
            Ok(())
        }
    }

    let mut sink = Collect(Vec::new());
    let mut walker = tar::TarWalker::new();
    let mut walk_error: Option<tar::TarError> = None;
    // Progress while the archive is scanned: the unpacked total is not known yet,
    // so the UI shows its unknown-total animation. One update per 256 KiB keeps
    // the observer overhead invisible next to the decode itself.
    let mut scanned: u64 = 0;
    let mut next_tick: u64 = 256 * 1024;
    let decoded = deb::decompress_stream(member.data, comp, deb::MAX_DECOMPRESSED, |chunk| {
        match walker.push(chunk, &mut sink) {
            Ok(()) => {
                scanned += chunk.len() as u64;
                if scanned >= next_tick {
                    observer.on_unpack_progress(pkg, scanned, 0);
                    while scanned >= next_tick {
                        next_tick += 256 * 1024;
                    }
                    // Interleave with the rest of the system: the scan is a long
                    // CPU-bound decode and the observer call above is the point
                    // where no ext2 transaction is held.
                    crate::task::scheduler::cooperative_checkpoint();
                }
                Ok(())
            }
            Err(e) => {
                walk_error = Some(e);
                // Abort the decode; the walker error is the real cause and is
                // reported below.
                Err(deb::DebError::DecompressFailed)
            }
        }
    });
    if let Some(e) = walk_error {
        crate::warn!("ppkg: data.tar scan failed pkg={} err={:?}", pkg, e);
        return Err(AptOpError::Parse {
            pkg: pkg.to_string(),
        });
    }
    if decoded.is_err() {
        crate::warn!(
            "ppkg: data.tar decode failed pkg={} (cap {} bytes)",
            pkg,
            deb::MAX_DECOMPRESSED
        );
        return Err(AptOpError::Parse {
            pkg: pkg.to_string(),
        });
    }
    if let Err(e) = walker.finish(&mut sink) {
        crate::warn!("ppkg: data.tar truncated pkg={} err={:?}", pkg, e);
        return Err(AptOpError::Parse {
            pkg: pkg.to_string(),
        });
    }
    Ok(sink.0)
}

/// Pass 2 of the streaming `.deb` install: decompress `data.tar` again and write
/// each planned member as its bytes arrive (no whole-archive buffer).
///
/// The tar walker enforces the archive structure and the declared sizes; this
/// driver only maps each accepted member ordinal onto the preflight-validated
/// [`install_fs::StreamingInstall`]. A filesystem refusal is carried out through
/// [`tar::TarError::VisitorFailure`] (the walker has no filesystem error type) and
/// unwrapped into [`AptOpError::Install`], exactly where the eager path mapped
/// [`install_fs::InstallError`].
fn extract_deb_stream(
    member: &deb::ArMember<'_>,
    comp: Compression,
    pkg: &str,
    unpack_total: u64,
    install: &mut install_fs::StreamingInstall,
    observer: &mut dyn InstallObserver,
) -> Result<(), AptOpError> {
    struct Sink<'a> {
        install: &'a mut install_fs::StreamingInstall,
        observer: &'a mut dyn InstallObserver,
        pkg: &'a str,
        unpack_total: u64,
        written: u64,
        next_tick: u64,
        error: Option<install_fs::InstallError>,
    }
    impl tar::TarVisitor for Sink<'_> {
        fn on_entry(&mut self, meta: &tar::TarMeta) -> Result<(), tar::TarError> {
            if self.error.is_some() {
                return Ok(());
            }
            match self.install.accept_entry(meta) {
                Ok(()) => Ok(()),
                Err(e) => {
                    self.error = Some(e);
                    Err(tar::TarError::VisitorFailure)
                }
            }
        }
        fn on_data(&mut self, chunk: &[u8]) -> Result<(), tar::TarError> {
            if self.error.is_some() {
                return Ok(());
            }
            match self.install.accept_chunk(chunk) {
                Ok(()) => {
                    self.written = self.written.saturating_add(chunk.len() as u64);
                    if self.written >= self.next_tick {
                        self.observer
                            .on_unpack_progress(self.pkg, self.written, self.unpack_total);
                        while self.written >= self.next_tick {
                            self.next_tick += 256 * 1024;
                        }
                        // The write just returned without holding an ext2 lock;
                        // let a waiting compositor draw a frame mid-unpack.
                        crate::task::scheduler::cooperative_checkpoint();
                    }
                    Ok(())
                }
                Err(e) => {
                    self.error = Some(e);
                    Err(tar::TarError::VisitorFailure)
                }
            }
        }
    }

    let mut sink = Sink {
        install,
        observer,
        pkg,
        unpack_total,
        written: 0,
        next_tick: 256 * 1024,
        error: None,
    };
    let mut walker = tar::TarWalker::new();
    let mut walk_error: Option<tar::TarError> = None;
    let decoded =
        deb::decompress_stream(
            member.data,
            comp,
            deb::MAX_DECOMPRESSED,
            |chunk| match walker.push(chunk, &mut sink) {
                Ok(()) => Ok(()),
                Err(e) => {
                    walk_error = Some(e);
                    Err(deb::DebError::DecompressFailed)
                }
            },
        );
    // The visitor's own error is the most specific outcome: unwrap it before the
    // sentinel it produced and before generic decode errors.
    if let Some(e) = sink.error.take() {
        crate::debug!("ppkg: streaming install failed pkg={} err={}", pkg, e);
        return Err(AptOpError::Install {
            pkg: pkg.to_string(),
        });
    }
    if let Some(e) = walk_error {
        crate::warn!("ppkg: data.tar walk failed pkg={} err={:?}", pkg, e);
        return Err(AptOpError::Parse {
            pkg: pkg.to_string(),
        });
    }
    if decoded.is_err() {
        crate::warn!(
            "ppkg: data.tar decode failed pkg={} (cap {} bytes)",
            pkg,
            deb::MAX_DECOMPRESSED
        );
        return Err(AptOpError::Parse {
            pkg: pkg.to_string(),
        });
    }
    if let Err(e) = walker.finish(&mut sink) {
        crate::warn!("ppkg: data.tar truncated pkg={} err={:?}", pkg, e);
        return Err(AptOpError::Parse {
            pkg: pkg.to_string(),
        });
    }
    Ok(())
}

/// A phase of one package's installation, reported to an [`InstallObserver`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InstallPhase {
    /// Fetching the `.deb` (the transport draws its own byte bar meanwhile).
    Download,
    /// Checking the signed index digest/size before unpacking.
    Verify,
    /// Planning the archive and writing its members.
    Unpack,
}

/// Progress/notification interface for the shell's install UI
/// (`ppkg install`, see `shell::progress`).
///
/// `apt` drives the whole transaction synchronously, so the observer is a plain
/// `&mut dyn` call-back channel — no threads, no polling and no change to the
/// install logic or its ordering. Every method has a default no-op, so a caller
/// that only cares about a subset (or nothing, via [`install`]) implements none.
pub trait InstallObserver {
    /// The resolver produced the transaction plan: `(package, declared .deb size)`.
    fn on_plan(&mut self, _packages: &[(String, u64)]) {}
    /// A package entered `phase`.
    fn on_phase(&mut self, _pkg: &str, _phase: InstallPhase) {}
    /// Bytes written while unpacking `pkg`; `total == 0` while the plan (and so
    /// the unpacked size) is still being discovered.
    fn on_unpack_progress(&mut self, _pkg: &str, _done: u64, _total: u64) {}
    /// A package finished; `files` entries were created.
    fn on_package_done(&mut self, _pkg: &str, _files: usize) {}
}

/// The no-op observer behind the programmatic [`install`] API.
struct NullObserver;
impl InstallObserver for NullObserver {}

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
    let mut observer = NullObserver;
    install_observed(name, &mut observer)
}

/// [`install`] with a progress observer (the `ppkg install` UI).
///
/// Identical transaction, resolution, verification and install semantics; the
/// observer only receives call-backs. A [`NullObserver`] is exactly [`install`].
pub fn install_observed(
    name: &str,
    observer: &mut dyn InstallObserver,
) -> Result<Vec<String>, AptOpError> {
    #[cfg(not(feature = "insecure_network_demo"))]
    {
        let _ = observer;
        return Err(AptOpError::NetworkDisabled);
    }

    // W0: phase timers for the whole transaction; one `ppkg: timings cmd=install`
    // line on success and failure alike.
    let mut t = Timings::begin("install");
    let result = install_timed(name, &mut t, observer);
    if let Ok(installed) = &result {
        t.count("pkgs", installed.len() as u64);
    }
    t.finish();
    result
}

/// One resolved package of an install transaction: the pool filename plus the
/// digest and size the **same** signed stanza declares for it.
struct Target {
    pkg: String,
    filename: String,
    sha256: Option<[u8; 32]>,
    size: u64,
}

/// The body of [`install`], taking the phase timer so every early return still
/// reports what it measured.
fn install_timed(
    name: &str,
    t: &mut Timings,
    observer: &mut dyn InstallObserver,
) -> Result<Vec<String>, AptOpError> {
    let cfg = config();

    // Snapshot the session installed-set for the resolver.
    let already = INSTALLED.lock().clone();

    // Plan the transaction and capture, for each package, the pool filename AND
    // the digest the signed index declares for it — from the SAME record, so the
    // URL and the digest can never come from different stanzas. The index lock is
    // held only for this short, network-free span.
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

    t.phase("resolve_ms");
    let total = targets.len();
    if total > 0 {
        let plan: Vec<&str> = targets.iter().map(|t| t.pkg.as_str()).collect();
        crate::info!(
            "ppkg: {} new package(s) to install: {}",
            total,
            plan.join(" ")
        );
        let sizes: Vec<(String, u64)> = targets
            .iter()
            .map(|target| (target.pkg.clone(), target.size))
            .collect();
        observer.on_plan(&sizes);
    }

    // W2: one `sync` for the whole transaction. `wrote_any` is conservative: set
    // before a package's install step, so an abort after partial writes still
    // syncs once on the way out. ext2 journals every mutation at `commit` and its
    // `sync()` is a documented no-op today; this is nonetheless the correct
    // transaction boundary and keeps crash-consistency semantics identical.
    let mut wrote_any = false;
    let mut failure: Option<AptOpError> = None;
    let mut installed: Vec<String> = Vec::new();

    for (i, target) in targets.into_iter().enumerate() {
        let step = i + 1;
        let pkg = target.pkg.clone();
        wrote_any = true;
        match install_target(&cfg, target, step, total, t, observer) {
            Ok(n) => {
                t.count("files", n as u64);
                INSTALLED.lock().insert(pkg.clone());
                installed.push(pkg);
            }
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }

    if wrote_any {
        if let Ok(node) = crate::vfs::lookup_path("/mnt") {
            node.sync();
        }
    }
    t.phase("sync_ms");

    if let Some(e) = failure {
        return Err(e);
    }
    Ok(installed)
}

/// One package of an `apt install` transaction: fetch, verify against the signed
/// index, then the two-pass streaming install (see [`install`]).
///
/// Returns the number of entries the installer created. Every phase is charged to
/// `t`; `step`/`total` only shape the log lines.
fn install_target(
    cfg: &AptConfig,
    target: Target,
    step: usize,
    total: usize,
    t: &mut Timings,
    observer: &mut dyn InstallObserver,
) -> Result<usize, AptOpError> {
    let pkg = target.pkg;
    let filename = target.filename;
    if filename.is_empty() {
        return Err(AptOpError::Download { pkg: pkg.clone() });
    }
    // The payload is bound to the signed metadata through this digest. A stanza
    // without one cannot be verified, so it is refused instead of installed on
    // faith.
    let Some(expected_digest) = target.sha256 else {
        verify_fail("deb", "DigestUnavailable", &format!(" pkg={}", pkg));
        return Err(AptOpError::DigestUnavailable { pkg });
    };
    let url = format!("{}/{}", cfg.base, filename);
    crate::info!(
        "ppkg: [{}/{}] Get {} <- {}://{}{}",
        step,
        total,
        pkg,
        cfg.scheme(),
        cfg.host,
        url
    );

    observer.on_phase(&pkg, InstallPhase::Download);
    let fetched = cfg.fetch(&url).map_err(|e| match e {
        crate::net::http_fetch::FetchError::NoNetwork => AptOpError::NoNetwork,
        _ => AptOpError::Download { pkg: pkg.clone() },
    });
    t.phase("fetch_ms");
    let bytes = fetched?;
    let dl = bytes.len();
    t.count("deb_bytes", dl as u64);

    // Signature -> Release -> Packages -> .deb: the digest from the signed index
    // is checked BEFORE the payload is parsed, decompressed or written to ext2.
    // Nothing below this line sees unverified bytes.
    observer.on_phase(&pkg, InstallPhase::Verify);
    verify_package_body(&pkg, &bytes, &expected_digest, target.size)?;
    t.phase("verify_ms");

    let members = deb::parse_ar(&bytes).map_err(|_| AptOpError::Parse { pkg: pkg.clone() })?;
    let deb_members =
        deb::locate_members(&members).map_err(|_| AptOpError::Parse { pkg: pkg.clone() })?;
    let comp = deb::compression_of(deb_members.data.name)
        .map_err(|_| AptOpError::Parse { pkg: pkg.clone() })?;
    t.phase("ar_ms");
    // W1: two-pass streaming install. Pass 1 decompresses `data.tar` into metadata
    // only and plans the install; the security planner's verdict over that metadata
    // is what authorizes any write. Pass 2 decompresses again and writes each
    // planned member as its bytes stream past, so neither the decompressed tar nor
    // a `Vec<TarEntry>` is ever resident.
    observer.on_phase(&pkg, InstallPhase::Unpack);
    let metas = scan_deb_metadata(&deb_members.data, comp, &pkg, observer)?;
    t.phase("scan_ms");
    let plan = plan_install_meta(&metas);
    // The unpacked-byte total is now known: sum of every planned file member.
    let unpack_total: u64 = plan
        .ops
        .iter()
        .map(|op| match op {
            super::install::MetaOp::File { size, .. } => *size,
            _ => 0,
        })
        .sum();
    observer.on_unpack_progress(&pkg, 0, unpack_total);
    t.phase("plan_ms");
    // The plan is fully validated (F-x86n-1/F-r3-vfs-1/F-r4-vfs-1) before the
    // first inode — preflight atomicity is unchanged by streaming.
    let mut install = install_fs::StreamingInstall::prepare(&plan, "/mnt")
        .map_err(|_| AptOpError::Install { pkg: pkg.clone() })?;
    extract_deb_stream(
        &deb_members.data,
        comp,
        &pkg,
        unpack_total,
        &mut install,
        observer,
    )?;
    t.phase("extract_ms");
    let n = install
        .finish()
        .map_err(|_| AptOpError::Install { pkg: pkg.clone() })?;
    t.phase("link_ms");

    observer.on_package_done(&pkg, n);
    crate::info!(
        "ppkg: [{}/{}] Unpacked {} ({} files, {})",
        step,
        total,
        pkg,
        n,
        human_bytes(dl as u64)
    );
    Ok(n)
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
