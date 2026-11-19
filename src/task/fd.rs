//! Таблица файловых дескрипторов на `Compat_Process` (R2.4, R2.6, R2.14).
//!
//! Сопоставляет небольшие целые fd открытым объектам: 0/1/2 заранее привязаны к
//! стандартным потокам (R2.2), новые дескрипторы выделяются как наименьший
//! свободный индекс `>= 3` (R2.4). Чистое выделение индексов и учёт закрытия
//! живут в [`super::fd_alloc`] без зависимостей, поэтому host-тестируются для
//! Property 7; этот модуль надстраивает ядерный [`OpenObject`] (со встроенным
//! `Arc<dyn VfsNode>`) и общее отображение [`Errno`].
#![allow(dead_code)]

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::x86_64::linux::epoll_sys::EpollEntry;
use crate::arch::x86_64::linux::errno::Errno;
use crate::arch::x86_64::linux::sock_shutdown;
use crate::sync::spinlock::Spinlock;
use crate::vfs::VfsNode;

use super::fd_alloc::FdSlots;

const PIPE_CAPACITY: usize = 64 * 1024;

/// Переданный через `SCM_RIGHTS` дескриптор: dup объекта и его `f_flags`.
///
/// Linux `dup`/`SCM_RIGHTS` сохраняют режим доступа (`f_flags`): принятый
/// memfd должен остаться writable, если отправитель открыл его на запись, —
/// иначе `mmap(MAP_SHARED, PROT_WRITE)` получателя отвечал бы `EACCES`.
/// `FD_CLOEXEC` сюда не входит: он появляется только по `MSG_CMSG_CLOEXEC`.
pub struct PassedFd {
    pub object: OpenObject,
    pub flags: u64,
}

/// `SCM_RIGHTS`, привязанный к позиции байтового потока AF_UNIX.
///
/// `pos`/`end` — абсолютные (с начала потока) позиции первого байта и конца
/// чанка `sendmsg`, вместе с которым пришли `objects`. Linux доставляет
/// ancillary data с первым байтом записи и не даёт одному `recvmsg` перейти
/// границу этой записи, поэтому приём останавливается на `end`: дескрипторы
/// приходят ровно один раз и не «уезжают» в середину следующего сообщения.
pub struct StreamCmsg {
    /// Абсолютная позиция первого байта sendmsg.
    pub pos: u64,
    /// Абсолютная позиция сразу за чанком sendmsg (граница чтения).
    pub end: u64,
    /// dup'ы переданных дескрипторов; закрываются, когда cmsg отброшен,
    /// доставлен получателю или сокет уничтожен.
    pub objects: Vec<PassedFd>,
}

struct PipeState {
    bytes: VecDeque<u8>,
    readers: usize,
    writers: usize,
    /// Читающая половина этой стороны закрыта `shutdown(SHUT_RD)`: `read`
    /// отвечает `Eof` (Linux: последующие `recv` → 0), а пир при записи в эту
    /// очередь получает `EPIPE` — байты больше никто не заберёт.
    read_shut: bool,
    /// Пишущая половина этой стороны закрыта `shutdown(SHUT_WR)`: читатель
    /// очереди видит `Eof`, хотя `Arc` пишущего конца ещё жив (обычный `close`
    /// выставил бы то же самое, сняв счётчик `writers`).
    write_shut: bool,
    /// Сколько байт всего записано в очередь (абсолютная позиция конца).
    written_total: u64,
    /// Сколько байт всего прочитано (абсолютная позиция начала доступного).
    read_total: u64,
    /// Ждущие `SCM_RIGHTS`; строго возрастают по `pos`, все `pos >= read_total`.
    cmsgs: VecDeque<StreamCmsg>,
    /// `SO_RCVTIMEO` для блокирующего чтения этой очереди, в миллисекундах;
    /// `0` — ждать бесконечно. Настройка живёт на ОЧЕРЕДИ, а не на fd: у пары
    /// AF_UNIX очередь направления принадлежит ровно одному читающему концу, а
    /// `dup`/`fork` по Linux делят настройку сокета между дескрипторами.
    rcv_timeout_ms: u64,
    /// `SO_SNDTIMEO` для блокирующей записи в эту очередь, в миллисекундах;
    /// `0` — ждать бесконечно. Отдельное поле, а не общий «таймаут сокета»:
    /// `recv` и `send` одной пары смотрят в РАЗНЫЕ очереди, и их настройки не
    /// должны перетирать друг друга.
    snd_timeout_ms: u64,
}

impl PipeState {
    fn new() -> Self {
        Self {
            bytes: VecDeque::new(),
            readers: 1,
            writers: 1,
            read_shut: false,
            write_shut: false,
            written_total: 0,
            read_total: 0,
            cmsgs: VecDeque::new(),
            rcv_timeout_ms: 0,
            snd_timeout_ms: 0,
        }
    }
}

pub struct PipeEndpoint {
    state: Arc<Spinlock<PipeState>>,
    read_end: bool,
    nonblocking: bool,
}
pub enum PipeReadResult {
    Data(usize),
    WouldBlock,
    Eof,
}
pub enum PipeWriteResult {
    Data(usize),
    WouldBlock,
    Broken,
}

/// Результат `recvmsg`-чтения: байты и забранный cmsg (если он пришёлся на
/// это чтение). `objects` непусто, только когда поток достиг позиции cmsg.
pub struct ScmRead {
    pub n: usize,
    pub objects: Vec<PassedFd>,
}

