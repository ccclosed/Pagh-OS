//! Effectful `.deb` fetch over a smoltcp TCP socket (Component 7, `Package_Fetcher`).
//!
//! This is the **kernel-only** half of the `Package_Fetcher`. The pure
//! request-building and response-head parsing live in [`crate::net::http`]
//! (`parse_http_head` / `build_get_request`, task 7.1) and are shared into the
//! `host-tests` crate via a `#[path]` include; keeping the socket pump in this
//! separate file means `net/http.rs` stays purely host-includable (no smoltcp,
//! no globals) for properties P20/P21 (R11.6).
//!
//! [`fetch_deb`] performs one HTTP/1.1 `GET` over a freshly-opened TCP socket and
//! collects a `Content-Length`-sized body. It reuses the existing networking
//! primitives owned by the parent [`crate::net`] module — [`super::tcp_connect`]
//! to open the connection and the module-global `NET` state to pump the socket —
//! and the pure [`parse_http_head`]/[`build_get_request`] for all wire parsing.
//!
//! Failure handling (R8.3–R8.7, R12.4, R12.5): each distinct failure maps to a
//! [`FetchError`] variant, the partial buffer is discarded, the socket is
//! released, and exactly one structured diagnostic is emitted naming
//! `component=Package_Fetcher`, the stage, and the resource (host/path).

use alloc::vec::Vec;

use smoltcp::socket::tcp;
use smoltcp::wire::{IpAddress, IpEndpoint};

use super::http::{build_get_request, parse_http_head, HeadParse};
use super::{now, NET};
use crate::task::scheduler;
use crate::{error, info, warn};

