//! Timer/clock, eventfd and epoll syscalls: clock_getres(229), eventfd2(290),
//! epoll_create1(291), epoll_ctl(233), epoll_wait(232).
#![allow(dead_code)]
use super::check_user_ptr;
use crate::arch::x86_64::linux::errno::Errno;
use crate::sync::spinlock::Spinlock;
use crate::task::compat;
use crate::task::fd::OpenObject;
use alloc::sync::Arc;
use alloc::vec::Vec;

// ── clock_getres (229) ───────────────────────────────────────────────────────

/// `clock_getres` (229): report the resolution of a clock as a `struct timespec`.
/// We report 1 ms (= 10 000 000 ns) for all supported clocks, matching the
/// LAPIC tick granularity. `EINVAL` for unknown ids, consistent with
/// `clock_gettime`.
pub fn sys_clock_getres(clock_id: u64, ts_ptr: u64) -> Result<u64, Errno> {
    // Supported ids: CLOCK_REALTIME(0), CLOCK_MONOTONIC(1),
    // CLOCK_PROCESS_CPUTIME_ID(2), CLOCK_THREAD_CPUTIME_ID(3),
    // CLOCK_MONOTONIC_RAW(4), CLOCK_REALTIME_COARSE(5), CLOCK_MONOTONIC_COARSE(6),
    // CLOCK_BOOTTIME(7). libuv probes
    // CLOCK_MONOTONIC_COARSE(6) at loop init, so both must answer.
    if !matches!(clock_id, 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7) {
        return Err(Errno::EINVAL);
    }
    if ts_ptr == 0 {
        return Ok(0);
    } // null buf is valid: just validate the id
    check_user_ptr(ts_ptr, 16)?;
    // struct timespec { tv_sec: i64, tv_nsec: i64 } — 1 ms resolution
    let sec: i64 = 0;
    let nsec: i64 = (1_000_000_000 / crate::arch::x86_64::apic::TICK_HZ) as i64; // one LAPIC tick
                                                                                 // SAFETY: validated above
    unsafe {
        core::ptr::write_unaligned(ts_ptr as *mut i64, sec);
        core::ptr::write_unaligned((ts_ptr + 8) as *mut i64, nsec);
    }
    Ok(0)
}

// ── eventfd2 (290) ───────────────────────────────────────────────────────────

/// `eventfd2` (290): create an event file descriptor with initial value `initval`.
/// Flags: EFD_CLOEXEC(0x80000), EFD_NONBLOCK(0x800), EFD_SEMAPHORE(0x1).
pub fn sys_eventfd2(initval: u64, flags: u64) -> Result<u64, Errno> {
    let semaphore = flags & 1 != 0;
    let state = Arc::new(Spinlock::new(initval));
    let fd = compat::with_current_compat(|cs| {
        let fd = cs.fds.alloc(OpenObject::Eventfd {
            val: state,
            semaphore,
        });
        if flags & 0x80000 != 0 {
            cs.fds.set_cloexec(fd, true);
        } // EFD_CLOEXEC
        fd
    })
    .ok_or(Errno::EBADF)?;
    Ok(fd as u64)
}

// ── epoll_create1 (291) ──────────────────────────────────────────────────────

/// `epoll_create1` (291): create an epoll instance. `flags` may be
/// EPOLL_CLOEXEC (0x80000); others are ignored.
pub fn sys_epoll_create1(flags: u64) -> Result<u64, Errno> {
    let interests: Arc<Spinlock<Vec<EpollEntry>>> = Arc::new(Spinlock::new(Vec::new()));
    let fd = compat::with_current_compat(|cs| {
        let fd = cs.fds.alloc(OpenObject::Epoll { interests });
        if flags & 0x80000 != 0 {
            cs.fds.set_cloexec(fd, true);
        } // EPOLL_CLOEXEC
        fd
    })
    .ok_or(Errno::EBADF)?;
    Ok(fd as u64)
}

// ── epoll_ctl (233) ──────────────────────────────────────────────────────────