pub enum ScmReadResult {
    Data(ScmRead),
    WouldBlock,
    Eof,
}
impl PipeEndpoint {
    pub fn nonblocking(&self) -> bool {
        self.nonblocking
    }
    pub fn read(&self, dst: &mut [u8]) -> PipeReadResult {
        let mut s = self.state.lock();
        if !s.bytes.is_empty() {
            let n = core::cmp::min(dst.len(), s.bytes.len());
            for b in dst.iter_mut().take(n) {
                *b = s.bytes.pop_front().unwrap();
            }
            s.read_total += n as u64;
            // `read(2)`/`readv(2)` не принимают ancillary data: Linux при
            // проходе мимо позиции SCM_RIGHTS отбрасывает (закрывает) fd.
            // Забираем записи из-под лока и отпускаем после него: Drop объекта
            // может отпускать Arc-сокеты и брать лок их очереди.
            let mut skipped: Vec<StreamCmsg> = Vec::new();
            while let Some(front) = s.cmsgs.front() {
                if front.pos < s.read_total {
                    skipped.push(s.cmsgs.pop_front().unwrap());
                } else {
                    break;
                }
            }
            drop(s);
            drop(skipped);
            PipeReadResult::Data(n)
        } else if sock_shutdown::read_at_eof(s.writers, s.read_shut, s.write_shut) {
            PipeReadResult::Eof
        } else {
            PipeReadResult::WouldBlock
        }
    }

    /// Записать первые байты `sendmsg` и атомарно привязать к их позиции
    /// дескрипторы `objects` (одна блокировка: позиция и cmsg не разъедутся).
    ///
    /// `objects` снимаются только при успешной записи: на `WouldBlock`/`Broken`
    /// они остаются у вызывающего, который обязан либо повторить, либо закрыть
    /// их сам (не переданный fd не должен исчезать у отправителя). Пустой
    /// `objects` (`Some(empty)`) — обычная запись без cmsg.
    pub fn write_scm(&self, src: &[u8], objects: &mut Option<Vec<PassedFd>>) -> PipeWriteResult {
        let mut s = self.state.lock();
        if !sock_shutdown::write_alive(s.readers, s.read_shut, s.write_shut) {
            return PipeWriteResult::Broken;
        }
        if src.is_empty() {
            return PipeWriteResult::Data(0);
        }
        let room = PIPE_CAPACITY.saturating_sub(s.bytes.len());
        if room == 0 {
            return PipeWriteResult::WouldBlock;
        }
        let n = core::cmp::min(room, src.len());
        let pos = s.written_total;
        s.bytes.extend(src[..n].iter().copied());
        s.written_total += n as u64;
        if let Some(objs) = objects.take() {
            if !objs.is_empty() {
                s.cmsgs.push_back(StreamCmsg {
                    pos,
                    end: pos + n as u64,
                    objects: objs,
                });
            }
        }
        PipeWriteResult::Data(n)
    }

