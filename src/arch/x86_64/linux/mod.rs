//! Слой совместимости с x86_64-бинарями Linux.
//!
//! Дерево модулей содержит чистое, host-тестируемое ядро (кодирование errno,
//! маршаллинг ABI, валидацию диапазонов указателей, планирование syscall'ов,
//! диагностику) и эффектные ядерные оболочки, которые его приводят в действие.
//! Чистые модули разделяются в крейт `host-tests` через `#[path]` (R11.6);
//! эффектные обработчики и вход `linux_dispatch` — здесь.

pub mod errno;
pub mod regs;

// Следующие подмодули объявлены, чтобы дерево модулей существовало сразу;
// каждый вначале компилируется как чистая заготовка и наполняется в своём
// обработчике.
pub mod abi;
pub mod cmsg;
pub mod cred;
pub mod diag;
pub mod dirent;
pub mod io;
pub mod kill;
pub mod mem;
pub mod misc;
pub mod rand_clock;
pub mod sock_shutdown;
pub mod sock_wait;
pub mod stat;
pub mod timeconv;
pub mod tty;
pub mod validate;

// Ядерные эффектные оболочки обработчиков. Они НЕ включаются в `host-tests`
// через `#[path]` (так включаются только чистые модули выше), поэтому свободно
// используют VMM/PMM, VFS, планировщик и per-process compat-состояние. Хранение
// эффектных обработчиков здесь — а не в включаемых в host `io.rs`/`mem.rs` —
// и позволяет тем чистым планировщикам оставаться host-тестируемыми (R11.6).
pub mod epoll_sys;
pub mod inet_sock;
pub mod io_sys;
pub mod mem_sys;
pub mod process_sys;
pub mod rtc;
pub mod signal;
pub mod signal_frame;
pub mod signalfd;
pub mod timerfd;
pub mod trap_frame;
pub mod unix_sock;

use abi::nr as sysno;
use errno::{encode_errno, Errno};
use regs::SavedRegs;

/// Единая точка контроля пользовательских указателей (R1.5, R1.6).
///
/// Каждый обработчик, принимающий пользовательский указатель, зовёт это до
/// разыменования. Сначала идёт чистая проверка диапазона
/// [`validate::check_user_range`] (отвергает буферы на/выше `User_Addr_Max`,
/// переполнение или конец выше границы — R1.5), затем обход каждой страницы 4 КиБ
/// диапазона с подтверждением, что она отображена в ТЕКУЩЕМ адресном пространстве
/// через [`crate::memory::vmm::virt_to_phys`] (R1.6). Любой отказ даёт `EFAULT` без
/// чтения или записи буфера.
/// Ограничитель частоты диагностики `EINVAL` для syscall'а (защита от заливки
/// лога).
///
/// Диагностика нужна, чтобы назвать сбойный syscall на следующем запуске, но её
/// может вызвать атакующий: ring-3 цикл по `mprotect()` на адресах ядра давал
/// >1000 строк `[linux] EINVAL diag` в секунду, а каждый `warn!` доходит и в
/// serial, и в framebuffer — дешёвый DoS консоли/лога. Одной строки в секунду
/// достаточно, чтобы найти виновника.
fn einval_diag_allowed() -> bool {
    use core::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = crate::task::scheduler::ticks();
    let last = LAST.load(Ordering::Relaxed);
    if now.wrapping_sub(last) >= crate::arch::x86_64::apic::TICK_HZ {
        LAST.store(now, Ordering::Relaxed);
        true
    } else {
        false
    }
}

/// Буфер нулевой длины всегда принимается (он ничего не разыменовывает).
pub(crate) fn check_user_ptr(start: u64, len: u64) -> Result<(), Errno> {
    use validate::PtrCheck;
    if validate::check_user_range(start, len) == PtrCheck::Efault {
        return Err(Errno::EFAULT);
    }
    use x86_64::structures::paging::PageTableFlags;
    for page in validate::spanned_pages(start, len) {
        let flags = crate::memory::vmm::page_flags(page).ok_or(Errno::EFAULT)?;
        if !flags.contains(PageTableFlags::USER_ACCESSIBLE) {
            return Err(Errno::EFAULT);
        }
    }
    Ok(())
}

/// Проверить пользовательский диапазон, в который ядро будет **писать**:
/// [`check_user_ptr`] плюс право записи на каждой странице.
///
/// ПОЧЕМУ отдельная проверка: `check_user_ptr` доказывает только отображение и
/// доступность из ring 3. Store в present read-only страницу (например `.text`
/// ELF или `mprotect(PROT_READ)`) даёт #PF в ring 0, и раньше это могло
/// остановить машину: под `with_current_compat` обработчик отказа не имеет права
/// повторно брать реестр, а kill-путь брал его второй раз (F-r4-arch-1). Каждый
/// путь, который копирует данные НАРУЖУ, должен отвергнуть такой буфер с
/// `EFAULT` до записи.
///
/// fork-COW лист намеренно read-only: пользовательская запись в него законна и
/// обслуживается обработчиком #PF приватной копией, поэтому COW-лист без
/// `RO_BIT` (явный `mprotect(PROT_READ)`) считается записываемым. `RO_BIT` —
/// маркер «отображение запрещает запись», и запись в него обязана стать
/// SIGSEGV, а не EFAULT, но обработчик syscall до неё не доходит.
pub(crate) fn check_user_ptr_writable(start: u64, len: u64) -> Result<(), Errno> {
    use validate::PtrCheck;
    if validate::check_user_range(start, len) == PtrCheck::Efault {
        return Err(Errno::EFAULT);
    }
    use x86_64::structures::paging::PageTableFlags;
    for page in validate::spanned_pages(start, len) {
        // Эффективные права всего обхода: `USER_ACCESSIBLE` обязан стоять на
        // каждом уровне, иначе ring 3 не смог бы писать сам.
        let flags = crate::memory::vmm::page_flags(page).ok_or(Errno::EFAULT)?;
        if !flags.contains(PageTableFlags::USER_ACCESSIBLE) {
            return Err(Errno::EFAULT);
        }
        // `page_flags` пересекает флаги уровней и потому теряет программные
        // листовые биты (`COW_BIT`, `RO_BIT`); владение листом читается из
        // самого PTE. Пользовательские страницы ядро не мапит huge-страницами,
        // поэтому листовой PTE — 4 КиБ-страница, которую и увидит запись.
        let leaf = crate::memory::vmm::walk_pte(page)
            .map(|pte| pte.flags())
            .ok_or(Errno::EFAULT)?;
        let writable =
            flags.contains(PageTableFlags::WRITABLE) && leaf.contains(PageTableFlags::WRITABLE);
        let cow_writable = leaf.contains(crate::memory::vmm::COW_BIT)
            && !leaf.contains(crate::memory::vmm::RO_BIT);
        if !writable && !cow_writable {
            return Err(Errno::EFAULT);
        }
    }
    Ok(())
}

