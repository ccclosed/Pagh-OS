//! HTTPS over TLS 1.3 for the package manager — **fail-closed server
//! authentication** (issue #14).
//!
//! Every handshake authenticates the peer before a single HTTP byte is sent:
//!
//!   * the server certificate chain is validated down to a committed trust
//!     anchor ([`super::ca_bundle::CA_BUNDLE`]) — signatures, `cA=TRUE` on
//!     every issuer, validity windows ([`super::tls_chain::verify_chain`]);
//!   * the connection target is authorized against the leaf's SAN entries
//!     (RFC 6125, DNS names and IP literals; no CN fallback)
//!     ([`super::tls_auth::authenticate_server`]);
//!   * the system clock is gated: an unset RTC (below 2025-01-01) refuses the
//!     handshake rather than validating against a bogus "now"
//!     ([`super::tls_chain::CLOCK_FLOOR`]);
//!   * the handshake's `CertificateVerify` signature is checked against the
//!     verified leaf key ([`super::tls_auth::verify_certificate_verify`]).
//!     The transcript hash it signs is the CIPHER SUITE's hash
//!     ([`CIPHER_SUITE_HASH_LEN`]); the signature scheme only picks the digest
//!     the peer signed with, so SHA-384/512 schemes over this SHA-256 suite
//!     stay verifiable instead of becoming dead backends.
//!
//! Any failure aborts the handshake — there is no fallback path that keeps
//! the connection. The session RNG is backed by RDSEED/RDRAND and the
//! connection likewise fails closed when hardware entropy is unavailable.
//!
//! ## Where the diagnostics are
//!
//! The verifier reports every rejection through `error!` under the stable
//! marker `Package_Fetcher(tls): stage=verify cause=…` (`InvalidCertificate`,
//! `InvalidSignatureScheme`, `InvalidSignature`). `embedded-tls`'s own
//! handshake messages are `debug!`-level and do NOT reach the log: the facade
//! defaults to `Info`, which filters `debug!`/`trace!` out. Those `error!`
//! lines — not any embedded-tls output — are therefore the whole verifier
//! diagnostic surface, and what the smoke/E2E assertions grep.
//!
//! ## How it is wired (kernel-only module)
//!
//! `embedded-tls` is async (`embedded-io-async`). We do not have an async runtime,
//! so three small pieces bridge it onto the kernel's own TCP stack:
//!
//!   1. [`block_on`] — a minimal executor: it polls a pinned future in a loop with
//!      a no-op waker until it is `Ready`, spinning briefly between polls so the
//!      timer tick and QEMU can advance.
//!   2. [`TlsTransport`] — implements [`embedded_io_async::Read`]/[`Write`] over a
//!      TCP socket handle from the own stack. Each poll takes the `NET` lock,
//!      advances the stack once, then drains/fills the socket. It NEVER holds
//!      the `NET` lock across an `await`, mirrors the bounded locked-step +
//!      `spin_loop` discipline of `nc_echo`/`http_get`, and carries an
//!      inactivity budget so a stall returns an error instead of hanging.
//!   3. [`KernelRng`] — adapts the fail-closed hardware entropy API to the
//!      `RngCore + CryptoRng` traits required by `embedded-tls`.
//!
//! [`KernelProvider`] supplies both that RNG and [`KernelVerifier`], the
//! `embedded-tls` `TlsVerifier` implementation that adapts the handshake's
//! borrowed certificate entries and negotiated signature scheme onto the pure
//! decision layer in [`super::tls_auth`]. Because it always returns a
//! verifier, `embedded-tls`'s "no verifier ⇒ skip verification" path can
//! never be taken.
//!
//! [`https_get`] ties them together: resolve → connect → handshake → HTTP GET →
//! collect the `Content-Length` body, reusing the pure
//! [`build_get_request`](super::http::build_get_request) /
//! [`parse_http_head`](super::http::parse_http_head) for all wire formatting.
//!
//! ## Large streams (issue #19)
//!
//! #19 reported a deterministic `embedded-tls` stall "at ~12 MiB". It is **not
//! reproducible** on the current TCP stack: the 13,332,733-byte
//! `Packages.gz` of `stable/main/amd64` is decrypted end to end in one fetch
//! (≈2.1 MB/s under QEMU/TCG), and the live `apt update` completes over HTTPS
//! with the full ~13 MB index. The claim dates from the smoltcp-backed stack —
//! the transport code has been unchanged since, but the stack underneath it was
//! replaced (`net: replace smoltcp…`) and the window-update-ACK fix (x40)
//! landed in the same series; the live harness kept forcing cleartext HTTP
//! afterwards, so nobody re-measured.
//!
//! What this module keeps from that investigation, because it is what turns a
//! mystery stall into a named one:
//!
//!   * the byte-trace counters ([`TLS_RAW_IN`], [`TLS_READ_POLLS`],
//!     [`TLS_READ_PENDING`]) and the `stage=done` completion line, so every fetch
//!     is measurable from the serial log;
//!   * the `stage=stall` watchdog: if the decrypted body stops growing for
//!     [`TLS_STALL_REPORT_TICKS`], one line names the stuck layer — socket (no
//!     `raw_in` growth), transport (polls grow, `raw_in` does not) or record layer
//!     (both grow, the body does not) — together with the TCP flow-control
//!     snapshot from [`super::tcp::TcpSock::diag`];
//!   * the read-error diagnostic at the end of the stream, which preserves the
//!     `TlsError` and the exact shortfall instead of collapsing everything into a
//!     bare `FetchError::Incomplete`.
//!
//! The regression pin is `lx_tlsbig` (`selftest_lx::run_tls_big_check`): one
//! multi-MB GET against the same object, before any decompress/parse work.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use embedded_io::ErrorKind;
use embedded_tls::{
    Aes128GcmSha256, CertificateEntryRef, CertificateRef, CertificateVerifyRef, CryptoProvider,
    SignatureScheme, TlsConfig, TlsConnection, TlsContext, TlsError, TlsVerifier,
};

