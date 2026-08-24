//! Own TCP implementation (RFC 793 + modern practice subset).
//!
//! What is implemented (honest scope):
//!   * Full connection state machine: CLOSED/LISTEN/SYN-SENT/SYN-RCVD/
//!     ESTABLISHED/FIN-WAIT-1/FIN-WAIT-2/CLOSE-WAIT/LAST-ACK/CLOSING/TIME-WAIT.
//!   * Active open (`connect`) and passive open (`listen`, multiple concurrent
//!     children per listener).
//!   * Sliding-window flow control WITH window scaling (RFC 7323), MSS
//!     negotiation, immediate ACKs (no delayed-ACK).
//!   * Retransmission with Jacobson/Karels RTO estimation, Karn's algorithm,
//!     exponential backoff, persist-style zero-window probing, and a bounded
//!     retry budget per phase (connect give-up => refused).
//!   * Congestion control: slow start + congestion avoidance (Reno), fast
//!     retransmit on three duplicate ACKs. No SACK yet (documented gap; Reno
//!     recovery remains correct without it, just less efficient under loss).
//!   * Out-of-order receive queue with duplicate-ACK generation.
//!   * Graceful close both directions; RST handling/generation; TIME-WAIT with
//!     a shortened (2 s) 2*MSL appropriate for a hobby kernel.
//!
//! Deliberate simplifications (documented, safe over sane paths):
//!   * Segments starting BEFORE rcv_nxt are treated as pure duplicates and
//!     answered with a fresh ACK (no partial-overlap coalescing).
//!   * No SACK / timestamps options.
//!   * The urgent pointer is parsed and ignored.
//!
//! Every entry point runs under the stack lock from thread context (net poll
//! thread or syscall threads calling the `net::` primitives), so no internal
//! locking is needed here.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::wire::{self, tcp_build, tcp_parse, IpAddr, IpEndpoint};
use super::Stack;

/// Delayed-ACK policy (RFC 1123 §4.2.3.1 permits holding one segment's ACK):
/// emit an ACK every `N` in-order segments. Each outgoing frame is a VM exit
/// under QEMU, so halving the ACK rate measurably increases throughput; the
/// tail guard below bounds any hold-off to [`TAIL_ACK_DELAY_TICKS`].
const ACK_EVERY_N: u8 = 2;
/// Max ticks an in-order segment may wait for a coalesced ACK.
const TAIL_ACK_DELAY_TICKS: u64 = 16;

/// Maximum segment size we announce/use (Ethernet MTU 1500 - IP 20 - TCP 20).
const MSS: usize = 1460;
/// Peer MSS default when the option is absent (RFC 1122 minimum).
const DEFAULT_PEER_MSS: u16 = 536;
/// Initial RTO.
const RTO_INIT_MS: u32 = 300;
/// Maximum RTO backoff.
const RTO_MAX_MS: u32 = 30_000;
/// Retries before a connect gives up (~tens of seconds worst case; callers
/// impose their own shorter deadlines on top).
const CONNECT_MAX_RETRIES: u32 = 5;
/// Retries before an established connection is declared dead.
const DATA_MAX_RETRIES: u32 = 15;
/// TIME-WAIT duration (2*MSL shortened; see module docs).
const TIME_WAIT_TICKS: u64 = 2_000;
/// Initial congestion window in bytes (4 MSS).
const IW_BYTES: u32 = 4 * MSS as u32;
/// Cap on buffered out-of-order segments per connection.
const OOO_CAP: usize = 16;

// ─── Sequence helpers (wrapping-safe) ────────────────────────────────────────

fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}
fn seq_le(a: u32, b: u32) -> bool {
    a == b || seq_lt(a, b)
}
#[allow(dead_code)]
fn seq_gt(a: u32, b: u32) -> bool {
    seq_lt(b, a)
}

// ─── State ───────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Closed,
    Listen,
    SynSent,
    SynRcvd,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    LastAck,
    Closing,
    TimeWait,
}

impl State {
    /// Connection carries data in either direction.
    pub fn is_data_state(self) -> bool {
        matches!(
            self,
            State::Established | State::FinWait1 | State::FinWait2 | State::CloseWait
        )
    }
}

// ─── Socket ──────────────────────────────────────────────────────────────────

pub struct TcpSock {
    pub(crate) state: State,
    pub(crate) local_port: u16,
    pub(crate) remote: IpEndpoint,

    // Send side
    iss: u32,
    snd_una: u32,
    snd_nxt: u32,
    snd_wnd: u32, // peer-advertised window in BYTES (scale applied)
    snd_scale: u8,
    ctrl_needs_send: bool, // SYN / SYN-ACK not yet transmitted
    close_requested: bool,
    fin_seq: Option<u32>,

    // Receive side
    irs: u32,
    rcv_nxt: u32,
    rcv_scale: u8,
    peer_mss: u16,
    peer_fin_seq: Option<u32>,
    peer_fin_seen: bool,

    // Buffers
    tx: VecDeque<u8>,
    rx: VecDeque<u8>,
    tx_cap: usize,
    rx_cap: usize,
    ooo: Vec<(u32, Vec<u8>)>,

    // Bookkeeping
    ever_established: bool,
    /// In-order segments received since the last emitted ACK.
    unacked_segs: u8,
    /// Tick of the last emitted ACK (tail-guard for the delayed-ACK policy).
    last_ack_tick: u64,
    /// Free space at the moment the last segment advertised a window.
    last_adv_free: usize,
    /// App drained enough buffer to warrant an immediate window-update ACK.
    wnd_update_pending: bool,
    refused: bool,

    // SACK (RFC 2013/2883)
    /// Both sides advertised SACK-Permitted during handshake.
    sack_ok: bool,
    /// We included SACK-Permitted in our SYN / SYN-ACK.
    offered_sack: bool,
    /// Received SACKed ranges [start, end) above snd_una, sorted, disjoint.
    sacked: Vec<(u32, u32)>,

    // Timers / congestion
    cwnd: u32,
    ssthresh: u32,
    dup_acks: u8,
    rto_ms: u32,
    rto_deadline: Option<u64>,
    retries: u32,
    rtt_srtt_ms: Option<u32>,
    rtt_var_ms: u32,
    timing_end: Option<u32>,
    timing_start: Option<u64>,
    timewait_deadline: Option<u64>,
}

