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
    /// **INSECURE (VARIANT A):** when set, downloads go through
    /// [`net::tls::https_get`](crate::net::tls::https_get), which encrypts but does
    /// **not** verify the server certificate (no chain/hostname/expiry checks). The
    /// default mirror ships with this enabled so the out-of-box experience is
    /// HTTPS; an insecure-transport warning is logged on first use.
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
    /// HTTPS ([`net::tls::https_get`](crate::net::tls::https_get), INSECURE — no
    /// cert verification) or cleartext HTTP
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
/// NOTE: enabling HTTPS selects the **INSECURE** TLS path (no certificate
/// verification); a warning is logged on first download.
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
}

impl AptOpError {
    /// A human-readable, single-line message for the shell.
    pub fn message(&self) -> String {
        match self {
            AptOpError::NoNetwork => "no network - check `ifconfig`".to_string(),
            AptOpError::NoIndex => "no package index - run `apt update` first".to_string(),
            AptOpError::NotFound(n) => format!("package '{}' not found in index", n),
            AptOpError::Download { pkg } => format!("download failed for '{}'", pkg),
            AptOpError::Parse { pkg } => format!("could not parse package '{}'", pkg),
            AptOpError::IndexTooLarge => {
                "index too large for available RAM — use a smaller component or a \
                 local mirror (e.g. `apt setmirror http://10.0.2.2 /debian`)"
                    .to_string()
            }
            AptOpError::Install { pkg } => format!("install failed for '{}'", pkg),
        }
    }
}

/// Download and parse the repository `Packages` index into RAM, returning the
/// number of package records loaded.
///
/// Builds the index URL `{base}/dists/{suite}/{component}/binary-{arch}/Packages.gz`
/// and `GET`s it; if that download fails (e.g. the mirror 404s that compression),
/// it falls back to `.xz`, then to the uncompressed `Packages`.
///
/// ## Streaming, bounded-memory pipeline
///
/// The fetched body is **never** decompressed into one giant buffer (the old
/// "decompress the whole ~150 MiB index to a `Vec`, then parse" path that overran
/// the heap and looked like a hang). Instead the compressed body is decompressed
/// in fixed 64 KiB chunks ([`deb::decompress_stream`]) and each chunk is fed into
/// an incremental [`StanzaParser`], which emits one [`PkgRecord`] per completed
/// stanza and drops the chunk. Resident memory is therefore roughly the
/// compressed body (~10 MiB) + small decode/line buffers + the parsed in-RAM
/// index — there is no large intermediate. Progress is logged periodically so a
/// long index visibly advances instead of appearing hung. A runaway stream is
/// bounded by [`deb::MAX_INDEX_STREAM_BYTES`] and fails cleanly (no OOM abort).
pub fn update() -> Result<usize, AptOpError> {
    let cfg = config();
    let dir = format!(
        "{}/dists/{}/{}/binary-{}",
        cfg.base, cfg.suite, cfg.component, cfg.arch
    );

    // Preference order: gzip first (faster to decode at full-index scale), then
    // xz, then uncompressed.
    let candidates: [(String, Compression); 3] = [
        (format!("{}/Packages.gz", dir), Compression::Gzip),
        (format!("{}/Packages.xz", dir), Compression::Xz),
        (format!("{}/Packages", dir), Compression::None),
    ];

    let mut saw_no_network = false;

    for (url, comp) in candidates.iter() {
        crate::info!("apt: fetching index {}://{}{}", cfg.scheme(), cfg.host, url);
        match cfg.fetch(url) {
            Ok(bytes) => {
                // A successful download that fails to decompress/parse is a hard
                // error for this candidate; do not silently fall through.
                let index = stream_parse_index(&bytes, *comp)?;
                let count = index.len();
                *INDEX.lock() = Some(index);
                crate::info!("apt: index loaded ({} packages)", count);
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

    if saw_no_network {
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
                "apt: decompressed {} KiB, parsed {} packages...",
                decompressed / 1024,
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
                "apt: decompressed {} KiB, parsed {} packages",
                decompressed / 1024,
                builder.len()
            );
            Ok(PackageIndex::from_builder(builder))
        }
        Err(_) => Err(AptOpError::IndexTooLarge),
    }
}

/// Resolve and install `name` (and its not-yet-installed dependencies) from the
/// loaded index, returning the package names installed in dependency-first order.
///
/// Requires [`update`] to have been run (else [`AptOpError::NoIndex`]). The plan
/// is computed by [`resolve_install`] against a snapshot of the session
/// installed-set; each planned package's `.deb` is then fetched from
/// `{base}/{filename}`, parsed, decompressed, and written onto ext2 under `/mnt`.
/// Each successfully installed name is recorded in [`struct@INSTALLED`].
pub fn install(name: &str) -> Result<Vec<String>, AptOpError> {
    let cfg = config();

    // Snapshot the session installed-set for the resolver.
    let already = INSTALLED.lock().clone();

    // Plan the transaction and capture (name, pool filename) for each package,
    // holding the index lock only for this short, network-free span.
    let targets: Vec<(String, String)> = {
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
                t.push((rec.package().to_string(), rec.filename().to_string()));
            }
        }
        t
    };

    let mut installed: Vec<String> = Vec::new();

    for (pkg, filename) in targets {
        if filename.is_empty() {
            return Err(AptOpError::Download { pkg });
        }
        let url = format!("{}/{}", cfg.base, filename);
        crate::info!("apt: fetching {} <- {}://{}{}", pkg, cfg.scheme(), cfg.host, url);

        let bytes = cfg.fetch(&url).map_err(|e| {
            match e {
                crate::net::http_fetch::FetchError::NoNetwork => AptOpError::NoNetwork,
                _ => AptOpError::Download { pkg: pkg.clone() },
            }
        })?;

        let members =
            deb::parse_ar(&bytes).map_err(|_| AptOpError::Parse { pkg: pkg.clone() })?;
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
        crate::info!("apt: installed {} ({} files)", pkg, n);
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
