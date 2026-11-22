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

/// Консольная сессия текущей задачи без блокировки на таблице привязок.
///
/// Как [`current`], но `BINDS` берётся через `try_lock`: путь печати
/// (`kprint!` → serial) может оказаться в панике или в прерванном контексте,
/// где лок уже держится, и ждать его там нельзя. При занятом локе задача
/// считается boot-сессией — ошибка в сторону serial безопасна: диагностика не
/// теряется, а в окно уходит только вывод самой задачи сессии (её владелец
/// известен из атомика и в локе не нуждается).
pub fn current_try() -> usize {
    let pid = crate::task::scheduler::current_pid_atomic();
    let mut owners = [0u64; MAX_SESSIONS];
    for (slot, owner) in owners.iter_mut().zip(OWNERS.iter()) {
        *slot = owner.load(Ordering::Relaxed);
    }
    let bind = if BIND_COUNT.load(Ordering::Relaxed) == 0 {
        None
    } else {
        BINDS.try_lock().and_then(|binds| binds.get(&pid).copied())
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

/// Сессия, к которой привязана задача (`0` — boot-сессия или нет привязки).
///
/// Нужна маршрутизации ввода: глобальный `ps2_kbd::FG_PID` помнит последнюю
/// foreground-программу, но не то, из какого окна её запустили. Без этой сверки
/// окно отдавало бы ввод чужой программе: клиент окна — foreground-задача
/// boot-shell'а, поэтому его `write` в `/dev/vtinput1` уходил бы в глобальный
/// `STDIN_PENDING`, и session-shell не видел бы ни одной клавиши.
pub fn bound_session(pid: u64) -> usize {
    if pid == 0 || BIND_COUNT.load(Ordering::Relaxed) == 0 {
        return session_route::BOOT_SESSION;
    }
    BINDS
        .lock()
        .get(&pid)
        .map(|s| *s as usize)
        .unwrap_or(session_route::BOOT_SESSION)
}

/// Снять привязку задачи (вызывается на её выходе, чтобы таблица не росла).
pub fn unbind(pid: u64) {
    let mut binds = BINDS.lock();
    if binds.remove(&pid).is_some() {
        BIND_COUNT.store(binds.len() as u32, Ordering::Relaxed);
    }
}

/// Сколько fd устройств **каждой** сессии открыто (`vtfb{N}`, `vtinput{N}`,
/// `vtmouse{N}` суммарно).
///
/// «Последний close — reap»: пока счётчик > 0, окно живо; на переходе в 0
/// сессия получает запрос выхода. Счётчик сбрасывается при выходе сессии, чтобы
/// close «просроченных» fd прошлой генерации не убил новую (см. [`exit`]).
///
/// Счётчик **на сессию**, а не один на ядро: иначе закрытие второго окна
/// гасило бы первое, а переоткрытие первого считалось бы вторым клиентом.
static OPEN_FDS: [AtomicU32; MAX_SESSIONS] = [const { AtomicU32::new(0) }; MAX_SESSIONS];

/// PID задачи-клиента, открывшей устройства сессии (`0` — клиентов нет).
///
/// Окно открывает **три** устройства (поверхность, ввод, указатель), и каждое
/// — отдельный `open(2)`. Без этой записи второй же `open` того же клиента
/// получал бы `EBUSY` (сессия «занята»), то есть окно не могло бы подключиться
/// вообще. `EBUSY` адресован *другому* окну, а не повторному `open` того же
/// процесса.
static OPEN_CLIENT: [AtomicU64; MAX_SESSIONS] = [const { AtomicU64::new(0) }; MAX_SESSIONS];

/// Сериализует `open`/`close` устройств сессий.
///
/// Задача одна, но клиентов может быть несколько (композитор запускает окна
/// подряд, а `open`/`close` — системные вызовы): проверка «сессия жива» и
/// инкремент счётчика обязаны быть атомарны, иначе два окна одновременно
/// решат, что сессия свободна, и второе получит ту же поверхность. Лок берётся
/// только в `open`/`close` устройств сессии, не на путях печати/ввода.
static OPEN_LOCK: Spinlock<()> = Spinlock::new(());

/// Сессия, которую должен подхватить следующий [`session_entry`].
///
/// `kernel_thread_spawn_named` принимает `fn()` без аргументов, поэтому номер
/// сессии передаётся через слот. Спавн сериализован (одна сессия, один CPU):
/// слот пишется непосредственно перед спавном и читается в его начале.
static PENDING: AtomicU64 = AtomicU64::new(0);

/// Сколько fd устройств конкретной сессии открыто (для диагностики/тестов).
pub fn open_fds(session: usize) -> u32 {
    if session < MAX_SESSIONS {
        OPEN_FDS[session].load(Ordering::Relaxed)
    } else {
        0
    }
}

/// Суммарное число открытых fd всех терминальных сессий (диагностика).
pub fn total_open_fds() -> u32 {
    OPEN_FDS
        .iter()
        .map(|count| count.load(Ordering::Relaxed))
        .sum()
}

/// Результат [`note_open`]: почему `open` устройства сессии удался/нет.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionOpen {
    /// Сессия уже была жива, клиент подключился к ней.
    Attached,
    /// Сессия поднята этим `open` (первый клиент).
    Started,
    /// Сессия уже принадлежит другому живому клиенту — `EBUSY`.
    Busy,
    /// Сессию поднять нельзя (нет фреймбуфера/PMM) — `ENODEV`.
    Unavailable,
}

/// Учесть `open` устройства сессии: поднять сессию, если она ещё не запущена.
///
/// Первый `open` любого из двух устройств (`/dev/vtfb1`, `/dev/vtinput1`)
/// спавнит задачу shell-сессии: поверхность аллоцируется **здесь**, в контексте
/// открывающего, чтобы `mmap`/`ioctl` сразу после `open` видели готовую
/// геометрию. `false` — сессию поднять нельзя (нет фреймбуфера/PMM), и `open`
/// обязан ответить `ENODEV`, а не отдать fd на мёртвое окно.
pub fn note_open(session: usize) -> SessionOpen {
    note_open_as(session, crate::task::scheduler::current_pid())
}

/// [`note_open`] от имени конкретного процесса.
///
/// Отдельная точка входа нужна тестам: правило «повторный open того же клиента
/// — не занятость» проверяется только с двумя РАЗНЫМИ pid'ами, а self-test
/// исполняется в одной задаче. В бою единственный вызывающий — [`note_open`].
pub fn note_open_as(session: usize, opener: u64) -> SessionOpen {
    if session == session_route::BOOT_SESSION || session >= MAX_SESSIONS {
        return SessionOpen::Unavailable;
    }
    // `live` + снятие reap — та же гонка, что в #174: клиент закрывает оба
    // устройства (последний close просит reap) и сразу открывает их заново;
    // задача сессии проверяет флаг только на границе цикла, поэтому без отмены
    // она выходит уже ПОСЛЕ нового open, и клиент получает `ENODEV`. Отмена
    // безопасна: open — намерение продолжать; если fd не останется, следующий
    // close снова поставит флаг.
    let _serial = OPEN_LOCK.lock();
    let outcome = if live(session) {
        // Живая сессия с клиентами — не наша: два окна не должны делить
        // поверхность, ввод и scrollback (изоляция сессий). Повторный `open`
        // **того же** клиента (второе/третье устройство окна) — не занятость.
        // Без клиентов (окно закрыли) — это переподключение к той же сессии,
        // и запрос выхода снимается.
        let client = OPEN_CLIENT[session].load(Ordering::Relaxed);
        if OPEN_FDS[session].load(Ordering::Relaxed) > 0 && client != 0 && client != opener {
            return SessionOpen::Busy;
        }
        clear_reap(session);
        SessionOpen::Attached
    } else {
        if !start(session) {
            return SessionOpen::Unavailable;
        }
        SessionOpen::Started
    };
    clear_reap(session);
    OPEN_CLIENT[session].store(opener, Ordering::Relaxed);
    OPEN_FDS[session].fetch_add(1, Ordering::Relaxed);
    outcome
}

/// Учесть `close` устройства сессии; на последнем — запросить выход.
///
/// Счётчик не может уйти ниже нуля: close fd, пережившего `exit` сессии,
/// видит 0 (счётчик сброшен выходом) и ничего не делает, поэтому не может
/// «дожать» до reap только что поднятую новую сессию.
pub fn note_close(session: usize) {
    if session == session_route::BOOT_SESSION || session >= MAX_SESSIONS {
        return;
    }
    let _serial = OPEN_LOCK.lock();
    let mut cur = OPEN_FDS[session].load(Ordering::Relaxed);
    loop {
        if cur == 0 {
            return;
        }
        match OPEN_FDS[session].compare_exchange_weak(
            cur,
            cur - 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                if cur == 1 {
                    request_reap(session);
                }
                return;
            }
            Err(actual) => cur = actual,
        }
    }
}