/// Выдать диагностику `-ENOSYS` для `nr` не чаще раза на номер (R12.2).
///
/// Для Compat_Process per-process множество `nosys_logged` дедуплицирует запись,
/// поэтому каждый отдельный неподдерживаемый номер логируется один раз. У
/// нативной задачи (нет compat-состояния) per-process множества нет, поэтому
/// запись логируется каждый раз — нативные задачи законно не доходят до
/// неподдерживаемого пути.
fn log_nosys(nr: u64) {
    match compat_log_nosys(nr) {
        Some(true) | None => {
            crate::warn!("[linux] unsupported syscall nr={} -> ENOSYS", nr);
        }
        Some(false) => { /* already logged once for this process */ }
    }
}

/// Записать `nr` в множество `nosys_logged` работающего процесса, вернув, было ли
/// это первое появление (`Some(true)`), повтором (`Some(false)`) или
/// compat-состояния нет (`None`).
fn compat_log_nosys(nr: u64) -> Option<bool> {
    crate::task::compat::with_current_compat(|cs| diag::should_log_nosys(&mut cs.nosys_logged, nr))
}

/// Маршрутизировать поддерживаемый syscall Linux в его эффективный обработчик,
/// вернув `Result<u64, Errno>` обработчика (вызывающий складывает его в `rax`).
/// Гейт поддерживаемости уже проверен, поэтому последняя ветка на практике
/// недостижима.
fn dispatch_supported(nr: u64, a: &[u64; 6]) -> Result<u64, Errno> {
    match nr {
        // ── I/O ──
        sysno::READ => io_sys::sys_read(a[0], a[1], a[2]),
        sysno::WRITE => io_sys::sys_write(a[0], a[1], a[2]),
        sysno::WRITEV => io_sys::sys_writev(a[0], a[1], a[2]),
        sysno::OPEN => io_sys::sys_open(a[0], a[1], a[2]),
        sysno::OPENAT => io_sys::sys_openat(a[0], a[1], a[2], a[3]),
        sysno::CLOSE => io_sys::sys_close(a[0]),
        sysno::LSEEK => io_sys::sys_lseek(a[0], a[1], a[2]),
        sysno::PREAD64 => io_sys::sys_pread64(a[0], a[1], a[2], a[3]),
        sysno::PWRITE64 => io_sys::sys_pwrite64(a[0], a[1], a[2], a[3]),
        sysno::READV => io_sys::sys_readv(a[0], a[1], a[2]),
        sysno::PREADV => io_sys::sys_preadv(a[0], a[1], a[2], a[3]),
        sysno::PWRITEV => io_sys::sys_pwritev(a[0], a[1], a[2], a[3]),
        sysno::FSYNC | sysno::FDATASYNC => io_sys::sys_fsync(a[0]),
        sysno::RENAME => io_sys::sys_rename(a[0], a[1]),
        sysno::RENAMEAT => io_sys::sys_renameat(a[0], a[1], a[2], a[3]),
        sysno::RENAMEAT2 => io_sys::sys_renameat2(a[0], a[1], a[2], a[3], a[4]),
        sysno::STAT => io_sys::sys_stat(a[0], a[1]),
        sysno::FSTAT => io_sys::sys_fstat(a[0], a[1]),
        sysno::LSTAT => io_sys::sys_lstat(a[0], a[1]),
        sysno::NEWFSTATAT => io_sys::sys_newfstatat(a[0], a[1], a[2], a[3]),
        sysno::IOCTL => io_sys::sys_ioctl(a[0], a[1], a[2]),
        sysno::ACCESS => io_sys::sys_access(a[0], a[1]),
        sysno::SENDFILE => io_sys::sys_sendfile(a[0], a[1], a[2], a[3]),
        sysno::COPY_FILE_RANGE => io_sys::sys_copy_file_range(a[0], a[1], a[2], a[3], a[4], a[5]),
        sysno::MKDIR => io_sys::sys_mkdir(a[0], a[1]),
        sysno::RMDIR => io_sys::sys_rmdir(a[0]),
        sysno::UNLINK => io_sys::sys_unlink(a[0]),
        sysno::MKDIRAT => io_sys::sys_mkdirat(a[0], a[1], a[2]),
        sysno::UNLINKAT => io_sys::sys_unlinkat(a[0], a[1], a[2]),
        sysno::FCHOWNAT => io_sys::sys_fchownat(a[0], a[1], a[2], a[3], a[4]),
        sysno::CHMOD => io_sys::sys_chmod(a[0], a[1]),
        sysno::FCHMOD => io_sys::sys_fchmod(a[0], a[1]),
        sysno::FCHMODAT => io_sys::sys_fchmodat(a[0], a[1], a[2], a[3]),
        sysno::FACCESSAT => io_sys::sys_faccessat(a[0], a[1], a[2], a[3]),
        sysno::CHOWN => io_sys::sys_chown(a[0], a[1], a[2]),
        sysno::FCHOWN => io_sys::sys_fchown(a[0], a[1], a[2]),
        sysno::POLL => io_sys::sys_poll(a[0], a[1], a[2]),
        sysno::SELECT => io_sys::sys_select(a[0], a[1], a[2], a[3], a[4]),
        sysno::PSELECT6 => io_sys::sys_pselect6(a[0], a[1], a[2], a[3], a[4], a[5]),
        sysno::PPOLL => io_sys::sys_ppoll(a[0], a[1], a[2], a[3]),
        sysno::CLOCK_GETRES => epoll_sys::sys_clock_getres(a[0], a[1]),
        // Стенное время здесь только читается (производное от тиков); его
        // установка — EPERM.
        sysno::CLOCK_SETTIME => Err(crate::arch::x86_64::linux::errno::Errno::EPERM),
        sysno::EVENTFD2 => epoll_sys::sys_eventfd2(a[0], a[1]),
        sysno::EVENTFD => epoll_sys::sys_eventfd2(a[0], 0),
        sysno::TIMERFD_CREATE => timerfd::sys_timerfd_create(a[0], a[1]),
        sysno::TIMERFD_SETTIME => timerfd::sys_timerfd_settime(a[0], a[1], a[2], a[3]),
        sysno::TIMERFD_GETTIME => timerfd::sys_timerfd_gettime(a[0], a[1]),
        sysno::SIGNALFD => signalfd::sys_signalfd(a[0], a[1], a[2]),
        sysno::SIGNALFD4 => signalfd::sys_signalfd4(a[0], a[1], a[2], a[3]),
        sysno::EPOLL_CREATE1 => epoll_sys::sys_epoll_create1(a[0]),
        sysno::EPOLL_CREATE => epoll_sys::sys_epoll_create1(0),
        sysno::SCHED_SETAFFINITY => misc::sys_sched_setaffinity(a[0], a[1], a[2]),
        sysno::SCHED_GETAFFINITY => misc::sys_sched_getaffinity(a[0], a[1], a[2]),
        sysno::GETCPU => misc::sys_getcpu(a[0], a[1], a[2]),
        sysno::FADVISE64 => misc::sys_fadvise64(a[0], a[1], a[2], a[3]),
        sysno::EPOLL_CTL => epoll_sys::sys_epoll_ctl(a[0], a[1], a[2], a[3]),
        sysno::EPOLL_WAIT => epoll_sys::sys_epoll_wait(a[0], a[1], a[2], a[3]),
        sysno::PIPE => io_sys::sys_pipe(a[0]),
        sysno::PIPE2 => io_sys::sys_pipe2(a[0], a[1]),
        // ── поддержка uv_spawn в nvim ──
        sysno::SOCKETPAIR => io_sys::sys_socketpair(a[0], a[1], a[2], a[3]),
        sysno::STATX => io_sys::sys_statx(a[0], a[1], a[2], a[3], a[4]),
        sysno::EPOLL_PWAIT => epoll_sys::sys_epoll_pwait(a[0], a[1], a[2], a[3], a[4]),
        sysno::SOCKET => {
            if a[0] == crate::arch::x86_64::linux::inet_sock::AF_INET {
                crate::arch::x86_64::linux::inet_sock::sys_socket_in(a[0], a[1])
            } else {
                unix_sock::sys_socket(a[0], a[1], a[2])
            }
        }
        sysno::CONNECT => {
            // Подсмотреть семейство sockaddr, чтобы выбрать стек (реестр AF_UNIX
            // vs собственный стек AF_INET).
            let fam = if a[1] != 0 && a[2] >= 2 {
                // Прежний комментарий обещал «проверку границ в стиле
                // check_user_ptr», но проверялся только `a[2]` (длина):
                // `a[1] = 0x8000...` заставлял чтение из ring 0 получить #GP, и
                // обработчик отказа останавливал машину. Проверить адрес, затем
                // читать.
                check_user_ptr(a[1], 2)?;
                // SAFETY: 2-байтное пользовательское чтение после того, как
                // `check_user_ptr` доказал, что `[a[1], a[1]+2)` в пользовательской
                // половине; невыровненное чтение только поля sa_family, никогда за
                // пределами 2 байт.
                (unsafe { core::ptr::read_unaligned(a[1] as *const u16) }) as u64
            } else {
                1
            };
            if fam == crate::arch::x86_64::linux::inet_sock::AF_INET {
                let is_udp = crate::task::compat::with_current_compat(|cs| {
                    matches!(
                        cs.fds.get(a[0] as u32),
                        Some(crate::task::fd::OpenObject::InetUdp(_))
                    )
                })
                .unwrap_or(false);
                if is_udp {
                    crate::arch::x86_64::linux::inet_sock::udp_connect_fd(a[0], a[1], a[2])
                } else {
                    crate::arch::x86_64::linux::inet_sock::sys_connect_tcp(a[0], a[1], a[2])
                }
            } else {
                unix_sock::sys_connect(a[0], a[1], a[2])
            }
        }
        sysno::SENDTO => {
            // Маршрутизировать по типу fd. Rust-std зовёт `send()` (== `sendto`)
            // для AF_UNIX и TCP, а не `write()`, поэтому одной UDP-ветки мало:
            // без неё `UnixStream::write` из дочернего демо-клиента получал
            // EINVAL. Для подключённого UDP-сокета glibc передаёт
            // dest_addr=NULL (стиль write); явный адрес побеждает.
            enum SendTarget {
                Udp,
                Tcp,
                Unix,
            }
            let target =
                crate::task::compat::with_current_compat(|cs| match cs.fds.get(a[0] as u32) {
                    None => Err(Errno::EBADF),
                    Some(crate::task::fd::OpenObject::InetUdp(_)) => Ok(SendTarget::Udp),
                    Some(crate::task::fd::OpenObject::InetTcp(_)) => Ok(SendTarget::Tcp),
                    Some(crate::task::fd::OpenObject::Socket { .. }) => Ok(SendTarget::Unix),
                    Some(crate::task::fd::OpenObject::UnixListener(_))
                    | Some(crate::task::fd::OpenObject::UnixSocketUnbound { .. }) => {
                        Err(Errno::ENOTCONN)
                    }
                    Some(_) => Err(Errno::ENOTSOCK),
                })
                .unwrap_or(Err(Errno::EBADF))?;
            if matches!(target, SendTarget::Unix) {
                return crate::arch::x86_64::linux::unix_sock::sys_sendto_unix(
                    a[0], a[1], a[2], a[3], a[4], a[5],
                );
            }
            if matches!(target, SendTarget::Tcp) {
                const STREAM_SEND_FLAGS: u64 = (crate::arch::x86_64::linux::cmsg::MSG_DONTWAIT
                    | crate::arch::x86_64::linux::cmsg::MSG_EOR
                    | crate::arch::x86_64::linux::cmsg::MSG_NOSIGNAL)
                    as u64;
                if a[3] & !STREAM_SEND_FLAGS != 0 {
                    return Err(Errno::EINVAL);
                }
                // TCP — поток байтов, а не датаграмма: длина больше
                // [MAX_SOCKET_IO] здесь не ошибка (Linux копирует сколько
                // помещается и возвращает короткую запись), поэтому предел
                // работает как граница одной копии, а не как EMSGSIZE.
                let len = crate::arch::x86_64::linux::abi::stream_send_len(a[2]);
                check_user_ptr(a[1], len)?;
                let data = crate::arch::x86_64::linux::io_sys::copy_in_pub(a[1], len);
                let n = crate::arch::x86_64::linux::inet_sock::tcp_write(a[0], &data)?;
                return Ok(n as u64);
            }
            // Класс аллокационного переполнения: обход указателя ниже ограничен,
            // но КОПИРОВАНИЕ обязано использовать ту же границу, иначе вызывающий
            // всего с 1 МиБ отображённых страниц заставит ядро выделить/скопировать
            // сырой (выбранный атакующим) счётчик. Слишком большие датаграммы —
            // EMSGSIZE до любой аллокации.
            let len = crate::arch::x86_64::linux::abi::socket_payload_len(a[2], true)?;
            check_user_ptr(a[1], len)?;
            let data = crate::arch::x86_64::linux::io_sys::copy_in_pub(a[1], len);
            let n = if a[4] != 0 && a[5] >= 8 {
                crate::arch::x86_64::linux::inet_sock::udp_sendto_fd(a[0], &data, a[4], a[5])?
            } else {
                crate::arch::x86_64::linux::inet_sock::udp_write_fd(a[0], &data)?
            };
            Ok(n as u64)
        }
        sysno::RECVFROM => {
            // Маршрутизировать по типу fd: UDP-сокеты резолвера glibc,
            // TCP/AF_UNIX — `recv()` из Rust-std (== `recvfrom` с NULL-адресом).
            enum RecvTarget {
                Udp,
                Tcp,
                Unix,
            }
            let target =
                crate::task::compat::with_current_compat(|cs| match cs.fds.get(a[0] as u32) {
                    None => Err(Errno::EBADF),
                    Some(crate::task::fd::OpenObject::InetUdp(_)) => Ok(RecvTarget::Udp),
                    Some(crate::task::fd::OpenObject::InetTcp(_)) => Ok(RecvTarget::Tcp),
                    Some(crate::task::fd::OpenObject::Socket { .. }) => Ok(RecvTarget::Unix),
                    Some(crate::task::fd::OpenObject::UnixListener(_))
                    | Some(crate::task::fd::OpenObject::UnixSocketUnbound { .. }) => {
                        Err(Errno::ENOTCONN)
                    }
                    Some(_) => Err(Errno::ENOTSOCK),
                })
                .unwrap_or(Err(Errno::EBADF))?;
            if matches!(target, RecvTarget::Unix) {
                return crate::arch::x86_64::linux::unix_sock::sys_recvfrom_unix(
                    a[0], a[1], a[2], a[3], a[4], a[5],
                );
            }
            if matches!(target, RecvTarget::Tcp) {
                const STREAM_RECV_FLAGS: u64 =
                    crate::arch::x86_64::linux::cmsg::MSG_DONTWAIT as u64;
                if a[3] & !STREAM_RECV_FLAGS != 0 {
                    return Err(Errno::EINVAL);
                }
                let cap = crate::arch::x86_64::linux::abi::socket_payload_len(a[2], false)?;
                check_user_ptr_writable(a[1], cap)?;
                let mut data = alloc::vec![0u8; cap as usize];
                let n = crate::arch::x86_64::linux::inet_sock::tcp_read(a[0], &mut data)?;
                crate::arch::x86_64::linux::io_sys::copy_out_pub(a[1], &data[..n])?;
                if a[4] != 0 && a[5] != 0 {
                    check_user_ptr_writable(a[5], 4)?;
                    // SAFETY: 4-байтный socklen_t проверен выше; подключённый
                    // stream-сокет адреса отправителя не имеет.
                    unsafe { core::ptr::write_unaligned(a[5] as *mut u32, 0) };
                }
                return Ok(n as u64);
            }
            // Та же ошибка границы аллокации, что в sendto: счётчик — это только
            // ёмкость буфера вызывающего, поэтому ограничиваем его MAX_SOCKET_IO,
            // валидируем ровно диапазон, который ядро выделит/скопирует, и никогда
            // не просим сырой u64-счётчик.
            let len = crate::arch::x86_64::linux::abi::socket_payload_len(a[2], false)?;
            check_user_ptr_writable(a[1], len)?;
            // Буфер полезной нагрузки проверен, но эта ветка — единственный
            // сокетный путь, пишущий sockaddr обратно в память пользователя. Оба
            // выхода — простые пользовательские указатели (16 байт достигали
            // адреса ядра и перезаписывали TICK_COUNT). Проверить их до приёма,
            // чтобы управляемые партнёром данные не попали по непроверенному
            // адресу.
            if a[4] != 0 && a[5] != 0 {
                // `addrlen` — socklen_t (4 байта); sockaddr — 16 байт
                // (sockaddr_in) или 28 (sockaddr_in6), проверяется по семейству
                // ниже.
                check_user_ptr_writable(a[5], 4)?;
            } else {
                // Один указатель без другого — бессмысленный запрос; Linux в этом
                // случае игнорирует выход, и мы тоже.
            }
            let mut buf = alloc::vec![0u8; len as usize];
            let (n, src) = crate::arch::x86_64::linux::inet_sock::udp_recvfrom_fd(a[0], &mut buf)?;
            crate::arch::x86_64::linux::io_sys::copy_out_pub(a[1], &buf[..n])?;
            if a[4] != 0 && a[5] != 0 {
                match src.addr {
                    crate::net::IpAddr::V4(v4) => {
                        check_user_ptr_writable(a[4], 16)?;
                        // struct sockaddr_in { family=2, port(be16), addr, zero }
                        let mut sa = [0u8; 16];
                        sa[0..2].copy_from_slice(&2u16.to_ne_bytes());
                        sa[2..4].copy_from_slice(&src.port.to_be_bytes());
                        sa[4..8].copy_from_slice(&v4.octets());
                        // SAFETY: addrlen (a[5]) != 0 проверен выше; фиксированная
                        // 16-байтная структура + 4-байтный socklen_t пишутся в память
                        // пользователя по указателям из syscall, оба допускают
                        // невыровненность.
                        unsafe {
                            core::ptr::write_unaligned(a[4] as *mut [u8; 16], sa);
                            core::ptr::write_unaligned(a[5] as *mut u32, 16);
                        }
                    }
                    crate::net::IpAddr::V6(v6) => {
                        check_user_ptr_writable(a[4], 28)?;
                        // struct sockaddr_in6 { family=10, port(be16), flowinfo,
                        // addr(16), scope_id }
                        let mut sa = [0u8; 28];
                        sa[0..2].copy_from_slice(&10u16.to_ne_bytes());
                        sa[2..4].copy_from_slice(&src.port.to_be_bytes());
                        sa[8..24].copy_from_slice(&v6.octets());
                        // SAFETY: addrlen (a[5]) != 0 проверен выше; фиксированная
                        // 28-байтная структура + 4-байтный socklen_t пишутся в память
                        // пользователя по указателям из syscall, оба допускают
                        // невыровненность.
                        unsafe {
                            core::ptr::write_unaligned(a[4] as *mut [u8; 28], sa);
                            core::ptr::write_unaligned(a[5] as *mut u32, 28);
                        }
                    }
                }
            }
            Ok(n as u64)
        }
        // ── AF_UNIX SCM_RIGHTS: fd passing для userspace-композитора ──
        sysno::SENDMSG => unix_sock::sys_sendmsg(a[0], a[1], a[2]),
        sysno::RECVMSG => unix_sock::sys_recvmsg(a[0], a[1], a[2]),
        // ── shutdown: половинное закрытие направления (48) ──
        // AF_UNIX ставит флаги на очередях направлений, AF_INET TCP заказывает
        // FIN; и то и другое будит заблокированного читателя/писателя.
        sysno::SHUTDOWN => unix_sock::sys_shutdown(a[0], a[1]),
        sysno::ACCEPT => unix_sock::sys_accept(a[0], a[1], a[2]),
        sysno::BIND => unix_sock::sys_bind(a[0], a[1], a[2]),
        sysno::LISTEN => unix_sock::sys_listen(a[0], a[1]),
        sysno::GETSOCKNAME => unix_sock::sys_getsockname(a[0], a[1], a[2]),
        sysno::ACCEPT4 => unix_sock::sys_accept4(a[0], a[1], a[2], a[3]),
        sysno::SETSOCKOPT => unix_sock::sys_setsockopt(a[0], a[1], a[2], a[3], a[4]),
        sysno::GETSOCKOPT => {
            let is_inet = crate::task::compat::with_current_compat(|cs| {
                matches!(
                    cs.fds.get(a[0] as u32),
                    Some(crate::task::fd::OpenObject::InetTcp(_))
                )
            })
            .unwrap_or(false);
            if is_inet {
                crate::arch::x86_64::linux::inet_sock::getsockopt_in(None, a[1], a[2], a[3], a[4])
            } else {
                unix_sock::sys_getsockopt(a[0], a[1], a[2], a[3], a[4])
            }
        }
        sysno::SETSID => misc::sys_setsid(),
        sysno::SETPGID => misc::sys_setpgid(a[0], a[1]),
        sysno::GETPGRP => misc::sys_getpgrp(),
        sysno::GETPGID => misc::sys_getpgid(a[0]),
        sysno::UMASK => misc::sys_umask(a[0]),
        sysno::FLOCK => misc::sys_flock(a[0], a[1]),
        // ── каталог / путь / fd ──
        sysno::GETDENTS64 => io_sys::sys_getdents64(a[0], a[1], a[2]),
        sysno::GETCWD => io_sys::sys_getcwd(a[0], a[1]),
        sysno::CHDIR => io_sys::sys_chdir(a[0]),
        sysno::FCHDIR => io_sys::sys_fchdir(a[0]),
        sysno::DUP => io_sys::sys_dup(a[0]),
        sysno::DUP2 => io_sys::sys_dup2(a[0], a[1]),
        sysno::DUP3 => io_sys::sys_dup3(a[0], a[1], a[2]),
        sysno::FCNTL => io_sys::sys_fcntl(a[0], a[1], a[2]),
        sysno::TRUNCATE => io_sys::sys_truncate(a[0], a[1]),
        sysno::FTRUNCATE => io_sys::sys_ftruncate(a[0], a[1]),
        sysno::READLINK => io_sys::sys_readlink(a[0], a[1], a[2]),
        sysno::READLINKAT => io_sys::sys_readlinkat(a[0], a[1], a[2], a[3]),
        sysno::STATFS => io_sys::sys_statfs(a[0], a[1]),
        sysno::FSTATFS => io_sys::sys_fstatfs(a[0], a[1]),
        // ── память ──
        sysno::BRK => mem_sys::sys_brk(a[0]),
        sysno::MMAP => mem_sys::sys_mmap(a[0], a[1], a[2], a[3], a[4], a[5]),
        sysno::MUNMAP => mem_sys::sys_munmap(a[0], a[1]),
        sysno::MREMAP => mem_sys::sys_mremap(a[0], a[1], a[2], a[3], a[4]),
        sysno::MPROTECT => mem_sys::sys_mprotect(a[0], a[1], a[2]),
        // `madvise` чисто рекомендателен: принять любую подсказку и ничего не
        // делать.
        sysno::TGKILL => misc::sys_tgkill(a[0], a[1], a[2]),
        sysno::KILL => signal::sys_kill(a[0], a[1]),
        sysno::MADVISE => Ok(0),
        // ── прочее + процессы ──
        sysno::GETPID => misc::sys_getpid(),
        sysno::WAIT4 => process_sys::sys_wait4(a[0], a[1], a[2], a[3]),
        sysno::UNAME => misc::sys_uname(a[0]),
        sysno::ARCH_PRCTL => misc::sys_arch_prctl(a[0], a[1]),
        sysno::SET_TID_ADDRESS => misc::sys_set_tid_address(a[0]),
        sysno::CLOCK_GETTIME => misc::sys_clock_gettime(a[0], a[1]),
        sysno::GETRANDOM => misc::sys_getrandom(a[0], a[1], a[2]),
        sysno::FUTEX => process_sys::sys_futex(a[0], a[1], a[2], a[3], a[4], a[5]),
        // ── идентичность / информация / время / сон / планирование / сигналы ──
        sysno::GETUID => misc::sys_getuid(),
        sysno::GETGID => misc::sys_getgid(),
        sysno::GETEUID => misc::sys_geteuid(),
        sysno::GETEGID => misc::sys_getegid(),
        sysno::SETUID => misc::sys_setuid(a[0]),
        sysno::SETGID => misc::sys_setgid(a[0]),
        sysno::SETREUID => misc::sys_setreuid(a[0], a[1]),
        sysno::SETREGID => misc::sys_setregid(a[0], a[1]),
        sysno::SETRESUID => misc::sys_setresuid(a[0], a[1], a[2]),
        sysno::SETRESGID => misc::sys_setresgid(a[0], a[1], a[2]),
        sysno::GETRESUID => misc::sys_getresuid(a[0], a[1], a[2]),
        sysno::GETRESGID => misc::sys_getresgid(a[0], a[1], a[2]),
        sysno::GETPPID => misc::sys_getppid(),
        sysno::GETTID => misc::sys_gettid(),
        sysno::GETTIMEOFDAY => misc::sys_gettimeofday(a[0], a[1]),
        sysno::TIME => misc::sys_time(a[0]),
        sysno::NANOSLEEP => misc::sys_nanosleep(a[0], a[1]),
        sysno::CLOCK_NANOSLEEP => misc::sys_clock_nanosleep(a[0], a[1], a[2], a[3]),
        sysno::SYSINFO => misc::sys_sysinfo(a[0]),
        sysno::SCHED_YIELD => misc::sys_sched_yield(),
        sysno::RT_SIGACTION => misc::sys_rt_sigaction(a[0], a[1], a[2], a[3]),
        sysno::RT_SIGPROCMASK => misc::sys_rt_sigprocmask(a[0], a[1], a[2], a[3]),
        sysno::SIGALTSTACK => misc::sys_sigaltstack(a[0], a[1]),
        sysno::SET_ROBUST_LIST => misc::sys_set_robust_list(a[0], a[1]),
        sysno::GET_ROBUST_LIST => misc::sys_get_robust_list(a[0], a[1], a[2]),
        sysno::RSEQ => misc::sys_rseq(),
        sysno::PRLIMIT64 => misc::sys_prlimit64(a[0], a[1], a[2], a[3]),
        sysno::GETRLIMIT => misc::sys_getrlimit(a[0], a[1]),
        sysno::SETRLIMIT => misc::sys_setrlimit(a[0], a[1]),
        sysno::FACCESSAT2 => io_sys::sys_faccessat(a[0], a[1], a[2], a[3]),
        sysno::READAHEAD => misc::sys_readahead(a[0], a[1], a[2]),
        sysno::SYNC => Ok(0),
        sysno::SYNCFS => misc::sys_syncfs(a[0]),
        sysno::GETPRIORITY => misc::sys_getpriority(a[0], a[1]),
        sysno::SETPRIORITY => misc::sys_setpriority(a[0], a[1], a[2]),
        sysno::PERSONALITY => Ok(0),
        sysno::SCHED_GETPARAM => misc::sys_sched_getparam(a[0], a[1]),
        sysno::SCHED_SETSCHEDULER => misc::sys_sched_setscheduler(a[0], a[1], a[2]),
        sysno::SCHED_GETSCHEDULER => Ok(0),
        sysno::SCHED_GET_PRIORITY_MAX | sysno::SCHED_GET_PRIORITY_MIN => Ok(0),
        sysno::SCHED_RR_GET_INTERVAL => misc::sys_sched_rr_get_interval(a[0], a[1]),
        sysno::RT_SIGPENDING => misc::sys_rt_sigpending(a[0], a[1]),
        sysno::SYNC_FILE_RANGE => misc::sys_sync_file_range(a[0], a[1], a[2], a[3]),
        sysno::FALLOCATE => misc::sys_fallocate(a[0], a[1], a[2], a[3]),
        sysno::MEMFD_CREATE => io_sys::sys_memfd_create(a[0], a[1]),
        sysno::GETRUSAGE => misc::sys_getrusage(a[0], a[1]),
        sysno::TIMES => misc::sys_times(a[0]),
        // `exit`/`exit_group` расходятся (никогда не возвращаются); `!`
        // приводится к типу ветки `Result`.
        sysno::EXIT => misc::sys_exit(a[0]),
        sysno::EXIT_GROUP => misc::sys_exit_group(a[0]),
        // Растяжка дрейфа `abi::SUPPORTED_SYSCALLS`: у числа, принятого гейтом,
        // обязана быть реальная ветка выше. Дойти сюда — значит перечисленный
        // список покрытия и этот match разошлись, поэтому назвать номер, а не
        // вернуть молчаливый вечный ENOSYS (тест в `abi.rs` закрепляет числа, эта
        // ветка — арм'ы).
        _ => {
            crate::error!(
                "[linux] nr={} is gated as supported but has NO dispatch arm (SUPPORTED_SYSCALLS drift)",
                nr
            );
            Err(Errno::ENOSYS)
        }
    }
}