impl TcpSock {
    fn new_active(
        remote: IpEndpoint,
        local_port: u16,
        rx_cap: usize,
        tx_cap: usize,
        iss: u32,
    ) -> Self {
        TcpSock {
            state: State::SynSent,
            local_port,
            remote,
            iss,
            snd_una: iss,
            snd_nxt: iss.wrapping_add(1), // SYN consumes one sequence number
            snd_wnd: 4096,
            snd_scale: 0,
            ctrl_needs_send: true,
            close_requested: false,
            fin_seq: None,
            irs: 0,
            rcv_nxt: 0,
            rcv_scale: Self::offer_scale(rx_cap),
            peer_mss: DEFAULT_PEER_MSS,
            peer_fin_seq: None,
            peer_fin_seen: false,
            tx: VecDeque::with_capacity(core::cmp::min(tx_cap, 1024)),
            rx: VecDeque::with_capacity(core::cmp::min(rx_cap, 1024)),
            tx_cap,
            rx_cap,
            ooo: Vec::new(),
            ever_established: false,
            unacked_segs: 0,
            last_ack_tick: 0,
            last_adv_free: 0,
            wnd_update_pending: false,
            refused: false,
            sack_ok: false,
            offered_sack: true,
            sacked: Vec::new(),
            cwnd: IW_BYTES,
            ssthresh: 65535,
            dup_acks: 0,
            rto_ms: RTO_INIT_MS,
            rto_deadline: None,
            retries: 0,
            rtt_srtt_ms: None,
            rtt_var_ms: 0,
            timing_end: None,
            timing_start: None,
            timewait_deadline: None,
        }
    }

    fn new_listen(local_port: u16) -> Self {
        let mut s = Self::new_active(
            IpEndpoint::new(IpAddr::V4(wire::Ipv4Addr::UNSPECIFIED), 0),
            local_port,
            4096,
            4096,
            1,
        );
        s.state = State::Listen;
        s.ctrl_needs_send = false;
        s
    }

    fn new_child(listener_port: u16, remote: IpEndpoint, irs: u32, iss: u32) -> Self {
        let mut s = Self::new_active(remote, listener_port, 4096, 4096, iss);
        s.state = State::SynRcvd;
        s.irs = irs;
        s.rcv_nxt = irs.wrapping_add(1);
        s.snd_una = iss;
        s.snd_nxt = iss.wrapping_add(1);
        s
    }

    /// Window-scale factor offered for `rx_cap`: smallest s with
    /// `(rx_cap >> s) <= 65535`, capped per RFC 7323.
    fn offer_scale(rx_cap: usize) -> u8 {
        let mut s = 0u8;
        let mut cap = rx_cap >> 8; // work in announced-field domain
        while cap > 255 && s < 14 {
            cap >>= 1;
            s += 1;
        }
        s
    }

    // ── Consumer-facing API (mirrors the old smoltcp Socket surface) ──

    pub fn state(&self) -> State {
        self.state
    }

    /// True when the connection is anything but fully closed/listening.
    pub fn is_active(&self) -> bool {
        !matches!(self.state, State::Closed | State::Listen)
    }

    /// May we enqueue more transmit bytes?
    pub fn may_send(&self) -> bool {
        matches!(
            self.state,
            State::SynRcvd
                | State::Established
                | State::FinWait1
                | State::FinWait2
                | State::CloseWait
                | State::LastAck
                | State::Closing
        )
    }

    /// May more receive bytes still arrive (peer half open)?
    pub fn may_recv(&self) -> bool {
        matches!(
            self.state,
            State::Established | State::FinWait1 | State::FinWait2
        )
    }

    /// Transmit buffer has room and the connection is up.
    pub fn can_send(&self) -> bool {
        self.may_send() && self.tx.len() < self.tx_cap
    }

    /// Received bytes are buffered.
    pub fn can_recv(&self) -> bool {
        !self.rx.is_empty()
    }

    /// Enqueue up to `data.len()` bytes. Returns how many were accepted.
    pub fn send_slice(&mut self, data: &[u8]) -> usize {
        if !self.can_send() {
            return 0;
        }
        let room = self.tx_cap - self.tx.len();
        let n = core::cmp::min(room, data.len());
        for &b in &data[..n] {
            self.tx.push_back(b);
        }
        n
    }

    /// Drain received bytes into `dst`. Returns bytes copied. Flags a
    /// window-update ACK when the app freed a meaningful chunk of the receive
    /// buffer — otherwise the peer only learns the window reopened after its
    /// (multi-second) persist probe.
    pub fn recv_slice(&mut self, dst: &mut [u8]) -> usize {
        let n = core::cmp::min(dst.len(), self.rx.len());
        for slot in dst.iter_mut().take(n) {
            *slot = self.rx.pop_front().unwrap_or(0);
        }
        if n > 0 && !self.wnd_update_pending {
            let free = self.rx_cap - self.rx.len();
            if free.saturating_sub(self.last_adv_free) >= self.rx_cap / 4 {
                self.wnd_update_pending = true;
            }
        }
        n
    }

    /// Peer half-closed and nothing left to read: read(2) sees EOF.
    pub fn eof_visible(&self) -> bool {
        self.peer_fin_seen && self.rx.is_empty()
    }

    /// App requests graceful close. The FIN goes out once TX drains.
    pub fn close(&mut self) {
        if self.state == State::Closed || self.fin_seq.is_some() {
            return;
        }
        self.close_requested = true;
    }

    /// Outstanding unacked DATA bytes (excludes a possibly-unacked FIN).
    fn outstanding_data(&self) -> u32 {
        let raw = self.snd_nxt.wrapping_sub(self.snd_una);
        let fin_pending = self
            .fin_seq
            .map(|f| seq_le(self.snd_una, f))
            .unwrap_or(false);
        if fin_pending {
            raw.saturating_sub(1)
        } else {
            raw
        }
    }

    /// Advertised receive-window field for the next outgoing segment.
    fn rcv_wnd_field(&self) -> u16 {
        let free = self.rx_cap - self.rx.len();
        if self.rcv_scale > 0 {
            (((free >> self.rcv_scale) as u32).max(1)).min(u16::MAX as u32) as u16
        } else {
            free.min(u16::MAX as usize) as u16
        }
    }

    /// Our negotiated MSS (bounded by the peer's announcement).
    fn effective_mss(&self) -> usize {
        core::cmp::min(MSS, self.peer_mss as usize)
    }

    /// Record peer-reported SACK blocks (clipped to unacked space), keeping
    /// the list sorted and disjoint.
    fn note_sacked(&mut self, blocks: &wire::heapless_shim::Vec<(u32, u32), 4>) {
        let mut changed = false;
        for &(l, r) in blocks.iter() {
            // Ignore anything not inside the unacked window.
            if seq_le(r, self.snd_una) || seq_lt(self.snd_nxt, l) {
                continue;
            }
            let l = if seq_lt(l, self.snd_una) {
                self.snd_una
            } else {
                l
            };
            let r = if seq_lt(self.snd_nxt, r) {
                self.snd_nxt
            } else {
                r
            };
            self.sacked.push((l, r));
            changed = true;
        }
        if changed {
            // Normalize: sort by left edge, merge overlapping/adjacent ranges.
            self.sacked.sort_by_key(|&(a, _)| a);
            let mut merged: Vec<(u32, u32)> = Vec::with_capacity(self.sacked.len());
            for &(l, r) in &self.sacked {
                match merged.last_mut() {
                    Some(last) if !seq_gt(l, last.1) => {
                        if seq_gt(r, last.1) {
                            last.1 = r;
                        }
                    }
                    _ => merged.push((l, r)),
                }
            }
            self.sacked = merged;
        }
        self.prune_sacked();
    }

