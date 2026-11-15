//! Syscall'ы таймеров/часов, eventfd и epoll: clock_getres(229),
//! eventfd2(290), epoll_create1(291), epoll_ctl(233), epoll_wait(232).
#![allow(dead_code)]
use super::{check_user_ptr, check_user_ptr_writable};
use crate::arch::x86_64::linux::errno::Errno;
use crate::sync::spinlock::Spinlock;
use crate::task::compat;
use crate::task::fd::{OpenObject, O_NONBLOCK, O_RDWR};
use alloc::sync::Arc;
use alloc::vec::Vec;

// ── clock_getres (229) ───────────────────────────────────────────────────────

/// `clock_getres` (229): сообщить разрешение часов как `struct timespec`.
/// Для всех поддерживаемых часов сообщаем 1 мс (= 10 000 000 нс), что совпадает
/// с гранулярностью тика LAPIC. `EINVAL` для неизвестных id, согласованно с
/// `clock_gettime`.
pub fn sys_clock_getres(clock_id: u64, ts_ptr: u64) -> Result<u64, Errno> {
    // Поддерживаемые id: CLOCK_REALTIME(0), CLOCK_MONOTONIC(1),
    // CLOCK_PROCESS_CPUTIME_ID(2), CLOCK_THREAD_CPUTIME_ID(3),
    // CLOCK_MONOTONIC_RAW(4), CLOCK_REALTIME_COARSE(5), CLOCK_MONOTONIC_COARSE(6),
    // CLOCK_BOOTTIME(7). libuv проверяет CLOCK_MONOTONIC_COARSE(6) при
    // инициализации цикла, поэтому отвечать должны оба.
    if !matches!(clock_id, 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7) {
        return Err(Errno::EINVAL);
    }
    if ts_ptr == 0 {
        return Ok(0);
    } // null-буфер допустим: проверяем только id
    check_user_ptr_writable(ts_ptr, 16)?;
    // struct timespec { tv_sec: i64, tv_nsec: i64 } — разрешение 1 мс
    let sec: i64 = 0;
    let nsec: i64 = (1_000_000_000 / crate::arch::x86_64::apic::TICK_HZ) as i64; // один тик LAPIC
                                                                                 // SAFETY: проверено выше
    unsafe {
        core::ptr::write_unaligned(ts_ptr as *mut i64, sec);
        core::ptr::write_unaligned((ts_ptr + 8) as *mut i64, nsec);
    }
    Ok(0)
}

// ── eventfd2 (290) ───────────────────────────────────────────────────────────

/// `eventfd2` (290): создать event-дескриптор с начальным значением `initval`.
/// Флаги: EFD_CLOEXEC(0x80000), EFD_NONBLOCK(0x800), EFD_SEMAPHORE(0x1).
pub fn sys_eventfd2(initval: u64, flags: u64) -> Result<u64, Errno> {
    let semaphore = flags & 1 != 0;
    let state = Arc::new(Spinlock::new(initval));
    let fd = compat::with_current_compat(|cs| {
        let fd = cs.fds.alloc_with_flags(
            OpenObject::Eventfd {
                val: state,
                semaphore,
            },
            O_RDWR | (flags & O_NONBLOCK),
        )?;
        if flags & 0x80000 != 0 {
            cs.fds.set_cloexec(fd, true);
        } // EFD_CLOEXEC
        Ok::<u64, Errno>(fd as u64)
    })
    .ok_or(Errno::EBADF)??;
    Ok(fd)
}

// ── epoll_create1 (291) ──────────────────────────────────────────────────────

/// `epoll_create1` (291): создать экземпляр epoll. `flags` может быть
/// EPOLL_CLOEXEC (0x80000); остальные игнорируются.
pub fn sys_epoll_create1(flags: u64) -> Result<u64, Errno> {
    let interests: Arc<Spinlock<Vec<EpollEntry>>> = Arc::new(Spinlock::new(Vec::new()));
    let fd = compat::with_current_compat(|cs| {
        let fd = cs
            .fds
            .alloc_with_flags(OpenObject::Epoll { interests }, O_RDWR)?;
        if flags & 0x80000 != 0 {
            cs.fds.set_cloexec(fd, true);
        } // EPOLL_CLOEXEC
        Ok::<u64, Errno>(fd as u64)
    })
    .ok_or(Errno::EBADF)??;
    Ok(fd)
}