/// Единая воронка для обоих входных стабов syscall Linux (`int80_stub` и
/// `syscall_entry` инструкции `syscall`).
///
/// ## Соглашение вызова
///
/// Оба стаба сохраняют все 15 GPR в идентичный фрейм [`SavedRegs`] на стеке ядра
/// и передают один указатель на этот фрейм (`rdi = &SavedRegs`). Единый
/// `*mut SavedRegs` — вместо размазывания шести регистров-аргументов Linux по
/// SysV C ABI — чисто обходит предел в шесть регистров и позволяет диспетчеру и
/// читать номер/аргументы из фрейма, и менять сохранённые регистры (например,
/// `FS.base`). Возвращаемое значение стаб пишет в сохранённый слот `rax`, так что
/// оно становится результатом syscall в `rax` при возврате в ring 3 (R1.2, R1.3);
/// все остальные GPR восстанавливаются без изменений (R1.7).
///
/// ## Статус: полная Linux-маршрутизация
///
/// Читает номер и аргументы через [`abi::marshal_args`] (R1.1, R1.8), затем:
///
///   1. **Шим приоритета.** Если у процесса НЕТ зарегистрированного
///      [`CompatState`](crate::task::compat::CompatState) (нативная задача pagh) и
///      номер — один из трёх legacy-вызовов pagh, он делегируется legacy-
///      диспетчеру, чтобы существующий путь загрузки/тестов работал.
///      Compat_Process (есть compat-состояние) вместо этого получает полную
///      семантику Linux — поэтому числовое пересечение с Linux `open`(2)/
///      `close`(3) решается правилом «нативное ⇒ legacy, Linux ⇒ Linux».
///   2. **Гейт supported-set (R1.4, R11.4, R11.5).** Неподдерживаемые номера
///      (включая `clone`/`fork`/`vfork`/`futex` и графические syscall'ы) логируют
///      одну диагностику nosys и возвращают `-ENOSYS` **до инспекции любого
///      указателя-аргумента**.
///   3. **Маршрутизация.** Поддерживаемые номера идут в обработчики io/mem/misc,
///      каждый из которых прогоняет свои указатели через единую точку
///      [`check_user_ptr`]. `Result<u64, Errno>` обработчика складывается
///      `Ok(v) -> v` / `Err(e) -> -errno` (R1.3) в значение, записываемое в `rax`;
///      все прочие GPR сохраняет входной стаб (R1.7).
///
/// Второй аргумент [`linux_dispatch`] для НАСТОЯЩЕГО syscall: вызывающий —
/// планируемая задача, чей фрейм безопасно припаркован, поэтому диспетчер может
/// размаскировать прерывания на время обработчика.
///
/// Boot-selftest вызывает диспетчер НАПРЯМУЮ и передаёт `0`: он работает на потоке
/// загрузки, который не планируемая задача (см. `linux_dispatch`).
/// Бит 0 аргумента `reentry_allowed` у [`linux_dispatch`]: вызывающий — настоящий
/// syscall на планируемой задаче (оба входных стаба передают его), поэтому
/// диспетчер может размаскировать прерывания на время обработчика.
pub const REENTRY_ALLOWED: u64 = 1;

