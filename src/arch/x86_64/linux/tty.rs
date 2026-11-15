//! Чистая модель управления заданиями управляющего терминала (семантика tty Linux).
//!
//! Консоль ядра — управляющий терминал pagh: каждый `Compat_Process` стартует с
//! stdin/stdout/stderr, заранее привязанными к ней
//! (`FdTable::with_standard_streams`). У терминала ровно ОДНА foreground-группа
//! процессов. Группа-владелец читает и пишет свободно; процесс из ЛЮБОЙ другой
//! группы при чтении получает `SIGTTIN`, а при записи — `SIGTTOU`, если у
//! терминала стоит `TOSTOP` (POSIX 11.1.4 "Terminal Access Control",
//! `termios(3)`). `tcgetpgrp`/`tcsetpgrp` — это `TIOCGPGRP`/`TIOCSPGRP` на том же
//! терминале.
//!
//! Модуль — чистое решение: [`TtyJobControl`] хранит foreground pgid + бит
//! `TOSTOP`, [`TtyJobControl::access`] классифицирует доступ, а
//! [`decode_pgrp_arg`]/[`may_set_foreground`] — правила входа `TIOCSPGRP`.
//! Ядерного состояния и локов здесь нет, поэтому `host-tests` включает этот же
//! исходник через `#[path]` (свойства в `properties/tty_job.rs`). Ядерное
//! состояние живёт в `src/drivers/tty.rs` (спинлок, группа шелла и вызов
//! сигнального пути); `io_sys` применяет вердикт к двум ioctl и к путям
//! read/write stdin/консоли.
//!
//! ОТКЛОНЕНИЯ, заявленные явно:
//!
//! * В pagh нет сессий. Правило `tcsetpgrp` «группа должна быть в сессии
//!   вызывающего» аппроксимируется [`may_set_foreground`] как «целевая группа
//!   жива, либо это текущая группа терминала, либо группа шелла ядра» — иначе
//!   `EPERM`, как errno Linux для группы вне сессии.
//! * Фоновый доступ не перезапускает прерванный syscall после `SIGCONT`:
//!   вызывающий получает `EINTR`, а остановку выполняет обычный путь доставки
//!   сигнала при возврате из syscall. Linux перезапускает read/write, когда
//!   диспозиция сигнала `SIG_DFL`, и возвращает `EINTR`, только если сработал
//!   обработчик.
//! * `TOSTOP` отслеживается и соблюдается, но по умолчанию ВКЛЮЧЁН (в Linux —
//!   выключен), поэтому первая запись в консоль фонового job останавливает его
//!   без запроса программы. `tcsetattr` снимает/ставит его как любой другой флаг,
//!   а `tcgetattr` сообщает реальное состояние.

use super::errno::Errno;
use super::signal_frame::{SIGTTIN, SIGTTOU};

/// `TIOCGPGRP`: прочитать foreground-группу процессов терминала (`tcgetpgrp`).
pub const TIOCGPGRP: u64 = 0x540F;
/// `TIOCSPGRP`: задать foreground-группу процессов терминала (`tcsetpgrp`).
pub const TIOCSPGRP: u64 = 0x5410;
/// `TOSTOP`, бит `c_lflag`, заставляющий фоновую запись генерировать `SIGTTOU`
/// (`termios(3)`; значение Linux, та же позиция бита, что у любого `c_lflag`).
pub const TOSTOP: u32 = 0x0000_0100;

/// Какую операцию с терминалом пытается выполнить compat-процесс.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TtyAccess {
    /// Чтение из управляющего терминала (stdin / консоль).
    Read,
    /// Запись в управляющий терминал (stdout / stderr).
    Write,
}

/// Что терминал говорит о доступе процесса из группы `caller`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TtyVerdict {
    /// Вызывающий владеет терминалом: операция проходит.
    Allow,
    /// Вызывающий в фоновой группе процессов: поставить этот стоп-сигнал
    /// группе и провалить вызов (`EINTR`).
    Background(u64),
}

/// Foreground-группа процессов терминала + политика записи `TOSTOP`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TtyJobControl {
    fg_pgid: u64,
    tostop: bool,
}

impl TtyJobControl {
    /// Терминал, принадлежащий `fg_pgid`, останавливающий фоновые записи при `tostop`.
    pub const fn new(fg_pgid: u64, tostop: bool) -> Self {
        Self { fg_pgid, tostop }
    }

    /// Группа процессов, владеющая терминалом сейчас (`tcgetpgrp`).
    pub const fn foreground(&self) -> u64 {
        self.fg_pgid
    }

    /// Передать терминал `pgid` (`tcsetpgrp`). Проверка входа — дело вызывающего:
    /// см. [`decode_pgrp_arg`] и [`may_set_foreground`].
    pub fn set_foreground(&mut self, pgid: u64) {
        self.fg_pgid = pgid;
    }

    /// Генерирует ли фоновая запись `SIGTTOU` (`TOSTOP`).
    pub const fn tostop(&self) -> bool {
        self.tostop
    }

    /// Применить бит `TOSTOP` из образа `c_lflag` для `TCSETS`.
    pub fn set_tostop(&mut self, on: bool) {
        self.tostop = on;
    }

    /// `true`, когда `pgid` владеет терминалом.
    pub const fn is_foreground(&self, pgid: u64) -> bool {
        self.fg_pgid == pgid
    }