    fn prune_sacked(&mut self) {
        // Drop ranges fully below snd_una; clip the straggler.
        self.sacked.retain(|&(_, r)| seq_gt(r, self.snd_una));
        for range in self.sacked.iter_mut() {
            if seq_lt(range.0, self.snd_una) {
                range.0 = self.snd_una;
            }
        }
    }

    /// Byte offset (relative to snd_una) of the first unacked, non-SACKed
    /// byte — the retransmission target. `None` when everything outstanding
    /// is already covered by SACK blocks.
    fn first_hole(&self) -> Option<usize> {
        let total = self.outstanding_data() as usize;
        let mut off = 0usize;
        // Walk sacked ranges (sorted, disjoint); find the first gap.
        for &(l, r) in self.sacked.iter() {
            let start = l.wrapping_sub(self.snd_una) as usize;
            let end = r.wrapping_sub(self.snd_una) as usize;
            if start > off {
                return Some(off.min(total)); // gap before this range
            }
            off = end.max(off);
        }
        if off < total {
            Some(off)
        } else {
            None // fully covered by SACK
        }
    }

    // Read-only accessors for diagnostics / connection-state checks.

    pub(crate) fn refused_flag(&self) -> bool {
        self.refused
    }

    pub(crate) fn ever_established(&self) -> bool {
        self.ever_established
    }
}

// ─── Table ───────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct TcpTable {
    socks: Vec<Option<TcpSock>>,
}

impl TcpTable {
    pub const fn new() -> Self {
        TcpTable { socks: Vec::new() }
    }

    fn alloc_slot(&mut self, sock: TcpSock) -> usize {
        for (i, s) in self.socks.iter().enumerate() {
            if s.is_none() {
                self.socks[i] = Some(sock);
                return i;
            }
        }
        self.socks.push(Some(sock));
        self.socks.len() - 1
    }

    pub(crate) fn get_mut(&mut self, h: usize) -> Option<&mut TcpSock> {
        self.socks.get_mut(h)?.as_mut()
    }

    pub(crate) fn get(&self, h: usize) -> Option<&TcpSock> {
        self.socks.get(h)?.as_ref()
    }

    /// Remove a slot explicitly (idempotent).
    pub(crate) fn remove(&mut self, h: usize) {
        if let Some(s) = self.socks.get_mut(h) {
            *s = None;
        }
    }

    #[allow(dead_code)]
    pub(crate) fn contains(&self, h: usize) -> bool {
        self.socks.get(h).map(|s| s.is_some()).unwrap_or(false)
    }

    /// Live handle list (diagnostics + echo service iteration).
    pub(crate) fn handles(&self) -> Vec<usize> {
        self.socks
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_some())
            .map(|(i, _)| i)
            .collect()
    }

    /// Is `port` used by any live socket (listeners included)?
    pub(crate) fn port_in_use(&self, port: u16) -> bool {
        self.socks
            .iter()
            .flatten()
            .any(|s| s.local_port == port && s.state != State::Closed)
    }

    /// Open an active connection. The SYN goes out on the next poll.
    /// `local_port` must be free (caller allocates it while holding the stack
    /// lock — this function may be called with the lock held).
    pub(crate) fn connect(
        &mut self,
        remote: IpEndpoint,
        local_port: u16,
        rx_bytes: usize,
        tx_bytes: usize,
    ) -> Result<usize, ()> {
        if self.port_in_use(local_port) {
            return Err(());
        }
        let iss = super::random_u32();
        let sock = TcpSock::new_active(
            remote,
            local_port,
            rx_bytes.max(512),
            tx_bytes.max(512),
            iss,
        );
        Ok(self.alloc_slot(sock))
    }

    /// Start listening on `port`. Children are created per incoming SYN.
    pub(crate) fn listen(&mut self, port: u16) -> Result<usize, ()> {
        for s in self.socks.iter().flatten() {
            if s.state == State::Listen && s.local_port == port {
                return Err(()); // duplicate listener
            }
        }
        Ok(self.alloc_slot(TcpSock::new_listen(port)))
    }

    /// Reap sockets that finished their lifetimes (TIME-WAIT expiry included).
    fn reap_closed(&mut self, now: u64) {
        for slot in self.socks.iter_mut().flatten() {
            if slot.state == State::TimeWait {
                if let Some(d) = slot.timewait_deadline {
                    if now >= d {
                        slot.state = State::Closed;
                    }
                } else {
                    slot.timewait_deadline = Some(now + TIME_WAIT_TICKS);
                }
            }
        }
        for i in 0..self.socks.len() {
            let dead = matches!(&self.socks[i], Some(s) if s.state == State::Closed);
            if dead {
                self.socks[i] = None;
            }
        }
    }

    // ── Ingress dispatch ──

    pub(crate) fn input(st: &mut Stack, src_ip: IpAddr, dst_ip: IpAddr, seg: &[u8]) {
        let now = crate::task::scheduler::ticks();
        let Some((hdr, body)) = tcp_parse(src_ip, dst_ip, seg) else {
            return;
        };
        let src_ep = IpEndpoint::new(src_ip, hdr.src_port);
        let dst_port = hdr.dst_port;

        // Exact-match connection first, then a listener.
        let exact = st.tcp.socks.iter().position(|s| {
            matches!(s.as_ref(), Some(k)
                if k.local_port == dst_port
                    && k.remote.addr == src_ip
                    && k.remote.port == hdr.src_port
                    && k.state != State::Listen)
        });
        let target = match exact {
            Some(i) => Some((i, false)),
            None => st
                .tcp
                .socks
                .iter()
                .position(|s| {
                    matches!(s.as_ref(), Some(k)
                        if k.state == State::Listen && k.local_port == dst_port)
                })
                .map(|i| (i, true)),
        };

        match target {
            None => {
                // No socket: reply RST unless the segment itself was RST.
                if hdr.flags & wire::TCP_RST == 0 {
                    let (seq, ack) = if hdr.flags & wire::TCP_ACK != 0 {
                        (0u32, hdr.seq)
                    } else {
                        (
                            hdr.ack,
                            hdr.seq.wrapping_add(
                                body.len() as u32 + (hdr.flags & wire::TCP_SYN != 0) as u32,
                            ),
                        )
                    };
                    let rst = tcp_build(
                        IpEndpoint::new(dst_ip, dst_port),
                        src_ep,
                        seq,
                        ack,
                        wire::TCP_RST | wire::TCP_ACK,
                        0,
                        &[],
                        &[],
                    );
                    super::ip::output(st, None, src_ip, wire::PROTO_TCP, &rst, now);
                }
            }
            Some((idx, is_listener)) => {
                if is_listener {
                    listener_input(st, idx, src_ep, &hdr, body, now);
                } else {
                    connected_input(st, idx, src_ep, &hdr, body, now);
                }
            }
        }
    }

    /// Per-poll output pass: transmit new segments, FINs, retransmits.
    ///
    /// Free function over the whole stack so per-socket stepping can reach the
    /// IP/ARP layers below without splitting borrows (the socket table lives
    /// INSIDE the stack).
    pub(crate) fn poll_all(st: &mut Stack, now: u64) {
        let count = st.tcp.socks.len();
        for h in 0..count {
            if st.tcp.socks[h].is_some() {
                step_sock(st, h, now);
            }
        }
        st.tcp.reap_closed(now);
    }
}