/// Бит 1 `reentry_allowed`: syscall пришёл через `int 0x80` (его ставит только
/// `syscall::int80_stub`, то есть legacy-вход pagh).
///
/// Инвариант 2 из AGENTS.md запрещает этот путь для Compat_Process: на нём
/// сохранённые `rcx`/`r11` — обычные пользовательские GPR, а слово по
/// `SavedRegs + 120` — положенный CPU user RIP, а не per-task слот user-RSP, на
/// который опираются `execve`, `clone` и доставка сигналов. `linux_dispatch`
/// записывает наблюдение на процесс, чтобы сигнальный путь мог отказаться от
/// доставки фрейма (`signal::deliver_one_pending_syscall`) вместо записи
/// `rt_sigframe` поверх кода пользователя. У нативных задач compat-состояния нет
/// (флаг тогда нигде не записывается), и они работают как раньше.
pub const INT80_ENTRY: u64 = 2;

/// # Safety
///
/// `regs` обязан указывать на полностью инициализированный фрейм [`SavedRegs`]
/// на текущем стеке ядра, точно как его строят входные стабы. Стабы это
/// гарантируют.
///
/// `reentry_allowed` должен быть [`REENTRY_ALLOWED`], только когда вызывающий
/// действительно syscall на планируемой задаче (оба входных стаба передают его);
/// boot-selftest передаёт 0.
#[no_mangle]
pub extern "C" fn linux_dispatch(regs: *mut SavedRegs, reentry_allowed: u64) -> u64 {
    // SAFETY: входные стабы всегда передают указатель на 15-регистровый фрейм,
    // который только что положили на собственный стек ядра текущей задачи; он
    // живёт дольше вызова и уникально принадлежит на время (у каждой задачи свой
    // стек ядра, поэтому вытеснение не может алиасить фрейм).
    let r = unsafe { &mut *regs };

    // Стабы входят с замаскированным IF (SFMASK / шлюз прерывания). Фрейм и
    // per-task слот user-RSP теперь безопасно припаркованы на стеке ядра этой
    // задачи, поэтому окно безопасно для вытеснения: включаем прерывания, чтобы
    // таймер тикал, пока обработчики блокируются (futex/poll/pipe, nanosleep).
    // С замаскированным IF `ticks()` не растёт — таймауты не срабатывают, — а
    // `hlt` в `sleep_ticks` спал бы вечно. Хвост выхода syscall снова маскирует
    // IF (`cli`) до снятия фрейма.
    //
    // Зависит от ВЫЗЫВАЮЩЕГО, а не от текущего IF: каждый вызывающий приходит с
    // замаскированным IF (SFMASK на `syscall`, шлюз прерывания для `int 0x80`),
    // поэтому проверка живого флага никогда ничего не размаскировала бы. Разница —
    // кто зовёт, см. REENTRY_ALLOWED.
    //
    // Boot-selftest вызывает диспетчер НАПРЯМУЮ, с потока загрузки, всё ещё с
    // замаскированными прерываниями (весь путь загрузки идёт так, пока
    // `boot::kernel_main` не включит их перед idle-циклом). Размаскировка там —
    // тихое зависание машины, а не ускорение: у потока загрузки
    // `current_pid() == IDLE_PID`, поэтому первый тик таймера складывает его стек
    // под idle-задачу и переключается на всегда готовые потоки шелла/net, а
    // поскольку те двое постоянно в ready-очереди, тик никогда не восстановит
    // поток загрузки — он навсегда останется припаркованным посреди `sti`. Этот
    // вызывающий передаёт нулевой `reentry_allowed` и оставляет прерывания как
    // были; поток загрузки доходит до idle-цикла всё ещё с маской, и
    // `boot::kernel_main` включает их там, как и задумано.
    if reentry_allowed & REENTRY_ALLOWED != 0 {
        crate::arch::cpu::enable_interrupts();
    }

    // Наблюдение `int 0x80` (бит 1) записывается на процесс и здесь никогда не
    // используется: диспетчер сам может обслужить такого вызывающего (legacy-
    // нативный процесс и до-миграционный compat-тест оба так делали), но
    // сигнальный путь не может доверять смыслу его сохранённых регистров (см.
    // INT80_ENTRY).
    if reentry_allowed & INT80_ENTRY != 0 {
        crate::task::compat::note_int80_entry();
    }

    let (nr, args) = abi::marshal_args(r.rax, r.rdi, r.rsi, r.rdx, r.r10, r.r8, r.r9);

    // Учёт контекста постмортема #GP (последний диспетчеризованный syscall).
    let cur_pid = crate::task::scheduler::current_pid();
    crate::arch::x86_64::idt::note_syscall(cur_pid, nr);

    // ── 1. Шим приоритета: нативные задачи сохраняют legacy-маршрутизацию pagh ──
    // Процесс С compat-состоянием — это Linux Compat_Process, он обходит шим и
    // получает полную семантику Linux для каждого номера (включая 1/2/3).
    if !crate::task::compat::current_has_compat() {
        use crate::arch::x86_64::syscall::{legacy_dispatch, SYS_EXIT, SYS_WRITE, SYS_YIELD};
        if matches!(nr, SYS_WRITE | SYS_EXIT | SYS_YIELD) {
            // a1/a2/a3 == rdi/rsi/rdx, как в legacy-соглашении о трёх аргументах.
            return legacy_dispatch(nr, args[0], args[1], args[2]);
        }
    }

    // ── 2. Гейт supported-set ДО инспекции указателей (R1.4) ──
    if !abi::is_supported(nr) {
        log_nosys(nr);
        return encode_errno(Errno::ENOSYS);
    }

    // ── 3. Маршрутизация в обработчик и складывание результата в rax (R1.3) ──
    // Обернуть маршрутизированный обработчик таблицей watchdog'а зависших
    // syscall'ов, чтобы тихий блок в ядре назвал свой pid + syscall в логе.
    let wd_pid = crate::task::scheduler::current_pid();
    inflight_enter(wd_pid, nr, args[0]);
    let result = if nr == sysno::EXECVE {
        process_sys::sys_execve(r, args[0], args[1], args[2])
    } else if nr == sysno::CLONE {
        process_sys::sys_clone(r, args[0], args[1], args[2], args[3], args[4])
    } else if nr == sysno::RT_SIGRETURN {
        // Нужен сохранённый фрейм (per-task слот user-RSP +120), а не шесть
        // аргументов.
        signal::sys_rt_sigreturn(r)
    } else {
        dispatch_supported(nr, &args)
    };
    inflight_exit(wd_pid);
    let out = match result {
        Ok(v) => v,
        Err(e) => {
            // Возвраты EINVAL логируются с номером syscall и сырыми аргументами.
            // Userspace показывает голый OSError(EINVAL) (python importlib
            // get_data) без способа понять, какой syscall его дал; это указывает
            // виновника на следующем запуске. EINVAL у readlink означает «не
            // симлинк» — законный ответ для regular-файла, а не сбойный syscall.
            if matches!(e, Errno::EINVAL) && nr != sysno::READLINK && einval_diag_allowed() {
                crate::warn!(
                    "[linux] EINVAL diag: nr={} args=[{:#x}, {:#x}, {:#x}, {:#x}]",
                    nr,
                    args[0],
                    args[1],
                    args[2],
                    args[3]
                );
            }
            encode_errno(e)
        }
    };
    // ── 4. Доставка сигнала в точке возврата из syscall ──
    // После складывания результата, но ДО того как входной стаб запишет его в
    // сохранённый слот `rax` и снимет фрейм: ожидающий незаблокированный сигнал
    // с пользовательским обработчиком получает `rt_sigframe` на пользовательском
    // стеке, а сохранённые регистры переписываются так, что `sysretq` попадает в
    // обработчик; сигнал с завершением по умолчанию завершает процесс прямо
    // здесь. Результат прерванного syscall едет внутри сохранённого `rax` фрейма,
    // поэтому обработчик, вернувшийся через `rt_sigreturn`, возобновляется с
    // правильным значением. Нативные задачи (нет compat-состояния) проходят мимо
    // за одну атомарную загрузку.
    signal::deliver_one_pending_syscall(r, out);
    out
}