    /// Классифицировать доступ группы `caller`.
    ///
    /// Чтение из фоновой группы останавливает её всегда (`SIGTTIN`, при любой
    /// конфигурации терминала). Запись останавливает (`SIGTTOU`) только при
    /// `TOSTOP`; иначе фоновый вывод доставляется нормально.
    pub const fn access(&self, caller: u64, kind: TtyAccess) -> TtyVerdict {
        if self.fg_pgid == caller {
            return TtyVerdict::Allow;
        }
        match kind {
            TtyAccess::Read => TtyVerdict::Background(SIGTTIN),
            TtyAccess::Write => {
                if self.tostop {
                    TtyVerdict::Background(SIGTTOU)
                } else {
                    TtyVerdict::Allow
                }
            }
        }
    }
}

/// Два запроса ioctl, адресующие foreground-группу.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PgrpRequest {
    /// `TIOCGPGRP` — прочитать (`tcgetpgrp`).
    Get,
    /// `TIOCSPGRP` — задать (`tcsetpgrp`).
    Set,
}

/// Классифицировать `request`: `Some(Get)`/`Some(Set)` для двух ioctl
/// foreground-группы, `None` для всего остального (случай `ENOTTY` у
/// вызывающего).
pub const fn pgrp_request(request: u64) -> Option<PgrpRequest> {
    if request == TIOCGPGRP {
        Some(PgrpRequest::Get)
    } else if request == TIOCSPGRP {
        Some(PgrpRequest::Set)
    } else {
        None
    }
}

/// Декодировать `int`, прочитанный из пользовательского аргумента `TIOCSPGRP`.
///
/// Слово — C `int`, поэтому старший бит знаковый: отрицательный pgid — `EINVAL`
/// (`tiocspgrp` в Linux отвергает `pgrp < 0` до любого поиска). Ноль
/// декодируется, но никогда не называет группу; [`may_set_foreground`] его
/// отвергает.
pub const fn decode_pgrp_arg(raw: u32) -> Result<u64, Errno> {
    if (raw as i32) < 0 {
        Err(Errno::EINVAL)
    } else {
        Ok(raw as u64)
    }
}

/// Может ли `target` стать foreground-группой терминала?
///
/// * `live` — хотя бы один зарегистрированный процесс имеет `target` своим pgid.
/// * `shell_pgid` — собственная группа шелла ядра, владелец терминала по умолчанию.
/// * `current_fg` — группа, на которой терминал сейчас (идемпотентная установка).
///
/// `Ok(())` для живой группы, группы шелла или текущей группы; иначе `EPERM`
/// (аппроксимация проверки сессии в pagh) и для бессмысленной группы 0.
pub fn may_set_foreground(
    target: u64,
    live: bool,
    shell_pgid: u64,
    current_fg: u64,
) -> Result<(), Errno> {
    if target == 0 {
        return Err(Errno::EPERM);
    }
    if live || target == shell_pgid || target == current_fg {
        Ok(())
    } else {
        Err(Errno::EPERM)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_is_never_signalled() {
        let tty = TtyJobControl::new(7, true);
        assert_eq!(tty.access(7, TtyAccess::Read), TtyVerdict::Allow);
        assert_eq!(tty.access(7, TtyAccess::Write), TtyVerdict::Allow);
        assert_eq!(
            tty.access(8, TtyAccess::Read),
            TtyVerdict::Background(SIGTTIN)
        );
        assert_eq!(
            tty.access(8, TtyAccess::Write),
            TtyVerdict::Background(SIGTTOU)
        );
    }

    #[test]
    fn tostop_gates_only_background_writes() {
        let tty = TtyJobControl::new(3, false);
        assert_eq!(tty.access(9, TtyAccess::Write), TtyVerdict::Allow);
        assert_eq!(
            tty.access(9, TtyAccess::Read),
            TtyVerdict::Background(SIGTTIN)
        );
    }

    #[test]
    fn negative_pgrp_is_einval_and_zero_is_not_a_group() {
        assert_eq!(decode_pgrp_arg(0xFFFF_FFFF), Err(Errno::EINVAL));
        assert_eq!(decode_pgrp_arg(0x8000_0000), Err(Errno::EINVAL));
        assert_eq!(decode_pgrp_arg(0), Ok(0));
        assert_eq!(decode_pgrp_arg(4), Ok(4));
        assert_eq!(may_set_foreground(0, true, 1, 2), Err(Errno::EPERM));
        assert_eq!(may_set_foreground(5, true, 1, 2), Ok(()));
        assert_eq!(may_set_foreground(1, false, 1, 2), Ok(()));
        assert_eq!(may_set_foreground(2, false, 1, 2), Ok(()));
        assert_eq!(may_set_foreground(5, false, 1, 2), Err(Errno::EPERM));
    }

    #[test]
    fn request_constants_are_the_linux_ones() {
        assert_eq!(TIOCGPGRP, 0x540F);
        assert_eq!(TIOCSPGRP, 0x5410);
        assert_eq!(pgrp_request(TIOCGPGRP), Some(PgrpRequest::Get));
        assert_eq!(pgrp_request(TIOCSPGRP), Some(PgrpRequest::Set));
        assert_eq!(pgrp_request(0x5401), None);
        assert_eq!(pgrp_request(0x5413), None);
    }
}