// ─── Segment emission ────────────────────────────────────────────────────────

/// Emit one segment for socket `h`.
fn emit(st: &mut Stack, h: usize, seq: u32, ack: u32, flags: u8, opts: &[u8], payload: &[u8]) {
    let Some(s) = st.tcp.get_mut(h) else { return };
    // Source family follows the remote: a v6 peer answers from our v6 address.
    let src_addr = match s.remote.addr {
        IpAddr::V4(_) => IpAddr::V4(
            st.cidr
                .map(|c| c.addr)
                .unwrap_or(wire::Ipv4Addr::UNSPECIFIED),
        ),
        IpAddr::V6(_) => {
            if s.remote.addr.to_v4_mapped().is_none() && !s.remote.addr.is_ipv4() {
                match &s.remote.addr {
                    IpAddr::V6(v6) if v6.is_link_local() => IpAddr::V6(st.ll_addr6),
                    _ => IpAddr::V6(st.cidr6.map(|c| c.addr).unwrap_or(st.ll_addr6)),
                }
            } else {
                IpAddr::V4(
                    st.cidr
                        .map(|c| c.addr)
                        .unwrap_or(wire::Ipv4Addr::UNSPECIFIED),
                )
            }
        }
    };
    let src = IpEndpoint::new(src_addr, s.local_port);
    let dst = s.remote;
    let wnd = s.rcv_wnd_field();
    s.last_adv_free = s.rx_cap - s.rx.len();
    let seg = tcp_build(src, dst, seq, ack, flags, wnd, opts, payload);
    let now = crate::task::scheduler::ticks();
    super::ip::output(st, None, dst.addr, wire::PROTO_TCP, &seg, now);
}

/// Regular segments carry no options (kept as a hook for timestamps).
fn no_opts() -> alloc::vec::Vec<u8> {
    alloc::vec::Vec::new()
}

/// SYN / SYN-ACK options: MSS (+ window scale when offered).
/// MUST stay a multiple of 4 bytes: MSS alone is 4; adding the 1-byte NOP +
/// 3-byte WS keeps 8. An unaligned block would push trailing bytes past the
/// declared data offset and they would surface as phantom payload on the wire.
/// Options for our SYN / SYN-ACK: MSS (+ WS + SACK-Permitted), padded to a
/// 4-byte boundary with NOPs.
fn ctrl_options(scale: u8) -> alloc::vec::Vec<u8> {
    let mut o = Vec::new();
    o.extend_from_slice(&[2, 4]);
    o.extend_from_slice(&(MSS as u16).to_be_bytes());
    if scale > 0 {
        o.extend_from_slice(&[1]); // NOP aligns the 3-byte WS option
        o.extend_from_slice(&[3, 3, scale]);
    }
    // SACK-Permitted (kind 4, 2 bytes).
    o.extend_from_slice(&wire::sack_permitted_option());
    while !o.len().is_multiple_of(4) {
        o.push(1); // NOP padding to alignment
    }
    debug_assert!(o.len().is_multiple_of(4));
    o
}

/// ACK-segment options carrying current SACK blocks (when negotiated).
fn ack_options_with_sack(s: &TcpSock) -> alloc::vec::Vec<u8> {
    if !s.sack_ok || s.ooo.is_empty() {
        return alloc::vec::Vec::new();
    }
    let mut edges: Vec<(u32, u32)> = s
        .ooo
        .iter()
        .map(|(sq, d)| (*sq, sq.wrapping_add(d.len() as u32)))
        .collect();
    edges.sort_by_key(|&(l, _)| l);
    // Merge overlapping/adjacent ranges.
    let mut merged: Vec<(u32, u32)> = Vec::with_capacity(edges.len());
    for &(l, r) in &edges {
        match merged.last_mut() {
            Some(last) if l <= last.1 => last.1 = last.1.max(r),
            _ => merged.push((l, r)),
        }
    }
    wire::sack_option(&merged)
}

// ─── Listener path ───────────────────────────────────────────────────────────

/// Answer a bare SYN by creating a child socket in SYN-RCVD + SYN-ACK.
fn listener_input(
    st: &mut Stack,
    listener_idx: usize,
    src_ep: IpEndpoint,
    hdr: &wire::TcpHeader,
    body: &[u8],
    _now: u64,
) {
    if hdr.flags & wire::TCP_SYN == 0 || hdr.flags & wire::TCP_ACK != 0 || !body.is_empty() {
        return; // listeners only react to bare SYNs
    }
    let port = st.tcp.get(listener_idx).unwrap().local_port;
    let iss = super::random_u32();

    let child_idx = {
        let mut child = TcpSock::new_child(port, src_ep, hdr.seq, iss);
        child.peer_mss = hdr.mss.unwrap_or(DEFAULT_PEER_MSS);
        // Scaling applies only when BOTH SYNs carried the option (RFC 7323):
        // the client's SYN had no WS option (this is the listener path), so
        // both scales stay 0 unless the peer offered one.
        child.sack_ok = child.offered_sack && hdr.sack_permitted;
        if let Some(w) = hdr.wscale {
            child.snd_scale = w.min(14);
            child.rcv_scale = TcpSock::offer_scale(4096);
        }
        st.tcp.alloc_slot(child)
    };

    let (iss2, scale) = {
        let s = st.tcp.get(child_idx).unwrap();
        (s.iss, s.rcv_scale)
    };
    let opts = ctrl_options(scale);
    emit(
        st,
        child_idx,
        iss2,
        hdr.seq.wrapping_add(1),
        wire::TCP_SYN | wire::TCP_ACK,
        &opts,
        &[],
    );
}

// ─── Connected-socket ingress ────────────────────────────────────────────────

