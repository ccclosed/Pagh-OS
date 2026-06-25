//! HTTPS over TLS 1.3 for the package manager — **VARIANT A: INSECURE**.
//!
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │  ⚠️  SECURITY WARNING — READ THIS BEFORE TRUSTING THIS TRANSPORT  ⚠️       │
//! │                                                                           │
//! │  This module establishes an ENCRYPTED but UNAUTHENTICATED TLS 1.3         │
//! │  session. It performs **NO certificate verification whatsoever**:         │
//! │    * the server certificate chain is NOT validated against any CA,        │
//! │    * the hostname is NOT checked against the certificate,                 │
//! │    * certificate expiry / revocation is NOT checked,                      │
//! │    * the `CertificateVerify` signature is accepted blindly.               │
//! │                                                                           │
//! │  It is driven through `embedded_tls::UnsecureProvider` (the `NoVerify`    │
//! │  path) ON PURPOSE, to get working encrypted transport now. Because the    │
//! │  peer is never authenticated, the channel is trivially defeated by an     │
//! │  active man-in-the-middle: an attacker who can intercept traffic can      │
//! │  present any certificate and read/modify everything. Treat downloaded     │
//! │  packages as UNTRUSTED.                                                    │
//! │                                                                           │
//! │  Compounding this, the session RNG (see [`KernelRng`]) is a WEAK,         │
//! │  non-cryptographic xorshift seeded from `RDTSC` — not a CSPRNG — so the   │
//! │  key exchange is not secure against a capable attacker either.            │
//! │                                                                           │
//! │  This is acceptable ONLY for this hobby-OS / QEMU demo. The module is     │
//! │  deliberately structured (a pluggable verifier sits behind                │
//! │  `UnsecureProvider`) so full chain/hostname/expiry verification can be    │
//! │  added later without reworking the transport or executor.                 │
//! └─────────────────────────────────────────────────────────────────────────┘
//!
//! ## How it is wired (kernel-only module)
//!
//! `embedded-tls` is async (`embedded-io-async`). We do not have an async runtime,
//! so three small pieces bridge it onto the existing smoltcp socket pump:
//!
//!   1. [`block_on`] — a minimal executor: it polls a pinned future in a loop with
//!      a no-op waker until it is `Ready`, spinning briefly between polls so the
//!      100 Hz timer tick and QEMU can advance.
//!   2. [`TlsTransport`] — implements [`embedded_io_async::Read`]/[`Write`] over a
//!      smoltcp TCP [`SocketHandle`]. Each poll takes the `NET` lock, advances
//!      smoltcp once, then drains/fills the socket. It NEVER holds the `NET` lock
//!      across an `await`, mirrors the bounded locked-step + `spin_loop` discipline
//!      of `nc_echo`/`http_get`, and carries an inactivity budget so a stall
//!      returns an error instead of hanging.
//!   3. [`KernelRng`] — wraps the kernel entropy source into the `rand_core`
//!      `RngCore + CryptoRng` traits `embedded-tls` requires (WEAK, see above).
//!
//! [`https_get`] ties them together: resolve → connect → handshake → HTTP GET →
//! collect the `Content-Length` body, reusing the pure
//! [`build_get_request`](super::http::build_get_request) /
//! [`parse_http_head`](super::http::parse_http_head) for all wire formatting.

use alloc::vec;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::pin;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use embedded_io::ErrorKind;
use embedded_tls::{
    Aes128GcmSha256, TlsConfig, TlsConnection, TlsContext, TlsError, UnsecureProvider,
};
use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp;
use smoltcp::wire::{IpAddress, IpEndpoint};

use super::http::{build_get_request, parse_http_head, HeadParse};
use super::http_fetch::FetchError;
use super::{now, NET};
use crate::arch::x86_64::linux::misc;
use crate::task::scheduler;
use crate::{error, warn};

/// Default HTTPS port.
pub const HTTPS_PORT: u16 = 443;

/// Connect timeout in 100 Hz scheduler ticks (~3 s), matching `http_get`.
const CONNECT_TIMEOUT_TICKS: u64 = 300;
/// Inactivity (idle) timeout for the TLS transport in 100 Hz ticks (~15 s). It is
/// re-armed on every byte sent or received, so a slow-but-progressing handshake or
/// download over QEMU NAT is not killed — only a genuine stall aborts.
const TLS_IDLE_TIMEOUT_TICKS: u64 = 1500;
/// TLS record buffer size. 16 KiB+ is the safe maximum for any TLS 1.3 record.
const RECORD_BUF: usize = 16 * 1024 + 256;
/// rx/tx smoltcp socket buffer sizes for the TLS connection.
///
/// THROUGHPUT (#1 lever): over QEMU user-net NAT the achievable bandwidth is
/// bounded by `window / RTT` (bandwidth-delay product). The old 16 KiB receive
/// window capped a ~10 MiB `Packages.gz` to roughly one 16 KiB window per RTT.
/// We raise rx to 256 KiB so smoltcp negotiates TCP window scaling (available in
/// smoltcp 0.12) and many more bytes are in flight per round trip. The heap is
/// 256 MiB, so a 256 KiB per-connection buffer is comfortably affordable. tx is
/// kept modest (16 KiB): we only ever send a small GET request.
const TLS_RX_BYTES: usize = 256 * 1024;
const TLS_TX_BYTES: usize = 16 * 1024;
/// Upper bound on the decrypted HTTP response we will buffer.
const MAX_TOTAL: usize = 32 * 1024 * 1024;

