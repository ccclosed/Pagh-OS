//! Чистая политика `shutdown(2)` (nr 48).
//!
//! `shutdown` не отдаёт дескриптор: он закрывает НАПРАВЛЕНИЕ сокета. Поэтому
//! решения, которые обязаны совпадать с Linux, сведены здесь, чтобы их можно
//! было проверить на хосте без compat-состояния и таблицы fd:
//!
//!  * порядок проверок — `EBADF` (нет дескриптора), `ENOTSOCK` (дескриптор не
//!    сокет), затем `EINVAL` (значение `how`), и только потом состояние
//!    соединения (`ENOTCONN`). Так это делает `sockfd_lookup_light` +
//!    `inet_shutdown`/`unix_shutdown`;
//!  * `SHUT_WR` — пишущая половина закрыта: пир-читатель видит EOF (`Ok(0)`), а
//!    собственная запись отвечает `EPIPE`;
//!  * `SHUT_RD` — приём закрыт: собственные `recv` отвечают `0`, а пир при
//!    записи получает `EPIPE` (байты больше никто не заберёт);
//!  * `SHUT_RDWR` — обе половины; повторный вызов идемпотентен.
//!
//! Расхождения протоколов, которые Linux делает видимыми и которые здесь
//! сохранены (проверено пробой на хосте, Linux 7.x):
//!
//!  * AF_UNIX без соединения (несвязанный, слушающий) — успешный no-op, а не
//!    `ENOTCONN`;
//!  * TCP до `connect` — `ENOTCONN`; UDP — `ENOTCONN` всегда (даже
//!    подключённый: `udp_shutdown` не умеет половинного закрытия);
//!  * `EINVAL` за невалидный `how` — для ЛЮБОГО сокета, включая несоединённый.
//!
//! Сами флаги живут в очереди направления (`task::fd::PipeState`) и в
//! `InetTcp`; этот модуль — единственный ответ на вопрос «что значит это
//! состояние для read/write/poll», чтобы заблокированный вызов просыпался, а не
//! крутил `yield_current()` вечно.
//!
//! Модуль — host-тестируемый шов, подключённый в `host-tests/src/lib.rs`
//! (свойство `p59`).

use super::errno::Errno;

/// `SHUT_RD` — закрыть читающую половину.
pub const SHUT_RD: u64 = 0;
/// `SHUT_WR` — закрыть пишущую половину.
pub const SHUT_WR: u64 = 1;
/// `SHUT_RDWR` — обе половины.
pub const SHUT_RDWR: u64 = 2;

/// Проверенный аргумент `how` из `shutdown(2)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum How {
    /// `SHUT_RD`: дальше читать нельзя (`recv` → 0).
    Read,
    /// `SHUT_WR`: дальше писать нельзя (пир видит EOF, своя запись → `EPIPE`).
    Write,
    /// `SHUT_RDWR`: обе половины.
    Both,
}

impl How {
    /// Разобрать сырой `how`. Всё, кроме 0/1/2, — `EINVAL` (Linux проверяет
    /// значение уже после того, как дескриптор найден и оказался сокетом).
    pub fn parse(how: u64) -> Result<How, Errno> {
        match how {
            SHUT_RD => Ok(How::Read),
            SHUT_WR => Ok(How::Write),
            SHUT_RDWR => Ok(How::Both),
            _ => Err(Errno::EINVAL),
        }
    }

    /// Закрывает ли это вызывающую читающую половину?
    pub fn shuts_read(self) -> bool {
        matches!(self, How::Read | How::Both)
    }

    /// Закрывает ли это вызывающую пишущую половину?
    pub fn shuts_write(self) -> bool {
        matches!(self, How::Write | How::Both)
    }
}

/// К чему свёлся дескриптор. Имена называют только то, что решает политику:
/// вид сокета из `OpenObject` плюс состояние соединения.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FdKind {
    /// Дескриптора нет в таблице — `EBADF`.
    Absent,
    /// Валидный дескриптор, который не сокет: файл, каталог, pipe, консоль,
    /// eventfd, epoll… — `ENOTSOCK`.
    NotSocket,
    /// Подключённый AF_UNIX `SOCK_STREAM` (socketpair, connect, accept).
    UnixConnected,
    /// AF_UNIX без соединения: несвязанный или слушающий. Linux отвечает
    /// успехом (закрывать нечего), но `how` всё равно проверяет.
    UnixUnconnected,
    /// AF_INET TCP в состоянии ESTABLISHED.
    TcpConnected,
    /// TCP до `connect` (или после полного `close`) — `ENOTCONN`.
    TcpUnconnected,
    /// AF_INET UDP: `udp_shutdown` половинного закрытия не умеет — `ENOTCONN`.
    Udp,
}