// ─── watchdog зависших syscall'ов ──────────────────────────────────
// Тихое зависание (чёрный экран nvim) означает, что какой-то Compat_Process
// навсегда припаркован внутри одного блокирующего syscall, и в логе нет ничего,
// что сказало бы, КАКОЙ pid в КАКОМ syscall. linux_dispatch оборачивает каждый
// маршрутизированный syscall в inflight_enter/inflight_exit; watchdog_tick
// (двигаемый yield_current, то есть самими задачами, крутящимися в блокирующих
// циклах) сканирует таблицу in-flight примерно раз в секунду и сообщает о любом,
// застрявшем дольше 5 секунд, повторяя каждые 30 секунд. Записи вышедших pid
// вычищаются лениво. Значения: pid -> (nr, arg0, start_tick, last_warn_tick,
// last_dump_tick).
static SYSCALL_INFLIGHT: crate::sync::spinlock::Spinlock<
    alloc::collections::BTreeMap<u64, (u64, u64, u64, u64, u64)>,
> = crate::sync::spinlock::Spinlock::new(alloc::collections::BTreeMap::new());
static WD_LAST_SCAN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn inflight_enter(pid: u64, nr: u64, arg0: u64) {
    let now = crate::task::scheduler::ticks();
    SYSCALL_INFLIGHT.lock().insert(pid, (nr, arg0, now, 0, 0));
}