/// Why a `.deb` fetch failed (design component 7). Each variant carries enough
/// context for the single structured diagnostic emitted at the failure site.
#[derive(Debug, PartialEq, Eq)]
pub enum FetchError {
    /// No network interface has an assigned address; no connection was attempted
    /// (R8.7).
    NoNetwork,
    /// The TCP connection could not be established within the connect timeout
    /// (or was refused); the socket has been released (R8.6).
    ConnectTimeout,
    /// The HTTP response status was not 200; carries the numeric code (R8.5).
    Status(u16),
    /// The 200 response had no parseable `Content-Length`; data discarded (R8.3).
    UnknownLength,
    /// The connection closed before `Content-Length` bytes arrived; the partial
    /// buffer was discarded (R8.4).
    Incomplete,
    /// The read timeout elapsed before `Content-Length` bytes arrived; the
    /// partial buffer was discarded (R8.4).
    ReadTimeout,
    /// A TLS-layer failure (`https_get` only): the handshake did not complete,
    /// or an encrypted record could not be sent/received. Carries a short static
    /// stage label for the diagnostic (e.g. `"handshake"`, `"write"`, `"read"`).
    ///
    /// NOTE: with VARIANT A (no certificate verification) this never represents a
    /// *rejected* certificate — chains are not validated — only transport/crypto
    /// or protocol failures.
    Tls(&'static str),
}

/// A successfully downloaded `.deb` image.
pub struct DebBytes(pub Vec<u8>);

/// Connect timeout in 100 Hz scheduler ticks (~3 s).
const CONNECT_TIMEOUT_TICKS: u64 = 300;
/// Idle read timeout in 100 Hz scheduler ticks (~15 s). This is an *inactivity*
/// timeout: it is re-armed every time new body bytes arrive, so a large but
/// steadily-progressing download (e.g. a multi-megabyte Debian `Packages` index
/// over slow QEMU user-net NAT) is not killed by a fixed total-transfer
/// deadline — only a genuine stall (no bytes for this long) aborts the fetch.
const READ_TIMEOUT_TICKS: u64 = 1500;
/// Hard upper bound on pump iterations, a safety net independent of the clock.
const MAX_STEPS: u32 = 2_000_000;
/// Upper bound on the bytes we will buffer for one response (head + body), to
/// keep the fetch memory-bounded regardless of the advertised `Content-Length`.
const MAX_TOTAL: usize = 32 * 1024 * 1024;

/// rx/tx smoltcp socket buffer sizes for the cleartext HTTP fetch.
///
/// THROUGHPUT: like the TLS path, the cleartext index/`.deb` download is bounded
/// by the TCP receive window over QEMU user-net NAT (`window / RTT`). A 256 KiB
/// receive window (smoltcp 0.12 negotiates window scaling for buffers > 64 KiB)
/// lets far more data be in flight per round trip, which also benefits the
/// local-mirror path. tx stays modest (16 KiB): we only send a small GET.
const HTTP_RX_BYTES: usize = 256 * 1024;
const HTTP_TX_BYTES: usize = 16 * 1024;

/// Download a single `.deb` from `host:port` at `path` over HTTP/1.1 (R8.1–R8.7).
///
/// A thin wrapper over [`http_get`]: it performs the same HTTP GET and wraps the
/// returned body in [`DebBytes`]. `host` may be an IPv4 literal or a hostname
/// (resolved via DNS). Default `.deb` mirror port is 80.
pub fn fetch_deb(host: &str, port: u16, path: &str) -> Result<DebBytes, FetchError> {
    http_get(host, port, path).map(DebBytes)
}

/// Perform one HTTP/1.1 `GET` of `http://host:port/path` and return the
/// `Content-Length`-sized response body (R8.1–R8.7).
///
/// `host` may be a dotted-quad IPv4 literal or a hostname; it is resolved to an
/// address via [`crate::net::resolve`] (DNS), while the request's `Host:` header
/// always carries the original `host` string (never the resolved IP). On success
/// returns the body bytes; on any failure the partial buffer is discarded, the
/// TCP socket is released, and exactly one structured diagnostic is emitted
/// (`component=Package_Fetcher`, stage, resource).
pub fn http_get(host: &str, port: u16, path: &str) -> Result<Vec<u8>, FetchError> {
    // R8.7: with no assigned interface address, fail WITHOUT attempting a
    // connection.
    if super::ip_config().is_none() {
        warn!(
            "Package_Fetcher: stage=preflight host={} path={} cause=NoNetwork (no interface address)",
            host, path
        );
        return Err(FetchError::NoNetwork);
    }

    // Resolve the host to an address. Accepts an IPv4 literal directly or a
    // hostname via DNS; a resolution failure is reported as a connect failure
    // without opening a socket.
    let addr = match super::resolve(host) {
        Some(a) => a,
        None => {
            error!(
                "Package_Fetcher: stage=resolve host={} path={} cause=ConnectTimeout (could not resolve host)",
                host, path
            );
            return Err(FetchError::ConnectTimeout);
        }
    };
    let remote = IpEndpoint::new(IpAddress::Ipv4(addr), port);

    // Open the outbound TCP socket via the existing net primitive, with a large
    // receive window (THROUGHPUT, see HTTP_RX_BYTES). `tcp_connect_buffered`
    // already removes the socket on a synchronous connect error.
    let handle = match super::tcp_connect_buffered(remote, HTTP_RX_BYTES, HTTP_TX_BYTES) {
        Ok(h) => h,
        Err(_) => {
            error!(
                "Package_Fetcher: stage=connect host={} path={} cause=ConnectTimeout (socket open failed)",
                host, path
            );
            return Err(FetchError::ConnectTimeout);
        }
    };

    let connect_deadline = scheduler::ticks() + CONNECT_TIMEOUT_TICKS;
    let mut read_deadline: Option<u64> = None;

    let mut established = false;
    let mut sent = false;
    let mut buf: Vec<u8> = Vec::new();
    let mut head: Option<(usize, u64)> = None; // (body_off, content_length)
    let mut tmp = [0u8; 2048];
    // High-water mark of received bytes, used to re-arm the idle read timeout
    // whenever the download makes progress.
    let mut last_len: usize = 0;
    // Last whole-MiB mark we logged, so progress is reported ~every 1 MiB.
    let mut logged_mib: usize = 0;

    for _ in 0..MAX_STEPS {
        // Bytes buffered before this pump step; compared after the drain to tell
        // whether THIS iteration moved any new data (drives the progress-aware
        // inter-step wait below).
        let len_before = buf.len();

        // One pump step under the NET lock; the socket borrow ends with the block.
        let outcome = {
            let mut guard = NET.lock();
            let state = match guard.as_mut() {
                Some(s) => s,
                // Interface disappeared mid-fetch: discard and report incomplete
                // (the socket is gone with the state).
                None => return fail_after(FetchError::Incomplete, host, path),
            };

            let ts = now();
            let _ = state.iface.poll(ts, &mut state.device, &mut state.sockets);

            let sock = state.sockets.get_mut::<tcp::Socket>(handle);

            if sock.is_active() {
                established = true;
            }

            let mut step = Step::Continue;

            if !established {
                // Reaching Closed before we ever became active means the
                // connection was refused / unreachable.
                if sock.state() == tcp::State::Closed {
                    step = Step::Fail(FetchError::ConnectTimeout);
                }
            } else {
                // Send the request exactly once, as soon as tx is writable. The
                // `Host:` header carries the original hostname, not the IP.
                if !sent && sock.can_send() {
                    let req = build_get_request(host, path);
                    let _ = sock.send_slice(&req);
                    sent = true;
                }

                // Drain whatever is available into the accumulation buffer.
                if sock.can_recv() {
                    loop {
                        match sock.recv_slice(&mut tmp) {
                            Ok(0) => break,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                            }
                            Err(_) => break,
                        }
                        if buf.len() > MAX_TOTAL {
                            break;
                        }
                    }
                }

                if buf.len() > MAX_TOTAL {
                    step = Step::Fail(FetchError::Incomplete);
                }

                // Parse the response head once it is complete.
                if matches!(step, Step::Continue) && head.is_none() {
                    match parse_http_head(&buf) {
                        HeadParse::Need => {}
                        HeadParse::Malformed => {
                            step = Step::Fail(FetchError::Incomplete);
                        }
                        HeadParse::Done {
                            status,
                            content_length,
                            body_off,
                        } => {
                            if status != 200 {
                                // R8.5: non-200 -> Status(code), discard.
                                step = Step::Fail(FetchError::Status(status));
                            } else if let Some(cl) = content_length {
                                head = Some((body_off, cl));
                            } else {
                                // R8.3: 200 without parseable Content-Length.
                                step = Step::Fail(FetchError::UnknownLength);
                            }
                        }
                    }
                }

                // Body complete? (R8.2)
                if matches!(step, Step::Continue) {
                    if let Some((body_off, cl)) = head {
                        let received = (buf.len().saturating_sub(body_off)) as u64;
                        if received >= cl {
                            let end = body_off + cl as usize;
                            let body = buf[body_off..end].to_vec();
                            step = Step::Done(body);
                        }
                    }
                }

                // Peer closed and rx is drained before we completed: incomplete
                // download (R8.4), discard partial.
                if matches!(step, Step::Continue)
                    && sent
                    && !sock.may_recv()
                    && !sock.can_recv()
                {
                    step = Step::Fail(FetchError::Incomplete);
                }
            }

            step
        };

        match outcome {
            Step::Done(body) => {
                release(handle);
                return Ok(body);
            }
            Step::Fail(err) => {
                // The socket is released and the partial buffer (dropped with
                // this function's `buf`) is discarded on every failure path.
                release(handle);
                emit_failure(&err, host, path);
                return Err(err);
            }
            Step::Continue => {}
        }

        // Arm the read deadline once the connection is established, and re-arm
        // it (idle/inactivity timeout) whenever new body bytes have arrived, so
        // a steadily-progressing large download is not aborted mid-transfer.
        if established {
            if buf.len() > last_len {
                last_len = buf.len();
                read_deadline = Some(scheduler::ticks() + READ_TIMEOUT_TICKS);

                // OBSERVABILITY: emit a progress line roughly every 1 MiB of
                // received bytes so cleartext download throughput is measurable
                // from the serial log and a slow transfer is distinguishable from
                // a hang. Kept coarse (per ~MiB, not per packet).
                let mib = buf.len() / (1024 * 1024);
                if mib > logged_mib {
                    logged_mib = mib;
                    info!("net: downloaded {} KiB (http body)", buf.len() / 1024);
                }
            } else if read_deadline.is_none() {
                read_deadline = Some(scheduler::ticks() + READ_TIMEOUT_TICKS);
            }
        }

        // R8.6: connect timeout -> release the socket and report.
        if !established && scheduler::ticks() >= connect_deadline {
            release(handle);
            let err = FetchError::ConnectTimeout;
            emit_failure(&err, host, path);
            return Err(err);
        }

        // R8.4: read timeout before the body completed -> discard, release.
        if let Some(deadline) = read_deadline {
            if scheduler::ticks() >= deadline {
                release(handle);
                let err = FetchError::ReadTimeout;
                emit_failure(&err, host, path);
                return Err(err);
            }
        }

        // Release the lock and let time / QEMU advance between steps. The wait
        // is PROGRESS-AWARE, which is the timing direction that made the stall
        // disappear:
        //   * if this step drained new bytes, the transfer is active — do NOT
        //     halt; continue immediately (a tiny `spin_loop` only) so we keep
        //     draining the 256 KiB receive window at full speed;
        //   * otherwise rx was empty this step (idle wait) — `sleep_ticks(1)`
        //     HALTS the CPU until the next 100 Hz tick, letting virtio-net RX
        //     servicing, device IRQs, and the background `net_thread` deliver the
        //     next packets instead of being starved by a busy-spin.
        // With `sleep_ticks` the loop advances ~real time, so the established/
        // connect/read deadlines and the MAX_STEPS safety net all still hold.
        let made_progress = buf.len() > len_before;
        if made_progress {
            for _ in 0..256 {
                core::hint::spin_loop();
            }
        } else {
            scheduler::sleep_ticks(1);
        }
    }