const EPOLL_CTL_ADD: u64 = 1;
const EPOLL_CTL_DEL: u64 = 2;
const EPOLL_CTL_MOD: u64 = 3;

/// One registered interest in an epoll instance.
#[derive(Clone)]
pub struct EpollEntry {
    pub fd: i32,
    pub events: u32,
    pub data: u64,
}

/// `epoll_ctl` (233): add / modify / remove an interest in an epoll instance.
/// `event_ptr` layout: { events: u32, _pad: u32, data: u64 } (epoll_event).
pub fn sys_epoll_ctl(epfd: u64, op: u64, fd: u64, event_ptr: u64) -> Result<u64, Errno> {
    // Clone the interest list Arc out of the fd table without holding the
    // compat lock across the actual mutation.
    let interests = compat::with_current_compat(|cs| match cs.fds.get(epfd as u32) {
        Some(OpenObject::Epoll { interests }) => Some(Arc::clone(interests)),
        _ => None,
    })
    .flatten()
    .ok_or(Errno::EBADF)?;

    match op {
        EPOLL_CTL_ADD => {
            check_user_ptr(event_ptr, 12)?;
            // SAFETY: validated above; epoll_event is { u32 events, u8[8] data }
            let events = unsafe { core::ptr::read_unaligned(event_ptr as *const u32) };
            let data = unsafe { core::ptr::read_unaligned((event_ptr + 4) as *const u64) };
            let mut list = interests.lock();
            if list.iter().any(|e| e.fd == fd as i32) {
                // Linux semantics: ADD on an already-registered fd is EEXIST.
                return Err(Errno::EEXIST);
            }
            list.push(EpollEntry {
                fd: fd as i32,
                events,
                data,
            });
        }
        EPOLL_CTL_MOD => {
            check_user_ptr(event_ptr, 12)?;
            // SAFETY: validated above; epoll_event is { u32 events, u8[8] data }
            let events = unsafe { core::ptr::read_unaligned(event_ptr as *const u32) };
            let data = unsafe { core::ptr::read_unaligned((event_ptr + 4) as *const u64) };
            let mut list = interests.lock();
            match list.iter_mut().find(|e| e.fd == fd as i32) {
                Some(e) => {
                    e.events = events;
                    e.data = data;
                }
                // Linux semantics: MOD on an unregistered fd is ENOENT.
                None => return Err(Errno::ENOENT),
            }
        }
        EPOLL_CTL_DEL => {
            let mut list = interests.lock();
            let present = list.iter().any(|e| e.fd == fd as i32);
            if !present {
                // Linux semantics: DEL of an unregistered fd is ENOENT (and
                // does not require a valid event_ptr — none is read here).
                return Err(Errno::ENOENT);
            }
            list.retain(|e| e.fd != fd as i32);
        }
        _ => return Err(Errno::EINVAL),
    }
    Ok(0)
}

// ── epoll_wait (232) ─────────────────────────────────────────────────────────

const EPOLLIN: u32 = 0x0001;
const EPOLLOUT: u32 = 0x0004;
const EPOLLERR: u32 = 0x0008;
const EPOLLHUP: u32 = 0x0010;
const EPOLLRDHUP: u64 = 0x2000;

