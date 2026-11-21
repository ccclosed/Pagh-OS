//! Kernel-side состояние управляющего терминала: **на каждую консольную сессию**.
//!
//! Консоль — управляющий терминал pagh. Boot-консоль (сессия 0) и каждое
//! терминальное окно (`/dev/vtfb1` + `/dev/vtinput1`, сессии `>= 1`) — отдельные
//! управляющие терминалы: у каждого СВОЯ foreground process group
//! (`tcgetpgrp`/`tcsetpgrp`, то есть `TIOCGPGRP`/`TIOCSPGRP`), свой бит `TOSTOP`
//! и свой `winsize` (`TIOCGWINSZ`/`TIOCSWINSZ` + `SIGWINCH`). Общий глобальный
//! foreground pgid означал бы, что два окна (или окно и boot-shell) отбирают
//! терминал друг у друга; здесь состояние — массивы по индексу консольной
//! сессии (`drivers::session`).
//!
//! Чистая модель решения (какая группа владеет, что делает фоновый доступ, как
//! валидируется аргумент `TIOCSPGRP`, как кодируется `struct winsize`) живёт в
//! [`crate::arch::x86_64::linux::tty`] и host-property-тестируется; этот модуль —
//! kernel-side состояние и эффектный край:
//!
//! * владеет спинлоками на сессию: foreground pgid + `TOSTOP` + `winsize`;
//! * резолвит «владельца ещё нет» в собственную группу shell этой сессии
//!   (владелец терминала по умолчанию — kernel-shell сессии, [`init_shell`]);
//! * [`check_access`] — хук, который зовут пути чтения stdin/консоли и записи:
//!   фоновое задание получает `SIGTTIN` (чтение) или `SIGTTOU` (запись при
//!   `TOSTOP`) в очередь на всю свою группу процессов, а syscall падает с
//!   `EINTR`, так что обычный путь доставки останавливает его на возврате из
//!   syscall;
//! * [`set_winsize`] применяет размер окна от клиента и шлёт `SIGWINCH`
//!   foreground-группе ИМЕННО этой сессии.
//!
//! КТО ТАКОЕ JOB. Управлению доступом к терминалу подлежит только процесс,
//! которому shell передал терминал (через `lxrun`/`fg`, см.
//! `CompatState::tty_job`). Собственные неинтерактивные spawn'ы ядра
//! (верификационные бинарники apt, selftest harness) терминал никогда не
//! получали, и остановка одного из них из-за того, что shell владеет tty,
//! подвесила бы загрузку или E2E-поток, который его ждёт. Форк-ребёнок
//! наследует флаг и привязку к сессии ([`crate::drivers::session::inherit_bind`]),
//! так что подпроцессы задания остаются под управлением терминала.
//!
//! `TOSTOP` по умолчанию ВЫКЛЮЧЕН (как в Linux). Это не косметика: композитор
//! desktop'а запущен фоновым job'ом boot-консоли и пишет диагностику; включённый
//! по умолчанию `TOSTOP` останавливал всю его группу `SIGTTOU`, и окна
//! переставали отвечать (найдено в live-QEMU). Фоновое ЧТЕНИЕ по-прежнему всегда
//! даёт `SIGTTIN`.
//!
//! Пределы доставки — сигнального слоя, не этого модуля: сигнал ставится в
//! очередь и наблюдается на возврате из syscall (или проверкой `EINTR`
//! блокирующего ожидания), так что фоновое задание, припаркованное в долгом
//! вычислении ядра, остановится на следующей границе syscall, а не мгновенно.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::x86_64::linux::errno::Errno;
use crate::arch::x86_64::linux::signal;
use crate::arch::x86_64::linux::signal_frame::{SIGTTIN, SIGTTOU, SIGWINCH};
use crate::arch::x86_64::linux::tty::{
    surface_winsize, winsize_changed, TtyAccess, TtyJobControl, TtyVerdict, Winsize,
};
use crate::drivers::session_route::{BOOT_SESSION, MAX_SESSIONS};
use crate::sync::spinlock::Spinlock;

/// The terminal has no explicit owner until the shell claims it. The boot shell
/// is the first `kernel_thread_spawn` in `boot::kernel_main` and the pid counter
/// starts at 1, so 1 is the correct answer even before [`init_shell`] runs
/// (which it does at the top of `shell_main`).
const SHELL_PGID_DEFAULT: u64 = 1;

/// Foreground pgid `0` means "not claimed yet": [`foreground_pgid`] resolves it
/// to the shell's group. A real process group id is never 0.
const UNOWNED: u64 = 0;

/// Состояние job-control каждой консольной сессии: foreground pgid + `TOSTOP`.
///
/// `TOSTOP` стартует ВЫКЛЮЧЕННЫМ, как в Linux: фоновая запись не паркует
/// композитор, пока программа сама не поставит бит через `tcsetattr`.
static TTY: [Spinlock<TtyJobControl>; MAX_SESSIONS] =
    [const { Spinlock::new(TtyJobControl::new(UNOWNED, false)) }; MAX_SESSIONS];