fn inflight_exit(pid: u64) {
    SYSCALL_INFLIGHT.lock().remove(&pid);
}

/// Перезапустить часы застрявшего syscall для in-flight записи `pid`
/// (вызывается при возобновлении задачи, припаркованной SIGSTOP). Без этого
/// пауза засчиталась бы в возраст watchdog'а, и возобновлённая задача была бы
/// объявлена «застрявшей» сразу после продолжения.
pub fn inflight_refresh(pid: u64) {
    let now = crate::task::scheduler::ticks();
    if let Some(e) = SYSCALL_INFLIGHT.lock().get_mut(&pid) {
        e.2 = now;
        e.3 = 0;
    }
}

/// Человеческое имя syscall'ов, в которых задача реально может блокироваться.
///
/// `sendto`/`sendmsg` здесь, потому что запись в полную очередь AF_UNIX уступает
/// CPU так же, как чтение. `recvfrom`/`recvmsg` были главным пробелом: именно они
/// печатались как `stuck in syscall ? (nr=45 arg0=0x3 closed)`, пока не был
/// закрыт класс вечного ожидания на закрытом fd (`sock_wait`). `lstat`(6) и
/// `shutdown`(48) не блокируются, но названы, чтобы такой же лог никогда не
/// показывал «?» для сисколла, который кто-то увидит в профиле.
fn wd_sys_name(nr: u64) -> &'static str {
    match nr {
        0 => "read",
        1 => "write",
        6 => "lstat",
        7 => "poll",
        20 => "writev",
        23 => "select",
        35 => "nanosleep",
        43 => "accept",
        44 => "sendto",
        45 => "recvfrom",
        46 => "sendmsg",
        47 => "recvmsg",
        48 => "shutdown",
        61 => "wait4",
        202 => "futex",
        232 => "epoll_wait",
        270 => "pselect6",
        271 => "ppoll",
        281 => "epoll_pwait",
        _ => "?",
    }
}