/// `epoll_wait` (232): wait for events on an epoll instance.
/// `events_ptr` must point to `maxevents` * 12 bytes (struct epoll_event[]).
/// `timeout_ms`: -1 = infinite, 0 = return immediately.
pub fn sys_epoll_wait(
    epfd: u64,
    events_ptr: u64,
    maxevents: u64,
    timeout_ms: u64,
) -> Result<u64, Errno> {
    if maxevents == 0 || maxevents > 1024 {
        return Err(Errno::EINVAL);
    }
    let event_sz: u64 = 12; // sizeof(struct epoll_event) = 4+8
    check_user_ptr(events_ptr, maxevents * event_sz)?;

    let interests = compat::with_current_compat(|cs| match cs.fds.get(epfd as u32) {
        Some(OpenObject::Epoll { interests }) => Some(Arc::clone(interests)),
        _ => None,
    })
    .flatten()
    .ok_or(Errno::EBADF)?;

    let timeout_i = timeout_ms as i64;
    let deadline = if timeout_i < 0 {
        None
    } else {
        let ms = timeout_i as u64;
        Some(crate::task::scheduler::ticks().saturating_add((ms.saturating_add(9)) / 10))
    };

    loop {
        let snapshot: Vec<EpollEntry> = interests.lock().clone();
        let mut out = 0usize;
        for entry in &snapshot {
            if out >= maxevents as usize {
                break;
            }
            let revents = poll_fd(entry.fd, entry.events);
            if revents != 0 {
                let dst = events_ptr + (out as u64) * event_sz;
                // SAFETY: buffer validated above
                unsafe {
                    core::ptr::write_unaligned(dst as *mut u32, revents);
                    core::ptr::write_unaligned((dst + 4) as *mut u64, entry.data);
                }
                out += 1;
            }
        }
        let timed_out = deadline
            .map(|d| crate::task::scheduler::ticks() >= d)
            .unwrap_or(false);
        if out > 0 || timed_out || timeout_i == 0 {
            return Ok(out as u64);
        }
        // Deliverable signal → -EINTR (libuv's `errno == EINTR` assertion
        // already expects this shape); the dispatch epilogue delivers.
        if super::signal::has_deliverable_current() {
            return Err(Errno::EINTR);
        }
        crate::task::scheduler::yield_current();
    }
}

/// `epoll_pwait` (281): `epoll_wait` plus a sigmask. The atomic
/// swap-to-sigmask-then-restore semantics are not implemented (the mask is
/// accepted and ignored); the wait itself IS signal-interruptible — a
/// deliverable signal returns `-EINTR`, which libuv's uv__io_poll already
/// tolerates (its `errno == EINTR` assertion motivated this syscall).
pub fn sys_epoll_pwait(
    epfd: u64,
    events_ptr: u64,
    maxevents: u64,
    timeout_ms: u64,
    _sigmask: u64,
) -> Result<u64, Errno> {
    sys_epoll_wait(epfd, events_ptr, maxevents, timeout_ms)
}

/// Compute revents for a single fd given requested events.
fn poll_fd(fd: i32, events: u32) -> u32 {
    use crate::task::fd::OpenObject;
    if fd < 0 {
        return 0;
    }
    let result = crate::task::compat::with_current_compat(|cs| {
        match cs.fds.get(fd as u32) {
            None => EPOLLERR,
            // AF_INET sockets: report both directions; curl re-checks via send/recv.
            Some(OpenObject::InetTcp(_)) | Some(OpenObject::InetUdp(_)) => EPOLLIN | EPOLLOUT,
            Some(OpenObject::Stdin) => {
                // Report readable only when a read can actually
                // make progress. The old always-ready answer made libuv issue
                // a read that blocked in-kernel and stalled nvim's TUI loop
                // before the first frame was drawn.
                if events & EPOLLIN != 0 && super::io_sys::stdin_input_available() {
                    EPOLLIN
                } else {
                    0
                }
            }
            Some(OpenObject::Console) => {
                if events & EPOLLOUT != 0 {
                    EPOLLOUT
                } else {
                    0
                }
            }
            Some(OpenObject::PipeRead(e)) => {
                let mut r = 0u32;
                if e.read_ready() && events & EPOLLIN != 0 {
                    r |= EPOLLIN;
                }
                if e.peer_closed() {
                    r |= EPOLLHUP;
                }
                r
            }
            Some(OpenObject::PipeWrite(e)) => {
                let mut r = 0u32;
                if e.write_ready() && events & EPOLLOUT != 0 {
                    r |= EPOLLOUT;
                }
                if e.peer_closed() {
                    r |= EPOLLERR;
                }
                r
            }
            Some(OpenObject::Socket { rx, tx }) => {
                let mut r = 0u32;
                if rx.read_ready() && events & EPOLLIN != 0 {
                    r |= EPOLLIN;
                }
                if tx.write_ready() && events & EPOLLOUT != 0 {
                    r |= EPOLLOUT;
                }
                if rx.peer_closed() {
                    r |= EPOLLHUP;
                }
                r
            }
            // A listener is "readable" when a connection is queued.
            Some(OpenObject::UnixListener(l)) => {
                if events & EPOLLIN != 0 && !l.inner.lock().pending.is_empty() {
                    EPOLLIN
                } else {
                    0
                }
            }
            Some(OpenObject::UnixSocketUnbound { .. }) => 0,
            Some(OpenObject::Eventfd { val, .. }) => {
                let v = *val.lock();
                if v > 0 && events & EPOLLIN != 0 {
                    EPOLLIN
                } else {
                    0
                }
            }
            Some(OpenObject::File { .. }) | Some(OpenObject::Dir { .. }) => {
                events & (EPOLLIN | EPOLLOUT)
            }
            Some(OpenObject::Epoll { .. }) => 0,
        }
    });
    let _ = EPOLLRDHUP;
    result.unwrap_or(0)
}