/// Собственная группа процессов kernel-shell каждой сессии (владелец по
/// умолчанию). `0` — ещё не заявлена; [`shell_pgid_for`] отвечает 1 для boot и
/// pid'ом задачи окна для сессии окна.
static SHELL_PGIDS: [AtomicU64; MAX_SESSIONS] = [const { AtomicU64::new(0) }; MAX_SESSIONS];

/// `winsize` каждой сессии; `None` — клиент ещё не присылал `TIOCSWINSZ`, и
/// размер вычисляется из поверхности консоли сессии.
static WINSIZES: [Spinlock<Option<Winsize>>; MAX_SESSIONS] =
    [const { Spinlock::new(None) }; MAX_SESSIONS];

fn slot(session: usize) -> usize {
    if session < MAX_SESSIONS {
        session
    } else {
        BOOT_SESSION
    }
}

/// `winsize` по умолчанию для сессии: живая геометрия ЕЁ консоли.
///
/// Boot-сессия отвечает ровно как раньше — `drivers::vt::dimensions()`
/// (160×43 на экране 1280×800). Окно — размер своей поверхности в клетках;
/// приложение, запущенное в окне, видит свой терминал, а не boot-экран.
fn default_winsize(session: usize) -> Winsize {
    if session == BOOT_SESSION {
        let (cols, rows) = crate::drivers::vt::dimensions();
        return Winsize {
            rows,
            cols,
            xpixel: 0,
            ypixel: 0,
        };
    }
    let (width, height) = crate::drivers::framebuffer::writer_for(session).dimensions();
    if width == 0 || height == 0 {
        // Поверхность ещё не поднята: не выдумывать чужой размер.
        let (cols, rows) = crate::drivers::vt::dimensions();
        return Winsize {
            rows,
            cols,
            xpixel: 0,
            ypixel: 0,
        };
    }
    // Как VT: последняя строка занята status bar, поэтому текст занимает
    // `height - STATUS_BAR_HEIGHT`.
    let content_height = height.saturating_sub(crate::drivers::framebuffer::STATUS_BAR_HEIGHT);
    surface_winsize(
        width,
        content_height,
        crate::drivers::framebuffer::CHAR_WIDTH,
        crate::drivers::framebuffer::CHAR_HEIGHT,
    )
}

/// Record the shell's process group for `session` and, if that terminal has no
/// owner yet, hand it to the shell. Idempotent.
pub fn init_shell_for(session: usize, pgid: u64) {
    if pgid == 0 {
        return;
    }
    let s = slot(session);
    SHELL_PGIDS[s].store(pgid, Ordering::Relaxed);
    let mut tty = TTY[s].lock();
    if tty.foreground() == UNOWNED {
        tty.set_foreground(pgid);
    }
}

/// The kernel shell's process group of `session` (the value `lxrun` restores).
pub fn shell_pgid_for(session: usize) -> u64 {
    let s = slot(session);
    let stored = SHELL_PGIDS[s].load(Ordering::Relaxed);
    if stored != 0 {
        return stored;
    }
    if s == BOOT_SESSION {
        return SHELL_PGID_DEFAULT;
    }
    // Окно: владелец сессии — его shell-задача; до её первого тика — 1.
    let owner = crate::drivers::session::owner(s);
    if owner == 0 {
        SHELL_PGID_DEFAULT
    } else {
        owner
    }
}

/// [`shell_pgid_for`] для boot-консоли.
pub fn shell_pgid() -> u64 {
    shell_pgid_for(BOOT_SESSION)
}

/// The process group that currently owns the terminal of `session`
/// (`tcgetpgrp`): the shell's group when nothing else has claimed it.
pub fn foreground_pgid_for(session: usize) -> u64 {
    let s = slot(session);
    let fg = TTY[s].lock().foreground();
    if fg == UNOWNED {
        shell_pgid_for(s)
    } else {
        fg
    }
}

/// [`foreground_pgid_for`] для boot-консоли.
pub fn foreground_pgid() -> u64 {
    foreground_pgid_for(BOOT_SESSION)
}

/// Hand the terminal of `session` to `pgid` (`tcsetpgrp`). Validation of the
/// target belongs to the caller
/// ([`crate::arch::x86_64::linux::tty::may_set_foreground`]).
pub fn set_foreground_pgid_for(session: usize, pgid: u64) {
    TTY[slot(session)].lock().set_foreground(pgid);
}

/// [`set_foreground_pgid_for`] для boot-консоли.
pub fn set_foreground_pgid(pgid: u64) {
    set_foreground_pgid_for(BOOT_SESSION, pgid);
}

/// Give the terminal of `session` back to its shell (the foreground job is gone
/// or stopped).
pub fn restore_shell_foreground_for(session: usize) {
    set_foreground_pgid_for(session, shell_pgid_for(session));
}

/// Whether background terminal writes stop their writer (`TOSTOP`) on `session`.
pub fn tostop_for(session: usize) -> bool {
    TTY[slot(session)].lock().tostop()
}

/// [`tostop_for`] для boot-консоли.
pub fn tostop() -> bool {
    tostop_for(BOOT_SESSION)
}