use crate::net::{IpEndpoint, SocketHandle};

use super::http::{build_get_request, parse_http_head, HeadParse};
use super::http_fetch::FetchError;
use super::tls_auth::{authenticate_server, verify_certificate_verify, AuthError, LeafKey};
use super::tls_chain::TrustAnchor;
use super::tls_verify::Tls13Scheme;
use super::NET;
use crate::task::scheduler;
use crate::{error, warn};

/// Default HTTPS port.
pub const HTTPS_PORT: u16 = 443;

/// Width of the handshake **transcript hash** the verifier consumes: the hash
/// of the negotiated CIPHER SUITE, [`Aes128GcmSha256`] ⇒ SHA-256 ⇒ 32.
///
/// NOT the scheme's hash width (RFC 8446 §4.4.3): `ecdsa_secp384r1_sha384`
/// signs a SHA-384 digest *of this 32-byte value*, so a SHA-384-scheme leaf is
/// verifiable on this suite. Comparing the transcript hash against the scheme's
/// width — the earlier bug — rejected every P-384 / RSA-PSS-384 leaf as
/// `InvalidSignature` even though the signature was good.
///
/// The compile-time assertion below keeps this constant tied to the suite
/// actually instantiated here, so swapping `Aes128GcmSha256` for a SHA-384
/// suite cannot silently leave a stale width behind (`KernelVerifier` is
/// generic over the suite via [`CryptoProvider::CipherSuite`], so such a swap
/// would otherwise still compile).
const CIPHER_SUITE_HASH_LEN: usize = 32;
const _: () = assert!(
    CIPHER_SUITE_HASH_LEN
        == <<<Aes128GcmSha256 as embedded_tls::TlsCipherSuite>::Hash as sha2::digest::OutputSizeUser>::OutputSize as sha2::digest::typenum::Unsigned>::USIZE,
    "CIPHER_SUITE_HASH_LEN must match the negotiated cipher suite's hash width"
);

/// Connect timeout (~3 s), matching `http_get`.
const CONNECT_TIMEOUT_TICKS: u64 = crate::arch::x86_64::apic::ms_to_ticks(3_000);
/// Inactivity (idle) timeout for the TLS transport (~15 s). It is
/// re-armed on every byte sent or received, so a slow-but-progressing handshake or
/// download over QEMU NAT is not killed — only a genuine stall aborts.
const TLS_IDLE_TIMEOUT_TICKS: u64 = crate::arch::x86_64::apic::ms_to_ticks(15_000);
/// TLS record buffer size. 16 KiB+ is the safe maximum for any TLS 1.3 record.
const RECORD_BUF: usize = 16 * 1024 + 256;
/// Stall-report threshold for [`https_get`]: if the DECRYPTED body does not grow
/// for this long, emit one `stage=stall` diagnostic with the byte counters and a
/// socket snapshot (issue #19). A genuinely progressing-but-slow download never
/// trips it; a stop — which is what #19 was — names its own suspect immediately
/// instead of looking like a silent hang until the transport's idle budget fires.
const TLS_STALL_REPORT_TICKS: u64 = crate::arch::x86_64::apic::ms_to_ticks(5_000);
/// rx/tx TCP socket buffer sizes for the TLS connection.
///
/// THROUGHPUT (#1 lever): over QEMU user-net NAT the achievable bandwidth is
/// bounded by `window / RTT` (bandwidth-delay product). The old 16 KiB receive
/// window capped a ~10 MiB `Packages.gz` to roughly one 16 KiB window per RTT.
/// We raise rx to 256 KiB so the stack negotiates TCP window scaling and many
/// more bytes are in flight per round trip. The heap is
/// 256 MiB, so a 256 KiB per-connection buffer is comfortably affordable. tx is
/// kept modest (16 KiB): we only ever send a small GET request.
const TLS_RX_BYTES: usize = 256 * 1024;
const TLS_TX_BYTES: usize = 16 * 1024;
/// Upper bound on the decrypted HTTP response we will buffer.
const MAX_TOTAL: usize = 32 * 1024 * 1024;

// ───────────────────── hardware-backed TLS RNG ─────────────────────

/// Adapter from the kernel's fail-closed RDSEED/RDRAND entropy source to the
/// `rand_core` traits required by `embedded-tls`. `https_get` checks hardware
/// availability before allocating a socket; transient instruction failure after
/// that point is treated as a kernel invariant failure rather than substituting
/// predictable bytes.
struct KernelRng;

impl KernelRng {
    fn new() -> Self {
        KernelRng
    }
}

impl rand_core::RngCore for KernelRng {
    fn next_u32(&mut self) -> u32 {
        crate::security::entropy::secure_u64().expect("TLS hardware entropy disappeared") as u32
    }