/// Поднять сессию: offscreen-консоль плюс задача `shell_main_session`.
fn start(session: usize) -> bool {
    if !crate::drivers::framebuffer::init_session(session) {
        return false;
    }
    clear_reap(session);
    // Новая генерация окна начинает с чистого ввода: недоставленный хвост
    // прошлой пачки и недособранная ESC-последовательность не должны съесть
    // первые клавиши пользователя (см. `vtinput::reset_session`).
    crate::drivers::vt_mouse::reset(session);
    crate::drivers::vtinput::reset_session(session);
    PENDING.store(session as u64, Ordering::Relaxed);
    // Инвариант 6: путь спавна идёт с замаскированными прерываниями — тик не
    // должен увидеть задачу раньше, чем её владелец записан (иначе первый же
    // баннер ушёл бы в boot-консоль). Поэтому `set_owner` — в той же секции.
    x86_64::instructions::interrupts::without_interrupts(|| {
        let pid = crate::task::scheduler::kernel_thread_spawn_named(session_entry, "shell-term");
        set_owner(session, pid);
    });
    true
}

/// Точка входа задачи сессии: подхватить номер из [`PENDING`] и запустить REPL.
fn session_entry() {
    // Сессию ищем по pid-владельцу: `set_owner` записан спавнером под
    // маскированными прерываниями **до** того, как задача успела получить CPU,
    // поэтому гонки двух одновременных спавнов через общий слот нет. `PENDING`
    // остаётся запасным путём для вырожденного случая (владелец не записан).
    let pid = crate::task::scheduler::current_pid();
    let session = (1..MAX_SESSIONS)
        .find(|slot| OWNERS[*slot].load(Ordering::Relaxed) == pid)
        .unwrap_or_else(|| PENDING.load(Ordering::Relaxed) as usize);
    // Владелец записан спавнером под маскированными прерываниями; повторная
    // запись делает задачу владельцем и в вырожденном случае (pid переиспользован
    // после неудачного старта). Reap НЕ снимается: close, случившийся между
    // спавном и этим моментом, должен остаться в силе.
    set_owner(session, crate::task::scheduler::current_pid());
    crate::shell::shell_main_session(session)
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
    // Забыть fd прошлой генерации: их close придёт позже и не должен
    // сосчитаться как закрытие будущей сессии (см. [`note_close`]).
    OPEN_FDS[session].store(0, Ordering::Relaxed);
    OPEN_CLIENT[session].store(0, Ordering::Relaxed);
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
    // Указатель окна не должен переживать генерацию: зажатая кнопка/старая
    // позиция в переоткрытом окне выглядели бы как ввод, которого не было.
    crate::drivers::vt_mouse::reset(session);
    crate::drivers::vtinput::reset_session(session);
    let mut binds = BINDS.lock();
    binds.retain(|_, s| *s != session as u64);
    BIND_COUNT.store(binds.len() as u32, Ordering::Relaxed);
}