fn connected_input(
    st: &mut Stack,
    h: usize,
    _src_ep: IpEndpoint,
    hdr: &wire::TcpHeader,
    body: &[u8],
    now: u64,
) {
    // RST: tear down unconditionally (seq validity checks are advisory here).
    if hdr.flags & wire::TCP_RST != 0 {
        let s = st.tcp.get_mut(h).unwrap();
        if !s.ever_established {
            s.refused = true;
        }
        s.state = State::Closed; // reaped at poll end
        return;
    }

    match st.tcp.get(h).unwrap().state {
        State::SynSent => synsent_input(st, h, hdr, body, now),
        State::SynRcvd => synrcvd_input(st, h, hdr, body, now),
        _ => established_input(st, h, hdr, body, now),
    }
}

fn synsent_input(st: &mut Stack, h: usize, hdr: &wire::TcpHeader, body: &[u8], now: u64) {
    if hdr.flags & wire::TCP_SYN != 0 && hdr.flags & wire::TCP_ACK != 0 {
        let s = st.tcp.get_mut(h).unwrap();
        if hdr.ack != s.iss.wrapping_add(1) {
            return; // wrong ACK number: ignore
        }
        s.irs = hdr.seq;
        s.rcv_nxt = hdr.seq.wrapping_add(1);
        s.snd_una = hdr.ack;
        s.snd_nxt = hdr.ack;
        s.peer_mss = hdr.mss.unwrap_or(DEFAULT_PEER_MSS);
        // SACK is enabled only when BOTH sides advertised it.
        s.sack_ok = s.offered_sack && hdr.sack_permitted;
        // Scaling is enabled only if the peer echoed the option.
        match hdr.wscale {
            Some(w) => s.snd_scale = w.min(14),
            None => {
                s.snd_scale = 0;
                s.rcv_scale = 0;
            }
        }
        s.ever_established = true;
        s.state = State::Established;
        s.rto_deadline = None;
        s.retries = 0;

        // ACK the handshake immediately.
        let ack_no = s.rcv_nxt;
        let opts = no_opts();
        let seq = s.snd_nxt;
        emit(st, h, seq, ack_no, wire::TCP_ACK, &opts, &[]);

        // Piggybacked payload (rare) is processed right away.
        if !body.is_empty() {
            established_input(st, h, hdr, body, now);
        }
        return;
    }
    if hdr.flags & wire::TCP_SYN != 0 {
        // Simultaneous open: move to SYN-RCVD and answer with SYN-ACK.
        let s = st.tcp.get_mut(h).unwrap();
        s.irs = hdr.seq;
        s.rcv_nxt = hdr.seq.wrapping_add(1);
        s.state = State::SynRcvd;
        s.ctrl_needs_send = true;
    }
}

fn synrcvd_input(st: &mut Stack, h: usize, hdr: &wire::TcpHeader, body: &[u8], now: u64) {
    if hdr.flags & wire::TCP_ACK != 0 {
        let s = st.tcp.get_mut(h).unwrap();
        if seq_le(s.snd_una, hdr.ack) && seq_le(hdr.ack, s.snd_nxt) {
            s.ever_established = true;
            s.state = State::Established;
            s.rto_deadline = None;
            s.retries = 0;
            if !body.is_empty() {
                established_input(st, h, hdr, body, now);
            }
        }
    }
}

/// What `accept_stream` wants done afterwards.
enum After {
    Nothing,
    SendAck,
}

/// Main data/ACK/FIN processing for an established-direction socket.
fn established_input(st: &mut Stack, h: usize, hdr: &wire::TcpHeader, body: &[u8], now: u64) {
    // ── ACK processing ──
    if hdr.flags & wire::TCP_ACK != 0 {
        process_ack(st, h, hdr, now);
    }

    // ── Data / FIN acceptance ──
    let has_payload = !body.is_empty();
    let has_fin = hdr.flags & wire::TCP_FIN != 0;
    let mut after = After::Nothing;
    if has_payload || has_fin {
        after = accept_stream(st, h, hdr.seq, body, has_fin);
    }

    // ── Post-processing: FIN consumption, OOO promotion ──
    consume_fin_and_ooo(st, h);

    // ── Immediate ACK policy ──
    let alive = matches!(
        st.tcp.get(h).map(|k| k.state),
        Some(State::Established)
            | Some(State::FinWait1)
            | Some(State::FinWait2)
            | Some(State::CloseWait)
    );
    if alive {
        if let After::SendAck = after {
            let ack_no = st.tcp.get(h).unwrap().rcv_nxt;
            let opts = ack_options_with_sack(st.tcp.get(h).unwrap());
            let seq = st.tcp.get(h).unwrap().snd_nxt;
            emit(st, h, seq, ack_no, wire::TCP_ACK, &opts, &[]);
        }
    }
}