    fn next_u64(&mut self) -> u64 {
        crate::security::entropy::secure_u64().expect("TLS hardware entropy disappeared")
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        crate::security::entropy::fill(dest).expect("TLS hardware entropy disappeared");
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl rand_core::CryptoRng for KernelRng {}

// ─────────────────── certificate verification (issue #14) ───────────────────

/// Parsed trust anchors, built once per boot (see [`bundle_anchors`]).
static ANCHORS: spin::Mutex<Option<Arc<Vec<TrustAnchor<'static>>>>> = spin::Mutex::new(None);

/// The configured trust-anchor list from the committed CA bundle
/// ([`super::ca_bundle::CA_BUNDLE`], generated and pinned by
/// `tools/gen_ca_bundle.py`), parsed lazily and cached for the whole boot.
///
/// Each root is parsed with the kernel's own X.509 parser (property P48
/// proves every committed anchor parses and self-verifies). An anchor that
/// fails to parse is SKIPPED with a diagnostic rather than aborting the
/// whole handshake: skipping narrows the trust set, which can only turn a
/// would-be accept into a reject — exactly the fail-closed direction. The
/// alternative (one rotted bundle entry disabling HTTPS entirely) buys no
/// security.
///
/// WHY CACHED: the bundle is ≈28 KiB of DER across four self-signed roots, and
/// parsing it re-derives the subject `Name` and SPKI of every root on EVERY
/// handshake. Parsing once per boot is enough — the bytes are `include!`-ed
/// constants, and the anchors are immutable after construction. A poisoned or
/// empty cache is never a bypass: the worst case is the fail-closed
/// `ChainError::NoAnchor` reject below.
fn bundle_anchors() -> Arc<Vec<TrustAnchor<'static>>> {
    let mut cache = ANCHORS.lock();
    if let Some(anchors) = cache.as_ref() {
        return anchors.clone();
    }

    let mut out = Vec::with_capacity(super::ca_bundle::CA_BUNDLE.len());
    for (label, der) in super::ca_bundle::CA_BUNDLE.iter() {
        match super::x509::parse_certificate(der) {
            Ok((cert, rest)) if rest.is_empty() => out.push(TrustAnchor {
                subject: cert.subject,
                key: cert.spki.key,
            }),
            _ => error!(
                "net::tls: trust anchor {:?} in the CA bundle does not parse; skipping it \
                 (the bundle needs regeneration)",
                label
            ),
        }
    }

    // An empty anchor set makes EVERY handshake fail with `ChainError::NoAnchor`
    // — correct, but silent. Say why, once, at the moment it is discovered: the
    // operator otherwise sees only per-handshake "chain: NoAnchor" rejects with
    // no hint that no trust anchor was ever available (host property P48 catches
    // this in CI, but only a log line catches it in QEMU).
    if out.is_empty() {
        warn!(
            "net::tls: the committed CA bundle yielded no usable trust anchors — \
             every HTTPS handshake will be refused (ChainError::NoAnchor); check \
             net::ca_bundle / tools/gen_ca_bundle.py"
        );
    }

    let anchors = Arc::new(out);
    *cache = Some(anchors.clone());
    anchors
}

/// `embedded-tls` verifier driving the pure decision layer
/// ([`super::tls_auth`]) from the handshake.
///
/// State is owned, not borrowed: the certificate entries arrive as borrows of
/// the handshake's record buffers, so the leaf key and the transcript hash are
/// copied at `verify_certificate` time and used by `verify_signature` in the
/// same handshake. A verifier that was never given a certificate cannot
/// verify a signature ([`TlsError::InvalidCertificate`]), so the ordering
/// "certificate first, then CertificateVerify" is enforced, not assumed.
/// Counts completed server authentications, process-wide.
///
/// [`KernelVerifier::verify_certificate`] increments it only after the full
/// decision (chain → committed CA bundle, SAN hostname, validity + clock gate)
/// succeeded. [`https_get`] snapshots it before the handshake and requires the
/// value to have advanced before it will send a single application byte.
///
/// WHY THIS EXISTS ON TOP OF THE HANDSHAKE STATE MACHINE: a TLS 1.3 client must
/// reject a server `Finished` that was not preceded by `Certificate` and
/// `CertificateVerify` (RFC 8446 §4.4.2.4). `vendor/embedded-tls` did not
/// enforce that, so a peer could omit both messages, still reach
/// `ApplicationData`, and never invoke this verifier at all — the whole
/// fail-closed story silently bypassed. The vendored state machine is patched
/// to enforce it (`connection.rs`, `process_server_verify`, `certificate_received`
/// / `certificate_verified`), and this counter is the belt to that braces: it
/// makes "the handshake completed" and "a certificate was actually verified"
/// two separately checked facts at the kernel call site, so a future vendored
/// bump or a `vendor/` refresh cannot quietly restore the fail-open path.
static VERIFIED_HANDSHAKES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Snapshot of [`VERIFIED_HANDSHAKES`], taken before a handshake starts.
fn verified_handshakes() -> u64 {
    VERIFIED_HANDSHAKES.load(core::sync::atomic::Ordering::Acquire)
}

#[derive(Default)]
struct KernelVerifier {
    /// Connection target from `TlsConfig::server_name` — always set by
    /// [`https_get`]; `None` would fail closed inside `tls_auth`.
    host: Option<String>,
    /// Verified leaf key, set by `verify_certificate`.
    leaf_key: Option<LeafKey>,
    /// Handshake transcript hash at certificate time (the RFC 8446 §4.4.3
    /// signed message covers the transcript up to the certificate).
    transcript: Option<Vec<u8>>,
}

impl KernelVerifier {
    fn new() -> Self {
        KernelVerifier::default()
    }
}

impl TlsVerifier<Aes128GcmSha256> for KernelVerifier {
    fn set_hostname_verification(&mut self, hostname: &str) -> Result<(), TlsError> {
        self.host = Some(String::from(hostname));
        Ok(())
    }

