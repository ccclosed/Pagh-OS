//! Реестр консольных сессий: чья задача владеет какой консолью.
//!
//! Ядро обслуживает несколько консольных экземпляров:
//!
//! * **сессия 0** — boot/shell-консоль на Limine-scanout (единственная, которая
//!   видна на реальном экране, пока desktop не забрал `/dev/fb0`);
//! * **сессии 1..** — offscreen-поверхности терминальных окон desktop'а
//!   (`/dev/vtfb1` + `/dev/vtinput1`), которые композитор блитит в своё окно.
//!
//! Этот модуль — единственный владелец ответа «чей экран/кольцо/cwd сейчас
//! трогает эта задача». Решение принимает чистая [`session_route::route`] над
//! снимком атомиков; здесь — состояние и его изменение.
//!
//! # Почему атомики, а не один Spinlock
//!
//! Маршрут спрашивают `fb_print!`/`with`/`flush` — то есть код, который
//! вызывается из-под спинлоков соседних подсистем и из обработчиков, где
//! брать ещё один лок нельзя (нереентерабельность, инвариант 3).
//! Владельцы сессий и признак «привязок нет» читаются атомарно; `Spinlock`
//! берётся только когда привязки действительно существуют, то есть у
//! compat-детей сессии.
//!
//! # Владелец сильнее привязки
//!
//! Задача сессии не может быть перенаправлена в чужое окно: `route` сначала
//! сверяет pid с владельцами. Привязка (`bind`) существует для другого случая —
//! Linux-программа, запущенная `lxrun` в окне, это отдельная задача; её вывод
//! должен попасть в то же окно, поэтому shell привязывает pid ребёнка к своей
//! сессии.

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use super::session_route::{self, MAX_SESSIONS};
use crate::sync::spinlock::Spinlock;

/// PID задачи-владельца каждой сессии. `[0]` не используется: boot-сессия
/// существует всегда и ничьей задачей не «владеет».
static OWNERS: [AtomicU64; MAX_SESSIONS] = [const { AtomicU64::new(0) }; MAX_SESSIONS];

/// Жива ли сессия (её задача запущена). Сбрасывается при завершении задачи.
static LIVE: [AtomicBool; MAX_SESSIONS] = [const { AtomicBool::new(false) }; MAX_SESSIONS];

/// Привязки pid → сессия для compat-детей (`lxrun` в окне).
///
/// pid'ы выдаются монотонно, поэтому записи не переиспользуются; при
/// завершении сессии её привязки удаляются ([`reset`]).
static BINDS: Spinlock<BTreeMap<u64, u64>> = Spinlock::new(BTreeMap::new());

/// Число привязок — быстрый признак «можно не брать лок».
///
/// Пока окон нет (обычная boot-консоль), маршрут не берёт ни одного лока: это
/// и держит путь `fb_print!` в boot-сессии идентичным прежнему.
static BIND_COUNT: AtomicU32 = AtomicU32::new(0);

/// Консольная сессия текущей задачи: `0` — boot, `>= 1` — окно.
///
/// Горячий путь: два-три атомарных чтения и (при наличии привязок) короткий
/// лок. Никогда не берёт лок писателя консоли — вызывающие уже могут держать
/// его.
pub fn current() -> usize {
    let pid = crate::task::scheduler::current_pid_atomic();
    let mut owners = [0u64; MAX_SESSIONS];
    for (slot, owner) in owners.iter_mut().zip(OWNERS.iter()) {
        *slot = owner.load(Ordering::Relaxed);
    }
    let bind = if BIND_COUNT.load(Ordering::Relaxed) == 0 {
        None
    } else {
        BINDS.lock().get(&pid).copied()
    };
    session_route::route(pid, &owners, bind)
}

/// Записать задачу-владельца сессии (вызывается при её спавне).
pub fn set_owner(session: usize, pid: u64) {
    if session != session_route::BOOT_SESSION && session < MAX_SESSIONS {
        OWNERS[session].store(pid, Ordering::Relaxed);
        LIVE[session].store(true, Ordering::Relaxed);
    }
}