    /// `recvmsg`-чтение: те же байты, что `read`, но с доставкой стоящего на
    /// текущей позиции SCM_RIGHTS и остановкой на границе его чанка.
    ///
    /// Границы (повторяют Linux): данные перед cmsg читаются обычным
    /// `recvmsg`-вызовом; как только чтение входит в чанк с cmsg, cmsg
    /// забирается, а вызов не идёт дальше `end` этого чанка; cmsg, позицию
    /// которого прошёл обычный `read(2)`, уже отброшен [`Self::read`].
    pub fn read_scm(&self, dst: &mut [u8]) -> ScmReadResult {
        let mut s = self.state.lock();
        if s.bytes.is_empty() {
            return if sock_shutdown::read_at_eof(s.writers, s.read_shut, s.write_shut) {
                ScmReadResult::Eof
            } else {
                ScmReadResult::WouldBlock
            };
        }
        // Инвариант: read() отбрасывает пройденные cmsg. Защитно повторяем,
        // забирая записи из-под лока.
        let mut skipped: Vec<StreamCmsg> = Vec::new();
        while let Some(front) = s.cmsgs.front() {
            if front.pos < s.read_total {
                skipped.push(s.cmsgs.pop_front().unwrap());
            } else {
                break;
            }
        }
        let avail = s.bytes.len() as u64;
        let mut take = core::cmp::min(dst.len() as u64, avail);
        let mut objects: Vec<PassedFd> = Vec::new();
        if let Some(front) = s.cmsgs.front() {
            let rel = front.pos - s.read_total;
            if take > rel {
                // Чтение вошло в чанк с cmsg: доставить его и остановиться на
                // границе чанка (или на конце буфера вызывающего).
                let end_rel = front.end.saturating_sub(s.read_total);
                take = core::cmp::min(take, end_rel.max(rel + 1));
                objects = s.cmsgs.pop_front().map(|c| c.objects).unwrap_or_default();
            }
        }
        let n = take as usize;
        for b in dst.iter_mut().take(n) {
            *b = s.bytes.pop_front().unwrap();
        }
        s.read_total += n as u64;
        drop(s);
        drop(skipped);
        ScmReadResult::Data(ScmRead { n, objects })
    }
    pub fn write(&self, src: &[u8]) -> PipeWriteResult {
        let mut s = self.state.lock();
        if !sock_shutdown::write_alive(s.readers, s.read_shut, s.write_shut) {
            return PipeWriteResult::Broken;
        }
        let room = PIPE_CAPACITY.saturating_sub(s.bytes.len());
        if room == 0 {
            return PipeWriteResult::WouldBlock;
        }
        let n = core::cmp::min(room, src.len());
        s.bytes.extend(src[..n].iter().copied());
        // Обычная запись двигает абсолютную позицию потока ровно так же, как
        // [`Self::write_scm`]: `written_total` — это координата, в которой
        // [`Self::write_scm`] регистрирует `SCM_RIGHTS`, а `read_total` — сколько
        // байт потока уже потреблено. Если `write(2)` пишет мимо счётчика,
        // координаты расходятся: первое же `sendmsg` с fd получает `pos = 0`
        // (счётчик не двигался, хотя байты ушли), регистрирует cmsg ПОЗАДИ
        // `read_total`, и [`Self::read_scm`] выбрасывает её как «пройденную» —
        // дескриптор молча теряется, а `recvmsg` возвращает данные без cmsg.
        // Именно так SHM_OFFER убивал соединение с каждым реальным клиентом:
        // `hello`/`create` уходят через `write(2)`, а fd — следующим `sendmsg`.
        s.written_total += n as u64;
        PipeWriteResult::Data(n)
    }
    pub fn read_ready(&self) -> bool {
        let s = self.state.lock();
        sock_shutdown::read_ready(!s.bytes.is_empty(), s.writers, s.read_shut, s.write_shut)
    }
    pub fn write_ready(&self) -> bool {
        let s = self.state.lock();
        sock_shutdown::write_ready(
            s.readers,
            s.read_shut,
            s.write_shut,
            s.bytes.len() < PIPE_CAPACITY,
        )
    }
    pub fn peer_closed(&self) -> bool {
        let s = self.state.lock();
        if self.read_end {
            // EOF пира: последний пишущий конец уничтожен либо закрыл свою
            // половину через `shutdown(SHUT_WR)`.
            s.writers == 0 || s.write_shut
        } else {
            !sock_shutdown::write_alive(s.readers, s.read_shut, s.write_shut)
        }
    }
    /// Закрыть читающую половину этого конца (`shutdown(SHUT_RD)`, nr 48).
    ///
    /// Флаг на общей очереди, поэтому `dup`-дубликаты и форкнутые процессы
    /// видят то же самое — как в Linux, где `shutdown` действует на сокет, а не
    /// на номер дескриптора. Повторный вызов идемпотентен.
    pub fn shutdown_read(&self) {
        self.state.lock().read_shut = true;
    }
    /// Закрыть пишущую половину этого конца (`shutdown(SHUT_WR)`): пир-читатель
    /// увидит `Eof` / `POLLHUP`, собственная запись — `EPIPE`.
    pub fn shutdown_write(&self) {
        self.state.lock().write_shut = true;
    }
    /// Идентичность очереди: у всех концов (pipe и обоих концов socketpair)
    /// одинаковый указатель на общее состояние. Блокирующее чтение по нему
    /// спрашивает таблицу fd, жив ли ещё хоть один дескриптор этой очереди, — и
    /// так отличает «пир ещё может написать» от «читающий fd закрыли из
    /// соседнего потока, и ждать больше некому» (см. `linux::sock_wait`).
    pub fn queue_id(&self) -> usize {
        Arc::as_ptr(&self.state) as usize
    }
    /// Остались ли пишущие концы: для читающего конца это `writers == 0` (EOF).
    ///
    /// `shutdown(SHUT_WR)` на пишущей стороне закрывает направление раньше
    /// `close`, поэтому очередь сообщает 0 и тогда: блокирующее чтение обязано
    /// проснуться с `Ok(0)`, а не ждать, пока Arc пишущего конца исчезнет.
    pub fn writers(&self) -> usize {
        let s = self.state.lock();
        if s.write_shut {
            0
        } else {
            s.writers
        }
    }
    /// `SO_RCVTIMEO` этой очереди в миллисекундах (`0` — бесконечно).
    pub fn rcv_timeout_ms(&self) -> u64 {
        self.state.lock().rcv_timeout_ms
    }
    /// `SO_SNDTIMEO` этой очереди в миллисекундах (`0` — бесконечно).
    pub fn snd_timeout_ms(&self) -> u64 {
        self.state.lock().snd_timeout_ms
    }
    /// Задать `SO_RCVTIMEO` (миллисекунды, `0` — снять таймаут).
    pub fn set_rcv_timeout_ms(&self, ms: u64) {
        self.state.lock().rcv_timeout_ms = ms;
    }
    /// Задать `SO_SNDTIMEO` (миллисекунды, `0` — снять таймаут).
    pub fn set_snd_timeout_ms(&self, ms: u64) {
        self.state.lock().snd_timeout_ms = ms;
    }

    /// Клонировать этот конец pipe с другим флагом O_NONBLOCK (fcntl
    /// F_SETFL). Клон регистрируется как дополнительный читатель/писатель на
    /// общей очереди; счётчик оригинала падает, когда отпускается последний
    /// Arc.
    pub fn with_nonblocking(self: &Arc<Self>, on: bool) -> Arc<PipeEndpoint> {
        if self.nonblocking == on {
            return Arc::clone(self);
        }
        {
            let mut s = self.state.lock();
            if self.read_end {
                s.readers += 1
            } else {
                s.writers += 1
            }
        }
        Arc::new(PipeEndpoint {
            state: Arc::clone(&self.state),
            read_end: self.read_end,
            nonblocking: on,
        })
    }
}

/// Удаление последнего Arc-конца стороны уменьшает счётчик этой стороны, чтобы
/// противоположный конец увидел EOF (`writers == 0`) / EPIPE (`readers == 0`).
/// Без этого счётчики только росли: унаследованные форкнутым ребёнком концы
/// записи (особенно notify-pipe у run-command с CLOEXEC) никогда не
/// освобождались, и родитель навсегда зависал в `read()` — git clone вставал
/// сразу после запуска remote-хелпера.
impl Drop for PipeEndpoint {
    fn drop(&mut self) {
        let mut s = self.state.lock();
        if self.read_end {
            s.readers = s.readers.saturating_sub(1);
        } else {
            s.writers = s.writers.saturating_sub(1);
        }
    }
}