// ── epoll_ctl (233) ──────────────────────────────────────────────────────────

const EPOLL_CTL_ADD: u64 = 1;
const EPOLL_CTL_DEL: u64 = 2;
const EPOLL_CTL_MOD: u64 = 3;

/// Один зарегистрированный интерес в экземпляре epoll.
#[derive(Clone)]
pub struct EpollEntry {
    pub fd: i32,
    pub events: u32,
    pub data: u64,
}

/// `epoll_ctl` (233): добавить / изменить / удалить интерес в экземпляре epoll.
/// Раскладка `event_ptr`: { events: u32, _pad: u32, data: u64 } (epoll_event).
pub fn sys_epoll_ctl(epfd: u64, op: u64, fd: u64, event_ptr: u64) -> Result<u64, Errno> {
    // Клонировать Arc списка интересов из fd-таблицы, не держа compat-лок через
    // саму мутацию.
    let interests = compat::with_current_compat(|cs| match cs.fds.get(epfd as u32) {
        Some(OpenObject::Epoll { interests }) => Some(Arc::clone(interests)),
        _ => None,
    })
    .flatten()
    .ok_or(Errno::EBADF)?;

    match op {
        EPOLL_CTL_ADD => {
            check_user_ptr(event_ptr, 12)?;
            // SAFETY: проверено выше; epoll_event — это { u32 events, u8[8] data }
            let events = unsafe { core::ptr::read_unaligned(event_ptr as *const u32) };
            let data = unsafe { core::ptr::read_unaligned((event_ptr + 4) as *const u64) };
            let mut list = interests.lock();
            if list.iter().any(|e| e.fd == fd as i32) {
                // Семантика Linux: ADD на уже зарегистрированный fd — EEXIST.
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
            // SAFETY: проверено выше; epoll_event — это { u32 events, u8[8] data }
            let events = unsafe { core::ptr::read_unaligned(event_ptr as *const u32) };
            let data = unsafe { core::ptr::read_unaligned((event_ptr + 4) as *const u64) };
            let mut list = interests.lock();
            match list.iter_mut().find(|e| e.fd == fd as i32) {
                Some(e) => {
                    e.events = events;
                    e.data = data;
                }
                // Семантика Linux: MOD незарегистрированного fd — ENOENT.
                None => return Err(Errno::ENOENT),
            }
        }
        EPOLL_CTL_DEL => {
            let mut list = interests.lock();
            let present = list.iter().any(|e| e.fd == fd as i32);
            if !present {
                // Семантика Linux: DEL незарегистрированного fd — ENOENT (и не
                // требует валидного event_ptr — здесь он не читается).
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

/// `epoll_wait` (232): ждать событий на экземпляре epoll.
/// `events_ptr` должен указывать на `maxevents` * 12 байт (struct epoll_event[]).
/// `timeout_ms`: -1 = бесконечно, 0 = вернуться сразу.
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
    check_user_ptr_writable(events_ptr, maxevents * event_sz)?;

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
        // Проверка живости после каждой уступки CPU: записи событий строятся в
        // памяти ядра, поэтому сосед, снявший пользовательский массив, обязан
        // превратиться в -EFAULT на следующей итерации, а не в бесконечное
        // ожидание.
        check_user_ptr_writable(events_ptr, maxevents * event_sz)?;
        let snapshot: Vec<EpollEntry> = interests.lock().clone();
        // Собирать готовые события в памяти ЯДРА, а не прямо в массив
        // пользователя: массив проверялся один раз на входе, а цикл уступает CPU
        // между итерациями, и поток-сосед по CLONE_VM может снять его отображение
        // в этом окне (тот же класс, что poll/select). Устаревшая запись из ring 0
        // даёт #PF режима ядра с остановкой машины; перепроверенная публикация
        // ниже сообщает -EFAULT.
        let mut out: Vec<u8> = Vec::with_capacity(maxevents as usize * event_sz as usize);
        for entry in &snapshot {
            if (out.len() as u64) / event_sz >= maxevents {
                break;
            }
            let revents = poll_fd(entry.fd, entry.events);
            if revents != 0 {
                // `struct epoll_event` упакован на x86_64: { u32 events, u64 data }.
                out.extend_from_slice(&revents.to_ne_bytes());
                out.extend_from_slice(&entry.data.to_ne_bytes());
            }
        }
        let timed_out = deadline
            .map(|d| crate::task::scheduler::ticks() >= d)
            .unwrap_or(false);
        if !out.is_empty() || timed_out || timeout_i == 0 {
            if !out.is_empty() {
                // Перепроверить приёмник после ожидания перед записью.
                super::io_sys::copy_out_checked(events_ptr, &out)?;
            }
            return Ok((out.len() as u64) / event_sz);
        }
        // Доставимый сигнал → -EINTR (проверка `errno == EINTR` в libuv уже
        // ждёт эту форму); эпилог диспетчеризации доставит сигнал.
        if super::signal::has_deliverable_current() {
            return Err(Errno::EINTR);
        }
        crate::task::scheduler::yield_current();
    }
}

/// `epoll_pwait` (281): `epoll_wait` плюс sigmask. Атомарная семантика
/// «подменить маску, потом восстановить» не реализована (маска принимается и
/// игнорируется); само ожидание прерывается сигналом — доставимый сигнал даёт
/// `-EINTR`, что uv__io_poll из libuv уже терпит.
pub fn sys_epoll_pwait(
    epfd: u64,
    events_ptr: u64,
    maxevents: u64,
    timeout_ms: u64,
    _sigmask: u64,
) -> Result<u64, Errno> {
    sys_epoll_wait(epfd, events_ptr, maxevents, timeout_ms)
}

/// Вычислить revents для одного fd при запрошенных событиях.
fn poll_fd(fd: i32, events: u32) -> u32 {
    use crate::task::fd::OpenObject;
    if fd < 0 {
        return 0;
    }
    let result = crate::task::compat::with_current_compat(|cs| {
        match cs.fds.get(fd as u32) {
            None => EPOLLERR,
            // AF_INET-сокеты: сообщать оба направления; curl перепроверяет через send/recv.
            Some(OpenObject::InetTcp(_)) | Some(OpenObject::InetUdp(_)) => EPOLLIN | EPOLLOUT,
            Some(OpenObject::Stdin) => {
                // Сообщать читаемость, только когда чтение реально может
                // продвинуться. Прежний ответ «всегда готов» заставлял libuv
                // выдать read, который блокировался в ядре и вешал TUI-цикл
                // nvim до отрисовки первого кадра.
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
            // Слушатель «читаем», когда в очереди есть подключение.
            Some(OpenObject::UnixListener(l)) => {
                if events & EPOLLIN != 0 && !l.inner.lock().pending.is_empty() {
                    EPOLLIN
                } else {
                    0
                }
            }
            Some(OpenObject::UnixSocketUnbound { .. }) => 0,
            Some(OpenObject::Signalfd { mask }) => {
                // Эта ветка выполняется ВНУТРИ `with_current_compat`, то есть
                // `COMPAT_STATES` уже удерживается этим контекстом. Вызов
                // `compat::signalfd_readable` здесь взял бы нереентерабельный
                // спинлок второй раз и заблокировал CPU с выключенными
                // прерываниями (зависание машины на 4 syscall'ах из аудита).
                // Считаем готовность по состоянию, которым уже владеет
                // замыкание.
                let mask = *mask.lock();
                if cs.sig_pending & mask & !cs.sig_blocked != 0 {
                    EPOLLIN
                } else {
                    0
                }
            }
            Some(OpenObject::Timerfd(state)) => {
                if state.lock().pending(crate::task::scheduler::ticks()) > 0 {
                    EPOLLIN
                } else {
                    0
                }
            }
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

/// Напечатать список интересов epoll ТЕКУЩЕЙ задачи вместе с разрешённым
/// типом каждого fd и его готовностью ПРЯМО СЕЙЧАС. Вызывается watchdog'ом
/// зависших syscall'ов в контексте самой зависшей задачи. Прямо отвечает на
/// вопрос nvim: есть ли RPC-сокет в наборе, во что разрешается его fd и видит ли
/// poll поставленные в очередь байты.
pub(super) fn dump_epoll_self(epfd: u32) {
    use crate::task::fd::OpenObject;
    let pid = crate::task::scheduler::current_pid();
    // Снять снимок fd + типов под compat-локом; готовность считается после его
    // отпускания (poll_fd внутри берёт тот же лок).
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
                        Some(OpenObject::Timerfd(_)) => "timerfd",
                        Some(OpenObject::Signalfd { .. }) => "signalfd",
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