/// One-time "no certificate verification" warning latch.
static WARNED_NOVERIFY: AtomicBool = AtomicBool::new(false);
/// One-time "weak RNG" warning latch.
static WARNED_WEAK_RNG: AtomicBool = AtomicBool::new(false);

/// Emit the insecure-transport warning exactly once per boot.
fn warn_insecure_once() {
    if !WARNED_NOVERIFY.swap(true, Ordering::Relaxed) {
        warn!(
            "net::tls: HTTPS is INSECURE — TLS 1.3 certificate verification is NOT implemented \
             (no chain/hostname/expiry checks). The channel is encrypted but UNAUTHENTICATED and \
             can be man-in-the-middled. Downloaded data is UNTRUSTED. (VARIANT A)"
        );
    }
}

// ───────────────────────────── RNG (WEAK) ─────────────────────────────

/// RNG adapter handed to `embedded-tls` for TLS key exchange and nonces.
///
/// **NOT CRYPTOGRAPHICALLY SECURE.** It forwards to the kernel entropy source
/// ([`misc::next_rand_u64`]), which is a fast xorshift64 seeded once from `RDTSC`
/// XOR the scheduler tick counter. That is fine for ASLR-ish seeding but is *not*
/// a CSPRNG: its output is predictable to an attacker who can observe or guess the
/// seed, which means the TLS session's key material is not secure against a capable
/// adversary. The first time one is constructed it emits a one-time runtime warning.
///
/// Replacing this with a real CSPRNG (e.g. seeded from a hardware RNG / RDRAND and
/// run through a DRBG) is part of making this transport actually secure.
struct KernelRng;

impl KernelRng {
    fn new() -> Self {
        if !WARNED_WEAK_RNG.swap(true, Ordering::Relaxed) {
            warn!(
                "net::tls: TLS RNG is NOT cryptographically secure — it is a weak xorshift seeded \
                 from RDTSC, so this TLS session is not secure against a capable attacker \
                 (acceptable only for this hobby-OS/QEMU demo)."
            );
        }
        KernelRng
    }
}

impl rand_core::RngCore for KernelRng {
    fn next_u32(&mut self) -> u32 {
        misc::next_rand_u64() as u32
    }

    fn next_u64(&mut self) -> u64 {
        misc::next_rand_u64()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        let mut chunks = dest.chunks_exact_mut(8);
        for c in &mut chunks {
            c.copy_from_slice(&misc::next_rand_u64().to_le_bytes());
        }
        let rem = chunks.into_remainder();
        if !rem.is_empty() {
            let bytes = misc::next_rand_u64().to_le_bytes();
            rem.copy_from_slice(&bytes[..rem.len()]);
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

// SAFETY/SECURITY: this marker asserts the RNG is suitable for cryptographic use.
// It is a DELIBERATE LIE required to satisfy `embedded-tls`'s `CryptoRngCore`
// bound for the demo. The xorshift source is NOT a CSPRNG; see the type docs.
impl rand_core::CryptoRng for KernelRng {}

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
/// spinning briefly between `Pending` polls so the 100 Hz timer tick and QEMU can
/// advance. The transport adapter pumps smoltcp inside each of its own polls, so
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
                // Cooperative yield (halt until the next 100 Hz tick) instead of
                // a busy-spin: lets the net poll thread and timer service the
                // device without hot dual-poller contention.
                scheduler::sleep_ticks(1);
            }
        }
    }
}

// ────────────────────────── transport adapter ──────────────────────────