/// Состояние одного экземпляра `timerfd`.
///
/// Времена — в тиках планировщика (единственные часы ядра), истечение считается
/// лениво на read/poll: ни один IRQ таймера не обходит таблицу fd.
pub struct TimerfdState {
    /// Абсолютный тик следующего истечения; `None` = разряжен.
    pub deadline: Option<u64>,
    /// Интервал повторения в тиках; `0` = одноразовый.
    pub interval: u64,
    /// Накопленные истечения (их вернёт следующий read).
    pub expirations: u64,
    /// Создан ли дескриптор с TFD_NONBLOCK.
    pub nonblocking: bool,
}

impl TimerfdState {
    /// Истечения, наступившие к `now`, без мутации (используется poll/epoll).
    pub fn pending(&self, now: u64) -> u64 {
        if self.expirations > 0 {
            return self.expirations;
        }
        match self.deadline {
            Some(deadline) if now >= deadline => {
                if self.interval == 0 {
                    1
                } else {
                    (now - deadline) / self.interval + 1
                }
            }
            _ => 0,
        }
    }

    /// Потребить все наступившие истечения, сдвинув deadline.
    pub fn take(&mut self, now: u64) -> u64 {
        let due = self.pending(now);
        if due > 0 {
            self.expirations = 0;
            if self.interval == 0 {
                self.deadline = None;
            } else if let Some(deadline) = self.deadline {
                let elapsed_periods = (now.saturating_sub(deadline)) / self.interval + 1;
                self.deadline = Some(deadline + elapsed_periods * self.interval);
            }
        }
        due
    }
}

/// Объект, на который может ссылаться файловый дескриптор.
///
/// Стандартные потоки сводятся к консоли ядра / stdin; всё остальное — файл на
/// ext2, достижимый через VFS и несущий собственное смещение чтения/записи.
pub enum OpenObject {
    /// Консоль ядра (заранее привязана к fd 1 и 2 для stdout/stderr).
    Console,
    /// AF_INET TCP-сокет поверх собственного стека TCP ядра.
    InetTcp(Arc<crate::arch::x86_64::linux::inet_sock::InetTcp>),
    /// AF_INET UDP-сокет поверх собственного стека UDP ядра.
    InetUdp(Arc<crate::arch::x86_64::linux::inet_sock::InetUdp>),
    /// Стандартный ввод (заранее привязан к fd 0).
    Stdin,
    /// Открытый файл на ext2 и его текущее байтовое смещение.
    File {
        /// Узел VFS, лежащий за этим дескриптором.
        node: Arc<dyn VfsNode>,
        /// Абсолютный путь, под которым открыт дескриптор (для `/proc/.../fd`).
        path: String,
        /// Текущее смещение чтения/записи внутри файла.
        offset: u64,
    },
    /// Читающий конец pipe (байты приходят от писателя).
    PipeRead(Arc<PipeEndpoint>),
    /// Писательский конец pipe (байты уходят читателю).
    PipeWrite(Arc<PipeEndpoint>),
    /// Экземпляр `timerfd`; состояние разделено с читателями poll/epoll.
    Timerfd(Arc<Spinlock<TimerfdState>>),
    /// Экземпляр `signalfd`: маска сигналов, которые он потребляет.
    Signalfd { mask: Arc<Spinlock<u64>> },
    /// Счётчик eventfd (EFD_SEMAPHORE при semaphore=true).
    Eventfd {
        val: Arc<Spinlock<u64>>,
        semaphore: bool,
    },
    /// Один конец AF_UNIX stream-socketpair — перекрёстно связанная пара
    /// концов pipe (rx — входящие байты этого конца, tx — исходящие).
    Socket {
        rx: Arc<PipeEndpoint>,
        tx: Arc<PipeEndpoint>,
    },
    /// Привязанный/слушающий серверный AF_UNIX-сокет.
    UnixListener(Arc<UnixListenerState>),
    /// Socket(2), созданный, но ещё не привязанный и не подключённый.
    UnixSocketUnbound { nonblocking: bool },
    /// Экземпляр epoll со списком интересов.
    Epoll {
        interests: Arc<Spinlock<Vec<EpollEntry>>>,
    },
    /// Открытый каталог со снимком детей и курсором `getdents64`.
    Dir {
        /// Абсолютный путь, под которым открыт каталог (для `fchdir`).
        path: String,
        /// Дочерние узлы, снятые снимком при открытии.
        children: Vec<Arc<dyn VfsNode>>,
        /// Индекс следующего ребёнка, который выдаст `getdents64`.
        index: usize,
    },
}

/// Linux `O_RDONLY` (режим доступа 0).
pub const O_RDONLY: u64 = 0;
/// Linux `O_WRONLY`.
pub const O_WRONLY: u64 = 1;
/// Linux `O_RDWR`.
pub const O_RDWR: u64 = 2;
/// Linux `O_NONBLOCK` (x86_64).
pub const O_NONBLOCK: u64 = 0o4000;
/// Linux `O_CLOEXEC` (x86_64). Отслеживается отдельно в [`FdTable::cloexec`],
/// чтобы FD_CLOEXEC можно было переключать, не переписывая флаги открытия.
pub const O_CLOEXEC: u64 = 0o2000000;

/// Флаги открытия, которые Linux вычищает из `file->f_flags` после возврата
/// `open(2)` (`fs/open.c::do_dentry_open`: `f->f_flags &= ~(O_CREAT | O_EXCL |
/// O_NOCTTY | O_TRUNC | …)`). `/proc/<pid>/fdinfo/<n>` сообщает `f_flags`,
/// поэтому они не должны протекать в `flags:`, хоть вызывающий и передал их в
/// `open`.
const O_CREAT_FLAG: u64 = 0o100;
const O_EXCL_FLAG: u64 = 0o200;
const O_NOCTTY_FLAG: u64 = 0o400;
const O_TRUNC_FLAG: u64 = 0o1000;
const FDINFO_OPEN_TIME_FLAGS: u64 = O_CREAT_FLAG | O_EXCL_FLAG | O_NOCTTY_FLAG | O_TRUNC_FLAG;