/// Вызывается из `yield_current` (контекст потока, локов планировщика не
/// держим). `COMPAT_STATES` берётся только ПОСЛЕ отпускания in-flight лока,
/// поэтому два лока никогда не вкладываются и порядок не страдает.
pub fn watchdog_tick() {
    // Застрявшая задача сама крутится через yield_current, поэтому это
    // единственное место, где ЕЁ fd-таблица — текущая: печатаем её список
    // интересов epoll отсюда.
    maybe_dump_self();
    let now = crate::task::scheduler::ticks();
    let last = WD_LAST_SCAN.load(core::sync::atomic::Ordering::Relaxed);
    if now.saturating_sub(last) < 100 {
        return;
    } // скан не чаще раза в секунду
    if WD_LAST_SCAN
        .compare_exchange(
            last,
            now,
            core::sync::atomic::Ordering::Relaxed,
            core::sync::atomic::Ordering::Relaxed,
        )
        .is_err()
    {
        return;
    }
    let snapshot: alloc::vec::Vec<(u64, (u64, u64, u64, u64, u64))> = SYSCALL_INFLIGHT
        .lock()
        .iter()
        .map(|(&p, &e)| (p, e))
        .collect();
    for (pid, (nr, arg0, start, warned, _dumped)) in snapshot {
        if !crate::task::compat::compat_exists(pid) {
            SYSCALL_INFLIGHT.lock().remove(&pid);
            continue;
        }
        // Задача, припаркованная SIGSTOP, не застряла: она ОСТАНОВЛЕНА, и её
        // in-flight запись просто замирает вместе с ней (заблокированную в цикле
        // ожидания задачу может припарковать групповой stop из другого контекста).
        // Сообщение о ней заставило бы `[WATCHDOG]` — маркер проблемы E2E —
        // срабатывать для процесса, делающего ровно то, что ему велели. Часы
        // записи перезапускаются при возобновлении (`inflight_refresh`), поэтому
        // пауза никогда не считается «застреванием».
        if crate::task::scheduler::is_stopped(pid) {
            continue;
        }
        let age = now.saturating_sub(start);
        if age >= 500 && (warned == 0 || now.saturating_sub(warned) >= 3000) {
            let mut still_stuck = false;
            if let Some(e) = SYSCALL_INFLIGHT.lock().get_mut(&pid) {
                // Только тот же экземпляр syscall: свежая запись означает, что
                // задача двинулась дальше.
                if e.2 == start {
                    e.3 = now;
                    still_stuck = true;
                }
            }
            if still_stuck {
                crate::warn!(
                    "[WATCHDOG] pid={} stuck in syscall {} (nr={} arg0=0x{:x} {}) for {}s",
                    pid,
                    wd_sys_name(nr),
                    nr,
                    arg0,
                    crate::arch::x86_64::linux::io_sys::fd_kind(arg0),
                    age / crate::arch::x86_64::apic::TICK_HZ
                );
            }
        }
    }
}