/// Dump the CURRENT task's epoll interest list together with each
/// fd's resolved object type and its readiness AS OF RIGHT NOW. Called by the
/// stuck-syscall watchdog in the context of the stuck task itself. Answers
/// the nvim question directly: is the RPC socket in the set at all, what does
/// its fd resolve to, and does the poll see the queued bytes.
pub(super) fn dump_epoll_self(epfd: u32) {
    use crate::task::fd::OpenObject;
    let pid = crate::task::scheduler::current_pid();
    // Snapshot fds + types under the compat lock; readiness is computed after
    // it is released (poll_fd takes the same lock internally).
    let entries: alloc::vec::Vec<(i32, u32, &'static str)> =
        crate::task::compat::with_current_compat(|cs| {
            let interests = match cs.fds.get(epfd) {
                Some(OpenObject::Epoll { interests }) => alloc::sync::Arc::clone(interests),
                _ => return alloc::vec::Vec::new(),
            };
            let snap: alloc::vec::Vec<(i32, u32)> =
                interests.lock().iter().map(|e| (e.fd, e.events)).collect();
            snap.into_iter()
                .map(|(fd, ev)| {
                    let kind = match cs.fds.get(fd as u32) {
                        None => "closed",
                        Some(OpenObject::Stdin) => "stdin(kbd)",
                        Some(OpenObject::Console) => "console",
                        Some(OpenObject::PipeRead(_)) => "pipe-r",
                        Some(OpenObject::PipeWrite(_)) => "pipe-w",
                        Some(OpenObject::Socket { .. }) => "socket",
                        Some(OpenObject::InetTcp(_)) => "inet-tcp",
                        Some(OpenObject::InetUdp(_)) => "inet-udp",
                        Some(OpenObject::UnixListener(_)) => "unix-listener",
                        Some(OpenObject::UnixSocketUnbound { .. }) => "unix-unbound",
                        Some(OpenObject::Eventfd { .. }) => "eventfd",
                        Some(OpenObject::File { .. }) => "file",
                        Some(OpenObject::Dir { .. }) => "dir",
                        Some(OpenObject::Epoll { .. }) => "epoll",
                    };
                    (fd, ev, kind)
                })
                .collect()
        })
        .unwrap_or_default();
    if entries.is_empty() {
        crate::warn!(
            "[WATCHDOG]   pid={} epfd={}: interest list is EMPTY (or fd is not an epoll) — this loop can never wake",
            pid, epfd
        );
        return;
    }
    for (fd, ev, kind) in entries {
        let revents = poll_fd(fd, ev);
        crate::warn!(
            "[WATCHDOG]   pid={} epoll interest: fd={} type={} events=0x{:x} revents_now=0x{:x}",
            pid,
            fd,
            kind,
            ev,
            revents
        );
    }
}