/// Флаги открытия/статуса по умолчанию для объекта, созданного без явного
/// набора флагов. Это реальные режимы доступа объекта: stdio односторонний,
/// pipe односторонний, каждый сокет/объект событий читаемо-записываемый. Путь
/// открытия переопределяет это настоящими флагами вызывающего через
/// [`FdTable::alloc_with_flags`].
fn default_object_flags(obj: &OpenObject) -> u64 {
    match obj {
        OpenObject::Stdin | OpenObject::PipeRead(_) => O_RDONLY,
        OpenObject::Console | OpenObject::PipeWrite(_) => O_WRONLY,
        OpenObject::File { .. } | OpenObject::Dir { .. } => O_RDONLY,
        OpenObject::InetTcp(_)
        | OpenObject::InetUdp(_)
        | OpenObject::Socket { .. }
        | OpenObject::UnixListener(_)
        | OpenObject::UnixSocketUnbound { .. }
        | OpenObject::Timerfd(_)
        | OpenObject::Signalfd { .. }
        | OpenObject::Eventfd { .. }
        | OpenObject::Epoll { .. } => O_RDWR,
    }
}

impl Clone for OpenObject {
    fn clone(&self) -> Self {
        self.dup_clone()
    }
}

impl OpenObject {
    /// Создать независимый дубликат дескриптора для `dup`/`dup2`/`dup3`.
    ///
    /// Стандартные потоки клонируются тривиально; файл клонирует Arc-дескриптор
    /// узла и копирует текущее смещение (Linux `dup` делит open-file description,
    /// так что на момент дублирования смещение одно); каталог клонирует снятый
    /// список детей и курсор, чтобы дубликат продолжал обход с той же позиции.
    pub fn dup_clone(&self) -> OpenObject {
        match self {
            OpenObject::Console => OpenObject::Console,
            OpenObject::Stdin => OpenObject::Stdin,
            OpenObject::InetTcp(t) => OpenObject::InetTcp(Arc::clone(t)),
            OpenObject::InetUdp(u) => OpenObject::InetUdp(Arc::clone(u)),
            OpenObject::PipeRead(e) => OpenObject::PipeRead(Arc::clone(e)),
            OpenObject::PipeWrite(e) => OpenObject::PipeWrite(Arc::clone(e)),
            OpenObject::File { node, path, offset } => OpenObject::File {
                node: Arc::clone(node),
                path: path.clone(),
                offset: *offset,
            },
            OpenObject::Dir {
                path,
                children,
                index,
            } => OpenObject::Dir {
                path: path.clone(),
                children: children.clone(),
                index: *index,
            },
            OpenObject::Timerfd(state) => OpenObject::Timerfd(Arc::clone(state)),
            OpenObject::Signalfd { mask } => OpenObject::Signalfd {
                mask: Arc::clone(mask),
            },
            OpenObject::Eventfd { val, semaphore } => OpenObject::Eventfd {
                val: Arc::clone(val),
                semaphore: *semaphore,
            },
            OpenObject::Socket { rx, tx } => OpenObject::Socket {
                rx: Arc::clone(rx),
                tx: Arc::clone(tx),
            },
            OpenObject::UnixListener(l) => OpenObject::UnixListener(Arc::clone(l)),
            OpenObject::UnixSocketUnbound { nonblocking } => OpenObject::UnixSocketUnbound {
                nonblocking: *nonblocking,
            },
            OpenObject::Epoll { interests } => OpenObject::Epoll {
                interests: Arc::clone(interests),
            },
        }
    }
}

/// Таблица файловых дескрипторов процесса.
///
/// Тонкая ядерная обёртка над чистым учётом [`FdSlots`]: фиксирует хранимый тип
/// [`OpenObject`], закрепляет минимальный выделяемый дескриптор на 3 и
/// отображает чистую ошибку отсутствия fd в [`Errno::EBADF`].
#[derive(Clone)]
pub struct FdTable {
    slots: FdSlots<OpenObject>,
    /// Дескрипторы с флагом close-on-exec (O_CLOEXEC / FD_CLOEXEC).
    /// Вычищаются execve; на этом держится протокол error-pipe у fork+exec libuv.
    cloexec: BTreeSet<u32>,
    /// `f_flags`, с которыми открыт каждый дескриптор (без `O_CLOEXEC` — он в
    /// `cloexec`). Это настоящий источник `flags:` для `/proc/.../fdinfo`;
    /// `dup`/`dup2` его копируют, `close` удаляет, а пути `F_SETFL`/`FIONBIO`
    /// обновляют `O_NONBLOCK` при изменении.
    flags: BTreeMap<u32, u64>,
}

impl FdTable {
    /// Наименьший дескриптор, который может вернуть свежий `alloc`; 0/1/2
    /// зарезервированы под стандартные потоки (R2.4).
    const FIRST_DYNAMIC_FD: usize = 3;

    /// Создать таблицу с заранее привязанными стандартными потоками: fd 0 =
    /// stdin, fd 1 = консоль (stdout), fd 2 = консоль (stderr) (R2.2).
    pub fn with_standard_streams() -> Self {
        let mut initial: Vec<Option<OpenObject>> = Vec::with_capacity(Self::FIRST_DYNAMIC_FD);
        initial.push(Some(OpenObject::Stdin)); // fd 0
        initial.push(Some(OpenObject::Console)); // fd 1
        initial.push(Some(OpenObject::Console)); // fd 2
        let mut flags = BTreeMap::new();
        flags.insert(0, O_RDONLY);
        flags.insert(1, O_WRONLY);
        flags.insert(2, O_WRONLY);
        Self {
            slots: FdSlots::from_slots(initial),
            cloexec: BTreeSet::new(),
            flags,
        }
    }