/// Apply the `TOSTOP` bit from a `TCSETS` `c_lflag` image for `session`.
pub fn set_tostop_for(session: usize, on: bool) {
    TTY[slot(session)].lock().set_tostop(on);
}

/// [`set_tostop_for`] для boot-консоли.
pub fn set_tostop(on: bool) {
    set_tostop_for(BOOT_SESSION, on);
}

/// Текущий `winsize` сессии: явно присланный клиентом или выведенный из
/// поверхности.
pub fn winsize_for(session: usize) -> Winsize {
    let s = slot(session);
    if let Some(ws) = *WINSIZES[s].lock() {
        return ws;
    }
    default_winsize(s)
}

/// Применить `TIOCSWINSZ` для `session`.
///
/// Возвращает `true`, когда размер реально изменился и `SIGWINCH` был поставлен
/// foreground-группе. `SIGWINCH` шлётся только на изменение: повторная установка
/// того же размера (readline делает это на старте) не должна будить приложение.
pub fn set_winsize_for(session: usize, ws: Winsize) -> bool {
    let s = slot(session);
    let old = winsize_for(s);
    let changed = winsize_changed(&old, &ws);
    *WINSIZES[s].lock() = Some(ws);
    if !changed {
        return false;
    }
    // Снимок fg под локом tty, доставка — после: не держать два лока сразу.
    let fg = foreground_pgid_for(s);
    crate::info!(
        "[tty] session {} winsize {}x{} -> {}x{}, SIGWINCH to pgid {}",
        s,
        old.cols,
        old.rows,
        ws.cols,
        ws.rows,
        fg
    );
    if fg != 0 {
        // `SIGWINCH` по умолчанию игнорируется; программа с обработчиком
        // получит его на ближайшей границе syscall (тик/syscall-путь).
        let _ = signal::send_signal_to_pgid_all(fg, SIGWINCH);
    }
    true
}

/// [`set_winsize_for`] для boot-консоли (совместимая точка входа).
pub fn set_winsize(ws: Winsize) -> bool {
    set_winsize_for(BOOT_SESSION, ws)
}

/// Забыть терминальное состояние сессии (выход/переоткрытие окна).
///
/// Foreground-группа, группа shell, `TOSTOP` и `winsize` уходят в значения «не
/// заявлено»: новая сессия в том же слоте обязана начать с чистого терминала, а
/// не унаследовать foreground убитого окна.
pub fn reset_session(session: usize) {
    if session == BOOT_SESSION {
        return;
    }
    let s = slot(session);
    *TTY[s].lock() = TtyJobControl::new(UNOWNED, false);
    SHELL_PGIDS[s].store(0, Ordering::Relaxed);
    *WINSIZES[s].lock() = None;
    crate::drivers::ps2_kbd::set_fg_pid(s, 0);
    crate::drivers::ps2_kbd::set_ctrl_c(s, false);
    crate::drivers::ps2_kbd::set_ctrl_z(s, false);
    crate::shell::keys::clear_latches_for_session(s);
}

/// Terminal access control for the CURRENT process, against the terminal of the
/// console session the process belongs to.
///
/// Returns `Ok(())` when the access may proceed. A background JOB gets its
/// stop signal queued on its whole process group and `EINTR` back: the caller's
/// syscall returns, and the signal-delivery path then stops the group (default
/// disposition) or runs the handler, exactly like a Linux terminal. Native
/// kernel tasks and kernel-spawned non-jobs are never stopped.
pub fn check_access(kind: TtyAccess) -> Result<(), Errno> {
    let Some((pgid, is_job)) = crate::task::compat::with_current_compat(|cs| (cs.pgid, cs.tty_job))
    else {
        return Ok(());
    };
    if !is_job {
        return Ok(());
    }
    // Сессия — свойство задачи (владелец/привязка), поэтому фоновый ребёнок окна
    // проверяется против терминала ОКНА, а не boot-консоли. Привязку наследует
    // fork (`session::inherit_bind`), так что подпроцессы задания не «уезжают».
    let session = crate::drivers::session::current();
    let (verdict, fg) = {
        // Snapshot under the session's tty lock, then release it before touching
        // the compat registry inside `send_signal_to_pgid_all` (never hold two
        // kernel locks at once).
        let state = *TTY[slot(session)].lock();
        let fg = if state.foreground() == UNOWNED {
            shell_pgid_for(session)
        } else {
            state.foreground()
        };
        (
            TtyJobControl::new(fg, state.tostop()).access(pgid, kind),
            fg,
        )
    };
    match verdict {
        TtyVerdict::Allow => Ok(()),
        TtyVerdict::Background(sig) => {
            let (what, name) = if sig == SIGTTIN {
                ("read", "SIGTTIN")
            } else if sig == SIGTTOU {
                ("write", "SIGTTOU")
            } else {
                ("access", "stop signal")
            };
            crate::info!(
                "[tty] session {}: background {} by pgid {} (foreground {}) -> {} + EINTR",
                session,
                what,
                pgid,
                fg,
                name
            );
            let _ = signal::send_signal_to_pgid_all(pgid, sig);
            Err(Errno::EINTR)
        }
    }
}