/// Process a received ACK: slide the send window, learn the peer window,
/// update RTT/congestion state, drive FIN-related transitions.
fn process_ack(st: &mut Stack, h: usize, hdr: &wire::TcpHeader, now: u64) {
    let s = st.tcp.get_mut(h).unwrap();

    let valid = seq_lt(s.snd_una, hdr.ack) && seq_le(hdr.ack, s.snd_nxt);
    if valid {
        // Slide: release acknowledged TX bytes (bounded by buffered length —
        // any excess acknowledges the FIN).
        let advanced = hdr.ack.wrapping_sub(s.snd_una) as usize;
        let drop = advanced.min(s.tx.len());
        for _ in 0..drop {
            s.tx.pop_front();
        }
        s.snd_una = hdr.ack;

        // RTT sample (Karn-safe: samples are cancelled on retransmit).
        if let (Some(end), Some(t0)) = (s.timing_end, s.timing_start) {
            if !seq_lt(hdr.ack, end) {
                let rtt = now.saturating_sub(t0).min(u32::MAX as u64) as u32;
                match s.rtt_srtt_ms {
                    None => {
                        s.rtt_srtt_ms = Some(rtt.max(1));
                        s.rtt_var_ms = rtt / 2;
                    }
                    Some(prev) => {
                        let err = (rtt as i32) - (prev as i32);
                        s.rtt_var_ms = ((s.rtt_var_ms as i32)
                            + (err.abs() - (s.rtt_var_ms as i32)) / 4)
                            .max(1) as u32;
                        s.rtt_srtt_ms = Some(((prev as i32) + err / 8).max(1) as u32);
                    }
                }
                s.timing_end = None;
                s.timing_start = None;
            }
        }

        // Congestion control growth (only counts when data was outstanding).
        if s.outstanding_data() > 0 {
            if s.cwnd < s.ssthresh {
                s.cwnd = s.cwnd.saturating_add(s.effective_mss() as u32);
            } else {
                s.cwnd = s.cwnd.saturating_add(
                    ((s.effective_mss() as u32 * s.effective_mss() as u32) / s.cwnd.max(1)).max(1),
                );
            }
        }

        // Progress happened: reset the retransmit machinery, restart the timer
        // while data remains in flight.
        s.rto_deadline = None;
        s.retries = 0;
        s.dup_acks = 0;
        if s.outstanding_data() > 0 {
            s.rto_deadline = Some(now + s.rto_ms as u64);
        }

        // Fresh peer window (scaled).
        s.snd_wnd = (hdr.window as u32) << s.snd_scale;

        // Peer SACK blocks describe what IT received from us.
        if hdr.sack_blocks.iter().count() > 0 {
            s.note_sacked(&hdr.sack_blocks);
        } else {
            s.prune_sacked();
        }
    } else if hdr.ack == s.snd_una
        && hdr.flags & (wire::TCP_SYN | wire::TCP_FIN) == 0
        && s.outstanding_data() > 0
    {
        // Pure duplicate ACK.
        s.dup_acks = s.dup_acks.saturating_add(1);
        if s.dup_acks == 3 {
            // Fast retransmit: resend the first unacked segment.
            let n = s.effective_mss().min(s.tx.len()).min(u16::MAX as usize);
            if n > 0 {
                let mut payload = Vec::with_capacity(n);
                for b in s.tx.iter().take(n) {
                    payload.push(*b);
                }
                let seq = s.snd_una;
                let ack_no = s.rcv_nxt;
                s.ssthresh = (s.cwnd / 2).max(2 * s.effective_mss() as u32);
                s.cwnd = s.ssthresh;
                s.dup_acks = 0;
                s.rto_deadline = Some(now + s.rto_ms as u64);
                emit(
                    st,
                    h,
                    seq,
                    ack_no,
                    wire::TCP_ACK | wire::TCP_PSH,
                    &no_opts(),
                    &payload,
                );
                return;
            }
            s.dup_acks = 0;
        }
    } else {
        // Not a new ACK: still honour a fresh window advertisement and any
        // SACK information it carries.
        s.snd_wnd = (hdr.window as u32) << s.snd_scale;
        if hdr.sack_blocks.iter().count() > 0 {
            s.note_sacked(&hdr.sack_blocks);
        }
    }

    // FIN-acknowledgement driven transitions.
    let fin_covered = s.fin_seq.map(|f| seq_gt(hdr.ack, f)).unwrap_or(false);
    if fin_covered {
        match s.state {
            State::FinWait1 => s.state = State::FinWait2,
            State::Closing => {
                s.state = State::TimeWait;
                s.timewait_deadline = Some(now + TIME_WAIT_TICKS);
            }
            State::LastAck => s.state = State::Closed,
            _ => {}
        }
    }
}

/// Try to accept stream bytes + FIN from a segment starting at `seq`.
/// Returns whether an immediate ACK should be emitted.
fn accept_stream(st: &mut Stack, h: usize, seq: u32, body: &[u8], fin: bool) -> After {
    let s = st.tcp.get_mut(h).unwrap();
    if !s.state.is_data_state() {
        return After::Nothing;
    }
    let rcv = s.rcv_nxt;

    if seq == rcv {
        // In order: append what fits; anything beyond the buffer is left for
        // the peer to retransmit (we only ACK what we actually kept).
        let space = s.rx_cap - s.rx.len();
        let n = core::cmp::min(space, body.len());
        for &b in body.iter().take(n) {
            s.rx.push_back(b);
        }
        s.rcv_nxt = rcv.wrapping_add(n as u32);
        if fin {
            s.peer_fin_seq = Some(seq.wrapping_add(body.len() as u32));
            s.peer_fin_seen = true;
        }
        s.unacked_segs = s.unacked_segs.wrapping_add(1);
        if s.unacked_segs >= ACK_EVERY_N {
            s.unacked_segs = 0;
            s.last_ack_tick = crate::task::scheduler::ticks();
            let ack_no = s.rcv_nxt;
            let opts = ack_options_with_sack(s);
            let snd = s.snd_nxt;
            drop(s);
            emit(st, h, snd, ack_no, wire::TCP_ACK, &opts, &[]);
            After::Nothing
        } else {
            After::SendAck
        }
    } else if seq_lt(rcv, seq) {
        // Future segment: store out-of-order if it plausibly fits the flow.
        let off = seq.wrapping_sub(rcv) as usize;
        let buffered =
            s.rx_cap.saturating_sub(s.rx.len()) + s.ooo.iter().map(|(_, d)| d.len()).sum::<usize>();
        if off < buffered.max(1) && s.ooo.len() < OOO_CAP && !s.ooo.iter().any(|(sq, _)| *sq == seq)
        {
            s.ooo.push((seq, body.to_vec()));
        }
        // Duplicate-ACK nudges the peer to fill the gap; SACK blocks tell it
        // exactly which ranges arrived out of order.
        let ack_no = s.rcv_nxt;
        let opts = ack_options_with_sack(s);
        let snd = s.snd_nxt;
        drop(s);
        emit(st, h, snd, ack_no, wire::TCP_ACK, &opts, &[]);
        After::Nothing
    } else {
        // Segment starts before rcv_nxt. If it EXTENDS past rcv_nxt this is a
        // partial overlap: trim the already-received front and process the
        // remainder exactly like an in-order segment.
        let rel = rcv.wrapping_sub(seq) as usize;
        if rel < body.len() {
            let fin2 = fin; // FIN position is recomputed from the trimmed seq
            drop(s);
            return accept_stream(st, h, rcv, &body[rel..], fin2);
        }
        // Pure duplicate: re-ACK the current position.
        let ack_no = s.rcv_nxt;
        let opts = ack_options_with_sack(s);
        let snd = s.snd_nxt;
        drop(s);
        emit(st, h, snd, ack_no, wire::TCP_ACK, &opts, &[]);
        After::Nothing
    }
}

/// Consume the peer FIN once every preceding byte is delivered, promoting any
/// out-of-order segments that became in-order along the way.
fn consume_fin_and_ooo(st: &mut Stack, h: usize) {
    loop {
        let mut progressed = false;

        let s = st.tcp.get_mut(h).unwrap();
        // 1. FIN sitting exactly at the delivery boundary?
        if let Some(fpos) = s.peer_fin_seq {
            if s.rcv_nxt == fpos {
                s.rcv_nxt = fpos.wrapping_add(1);
                match s.state {
                    State::Established | State::SynRcvd => s.state = State::CloseWait,
                    State::FinWait1 => s.state = State::Closing,
                    State::FinWait2 => {
                        s.state = State::TimeWait;
                        s.timewait_deadline =
                            Some(crate::task::scheduler::ticks() + TIME_WAIT_TICKS);
                    }
                    _ => {}
                }
                progressed = true;
            }
        }

        // 2. An OOO segment landing exactly at rcv_nxt?
        let nxt = s.rcv_nxt;
        if let Some(pos) = s.ooo.iter().position(|(sq, _)| *sq == nxt) {
            let (_, data) = s.ooo.swap_remove(pos);
            let space = s.rx_cap - s.rx.len();
            let n = core::cmp::min(space, data.len());
            for &b in data.iter().take(n) {
                s.rx.push_back(b);
            }
            s.rcv_nxt = nxt.wrapping_add(n as u32);
            progressed = true;
        }

        if !progressed {
            break;
        }
    }
}