    /// Выделить наименьший свободный дескриптор `>= 3`, положить туда `obj` и
    /// вернуть дескриптор, вырастив таблицу при необходимости (R2.4).
    /// Записанные флаги открытия — реальный режим доступа объекта
    /// ([`default_object_flags`]); пути, знающие флаги `open(2)` вызывающего,
    /// используют [`Self::alloc_with_flags`].
    pub fn alloc(&mut self, obj: OpenObject) -> Result<u32, Errno> {
        let flags = default_object_flags(&obj);
        self.alloc_with_flags(obj, flags)
    }

    /// Выделить дескриптор и записать точные флаги открытия/статуса, которые он
    /// несёт. `O_CLOEXEC` здесь вычищается, потому что отслеживается в `cloexec`
    /// ([`Self::fdinfo_flags`] собирает их обратно), а флаги только создания,
    /// которые Linux убирает из `f_flags`, удаляются по той же причине.
    ///
    /// F-r4-life-3: падает с `EMFILE`, когда свободного слота ниже
    /// [`FD_TABLE_LIMIT`] нет (раньше таблица росла за предел).
    pub fn alloc_with_flags(&mut self, obj: OpenObject, flags: u64) -> Result<u32, Errno> {
        let fd = self
            .slots
            .try_alloc(Self::FIRST_DYNAMIC_FD, super::fd_alloc::FD_TABLE_LIMIT, obj)
            .map_err(|_| Errno::EMFILE)?;
        self.cloexec.remove(&fd); // переиспользованный слот не должен наследовать флаг
        self.flags
            .insert(fd, flags & !(O_CLOEXEC | FDINFO_OPEN_TIME_FLAGS));
        Ok(fd)
    }

    /// Записанные флаги открытия/статуса `fd` (без `O_CLOEXEC`) или `None`,
    /// когда дескриптор отсутствует.
    pub fn open_flags(&self, fd: u32) -> Option<u64> {
        self.flags.get(&fd).copied()
    }

    /// Значение `f_flags`, которое сообщает `/proc/<pid>/fdinfo/<fd>`:
    /// записанные флаги открытия плюс `O_CLOEXEC`, если на дескрипторе стоит
    /// `FD_CLOEXEC`.
    pub fn fdinfo_flags(&self, fd: u32) -> Option<u64> {
        let mut flags = self.open_flags(fd)?;
        if self.cloexec.contains(&fd) {
            flags |= O_CLOEXEC;
        }
        Some(flags)
    }

    /// Поставить или снять `O_NONBLOCK` в записанных флагах дескриптора после
    /// того, как `fcntl(F_SETFL)`/`ioctl(FIONBIO)` реально изменили состояние
    /// объекта.
    pub fn set_nonblocking_flag(&mut self, fd: u32, on: bool) {
        if let Some(flags) = self.flags.get_mut(&fd) {
            if on {
                *flags |= O_NONBLOCK;
            } else {
                *flags &= !O_NONBLOCK;
            }
        }
    }

    /// Позаимствовать объект по `fd` или `None` для дескриптора вне
    /// диапазона/пустого (вызывающий отображает `None` -> `EBADF`, R2.14).
    pub fn get(&self, fd: u32) -> Option<&OpenObject> {
        self.slots.get(fd)
    }

    /// Изменяемо позаимствовать объект по `fd` или `None` для дескриптора вне
    /// диапазона/пустого (вызывающий отображает `None` -> `EBADF`, R2.14).
    pub fn get_mut(&mut self, fd: u32) -> Option<&mut OpenObject> {
        self.slots.get_mut(fd)
    }

    /// Ссылается ли ХОТЬ ОДИН дескриптор процесса на ЧИТАЮЩИЙ конец очереди
    /// `queue_id`.
    ///
    /// Этот вопрос задаёт блокирующее чтение на каждой уступке CPU: закрытый из
    /// соседнего потока дескриптор не выставляет EOF (счётчики очереди считают
    /// живые концы, а не fd), и без этой проверки `recvfrom` ждал бы вечно —
    /// watchdog печатал `stuck in syscall ? (nr=45 arg0=0x3 closed)`. `dup`
    /// другого fd на ту же очередь по-прежнему считается владением, поэтому
    /// закрытие одного из дубликатов законное ожидание не прерывает.
    ///
    /// Учитываются именно ЧИТАЮЩИЕ концы: оставшийся в таблице пишущий конец
    /// (self-pipe, второй конец `socketpair` в том же процессе) данные в очередь
    /// класть может, но забрать их после закрытия читающего fd некому — это
    /// `EBADF`, а не продолжение ожидания.
    pub fn holds_read_end(&self, queue_id: usize) -> bool {
        self.slots.any(|obj| match obj {
            OpenObject::PipeRead(e) => e.queue_id() == queue_id,
            OpenObject::Socket { rx, .. } => rx.queue_id() == queue_id,
            _ => false,
        })
    }

    /// Ссылается ли ХОТЬ ОДИН дескриптор процесса на ПИШУЩИЙ конец очереди
    /// `queue_id`.
    ///
    /// Зеркало [`Self::holds_read_end`] для блокирующей ЗАПИСИ: `sendto`/
    /// `sendmsg`/`write` на полной очереди уступают CPU и перечитывают её, а
    /// закрытие пишущего fd из соседнего потока не меняет счётчики очереди —
    /// syscall держит её `Arc` сам. Без этой проверки такой `write` крутил бы
    /// `yield_current()` вечно (watchdog печатал `stuck in syscall ? (nr=1
    /// arg0=0x3 closed)`); `dup` другого fd на ту же очередь — по-прежнему
    /// владение.
    pub fn holds_write_end(&self, queue_id: usize) -> bool {
        self.slots.any(|obj| match obj {
            OpenObject::PipeWrite(e) => e.queue_id() == queue_id,
            OpenObject::Socket { tx, .. } => tx.queue_id() == queue_id,
            _ => false,
        })
    }