/// An `embedded-io[-async]` byte transport over a smoltcp TCP [`SocketHandle`].
///
/// Each poll takes the `NET` lock, advances smoltcp once (`iface.poll`), then
/// drains rx / fills tx for the held socket. No `await` happens while the lock is
/// held. An inactivity deadline (re-armed on every byte moved) bounds stalls.
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
    fn poll_write_impl(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, ErrorKind>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        {
            let mut guard = NET.lock();
            let state = match guard.as_mut() {
                Some(s) => s,
                None => return Poll::Ready(Err(ErrorKind::NotConnected)),
            };
            let ts = now();
            let _ = state.iface.poll(ts, &mut state.device, &mut state.sockets);
            let sock = state.sockets.get_mut::<tcp::Socket>(self.handle);

            // Connection gone for sending (peer reset / fully closed).
            if !sock.may_send() && !sock.is_active() {
                return Poll::Ready(Err(ErrorKind::BrokenPipe));
            }
            if sock.can_send() {
                match sock.send_slice(buf) {
                    Ok(0) => {}
                    Ok(n) => {
                        drop(guard);
                        self.touch();
                        return Poll::Ready(Ok(n));
                    }
                    Err(_) => return Poll::Ready(Err(ErrorKind::BrokenPipe)),
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
    fn poll_read_impl(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<Result<usize, ErrorKind>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        {
            let mut guard = NET.lock();
            let state = match guard.as_mut() {
                Some(s) => s,
                None => return Poll::Ready(Err(ErrorKind::NotConnected)),
            };
            let ts = now();
            let _ = state.iface.poll(ts, &mut state.device, &mut state.sockets);
            let sock = state.sockets.get_mut::<tcp::Socket>(self.handle);

            // THROUGHPUT: drain aggressively. Fill as much of `buf` as possible
            // across repeated `recv_slice` calls within this single lock hold
            // (mirrors the drain loop in `http_get`), instead of returning after
            // one `recv_slice`. With the large 256 KiB receive window this moves
            // up to a full `buf` (a TLS record buffer, ~16 KiB) out of the socket
            // per poll, rather than being throttled to one record per re-poll.
            let mut filled = 0usize;
            if sock.can_recv() {
                while filled < buf.len() {
                    match sock.recv_slice(&mut buf[filled..]) {
                        Ok(0) => break,
                        Ok(n) => filled += n,
                        Err(_) => {
                            // A receive error with nothing collected is a dead
                            // socket; if we already have bytes, return them and
                            // surface the error on the next poll.
                            if filled == 0 {
                                return Poll::Ready(Err(ErrorKind::BrokenPipe));
                            }
                            break;
                        }
                    }
                }
            }
            if filled > 0 {
                drop(guard);
                self.touch();
                return Poll::Ready(Ok(filled));
            }

            // Peer closed its half and rx is drained: clean EOF.
            if !sock.may_recv() && !sock.can_recv() {
                return Poll::Ready(Ok(0));
            }
        }
        if self.timed_out() {
            return Poll::Ready(Err(ErrorKind::TimedOut));
        }
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
        // smoltcp egress happens inside each poll; a no-op flush is correct here.
        Ok(())
    }
}

// ───────────────────────────── https_get ─────────────────────────────

/// Perform one HTTPS (TLS 1.3) `GET` of `https://host:port/path` and return the
/// `Content-Length`-sized response body.
///
/// **INSECURE (VARIANT A):** the TLS session is encrypted but the server is NOT
/// authenticated — no certificate chain, hostname, or expiry verification is
/// performed (see the module-level warning). A one-time runtime warning is logged
/// on first use. The default `port` for HTTPS is [`HTTPS_PORT`] (443).
///
/// Mirrors [`http_get`](super::http_fetch::http_get): `host` may be a dotted-quad
/// IPv4 literal or a hostname (resolved via [`resolve`](super::resolve)); the
/// request `Host:` header carries the original `host` string; the response head is
/// parsed with the shared pure [`parse_http_head`]. On any failure the socket is
/// released and exactly one structured diagnostic is emitted.
pub fn https_get(host: &str, port: u16, path: &str) -> Result<Vec<u8>, FetchError> {
    warn_insecure_once();

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
    let remote = IpEndpoint::new(IpAddress::Ipv4(addr), port);

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

    let result = block_on(async {
        // Handshake (NoVerify path — server NOT authenticated).
        let context = TlsContext::new(
            &config,
            UnsecureProvider::new::<Aes128GcmSha256>(KernelRng::new()),
        );
        tls.open(context).await.map_err(map_tls_err("handshake"))?;

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
        loop {
            let n = match tls.read(&mut tmp).await {
                Ok(0) => break, // clean EOF
                Ok(n) => n,
                // A read error after the peer closes (Connection: close) is the
                // normal end-of-stream for TLS; break and validate completeness.
                Err(_) => break,
            };
            buf.extend_from_slice(&tmp[..n]);
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

        // Stream ended before the full body arrived.
        Err(FetchError::Incomplete)
    });

    // Drop the TLS connection (and its borrows) before releasing the socket.
    drop(tls);
    release(handle);

    match result {
        Ok(body) => {
            super::progress::finish();
            Ok(body)
        }
        Err(e) => {
            emit_tls_failure(&e, host, path);
            Err(e)
        }
    }
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
            let ts = now();
            let _ = state.iface.poll(ts, &mut state.device, &mut state.sockets);
            let sock = state.sockets.get_mut::<tcp::Socket>(handle);

            if sock.is_active() {
                active = true;
            }
            // Reaching Closed before becoming active = refused / unreachable.
            if !active && sock.state() == tcp::State::Closed {
                return Err(FetchError::ConnectTimeout);
            }
            // Established and writable: ready to start the TLS handshake.
            if sock.state() == tcp::State::Established && sock.may_send() {
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

/// Release a still-open socket handle from the shared socket set (idempotent).
fn release(handle: SocketHandle) {
    if let Some(state) = NET.lock().as_mut() {
        state.sockets.remove(handle);
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
            "Package_Fetcher(tls): stage=tls:{} host={} path={} cause=Tls (handshake/record failure; INSECURE: no cert verification)",
            stage, host, path
        ),
    }
}