    fn verify_certificate(
        &mut self,
        transcript: &<Aes128GcmSha256 as embedded_tls::TlsCipherSuite>::Hash,
        cert: CertificateRef,
    ) -> Result<(), TlsError> {
        // Collect the raw DER of every entry. A non-X.509 entry (raw public
        // key) is outside the authenticated surface — reject rather than
        // skip it, since the peer would otherwise choose which entries count.
        let mut entries: Vec<&[u8]> = Vec::with_capacity(cert.entries.len());
        for entry in cert.entries.iter() {
            match entry {
                CertificateEntryRef::X509(der) => entries.push(der),
                CertificateEntryRef::RawPublicKey(_) => {
                    error!(
                        "Package_Fetcher(tls): stage=verify cause=InvalidCertificate \
                         (raw public key entry is not supported)"
                    );
                    return Err(TlsError::InvalidCertificate);
                }
            }
        }

        let anchors = bundle_anchors();
        let now = crate::arch::x86_64::linux::rtc::now_unix() as i64;
        match authenticate_server(&entries, self.host.as_deref(), &anchors, now) {
            Ok(auth) => {
                self.leaf_key = Some(auth.leaf_key);
                use sha2::Digest;
                self.transcript = Some(transcript.clone().finalize().to_vec());
                // The chain/SAN/clock decision passed for THIS connection's
                // leaf: record it so `https_get` can tell "handshake completed
                // with a verified server" from "handshake completed".
                VERIFIED_HANDSHAKES.fetch_add(1, core::sync::atomic::Ordering::Release);
                Ok(())
            }
            Err(e) => {
                // One structured diagnostic per failure class: the chain
                // reason is what an operator needs to tell "mirror is
                // broken" from "clock is unset" from "wrong hostname".
                match e {
                    AuthError::Chain(c) => error!(
                        "Package_Fetcher(tls): stage=verify cause=InvalidCertificate (chain: {:?})",
                        c
                    ),
                    AuthError::NoHostname => error!(
                        "Package_Fetcher(tls): stage=verify cause=InvalidCertificate \
                         (no hostname configured for the connection)"
                    ),
                    AuthError::HostnameMismatch => error!(
                        "Package_Fetcher(tls): stage=verify cause=InvalidCertificate \
                         (certificate does not authorize the connection host)"
                    ),
                    AuthError::UnsupportedLeafKey => error!(
                        "Package_Fetcher(tls): stage=verify cause=InvalidCertificate \
                         (leaf public key algorithm is not supported)"
                    ),
                }
                Err(TlsError::InvalidCertificate)
            }
        }
    }

    fn verify_signature(&mut self, verify: CertificateVerifyRef) -> Result<(), TlsError> {
        // Both facts must come from THIS handshake's verified certificate.
        let leaf_key = match self.leaf_key.as_ref() {
            Some(k) => k,
            None => return Err(TlsError::InvalidCertificate),
        };
        let hash = match self.transcript.as_ref() {
            Some(h) => h,
            None => return Err(TlsError::InvalidCertificate),
        };
        let scheme = tls13_scheme(verify.signature_scheme).ok_or_else(|| {
            error!(
                "Package_Fetcher(tls): stage=verify cause=InvalidSignatureScheme \
                 ({:?} is not accepted for TLS 1.3 CertificateVerify)",
                verify.signature_scheme
            );
            TlsError::InvalidSignatureScheme
        })?;
        verify_certificate_verify(
            scheme,
            CIPHER_SUITE_HASH_LEN,
            hash,
            verify.signature,
            leaf_key,
        )
        .map_err(|e| {
            error!(
                "Package_Fetcher(tls): stage=verify cause=InvalidSignature (CertificateVerify: {:?})",
                e
            );
            TlsError::InvalidSignature
        })
    }
}

/// Map the negotiated TLS `SignatureScheme` onto the verifier's own enum.
///
/// `None` for everything the pure layer does not implement — `rsa_pkcs1_*`
/// (banned in TLS 1.3: their presence is a downgrade marker) and
/// PSS-with-PSS-key (`rsa_pss_pss_*`, an SPKI family the X.509 layer rejects).
/// Everything else stays REACHABLE, including the SHA-384/512 schemes: their
/// signature digest is independent of the SHA-256 transcript hash this suite
/// negotiates, so a P-384 or RSA-PSS-384/512 leaf verifies here instead of
/// failing closed on a width check (`CIPHER_SUITE_HASH_LEN` explains that
/// distinction).
fn tls13_scheme(scheme: SignatureScheme) -> Option<Tls13Scheme> {
    match scheme {
        SignatureScheme::EcdsaSecp256r1Sha256 => Some(Tls13Scheme::EcdsaSecp256r1Sha256),
        SignatureScheme::EcdsaSecp384r1Sha384 => Some(Tls13Scheme::EcdsaSecp384r1Sha384),
        SignatureScheme::Ed25519 => Some(Tls13Scheme::Ed25519),
        SignatureScheme::RsaPssRsaeSha256 => Some(Tls13Scheme::RsaPssRsaeSha256),
        SignatureScheme::RsaPssRsaeSha384 => Some(Tls13Scheme::RsaPssRsaeSha384),
        SignatureScheme::RsaPssRsaeSha512 => Some(Tls13Scheme::RsaPssRsaeSha512),
        _ => None,
    }
}

/// The `embedded-tls` crypto provider: hardware entropy + the fail-closed
/// verifier. `verifier()` always returns `Ok`, so `embedded-tls`'s
/// "no verifier ⇒ skip certificate and signature verification" branch cannot
/// be reached on this path.
struct KernelProvider {
    rng: KernelRng,
    verifier: KernelVerifier,
}

impl KernelProvider {
    fn new() -> Self {
        KernelProvider {
            rng: KernelRng::new(),
            verifier: KernelVerifier::new(),
        }
    }
}

impl CryptoProvider for KernelProvider {
    type CipherSuite = Aes128GcmSha256;
    type Signature = p256::ecdsa::DerSignature;

    fn rng(&mut self) -> impl rand_core::CryptoRngCore {
        &mut self.rng
    }