    /// Ссылается ли ХОТЬ ОДИН дескриптор процесса на TCP-сокет `id`
    /// (идентичность — адрес `Arc<InetTcp>`, как [`Self::holds_read_end`]).
    ///
    /// Тот же вопрос задаёт блокирующее чтение TCP: закрытый из соседнего
    /// потока fd не выставляет EOF на соединении, поэтому без проверки
    /// `recvfrom`/`read` спал бы вечно (watchdog `nr=45 arg0=0x3 closed`).
    pub fn holds_inet_tcp(&self, id: usize) -> bool {
        self.slots.any(|obj| match obj {
            OpenObject::InetTcp(t) => Arc::as_ptr(t) as usize == id,
            _ => false,
        })
    }

    pub fn pipe(&mut self, nonblocking: bool) -> Result<(u32, u32), Errno> {
        let state = Arc::new(Spinlock::new(PipeState::new()));
        let r = Arc::new(PipeEndpoint {
            state: Arc::clone(&state),
            read_end: true,
            nonblocking,
        });
        let w = Arc::new(PipeEndpoint {
            state,
            read_end: false,
            nonblocking,
        });
        let extra = if nonblocking { O_NONBLOCK } else { 0 };
        let rfd = self.alloc_with_flags(OpenObject::PipeRead(r), O_RDONLY | extra)?;
        // F-r4-life-3: если второй конец не влез в предел, первый обязан быть
        // освобождён — иначе неудачный `pipe()` навсегда съедал бы один fd.
        let wfd = match self.alloc_with_flags(OpenObject::PipeWrite(w), O_WRONLY | extra) {
            Ok(fd) => fd,
            Err(e) => {
                let _ = self.close(rfd);
                return Err(e);
            }
        };
        Ok((rfd, wfd))
    }

    /// Текущая ёмкость слотов таблицы, сообщаемая как `FDSize:` в
    /// `/proc/self/status` (Linux сообщает размер выделенной таблицы, а не число
    /// открытых дескрипторов).
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Закрыть `fd`. Возвращает `Err(Errno::EBADF)`, если дескриптор отсутствует
    /// или уже закрыт, оставляя таблицу неизменной; иначе освобождает его и
    /// возвращает `Ok` (R2.6, R2.14).
    pub fn close(&mut self, fd: u32) -> Result<(), Errno> {
        let res = self.slots.close(fd).map_err(|_| Errno::EBADF);
        if res.is_ok() {
            self.cloexec.remove(&fd);
            self.flags.remove(&fd);
        }
        res
    }

    /// `dup` (32): продублировать `oldfd` в наименьший свободный дескриптор
    /// `>= 3` и вернуть новый дескриптор. `EBADF`, если `oldfd` не открыт.
    pub fn dup(&mut self, oldfd: u32) -> Result<u32, Errno> {
        self.dup_min(oldfd, Self::FIRST_DYNAMIC_FD as u32)
    }

    /// `fcntl(F_DUPFD)`: продублировать `oldfd` в наименьший свободный
    /// дескриптор `>= min` и вернуть его. `EBADF`, если `oldfd` не открыт.
    pub fn dup_min(&mut self, oldfd: u32, min: u32) -> Result<u32, Errno> {
        let dup = self.slots.get(oldfd).ok_or(Errno::EBADF)?.dup_clone();
        let flags = self
            .flags
            .get(&oldfd)
            .copied()
            .unwrap_or_else(|| default_object_flags(&dup));
        // `min` контролируется вызывающим (`fcntl(F_DUPFD, arg)`). Слой
        // syscall отвергает `arg >= NOFILE_MAX` с EINVAL, но сама таблица не даёт
        // вырасти за `FD_TABLE_LIMIT`, чтобы никакой путь не превратил огромный
        // минимум в аллокацию на сотни МиБ (EMFILE).
        let fd = self
            .slots
            .try_alloc(min as usize, super::fd_alloc::FD_TABLE_LIMIT, dup)
            .map_err(|_| Errno::EMFILE)?;
        self.cloexec.remove(&fd); // дубликат начинается без FD_CLOEXEC
        self.flags.insert(fd, flags);
        Ok(fd)
    }

    /// `dup2`/`dup3`: продублировать `oldfd` в явный дескриптор `newfd`, сначала
    /// закрыв то, что занимает `newfd`. Возвращает `newfd`. `EBADF`, если `oldfd`
    /// не открыт.
    ///
    /// При `oldfd == newfd` различие `dup2`/`dup3` обеспечивает вызывающий
    /// (`dup2` возвращает `newfd` без изменений; `dup3` — `EINVAL`); этот метод
    /// предполагает, что они различаются.
    pub fn dup_to(&mut self, oldfd: u32, newfd: u32) -> Result<u32, Errno> {
        // F-r4-life-3: Linux отвечает EBADF на `newfd >= RLIMIT_NOFILE`; прежний
        // путь растил `Vec` до выбранного вызывающим индекса и отвечал EINVAL.
        if newfd as usize >= super::fd_alloc::FD_TABLE_LIMIT {
            return Err(Errno::EBADF);
        }
        let dup = self.slots.get(oldfd).ok_or(Errno::EBADF)?.dup_clone();
        let flags = self
            .flags
            .get(&oldfd)
            .copied()
            .unwrap_or_else(|| default_object_flags(&dup));
        self.slots
            .try_set(newfd, super::fd_alloc::FD_TABLE_LIMIT, dup)
            .map_err(|_| Errno::EBADF)?;
        self.cloexec.remove(&newfd); // dup2/dup3 снимают FD_CLOEXEC с цели
        self.flags.insert(newfd, flags);
        Ok(newfd)
    }