    // Step budget exhausted: treat as a timeout, release and report.
    release(handle);
    let err = if established {
        FetchError::ReadTimeout
    } else {
        FetchError::ConnectTimeout
    };
    emit_failure(&err, host, path);
    Err(err)
}

/// The result of a single socket pump step.
enum Step {
    /// Keep pumping.
    Continue,
    /// The full body was collected.
    Done(Vec<u8>),
    /// Terminal failure (the socket has already been removed).
    Fail(FetchError),
}

/// Emit the failure diagnostic and return the corresponding `Err`. Used on the
/// rare mid-fetch teardown path where the interface state has vanished.
fn fail_after(err: FetchError, host: &str, path: &str) -> Result<Vec<u8>, FetchError> {
    emit_failure(&err, host, path);
    Err(err)
}

/// Release a still-open socket handle from the shared socket set. Idempotent: a
/// no-op if the interface or socket is already gone.
fn release(handle: smoltcp::iface::SocketHandle) {
    if let Some(state) = NET.lock().as_mut() {
        state.sockets.remove(handle);
    }
}

/// Emit exactly one structured diagnostic for a fetch failure (R12.4, R12.5):
/// component name, failed stage, resource (host/path), and cause category.
fn emit_failure(err: &FetchError, host: &str, path: &str) {
    match err {
        FetchError::NoNetwork => warn!(
            "Package_Fetcher: stage=preflight host={} path={} cause=NoNetwork",
            host, path
        ),
        FetchError::ConnectTimeout => error!(
            "Package_Fetcher: stage=connect host={} path={} cause=ConnectTimeout",
            host, path
        ),
        FetchError::Status(code) => error!(
            "Package_Fetcher: stage=response host={} path={} cause=Status({})",
            host, path, code
        ),
        FetchError::UnknownLength => error!(
            "Package_Fetcher: stage=response host={} path={} cause=UnknownLength",
            host, path
        ),
        FetchError::Incomplete => error!(
            "Package_Fetcher: stage=body host={} path={} cause=Incomplete",
            host, path
        ),
        FetchError::ReadTimeout => error!(
            "Package_Fetcher: stage=body host={} path={} cause=ReadTimeout",
            host, path
        ),
        FetchError::Tls(stage) => error!(
            "Package_Fetcher: stage=tls:{} host={} path={} cause=Tls (INSECURE: no cert verification)",
            stage, host, path
        ),
    }
}