/// pid задачи-владельца сессии (`0` — нет).
pub fn owner(session: usize) -> u64 {
    if session < MAX_SESSIONS {
        OWNERS[session].load(Ordering::Relaxed)
    } else {
        0
    }
}

/// Жива ли сессия: её задача запущена и ещё не вышла.
pub fn live(session: usize) -> bool {
    session != session_route::BOOT_SESSION
        && session < MAX_SESSIONS
        && LIVE[session].load(Ordering::Relaxed)
}

/// Привязать задачу `pid` к сессии (для compat-детей сессии).
///
/// `session == 0` — не привязка, а её отсутствие: запись удаляется, иначе
/// ребёнок «принадлежал» бы boot-консоли через таблицу.
pub fn bind(pid: u64, session: usize) {
    let mut binds = BINDS.lock();
    if session == session_route::BOOT_SESSION || session >= MAX_SESSIONS {
        binds.remove(&pid);
    } else {
        binds.insert(pid, session as u64);
    }
    BIND_COUNT.store(binds.len() as u32, Ordering::Relaxed);
}

/// Снять привязку задачи (вызывается на её выходе, чтобы таблица не росла).
pub fn unbind(pid: u64) {
    let mut binds = BINDS.lock();
    if binds.remove(&pid).is_some() {
        BIND_COUNT.store(binds.len() as u32, Ordering::Relaxed);
    }
}

/// Запрошен ли reap сессии (последний fd закрыт).
static REAP_REQUESTED: [AtomicBool; MAX_SESSIONS] =
    [const { AtomicBool::new(false) }; MAX_SESSIONS];

/// Попросить сессию завершиться (последний `close` её устройств).
///
/// Асинхронно убить kernel-поток нельзя: он может держать лок/ресурс в момент
/// вытеснения (`request_exit` снимает задачу на ближайшем тике, где бы она ни
/// была). Поэтому закрытие окна ставит флаг, а задача сессии проверяет его на
/// границе цикла ([`check_reap`]) и выходит сама — детерминированно, без
/// разрушения чужого состояния.
pub fn request_reap(session: usize) {
    if session != session_route::BOOT_SESSION && session < MAX_SESSIONS {
        REAP_REQUESTED[session].store(true, Ordering::Relaxed);
    }
}

/// Снять запрос reap (новая сессия в том же слоте).
pub fn clear_reap(session: usize) {
    if session < MAX_SESSIONS {
        REAP_REQUESTED[session].store(false, Ordering::Relaxed);
    }
}

/// Выход задачи сессии: забыть сессию, погасить её писателя и завершить задачу.
///
/// Вызывается только собственной задачей сессии (из [`check_reap`] или из
/// команды `exit`). `scheduler::exit_current` — штатный путь выхода kernel-потока:
/// задача уступает CPU и снимается тиком, стек освобождает reaper.
pub fn exit(session: usize) -> ! {
    reset(session);
    crate::drivers::framebuffer::reset_session(session);
    crate::task::scheduler::exit_current()
}

/// Проверить запрос reap и, если он есть, выйти (граница цикла shell).
pub fn check_reap(session: usize) {
    if session != session_route::BOOT_SESSION
        && session < MAX_SESSIONS
        && REAP_REQUESTED[session].load(Ordering::Relaxed)
    {
        exit(session);
    }
}

/// Забыть сессию: владелец, признак жизни и все её привязки.
///
/// Вызывается задачей сессии при выходе и спавном при переоткрытии окна.
/// Поверхность (`offscreen`) при этом не освобождается: кадры остаются
/// запиненными, потому что их могут держать живые mmap-отображения клиента
/// (контракт `vtfb1`: кадры не возвращаются в PMM, пока отображены).
pub fn reset(session: usize) {
    if session == session_route::BOOT_SESSION || session >= MAX_SESSIONS {
        return;
    }
    OWNERS[session].store(0, Ordering::Relaxed);
    LIVE[session].store(false, Ordering::Relaxed);
    let mut binds = BINDS.lock();
    binds.retain(|_, s| *s != session as u64);
    BIND_COUNT.store(binds.len() as u32, Ordering::Relaxed);
}