// ─── Per-poll egress stepping ────────────────────────────────────────────────

fn step_sock(st: &mut Stack, h: usize, now: u64) {
    timer_step(st, h, now);

    // Tail guard: never hold a lone segment's ACK longer than the delay bound.
    {
        let due = st
            .tcp
            .get(h)
            .map(|s| s.unacked_segs > 0 && now >= s.last_ack_tick + TAIL_ACK_DELAY_TICKS)
            .unwrap_or(false);
        if due {
            let ack_no = st.tcp.get(h).unwrap().rcv_nxt;
            let snd = st.tcp.get(h).unwrap().snd_nxt;
            emit(st, h, snd, ack_no, wire::TCP_ACK, &no_opts(), &[]);
            let s = st.tcp.get_mut(h).unwrap();
            s.unacked_segs = 0;
            s.last_ack_tick = now;
        }
    }

    // Window update after application reads: without this the peer learns the
    // window reopened only via its own persist timer (seconds of stall).
    {
        let fire = st
            .tcp
            .get(h)
            .map(|s| s.wnd_update_pending && s.state.is_data_state())
            .unwrap_or(false);
        if fire {
            let ack_no = st.tcp.get(h).unwrap().rcv_nxt;
            let snd = st.tcp.get(h).unwrap().snd_nxt;
            let opts = no_opts();
            emit(st, h, snd, ack_no, wire::TCP_ACK, &opts, &[]);
            let s = st.tcp.get_mut(h).unwrap();
            s.wnd_update_pending = false;
            s.last_adv_free = s.rx_cap - s.rx.len();
            s.unacked_segs = 0;
            s.last_ack_tick = now;
        }
    }

    let (state, ctrl_needed) = match st.tcp.get(h) {
        Some(s) => (s.state, s.ctrl_needs_send),
        None => return,
    };

    match state {
        State::SynSent | State::SynRcvd => {
            if ctrl_needed {
                let (seq, ack_no, flags, scale) = {
                    let s = st.tcp.get(h).unwrap();
                    let flags = if s.state == State::SynSent {
                        wire::TCP_SYN
                    } else {
                        wire::TCP_SYN | wire::TCP_ACK
                    };
                    let ack_no = if s.state == State::SynSent {
                        0
                    } else {
                        s.rcv_nxt
                    };
                    (s.iss, ack_no, flags, s.rcv_scale)
                };
                let opts = ctrl_options(scale);
                emit(st, h, seq, ack_no, flags, &opts, &[]);
                let s = st.tcp.get_mut(h).unwrap();
                s.ctrl_needs_send = false;
                if s.rto_deadline.is_none() {
                    s.rto_deadline = Some(now + s.rto_ms as u64);
                }
            }
        }
        State::Established
        | State::CloseWait
        | State::FinWait1
        | State::LastAck
        | State::Closing => {
            // Data segments while congestion + peer windows allow.
            loop {
                let (mss, wnd, cwnd, outstanding, tx_len, snd_nxt, ack_no) = {
                    let s = st.tcp.get(h).unwrap();
                    (
                        s.effective_mss(),
                        s.snd_wnd,
                        s.cwnd,
                        s.outstanding_data(),
                        s.tx.len(),
                        s.snd_nxt,
                        s.rcv_nxt,
                    )
                };
                let allowed = core::cmp::min(wnd, cwnd) as usize;
                if outstanding as usize >= allowed {
                    break;
                }
                let unsent = tx_len.saturating_sub(outstanding as usize);
                if unsent == 0 {
                    break;
                }
                let take =
                    core::cmp::min(mss, core::cmp::min(allowed - outstanding as usize, unsent));
                if take == 0 {
                    break;
                }

                // Copy the segment payload out of the ring buffer, then emit.
                let mut payload = Vec::with_capacity(take);
                {
                    let s = st.tcp.get(h).unwrap();
                    for b in s.tx.iter().skip(outstanding as usize).take(take) {
                        payload.push(*b);
                    }
                }
                // Karn-safe RTT timing: time only the oldest unmeasured span.
                {
                    let s = st.tcp.get_mut(h).unwrap();
                    if s.timing_end.is_none() {
                        s.timing_end = Some(snd_nxt.wrapping_add(take as u32));
                        s.timing_start = Some(now);
                    }
                }
                emit(
                    st,
                    h,
                    snd_nxt,
                    ack_no,
                    wire::TCP_ACK | wire::TCP_PSH,
                    &no_opts(),
                    &payload,
                );
                let s = st.tcp.get_mut(h).unwrap();
                s.snd_nxt = snd_nxt.wrapping_add(take as u32);
                if s.rto_deadline.is_none() {
                    s.rto_deadline = Some(now + s.rto_ms as u64);
                }
            }

            // FIN once everything buffered has been transmitted.
            let fin_open = st.tcp.get(h).map(|s| s.fin_seq.is_none()).unwrap_or(false);
            let close_req = st.tcp.get(h).map(|s| s.close_requested).unwrap_or(false);
            if close_req && fin_open {
                let ready = {
                    let s = st.tcp.get(h).unwrap();
                    s.outstanding_data() as usize == s.tx.len()
                };
                if ready {
                    let (seq, ack_no) = {
                        let s = st.tcp.get(h).unwrap();
                        (s.snd_nxt, s.rcv_nxt)
                    };
                    emit(
                        st,
                        h,
                        seq,
                        ack_no,
                        wire::TCP_ACK | wire::TCP_FIN,
                        &no_opts(),
                        &[],
                    );
                    let s = st.tcp.get_mut(h).unwrap();
                    s.fin_seq = Some(seq);
                    s.snd_nxt = seq.wrapping_add(1);
                    match s.state {
                        State::Established => s.state = State::FinWait1,
                        State::CloseWait => s.state = State::LastAck,
                        _ => {}
                    }
                    if s.rto_deadline.is_none() {
                        s.rto_deadline = Some(now + s.rto_ms as u64);
                    }
                }
            }
        }
        _ => {}
    }
}