    // ── учёт close-on-exec ──

    /// Поставить или снять флаг FD_CLOEXEC у `fd`.
    pub fn set_cloexec(&mut self, fd: u32, on: bool) {
        if on {
            self.cloexec.insert(fd);
        } else {
            self.cloexec.remove(&fd);
        }
    }

    /// Несёт ли `fd` флаг FD_CLOEXEC.
    pub fn is_cloexec(&self, fd: u32) -> bool {
        self.cloexec.contains(&fd)
    }

    /// Короткое человеческое имя того, что занимает `fd`.
    pub fn describe_fd(&self, fd: u32) -> &'static str {
        match self.slots.get(fd) {
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
        }
    }

    /// Закрыть все дескрипторы с флагом close-on-exec (вычистка при execve).
    pub fn close_cloexec(&mut self) {
        let fds = core::mem::take(&mut self.cloexec);
        for fd in fds {
            let _ = self.slots.close(fd);
            self.flags.remove(&fd);
        }
    }

    /// `socketpair(AF_UNIX, SOCK_STREAM)`: две перекрёстно связанные ядерные
    /// байтовые очереди. Каждый конец читает из одной очереди и пишет в другую,
    /// поэтому ОБА fd читаемы и записываемы (в отличие от pipe). Закрытие одного
    /// конца даёт партнёру EOF на чтении и EPIPE на записи.
    pub fn socketpair(&mut self, nonblocking: bool) -> Result<(u32, u32), Errno> {
        let q_ab = Arc::new(Spinlock::new(PipeState::new()));
        let q_ba = Arc::new(Spinlock::new(PipeState::new()));
        let a = OpenObject::Socket {
            rx: Arc::new(PipeEndpoint {
                state: Arc::clone(&q_ba),
                read_end: true,
                nonblocking,
            }),
            tx: Arc::new(PipeEndpoint {
                state: Arc::clone(&q_ab),
                read_end: false,
                nonblocking,
            }),
        };
        let b = OpenObject::Socket {
            rx: Arc::new(PipeEndpoint {
                state: q_ab,
                read_end: true,
                nonblocking,
            }),
            tx: Arc::new(PipeEndpoint {
                state: q_ba,
                read_end: false,
                nonblocking,
            }),
        };
        let extra = if nonblocking { O_NONBLOCK } else { 0 };
        let fa = self.alloc_with_flags(a, O_RDWR | extra)?;
        let fb = match self.alloc_with_flags(b, O_RDWR | extra) {
            Ok(fd) => fd,
            Err(e) => {
                // Симметрично `pipe`: не оставлять первый конец при EMFILE.
                let _ = self.close(fa);
                return Err(e);
            }
        };
        // Какие номера дескрипторов получают концы RPC-канала.

        Ok((fa, fb))
    }
}

// --- обвязка AF_UNIX listener'а -----------------------------------

/// Изменяемая половина слушающего AF_UNIX-сокета.
pub struct UnixListenerInner {
    /// listen(2) был вызван; иначе connect(2) отказывает.
    pub listening: bool,
    /// O_NONBLOCK: accept возвращает EAGAIN вместо блокировки.
    pub nonblocking: bool,
    /// Полностью подключённые серверные пары концов, поставленные connect(2) в
    /// очередь и ждущие accept(2).
    pub pending: VecDeque<(Arc<PipeEndpoint>, Arc<PipeEndpoint>)>,
}

/// Привязанный слушающий AF_UNIX stream-сокет (сервер uv_pipe в nvim).
pub struct UnixListenerState {
    /// Путь sockaddr_un, к которому привязан сокет.
    pub path: String,
    pub inner: Spinlock<UnixListenerInner>,
}

impl UnixListenerState {
    pub fn new(path: String, nonblocking: bool) -> Self {
        Self {
            path,
            inner: Spinlock::new(UnixListenerInner {
                listening: false,
                nonblocking,
                pending: VecDeque::new(),
            }),
        }
    }
}

/// Построить две пары концов подключённого AF_UNIX stream — (сторона клиента,
/// сторона сервера) — не выделяя fd. connect(2) кладёт серверную пару в очередь
/// listener'а и устанавливает клиентскую пару локально; позже accept(2)
/// превращает серверную пару в свежий fd.
pub fn socket_pair_endpoints(
    nonblocking_client: bool,
    nonblocking_server: bool,
) -> (
    (Arc<PipeEndpoint>, Arc<PipeEndpoint>),
    (Arc<PipeEndpoint>, Arc<PipeEndpoint>),
) {
    let q_cs = Arc::new(Spinlock::new(PipeState::new()));
    let q_sc = Arc::new(Spinlock::new(PipeState::new()));
    let client = (
        Arc::new(PipeEndpoint {
            state: Arc::clone(&q_sc),
            read_end: true,
            nonblocking: nonblocking_client,
        }),
        Arc::new(PipeEndpoint {
            state: Arc::clone(&q_cs),
            read_end: false,
            nonblocking: nonblocking_client,
        }),
    );
    let server = (
        Arc::new(PipeEndpoint {
            state: q_cs,
            read_end: true,
            nonblocking: nonblocking_server,
        }),
        Arc::new(PipeEndpoint {
            state: q_sc,
            read_end: false,
            nonblocking: nonblocking_server,
        }),
    );
    (client, server)
}