    fn verifier(&mut self) -> Result<&mut impl TlsVerifier<Self::CipherSuite>, TlsError> {
        Ok(&mut self.verifier)
    }
}

// ─────────────────────────── minimal executor ───────────────────────────

/// Build a no-op `RawWaker`: `block_on` re-polls unconditionally, so waking is a
/// no-op (the waker exists only to satisfy `Context`).
fn noop_raw_waker() -> RawWaker {
    fn no_op(_: *const ()) {}
    fn clone(_: *const ()) -> RawWaker {
        noop_raw_waker()
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
    RawWaker::new(core::ptr::null(), &VTABLE)
}

/// Minimal single-future executor: poll `fut` to completion with a no-op waker,
/// spinning briefly between `Pending` polls so the timer tick and QEMU can
/// advance. The transport adapter pumps the stack inside each of its own polls, so
/// re-polling here drives the network forward; the transport's inactivity budget
/// guarantees this loop terminates (with an error) rather than hanging on a stall.
fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = pin!(fut);
    // SAFETY: the vtable's clone/wake/drop are all no-ops over a null data pointer,
    // so the resulting `Waker` upholds the `RawWaker` contract trivially.
    let waker = unsafe { Waker::from_raw(noop_raw_waker()) };
    let mut cx = Context::from_waker(&waker);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => {
                // Cooperative yield (halt until the next timer tick) instead of
                // a busy-spin: lets the net poll thread and timer service the
                // device without hot dual-poller contention.
                scheduler::sleep_ticks(1);
            }
        }
    }
}

// ────────────────────────── transport adapter ──────────────────────────

/// Raw bytes pulled off the TCP socket by the TLS transport since the current
/// [`https_get`] started, and how many transport polls that took.
///
/// BYTE-TRACE counters (issue #19): they separate the three layers a large-stream
/// stall can hide in — the SOCKET (no `raw_in` growth: the peer stopped, e.g. a
/// closed window), the TRANSPORT (polls grow but `raw_in` does not: our pump
/// returns `Pending` while data sits unread), and the RECORD layer (both grow but
/// the decrypted body does not). Printed by the stall watchdog and at completion.
static TLS_RAW_IN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static TLS_READ_POLLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static TLS_READ_PENDING: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn trace_reset() {
    use core::sync::atomic::Ordering::Relaxed;
    TLS_RAW_IN.store(0, Relaxed);
    TLS_READ_POLLS.store(0, Relaxed);
    TLS_READ_PENDING.store(0, Relaxed);
}

fn trace_raw_in() -> u64 {
    TLS_RAW_IN.load(core::sync::atomic::Ordering::Relaxed)
}

/// An `embedded-io[-async]` byte transport over a TCP socket handle from the
/// own stack.
///
/// Each poll takes the `NET` lock, advances the stack once, then drains rx /
/// fills tx for the held socket. No `await` happens while the lock is held. An
/// inactivity deadline (re-armed on every byte moved) bounds stalls.
struct TlsTransport {
    handle: SocketHandle,
    /// Tick at which an idle transport gives up. Re-armed whenever bytes move.
    deadline: u64,
}

impl TlsTransport {
    fn new(handle: SocketHandle) -> Self {
        TlsTransport {
            handle,
            deadline: scheduler::ticks() + TLS_IDLE_TIMEOUT_TICKS,
        }
    }

    /// Re-arm the inactivity deadline after observable progress.
    fn touch(&mut self) {
        self.deadline = scheduler::ticks() + TLS_IDLE_TIMEOUT_TICKS;
    }

    /// Return `Ready(Err)` if the inactivity budget has elapsed, else `None`.
    fn timed_out(&self) -> bool {
        scheduler::ticks() >= self.deadline
    }