/// Когда ТЕКУЩАЯ задача — та, что застряла в epoll_wait (232) или epoll_pwait
/// (281), печатать её список интересов с живой готовностью примерно раз в 10 с.
/// Выполняется в контексте самой застрявшей задачи (она крутится через
/// yield_current), поэтому обычные хелперы fd текущей задачи разрешаются по
/// правильной таблице.
fn maybe_dump_self() {
    let pid = crate::task::scheduler::current_pid();
    let now = crate::task::scheduler::ticks();
    let epfd = {
        let mut map = SYSCALL_INFLIGHT.lock();
        match map.get_mut(&pid) {
            Some(e) if e.0 == 232 || e.0 == 281 => {
                let age = now.saturating_sub(e.2);
                if age >= 500 && now.saturating_sub(e.4) >= 1000 {
                    e.4 = now;
                    Some(e.1 as u32)
                } else {
                    None
                }
            }
            _ => None,
        }
    };
    if let Some(epfd) = epfd {
        // Строки [DIAG] времени spawn стираются, когда TUI очищает экран, а
        // дисплей QEMU не умеет прокручиваться назад. Заново печатать решающие
        // улики при каждом дампе: что stdio сейчас и что было НА МОМЕНТ execve
        // (после вычистки close-on-exec).
        let _ = crate::task::compat::with_current_compat(|cs| {
            crate::warn!(
                "[WATCHDOG] pid={} stdio now: fd0={} fd1={} fd2={} | at exec: fd0={} fd1={} fd2={}",
                pid,
                cs.fds.describe_fd(0),
                cs.fds.describe_fd(1),
                cs.fds.describe_fd(2),
                cs.exec_stdio[0],
                cs.exec_stdio[1],
                cs.exec_stdio[2]
            );
        });
        epoll_sys::dump_epoll_self(epfd);
    }
}
