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
//! [`Winsize`] и его кодирование — `struct winsize` / `TIOCGWINSZ` /
//! `TIOCSWINSZ`; [`surface_winsize`] переводит пиксельную поверхность сессии в
//! клетки. Ядерного состояния и локов здесь нет, поэтому `host-tests` включает
//! этот же исходник через `#[path]` (свойства в `properties/tty_job.rs`).
//! Ядерное состояние живёт в `src/drivers/tty.rs`: **массивы по консольным
//! сессиям** (foreground pgid, группа шелла, `TOSTOP`, `winsize`) и вызов
//! сигнального пути. `io_sys` применяет вердикт к ioctl и к путям read/write
//! stdin/консоли; сессия — это консоль, поэтому boot-консоль (0) и каждое окно
//! (`>= 1`) имеют СОБСТВЕННЫЙ управляющий терминал и не делят foreground-группу.
//!
//! ОТКЛОНЕНИЯ, заявленные явно:
//!
//! * В pagh нет полноценных POSIX-сессий (`setsid`/`TIOCSCTTY`). Правило
//!   `tcsetpgrp` «группа должна быть в сессии вызывающего» аппроксимируется
//!   [`may_set_foreground`] как «целевая группа жива, либо это текущая группа
//!   терминала, либо группа шелла ядра» — иначе `EPERM`, как errno Linux для
//!   группы вне сессии. Границей сессии служит консольная сессия
//!   (`drivers::session`), а не отдельная структура `struct tty_struct`.
//! * Фоновый доступ не перезапускает прерванный syscall после `SIGCONT`:
//!   вызывающий получает `EINTR`, а остановку выполняет обычный путь доставки
//!   сигнала при возврате из syscall. Linux перезапускает read/write, когда
//!   диспозиция сигнала `SIG_DFL`, и возвращает `EINTR`, только если сработал
//!   обработчик.
//! * `TOSTOP` отслеживается и соблюдается, но по умолчанию ВЫКЛЮЧЕН, как в
//!   Linux: фоновая запись в терминал не паркует писателя, пока программа сама
//!   не поставит бит через `tcsetattr`. Это обязательно для композитора desktop'а:
//!   он запущен фоновым job'ом boot-консоли и пишет диагностику. Включённый по
//!   умолчанию `TOSTOP` останавливал всю его группу (`SIGTTOU`), и окна
//!   переставали отвечать. Фоновое ЧТЕНИЕ по-прежнему всегда даёт `SIGTTIN`.
//! * `winsize` — состояние СЕССИИ, а не одной глобальной консоли: `TIOCGWINSZ`
//!   на stdin/stdout окна отвечает размером его поверхности, `TIOCSWINSZ`
//!   (клиент окна после resize) двигает его и шлёт `SIGWINCH` foreground-группе
//!   этой сессии. Пиксельная поверхность при этом остаётся размером с экран
//!   (сетка kernel-консоли фиксирована), клиент масштабирует кадр; поэтому
//!   приложение получает достоверный размер В КЛЕТКАХ, но не новый пиксельный
//!   кадр — это названная граница, а не притворство.

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

// ─── winsize (struct winsize, TIOCGWINSZ / TIOCSWINSZ) ──────────────────────

/// `TIOCGWINSZ`: прочитать размер окна терминала (`struct winsize`).
pub const TIOCGWINSZ: u64 = 0x5413;
/// `TIOCSWINSZ`: задать размер окна терминала.
pub const TIOCSWINSZ: u64 = 0x5414;

/// Размер `struct winsize` в байтах (4×u16, Linux UAPI).
pub const WINSIZE_SIZE: usize = 8;

/// `struct winsize` Linux: число строк/колонок и пиксельные размеры.
///
/// Поля — `u16` и little-endian на проводе, ровно как в UAPI; в ядре это
/// состояние КОНСОЛЬНОЙ СЕССИИ (`drivers::tty`), а не глобальной консоли.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Winsize {
    /// `ws_row` — строк (клеток) терминала.
    pub rows: u16,
    /// `ws_col` — колонок (клеток) терминала.
    pub cols: u16,
    /// `ws_xpixel` — ширина в пикселях (0, когда неизвестна).
    pub xpixel: u16,
    /// `ws_ypixel` — высота в пикселях (0, когда неизвестна).
    pub ypixel: u16,
}

impl Winsize {
    /// Размер, которого не бывает: оба счётчика клеток нулевые.
    ///
    /// Linux принимает `0×0` и шлёт `SIGWINCH`; pagh тоже принимает (клиент
    /// мог ещё не знать геометрию), но `TIOCGWINSZ` на неинициализированной
    /// сессии обязан вернуть осмысленный размер поверхности, а не этот ноль.
    pub const fn is_empty(&self) -> bool {
        self.rows == 0 || self.cols == 0
    }

    /// Совпадают ли размеры по всем четырём полям.
    pub const fn same(&self, other: &Winsize) -> bool {
        self.rows == other.rows
            && self.cols == other.cols
            && self.xpixel == other.xpixel
            && self.ypixel == other.ypixel
    }
}