/// RTO expiry: backoff, retransmit, give-up decisions.
fn timer_step(st: &mut Stack, h: usize, now: u64) {
    let fire = match st.tcp.get(h) {
        Some(s) => s.rto_deadline.map(|d| now >= d).unwrap_or(false),
        None => return,
    };
    if !fire {
        return;
    }

    let s = st.tcp.get_mut(h).unwrap();
    s.retries += 1;
    s.rto_ms = s.rto_ms.saturating_mul(2).min(RTO_MAX_MS);
    // Karn: a retransmitted span yields no RTT sample.
    s.timing_end = None;
    s.timing_start = None;
    // Reno RTO response: halve the window threshold, collapse cwnd.
    s.ssthresh = (s.cwnd / 2).max(2 * s.effective_mss() as u32);
    s.cwnd = s.effective_mss() as u32;

    let state = s.state;

    if state == State::SynSent || state == State::SynRcvd {
        if s.retries > CONNECT_MAX_RETRIES {
            s.refused = true;
            s.state = State::Closed;
            return;
        }
        let (seq, ack_no, flags, scale) = (
            s.iss,
            if state == State::SynSent {
                0
            } else {
                s.rcv_nxt
            },
            if state == State::SynSent {
                wire::TCP_SYN
            } else {
                wire::TCP_SYN | wire::TCP_ACK
            },
            s.rcv_scale,
        );
        s.rto_deadline = Some(now + s.rto_ms as u64);
        let opts = ctrl_options(scale);
        emit(st, h, seq, ack_no, flags, &opts, &[]);
        return;
    }

    if s.retries > DATA_MAX_RETRIES {
        // Give up honestly: the connection is dead.
        s.state = State::Closed;
        return;
    }

    s.rto_deadline = Some(now + s.rto_ms as u64);

    if state.is_data_state() {
        let outstanding = s.outstanding_data() as usize;
        let fin_unacked = s.fin_seq.map(|f| seq_le(s.snd_una, f)).unwrap_or(false);

        if outstanding == 0 && fin_unacked {
            // Resend a bare FIN.
            let fin_at = s.fin_seq.unwrap();
            let ack_no = s.rcv_nxt;
            emit(
                st,
                h,
                fin_at,
                ack_no,
                wire::TCP_ACK | wire::TCP_FIN,
                &no_opts(),
                &[],
            );
        } else if outstanding > 0 {
            // Resend starting at the first hole NOT already covered by a SACK
            // block; when everything outstanding is sacked there is nothing to
            // recover — just rearm the timer.
            if let Some(off) = s.first_hole() {
                let n = s.effective_mss().min(s.tx.len() - off);
                let mut payload = Vec::with_capacity(n);
                for b in s.tx.iter().skip(off).take(n) {
                    payload.push(*b);
                }
                let seq = s.snd_una.wrapping_add(off as u32);
                let ack_no = s.rcv_nxt;
                drop(s);
                emit(
                    st,
                    h,
                    seq,
                    ack_no,
                    wire::TCP_ACK | wire::TCP_PSH,
                    &no_opts(),
                    &payload,
                );
            }
        } else if s.snd_wnd == 0 && !s.tx.is_empty() {
            // Persist probe: one byte past the closed window elicits a window
            // update ACK from the peer.
            let b = s.tx.front().copied().unwrap_or(0);
            let seq = s.snd_nxt;
            let ack_no = s.rcv_nxt;
            emit(st, h, seq, ack_no, wire::TCP_ACK, &[], &[b]);
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_ordering_wraps() {
        assert!(seq_lt(0, 1));
        assert!(seq_lt(u32::MAX, 0)); // wrap
        assert!(!seq_lt(1, 0));
        assert!(seq_le(5, 5));
        assert!(seq_gt(2, 1));
        assert!(seq_gt(0, u32::MAX));
    }

    #[test]
    fn offer_scale_matches_capacity() {
        assert_eq!(TcpSock::offer_scale(4096), 0);
        assert_eq!(TcpSock::offer_scale(256 * 1024), 2);
        assert_eq!(TcpSock::offer_scale(1024 * 1024), 4);
    }

    #[test]
    fn outstanding_excludes_pending_fin() {
        let mut s = TcpSock::new_active(
            IpEndpoint::new(Ipv4Addr::new(10, 0, 0, 1), 80),
            40000,
            4096,
            4096,
            1000,
        );
        s.state = State::Established;
        s.snd_una = 1001; // SYN acked
        s.snd_nxt = 1001;
        assert_eq!(s.send_slice(b"hello"), 5);
        assert_eq!(s.outstanding_data(), 0, "nothing transmitted yet");
        s.snd_nxt = 1006;
        assert_eq!(s.outstanding_data(), 5);
        s.fin_seq = Some(1006);
        s.snd_nxt = 1007;
        assert_eq!(s.outstanding_data(), 5, "FIN must not count as data");
        s.snd_una = 1007;
        assert_eq!(s.outstanding_data(), 0);
    }

    #[test]
    fn first_hole_skips_sacked_ranges() {
        let mut s = TcpSock::new_active(
            IpEndpoint::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 80),
            40000,
            4096,
            4096,
            1000,
        );
        s.state = State::Established;
        s.snd_una = 1001;
        // 15 bytes outstanding at [1001, 1016).
        assert_eq!(s.send_slice(b"abcdefghijklmno"), 15);
        s.snd_nxt = 1016;

        // Nothing sacked -> hole at offset 0.
        assert_eq!(s.first_hole(), Some(0));

        // First 5 bytes sacked -> hole moves to offset 5.
        let b1 = wire::heapless_shim::Vec::<(u32, u32), 4>::from_slice(&[(1001, 1006)]);
        s.note_sacked(&b1);
        assert_eq!(s.first_hole(), Some(5));

        // Everything sacked -> no hole (nothing to retransmit).
        let b2 = wire::heapless_shim::Vec::<(u32, u32), 4>::from_slice(&[(1006, 1016)]);
        s.note_sacked(&b2);
        assert_eq!(s.first_hole(), None);

        // Cumulative ACK slides snd_una; ranges below it are pruned.
        s.snd_una = 1008;
        s.prune_sacked();
        assert!(s.sacked.iter().all(|&(l, _)| !seq_lt(l, 1008)));
        assert_eq!(s.first_hole(), Some(3));
    }

    #[test]
    fn recv_slice_drains_fifo() {
        let mut s = TcpSock::new_active(
            IpEndpoint::new(Ipv4Addr::new(10, 0, 0, 1), 80),
            40000,
            4096,
            4096,
            1000,
        );
        for b in b"abcdef" {
            s.rx.push_back(*b);
        }
        let mut buf = [0u8; 4];
        assert_eq!(s.recv_slice(&mut buf), 4);
        assert_eq!(&buf, b"abcd");
        assert_eq!(s.recv_slice(&mut buf), 2);
        assert_eq!(&buf[..2], b"ef");
        assert_eq!(s.recv_slice(&mut buf), 0);
    }
}