    /// One locked pump + send step. `Ready(Ok(n>0))` once tx accepted bytes,
    /// `Ready(Err)` on a dead socket/timeout, `Pending` (re-woken) otherwise.
    fn poll_write_impl(
        &mut self,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, ErrorKind>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        {
            let mut guard = NET.lock();
            let state = match guard.as_mut() {
                Some(s) => s,
                None => return Poll::Ready(Err(ErrorKind::NotConnected)),
            };
            state.step();

            let sock_state_ok = state
                .tcp
                .get(self.handle)
                .map(|k| k.may_send() || k.is_active())
                .unwrap_or(false);
            if !sock_state_ok {
                // Connection gone for sending (peer reset / fully closed).
                return Poll::Ready(Err(ErrorKind::BrokenPipe));
            }
            if state
                .tcp
                .get(self.handle)
                .map(|k| k.can_send())
                .unwrap_or(false)
            {
                let n = state
                    .tcp
                    .get_mut(self.handle)
                    .map(|k| k.send_slice(buf))
                    .unwrap_or(0);
                if n > 0 {
                    drop(guard);
                    self.touch();
                    return Poll::Ready(Ok(n));
                }
            }
        }
        if self.timed_out() {
            return Poll::Ready(Err(ErrorKind::TimedOut));
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    /// One locked pump + recv step. `Ready(Ok(n>=1))` with data, `Ready(Ok(0))` on
    /// a clean peer close (EOF), `Ready(Err)` on a dead socket/timeout, `Pending`
    /// (re-woken) otherwise.
    fn poll_read_impl(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, ErrorKind>> {
        use core::sync::atomic::Ordering::Relaxed;
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        TLS_READ_POLLS.fetch_add(1, Relaxed);
        {
            let mut guard = NET.lock();
            let state = match guard.as_mut() {
                Some(s) => s,
                None => return Poll::Ready(Err(ErrorKind::NotConnected)),
            };
            state.step();

            // THROUGHPUT: drain aggressively. Fill as much of `buf` as possible
            // across repeated `recv_slice` calls within this single lock hold
            // (mirrors the drain loop in `http_get`), instead of returning after
            // one `recv_slice`. With the large 256 KiB receive window this moves
            // up to a full `buf` (a TLS record buffer, ~16 KiB) out of the socket
            // per poll, rather than being throttled to one record per re-poll.
            let mut filled = 0usize;
            while filled < buf.len() {
                let n = state
                    .tcp
                    .get_mut(self.handle)
                    .map(|k| k.recv_slice(&mut buf[filled..]))
                    .unwrap_or(0);
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled > 0 {
                TLS_RAW_IN.fetch_add(filled as u64, Relaxed);
                drop(guard);
                self.touch();
                return Poll::Ready(Ok(filled));
            }

            // Peer closed its half and rx is drained: clean EOF.
            let eof = state
                .tcp
                .get(self.handle)
                .map(|k| !k.may_recv() && !k.can_recv())
                .unwrap_or(true);
            if eof {
                return Poll::Ready(Ok(0));
            }
        }
        if self.timed_out() {
            return Poll::Ready(Err(ErrorKind::TimedOut));
        }
        TLS_READ_PENDING.fetch_add(1, Relaxed);
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

impl embedded_io::ErrorType for TlsTransport {
    type Error = ErrorKind;
}

impl embedded_io_async::Read for TlsTransport {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        core::future::poll_fn(move |cx| self.poll_read_impl(cx, &mut buf[..])).await
    }
}

impl embedded_io_async::Write for TlsTransport {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        core::future::poll_fn(move |cx| self.poll_write_impl(cx, buf)).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        // egress happens inside each poll; a no-op flush is correct here.
        Ok(())
    }
}

// ───────────────────────────── https_get ─────────────────────────────

/// Perform one HTTPS (TLS 1.3) `GET` of `https://host:port/path` and return the
/// `Content-Length`-sized response body.
///
/// **Authenticated:** the handshake fails unless the server's certificate
/// chain validates against the committed CA bundle, authorizes `host` through
/// its SAN entries, and proves possession of the leaf key with a valid TLS 1.3
/// `CertificateVerify` — with the clock gate applied (an unset RTC refuses the
/// connection; see the module docs). The default `port` for HTTPS is
/// [`HTTPS_PORT`] (443).
///
/// Mirrors [`http_get`](super::http_fetch::http_get): `host` may be a dotted-quad
/// IPv4 literal or a hostname (resolved via [`resolve`](super::resolve)); the
/// request `Host:` header carries the original `host` string; the response head is
/// parsed with the shared pure [`parse_http_head`]. On any failure the socket is
/// released and exactly one structured diagnostic is emitted.
pub fn https_get(host: &str, port: u16, path: &str) -> Result<Vec<u8>, FetchError> {
    if !crate::security::entropy::is_available() {
        error!(
            "Package_Fetcher(tls): stage=entropy host={} path={} cause=Tls \
             (secure hardware entropy unavailable: CPUID reports {})",
            host,
            path,
            crate::security::entropy::capabilities_str()
        );
        return Err(FetchError::Tls("entropy"));
    }

    // Preflight: no interface address -> fail without a connection (R8.7).
    if super::ip_config().is_none() {
        warn!(
            "Package_Fetcher(tls): stage=preflight host={} path={} cause=NoNetwork (no interface address)",
            host, path
        );
        return Err(FetchError::NoNetwork);
    }

    // Resolve host (IPv4 literal or DNS).
    let addr = match super::resolve(host) {
        Some(a) => a,
        None => {
            error!(
                "Package_Fetcher(tls): stage=resolve host={} path={} cause=ConnectTimeout (could not resolve host)",
                host, path
            );
            return Err(FetchError::ConnectTimeout);
        }
    };
    let remote = IpEndpoint::new(addr, port);

    // Open the TCP socket with larger buffers for multi-KiB TLS records.
    let handle = match super::tcp_connect_buffered(remote, TLS_RX_BYTES, TLS_TX_BYTES) {
        Ok(h) => h,
        Err(_) => {
            error!(
                "Package_Fetcher(tls): stage=connect host={} path={} cause=ConnectTimeout (socket open failed)",
                host, path
            );
            return Err(FetchError::ConnectTimeout);
        }
    };

    // Locked-step pump until the TCP connection is established (or times out).
    if let Err(e) = pump_until_established(handle) {
        release(handle);
        emit_tls_failure(&e, host, path);
        return Err(e);
    }

    // Build the TLS connection over the established socket and run the exchange.
    let config = TlsConfig::new().with_server_name(host);
    let mut read_buf = vec![0u8; RECORD_BUF];
    let mut write_buf = vec![0u8; RECORD_BUF];
    let transport = TlsTransport::new(handle);
    let mut tls: TlsConnection<TlsTransport, Aes128GcmSha256> =
        TlsConnection::new(transport, &mut read_buf[..], &mut write_buf[..]);

    // Snapshot the authentication counter BEFORE the handshake: `open()`
    // returning `Ok` is not by itself proof that a server certificate was
    // verified, so the counter must advance (see `VERIFIED_HANDSHAKES`).
    let auth_before = verified_handshakes();

    // Byte-trace counters for the stall watchdog below (issue #19).
    trace_reset();
    let started_tick = scheduler::ticks();

    let result = block_on(async {
        // Handshake — server authenticated (chain + hostname + clock +
        // CertificateVerify) through `KernelProvider::verifier`.
        let context = TlsContext::new(&config, KernelProvider::new());
        tls.open(context).await.map_err(map_tls_err("handshake"))?;

        // The handshake completed. Belt to the state machine's braces: refuse
        // to move any application byte unless a certificate was actually
        // verified for THIS handshake. A peer that omits `Certificate` and
        // `CertificateVerify` (which the TLS 1.3 state machine must reject, and
        // now does) would land here with the verifier never invoked.
        if verified_handshakes() == auth_before {
            error!(
                "Package_Fetcher(tls): stage=verify host={} path={} cause=InvalidCertificate \
                 (handshake completed without a verified server certificate — refusing to \
                 continue)",
                host, path
            );
            return Err(FetchError::Tls("unverified"));
        }

        // Send the HTTP/1.1 GET through the encrypted channel.
        let req = build_get_request(host, path);
        let mut off = 0usize;
        while off < req.len() {
            let n = tls.write(&req[off..]).await.map_err(map_tls_err("write"))?;
            if n == 0 {
                break;
            }
            off += n;
        }
        tls.flush().await.map_err(map_tls_err("write"))?;

        // Read + decrypt the response, collecting the Content-Length body.
        let mut buf: Vec<u8> = Vec::new();
        let mut head: Option<(usize, u64)> = None;
        // App-level read buffer. embedded-tls returns at most
        // min(buf.len(), one decrypted record) per `tls.read()`, so a 4 KiB
        // buffer capped each executor round-trip to 4 KiB even though a TLS 1.3
        // record is ~16 KiB. Sizing this to a full record lets each `read()` pull
        // a whole ~16 KiB record per `block_on` poll cycle — fewer round-trips
        // through the inter-poll spin → higher throughput, especially on the tail.
        let mut tmp = [0u8; 16 * 1024];
        // In-place progress-bar throttle state: last integer percent drawn (once
        // Content-Length is known) and last 512 KiB step (before it is).
        let mut last_pct: u64 = u64::MAX;
        let mut byte_mark: usize = 0;
        // Stall watchdog state (issue #19): tick of the last body growth and
        // whether the current stall episode was already reported.
        let mut last_body_tick = scheduler::ticks();
        let mut last_body_len = 0usize;
        let mut stall_reported = false;
        // Why the record stream ended early (if it did). A TLS read error after
        // the peer closes (`Connection: close`) is a NORMAL end-of-stream — but
        // only when the body is already complete, which the loop checks below. If
        // the body is short, this error IS the failure, and dropping it (the old
        // `Err(_) => break`) is what made a stalled/failed stream look like a bare
        // "Incomplete" with no cause.
        let mut read_err: Option<TlsError> = None;
        loop {
            let n = match tls.read(&mut tmp).await {
                Ok(0) => break, // clean EOF
                Ok(n) => n,
                Err(e) => {
                    read_err = Some(e);
                    break;
                }
            };
            buf.extend_from_slice(&tmp[..n]);

            // BYTE TRACE (issue #19): a body that stops growing is the failure
            // this path used to hide. Report the episode ONCE with enough context
            // to name the stuck layer, then keep going (the transport's idle
            // budget still bounds the attempt and turns it into a clean error).
            if buf.len() > last_body_len {
                last_body_len = buf.len();
                last_body_tick = scheduler::ticks();
                stall_reported = false;
            } else if !stall_reported
                && scheduler::ticks().saturating_sub(last_body_tick) >= TLS_STALL_REPORT_TICKS
            {
                stall_reported = true;
                warn!(
                    "Package_Fetcher(tls): stage=stall host={} path={} cause=BodyStalled \
                     (no decrypted body for {} ms) body={} raw_in={} read_polls={} \
                     pending_polls={} sock={}",
                    host,
                    path,
                    (scheduler::ticks().saturating_sub(last_body_tick) * 1000)
                        / crate::arch::x86_64::apic::TICK_HZ.max(1),
                    buf.len(),
                    trace_raw_in(),
                    TLS_READ_POLLS.load(core::sync::atomic::Ordering::Relaxed),
                    TLS_READ_PENDING.load(core::sync::atomic::Ordering::Relaxed),
                    socket_diag(handle),
                );
            }

            if buf.len() > MAX_TOTAL {
                return Err(FetchError::Incomplete);
            }

            // OBSERVABILITY: redraw an in-place download progress bar (same line,
            // leading `\r`, no newline). Before the response head is parsed the
            // total is unknown, so we show the running byte count and refresh
            // ~every 512 KiB; once `Content-Length` is known we show a percentage
            // bar and refresh on each 1% advance. This both makes throughput
            // visible and distinguishes a slow-but-progressing transfer from a
            // genuine hang, without scrolling the console.
            let total = head.map(|(off, cl)| off as u64 + cl);
            let refresh = match total {
                Some(t) if t > 0 => {
                    let pct = 100 * (buf.len() as u64).min(t) / t;
                    if pct != last_pct {
                        last_pct = pct;
                        true
                    } else {
                        false
                    }
                }
                _ => {
                    let step = buf.len() / (512 * 1024);
                    if step > byte_mark {
                        byte_mark = step;
                        true
                    } else {
                        false
                    }
                }
            };
            if refresh {
                super::progress::show(&super::progress::line(buf.len() as u64, total));
            }

            if head.is_none() {
                match parse_http_head(&buf) {
                    HeadParse::Need => {}
                    HeadParse::Malformed => return Err(FetchError::Incomplete),
                    HeadParse::Done {
                        status,
                        content_length,
                        body_off,
                    } => {
                        if status != 200 {
                            return Err(FetchError::Status(status));
                        }
                        match content_length {
                            Some(cl) => {
                                head = Some((body_off, cl));
                            }
                            None => return Err(FetchError::UnknownLength),
                        }
                    }
                }
            }

            if let Some((body_off, cl)) = head {
                let received = (buf.len().saturating_sub(body_off)) as u64;
                if received >= cl {
                    let end = body_off + cl as usize;
                    return Ok(buf[body_off..end].to_vec());
                }
            }
        }

        // Stream ended before the full body arrived. If TLS itself failed (as
        // opposed to a clean peer close), say so with the underlying error and
        // the exact shortfall: that single line is what turns a mystery stall
        // into a named failure (issue #19).
        if let Some(e) = read_err {
            let got = head
                .map(|(off, cl)| (buf.len().saturating_sub(off) as u64, cl))
                .unwrap_or((0, 0));
            error!(
                "Package_Fetcher(tls): stage=read host={} path={} cause=Tls ({:?}) \
                 - the record stream failed after {} of {} body bytes ({} raw socket bytes, \
                 {} read polls)",
                host,
                path,
                e,
                got.0,
                got.1,
                trace_raw_in(),
                TLS_READ_POLLS.load(core::sync::atomic::Ordering::Relaxed),
            );
        }
        Err(FetchError::Incomplete)
    });

    // Drop the TLS connection (and its borrows) before releasing the socket.
    // (`tls` does not implement `Drop`; the binding is shadowed/moved into a
    // type-erased local so the socket is definitely free afterwards.)
    let _tls_done: TlsConnection<TlsTransport, Aes128GcmSha256> = tls;
    release(handle);

    match result {
        Ok(body) => {
            super::progress::finish();
            // THROUGHPUT EVIDENCE (issue #19): one line per completed fetch with
            // the raw/wire ratio and the poll counts, so a large-stream run is
            // measurable from the serial log instead of only "it finished".
            let elapsed = scheduler::ticks().saturating_sub(started_tick).max(1);
            crate::info!(
                "Package_Fetcher(tls): stage=done host={} path={} body={} raw_in={} \
                 read_polls={} pending_polls={} bytes_per_s={}",
                host,
                path,
                body.len(),
                trace_raw_in(),
                TLS_READ_POLLS.load(core::sync::atomic::Ordering::Relaxed),
                TLS_READ_PENDING.load(core::sync::atomic::Ordering::Relaxed),
                (body.len() as u64) * crate::arch::x86_64::apic::TICK_HZ / elapsed,
            );
            Ok(body)
        }
        Err(e) => {
            emit_tls_failure(&e, host, path);
            Err(e)
        }
    }
}

/// Snapshot of the TLS socket's TCP flow-control state (stall watchdog).
fn socket_diag(handle: SocketHandle) -> String {
    NET.lock()
        .as_ref()
        .and_then(|s| s.tcp.get(handle))
        .map(|k| k.diag())
        .unwrap_or_else(|| String::from("<socket gone>"))
}

/// Map an `embedded-tls` error to a [`FetchError::Tls`] carrying the failed stage.
fn map_tls_err(stage: &'static str) -> impl Fn(TlsError) -> FetchError {
    move |_e| FetchError::Tls(stage)
}

/// Pump the stack in short, individually-locked steps until the TCP connection
/// reaches a writable (Established) state, or fail with [`FetchError::ConnectTimeout`].
fn pump_until_established(handle: SocketHandle) -> Result<(), FetchError> {
    let connect_deadline = scheduler::ticks() + CONNECT_TIMEOUT_TICKS;
    let mut active = false;

    loop {
        {
            let mut guard = NET.lock();
            let state = match guard.as_mut() {
                Some(s) => s,
                None => return Err(FetchError::ConnectTimeout),
            };
            state.step();

            if state
                .tcp
                .get(handle)
                .map(|k| k.is_active())
                .unwrap_or(false)
            {
                active = true;
            }
            // Gone before becoming active = refused / unreachable.
            let dead = state
                .tcp
                .get(handle)
                .map(|k| !k.ever_established() && k.state() == super::tcp::State::Closed)
                .unwrap_or(true);
            if !active && dead {
                return Err(FetchError::ConnectTimeout);
            }
            // Established and writable: ready to start the TLS handshake.
            let ready = state
                .tcp
                .get(handle)
                .map(|k| k.state() == super::tcp::State::Established && k.may_send())
                .unwrap_or(false);
            if ready {
                return Ok(());
            }
        }

        if scheduler::ticks() >= connect_deadline {
            return Err(FetchError::ConnectTimeout);
        }
        // Cooperative yield instead of a busy-spin (see `block_on`).
        scheduler::sleep_ticks(1);
    }
}

/// Release a still-open socket handle from the TCP table (idempotent).
fn release(handle: SocketHandle) {
    if let Some(state) = NET.lock().as_mut() {
        state.tcp.remove(handle);
    }
}

/// Emit exactly one structured diagnostic for an HTTPS fetch failure.
fn emit_tls_failure(err: &FetchError, host: &str, path: &str) {
    // Terminate any in-place progress bar before the diagnostic line.
    super::progress::finish();
    match err {
        FetchError::NoNetwork => warn!(
            "Package_Fetcher(tls): stage=preflight host={} path={} cause=NoNetwork",
            host, path
        ),
        FetchError::ConnectTimeout => error!(
            "Package_Fetcher(tls): stage=connect host={} path={} cause=ConnectTimeout",
            host, path
        ),
        FetchError::Status(code) => error!(
            "Package_Fetcher(tls): stage=response host={} path={} cause=Status({})",
            host, path, code
        ),
        FetchError::UnknownLength => error!(
            "Package_Fetcher(tls): stage=response host={} path={} cause=UnknownLength",
            host, path
        ),
        FetchError::Incomplete => error!(
            "Package_Fetcher(tls): stage=body host={} path={} cause=Incomplete",
            host, path
        ),
        FetchError::ReadTimeout => error!(
            "Package_Fetcher(tls): stage=body host={} path={} cause=ReadTimeout",
            host, path
        ),
        FetchError::Tls(stage) => error!(
            "Package_Fetcher(tls): stage=tls:{} host={} path={} cause=Tls (handshake/record failure; no application data was exchanged)",
            stage, host, path
        ),
    }
}

// No `#[cfg(test)]` block here: this module is kernel-only (it needs
// `embedded-tls` + the kernel's net stack) and is NOT `#[path]`-included by the
// host test crate, so an in-module test would never be compiled or run. The
// guard for [`CIPHER_SUITE_HASH_LEN`] is the `const` assertion next to it, which
// the kernel build evaluates; the pure layers it feeds are property-tested in
// `host-tests/src/properties/p49.rs`.