/// Что именно закрыть у вызывающего сокета.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Закрывать нечего: AF_UNIX без соединения. Успех без последствий.
    Noop,
    /// Подключённая AF_UNIX-пара: выставить флаги на очередях направлений.
    Unix {
        /// Читающая половина (последующие `recv` → `Ok(0)`).
        read: bool,
        /// Пишущая половина (пир видит EOF, своя запись → `EPIPE`).
        write: bool,
    },
    /// TCP ESTABLISHED: закрыть направления в собственном стеке.
    Tcp {
        /// Приём закрыт: последующие `recv` → `Ok(0)`, входящие байты
        /// отбрасываются.
        read: bool,
        /// Передача закрыта: FIN после слива TX, собственная запись → `EPIPE`.
        write: bool,
    },
}

/// Решить `shutdown(fd, how)` в порядке Linux: дескриптор → вид → `how` →
/// состояние соединения.
///
/// Повторный `shutdown` той же половины успешен (Linux идемпотентен), поэтому
/// никакого состояния здесь нет — только решение.
pub fn plan(kind: FdKind, how: u64) -> Result<Action, Errno> {
    // Порядок проверок несёт смысл: не-сокет с мусорным `how` — это `ENOTSOCK`,
    // а не `EINVAL` (проба: pipe + how=99 → ENOTSOCK).
    match kind {
        FdKind::Absent => return Err(Errno::EBADF),
        FdKind::NotSocket => return Err(Errno::ENOTSOCK),
        _ => {}
    }
    let how = How::parse(how)?;
    match kind {
        FdKind::UnixConnected => Ok(Action::Unix {
            read: how.shuts_read(),
            write: how.shuts_write(),
        }),
        FdKind::UnixUnconnected => Ok(Action::Noop),
        FdKind::TcpConnected => Ok(Action::Tcp {
            read: how.shuts_read(),
            write: how.shuts_write(),
        }),
        FdKind::TcpUnconnected | FdKind::Udp => Err(Errno::ENOTCONN),
        // Отвергнуто первым match'ем выше.
        FdKind::Absent => Err(Errno::EBADF),
        FdKind::NotSocket => Err(Errno::ENOTSOCK),
    }
}

/// Читает ли конец очереди EOF, когда данных в ней нет?
///
/// `writers == 0` — пишущие концы закрыли (обычный `close` пира); `write_shut` —
/// пир сделал `shutdown(SHUT_WR)` (та же видимость EOF, но `Arc` ещё жив);
/// `read_shut` — вызывающий сам закрыл читающую половину.
pub fn read_at_eof(writers: usize, read_shut: bool, write_shut: bool) -> bool {
    read_shut || write_shut || writers == 0
}

/// Готов ли конец очереди сообщить `POLLIN`/`EPOLLIN`?
pub fn read_ready(has_data: bool, writers: usize, read_shut: bool, write_shut: bool) -> bool {
    has_data || read_at_eof(writers, read_shut, write_shut)
}

/// Можно ли ещё писать в эту очередь? `false` — `EPIPE`.
///
/// `readers == 0` — последний читающий конец уничтожен; `read_shut` — пир
/// сделал `shutdown(SHUT_RD)`, то есть байты больше никто не заберёт;
/// `write_shut` — вызывающий сам закрыл передачу через `shutdown(SHUT_WR)`.
pub fn write_alive(readers: usize, read_shut: bool, write_shut: bool) -> bool {
    !read_shut && !write_shut && readers > 0
}

/// Готов ли конец очереди сообщить `POLLOUT`/`EPOLLOUT`?
pub fn write_ready(readers: usize, read_shut: bool, write_shut: bool, has_room: bool) -> bool {
    write_alive(readers, read_shut, write_shut) && has_room
}
