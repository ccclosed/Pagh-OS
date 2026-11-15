//! Kernel-side состояние управляющего терминала: foreground process group.
//!
//! Консоль — управляющий терминал pagh. Каждый `Compat_Process` стартует с
//! stdin/stdout/stderr, привязанными к ней, и — ровно как Linux tty — терминалом
//! владеет ОДНА группа процессов за раз (`tcgetpgrp`/`tcsetpgrp`, то есть
//! `TIOCGPGRP`/`TIOCSPGRP`). Чистая модель решения (какая группа владеет, что делает
//! фоновый доступ, как валидируется аргумент `TIOCSPGRP`) живёт в
//! [`crate::arch::x86_64::linux::tty`] и host-property-тестируется; этот модуль —
//! kernel-side состояние и эффектный край:
//!
//! * владеет [`Spinlock`], держащим foreground pgid + `TOSTOP`;
//! * резолвит «владельца ещё нет» в собственную группу shell ядра (владелец
//!   терминала по умолчанию — shell, PID 1 — см. [`init_shell`]);
//! * [`check_access`] — хук, который зовут пути чтения stdin/консоли и записи:
//!   фоновое задание получает `SIGTTIN` (чтение) или `SIGTTOU` (запись при `TOSTOP`)
//!   в очередь на всю свою группу процессов, а syscall падает с `EINTR`, так что
//!   обычный путь доставки останавливает его на возврате из syscall.
//!
//! КТО ТАКОЕ JOB. Управлению доступом к терминалу подлежит только процесс, которому
//! shell передал терминал (через `lxrun`/`fg`, см. `CompatState::tty_job`). Собственные
//! неинтерактивные spawn'ы ядра (верификационные бинарники apt, selftest harness)
//! терминал никогда не получали, и остановка одного из них из-за того, что shell
//! владеет tty, подвесила бы загрузку или E2E-поток, который его ждёт. Форк-ребёнок
//! наследует флаг, так что подпроцессы задания остаются под управлением терминала.
//!
//! Пределы доставки — сигнального слоя, не этого модуля: сигнал ставится в очередь и
//! наблюдается на возврате из syscall (или проверкой `EINTR` блокирующего ожидания),
//! так что фоновое задание, припаркованное в долгом вычислении ядра, остановится на
//! следующей границе syscall, а не мгновенно.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::x86_64::linux::errno::Errno;
use crate::arch::x86_64::linux::signal;
use crate::arch::x86_64::linux::signal_frame::{SIGTTIN, SIGTTOU};
use crate::arch::x86_64::linux::tty::{TtyAccess, TtyJobControl, TtyVerdict};
use crate::sync::spinlock::Spinlock;

/// The terminal has no explicit owner until the shell claims it. The shell is
/// the first `kernel_thread_spawn` in `boot::kernel_main` and the pid counter
/// starts at 1, so 1 is the correct answer even before [`init_shell`] runs
/// (which it does at the top of `shell_main`).
const SHELL_PGID_DEFAULT: u64 = 1;

/// Foreground pgid `0` means "not claimed yet": [`foreground_pgid`] resolves it
/// to the shell's group. A real process group id is never 0.
const UNOWNED: u64 = 0;

/// The terminal's job-control state. `TOSTOP` starts SET: pagh is stricter than
/// Linux's default here on purpose — a background job that writes to the
/// terminal is stopped (`SIGTTOU`) unless it clears `TOSTOP` with `tcsetattr`,
/// which is what the terminal reports back through `tcgetattr`.
static TTY: Spinlock<TtyJobControl> = Spinlock::new(TtyJobControl::new(UNOWNED, true));

/// The kernel shell's own process group (the terminal's default owner).
static SHELL_PGID: AtomicU64 = AtomicU64::new(SHELL_PGID_DEFAULT);

/// Record the shell's process group and, if the terminal has no owner yet, hand
/// it to the shell. Called once from `shell_main`; idempotent.
pub fn init_shell(pgid: u64) {
    if pgid == 0 {
        return;
    }
    SHELL_PGID.store(pgid, Ordering::Relaxed);
    let mut tty = TTY.lock();
    if tty.foreground() == UNOWNED {
        tty.set_foreground(pgid);
    }
}

/// The kernel shell's process group (the value `lxrun` restores on exit).
pub fn shell_pgid() -> u64 {
    SHELL_PGID.load(Ordering::Relaxed)
}

/// The process group that currently owns the terminal (`tcgetpgrp`): the
/// shell's group when nothing else has claimed it.
pub fn foreground_pgid() -> u64 {
    let fg = TTY.lock().foreground();
    if fg == UNOWNED {
        shell_pgid()
    } else {
        fg
    }
}

/// Hand the terminal to `pgid` (`tcsetpgrp`). Validation of the target belongs
/// to the caller ([`crate::arch::x86_64::linux::tty::may_set_foreground`]).
pub fn set_foreground_pgid(pgid: u64) {
    TTY.lock().set_foreground(pgid);
}

/// Give the terminal back to the shell (the foreground job is gone or stopped).
pub fn restore_shell_foreground() {
    set_foreground_pgid(shell_pgid());
}

/// Whether background terminal writes stop their writer (`TOSTOP`).
pub fn tostop() -> bool {
    TTY.lock().tostop()
}

/// Apply the `TOSTOP` bit from a `TCSETS` `c_lflag` image.
pub fn set_tostop(on: bool) {
    TTY.lock().set_tostop(on);
}

/// Terminal access control for the CURRENT process.
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
    let verdict = {
        // Snapshot under the tty lock, then release it before touching the
        // compat registry inside `send_signal_to_pgid_all` (never hold two
        // kernel locks at once).
        let state = *TTY.lock();
        let fg = if state.foreground() == UNOWNED {
            shell_pgid()
        } else {
            state.foreground()
        };
        TtyJobControl::new(fg, state.tostop()).access(pgid, kind)
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
                "[tty] background {} by pgid {} (foreground {}) -> {} + EINTR",
                what,
                pgid,
                foreground_pgid(),
                name
            );
            let _ = signal::send_signal_to_pgid_all(pgid, sig);
            Err(Errno::EINTR)
        }
    }
}