/// Закодировать `struct winsize` в 8 байт little-endian (Linux UAPI).
pub fn encode_winsize(ws: &Winsize) -> [u8; WINSIZE_SIZE] {
    let mut out = [0u8; WINSIZE_SIZE];
    out[0..2].copy_from_slice(&ws.rows.to_le_bytes());
    out[2..4].copy_from_slice(&ws.cols.to_le_bytes());
    out[4..6].copy_from_slice(&ws.xpixel.to_le_bytes());
    out[6..8].copy_from_slice(&ws.ypixel.to_le_bytes());
    out
}

/// Декодировать 8 байт `struct winsize`; `None`, если байт меньше.
pub fn decode_winsize(bytes: &[u8]) -> Option<Winsize> {
    if bytes.len() < WINSIZE_SIZE {
        return None;
    }
    let at = |offset: usize| -> u16 { u16::from_le_bytes([bytes[offset], bytes[offset + 1]]) };
    Some(Winsize {
        rows: at(0),
        cols: at(2),
        xpixel: at(4),
        ypixel: at(6),
    })
}

/// Размер в клетках для поверхности `pixel_width × pixel_height` и клетки
/// `cell_w × cell_h`.
///
/// Деление целочисленное, как у `drivers::vt::ensure` (консоль заполняется
/// целыми клетками); нулевая клетка или нулевая поверхность дают `0×0`, а не
/// ложный размер. Минимум — одна клетка, когда поверхность непустая: иначе
/// `TIOCGWINSZ` вернул бы `0` строк для узкого окна, и приложение делило бы на
/// ноль.
pub fn surface_winsize(
    pixel_width: usize,
    pixel_height: usize,
    cell_w: usize,
    cell_h: usize,
) -> Winsize {
    if pixel_width == 0 || pixel_height == 0 || cell_w == 0 || cell_h == 0 {
        return Winsize::default();
    }
    Winsize {
        rows: (pixel_height / cell_h).max(1) as u16,
        cols: (pixel_width / cell_w).max(1) as u16,
        xpixel: core::cmp::min(pixel_width, u16::MAX as usize) as u16,
        ypixel: core::cmp::min(pixel_height, u16::MAX as usize) as u16,
    }
}

/// Какой запрос `winsize` пришёл: `TIOCGWINSZ` (Get) или `TIOCSWINSZ` (Set).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WinsizeRequest {
    /// `TIOCGWINSZ` — прочитать (ioctl заполняет 8 байт).
    Get,
    /// `TIOCSWINSZ` — задать (ioctl читает 8 байт).
    Set,
}

/// Классифицировать `request`: `Some(Get)`/`Some(Set)` для двух ioctl
/// `winsize`, `None` для остального (случай `ENOTTY` у вызывающего).
pub const fn winsize_request(request: u64) -> Option<WinsizeRequest> {
    if request == TIOCGWINSZ {
        Some(WinsizeRequest::Get)
    } else if request == TIOCSWINSZ {
        Some(WinsizeRequest::Set)
    } else {
        None
    }
}

/// Пора ли слать `SIGWINCH`: размер действительно изменился.
///
/// Linux шлёт `SIGWINCH` на `TIOCSWINSZ`; pagh шлёт его только когда новое
/// значение отличается, чтобы повторная установка того же размера не будила
/// приложение впустую (`stty size` не меняет терминал).
pub fn winsize_changed(old: &Winsize, new: &Winsize) -> bool {
    !old.same(new)
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

    #[test]
    fn winsize_is_the_linux_uapi_and_round_trips() {
        let ws = Winsize {
            rows: 43,
            cols: 160,
            xpixel: 1280,
            ypixel: 800,
        };
        let bytes = encode_winsize(&ws);
        assert_eq!(bytes, [43, 0, 160, 0, 0, 5, 32, 3]);
        assert_eq!(decode_winsize(&bytes), Some(ws));
        assert_eq!(decode_winsize(&bytes[..7]), None);
        assert_eq!(TIOCGWINSZ, 0x5413);
        assert_eq!(TIOCSWINSZ, 0x5414);
        assert_eq!(winsize_request(TIOCGWINSZ), Some(WinsizeRequest::Get));
        assert_eq!(winsize_request(TIOCSWINSZ), Some(WinsizeRequest::Set));
        assert_eq!(winsize_request(0x5401), None);
    }

    #[test]
    fn surface_winsize_never_reports_zero_cells() {
        let ws = surface_winsize(1280, 800, 8, 18);
        assert_eq!((ws.cols, ws.rows), (160, 44));
        let ws = surface_winsize(1, 1, 8, 18);
        assert_eq!((ws.cols, ws.rows), (1, 1));
        assert!(surface_winsize(0, 800, 8, 18).is_empty());
        assert!(surface_winsize(1280, 800, 0, 18).is_empty());
    }

    #[test]
    fn winsize_change_is_exact() {
        let a = Winsize {
            rows: 24,
            cols: 80,
            xpixel: 0,
            ypixel: 0,
        };
        let b = Winsize {
            rows: 25,
            cols: 80,
            xpixel: 0,
            ypixel: 0,
        };
        assert!(!winsize_changed(&a, &a));
        assert!(winsize_changed(&a, &b));
        assert!(winsize_changed(&b, &a));
    }
}
