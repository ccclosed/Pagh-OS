//! Состояние совместимости на `Compat_Process` (design "Compat_Process additions").
//!
//! Linux-процесс несёт больше состояния, чем нативная задача pagh: таблицу
//! дескрипторов (R2.4/R2.6/R2.14), учёт program break + анонимных `mmap`
//! (R3/R4), `FS.base` из `arch_prctl(ARCH_SET_FS)` (R2.9), thread id из
//! `set_tid_address` (R2.10/R7.2), набор уже сообщённых неподдерживаемых
//! syscall'ов для диагностики `nosys` не чаще одного раза (R12.2) и
//! нормализованный код выхода (R12.3).
//!
//! Комплект висит на [`Tcb`](super::scheduler::Tcb) как `Option<CompatState>`:
//! `None` у нативных задач pagh (`spawn_test_user_process`), `Some` только после
//! того, как `run_linux_binary` заполнит его для настоящего Linux-бинаря.
#![allow(dead_code)]

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::sync::Arc;

use alloc::vec::Vec;

use crate::arch::x86_64::linux::compat_guard::CompatEntry;
use crate::arch::x86_64::linux::cred::Creds;
use crate::arch::x86_64::linux::mem::VmRegionSet;
use crate::arch::x86_64::linux::signal_frame::{
    flush_cont_bits, flush_stop_bits, sigbit, SigAltStack, SignalAction, SIGNAL_COUNT,
};
use crate::sync::spinlock::Spinlock;
use crate::vfs::elf::LoadSegment;

use super::fd::FdTable;

/// Всё состояние совместимости с Linux, привязанное к `Compat_Process`.
#[derive(Clone)]
pub struct CompatState {
    /// Открытые дескрипторы процесса (0/1/2 заранее привязаны к std-потокам).
    pub fds: FdTable,
    /// Program break + учёт регионов анонимных `mmap`.
    pub vm: Arc<Spinlock<VmRegionSet>>,
    /// `FS.base`, задаётся через `arch_prctl(ARCH_SET_FS)` (R2.9).
    pub fs_base: u64,
    /// Thread id, сообщённый `set_tid_address` (R2.10).
    pub tid: u64,
    /// Id родительского процесса для getppid/wait4.
    pub ppid: u64,
    /// Id процесса, общий для всех потоков группы.
    pub tgid: u64,
    /// Id группы процессов (Linux `pgid`), независимый от группы потоков.
    /// Меняется `setpgid`; `kill(-pgid)` и `kill(0)` адресуют эту группу.
    pub pgid: u64,
    /// Передан ли процессу шеллом управляющий терминал как JOB'у (`lxrun` /
    /// `fg` → `drivers::tty`). Управлению доступа к терминалу подлежит только
    /// job (`SIGTTIN` на фоновое чтение, `SIGTTOU` на фоновую запись при
    /// `TOSTOP`); ядерные неинтерактивные запуски (верификационные бинари apt,
    /// harness selftest'а) терминала не получали и не должны им
    /// останавливаться. Наследуется `fork`/`clone`, поэтому подпроцессы job'а
    /// остаются под его терминалом.
    pub tty_job: bool,
    /// Адрес, очищаемый и будимый futex'ом при выходе потока.
    pub clear_child_tid: u64,
    /// Создаёт ли выход зомби, видимого для wait4.
    pub waitable: bool,
    /// Голова списка robust-futex Linux и размер ABI-структуры.
    pub robust_head: u64,
    pub robust_len: u64,
    /// Текущий рабочий каталог процесса в *ядерной* форме пути (свежий процесс
    /// стартует с `/mnt`, записываемого дерева ext2 — см. [`CompatState::new`]).
    /// Относительные пути `open`/`openat`/`access`/`chdir` разрешаются
    /// относительно него, поэтому хранимое значение обязано быть ядерным путём,
    /// который возвращает `resolve_path`; `getcwd` сообщает его дословно — потому
    /// гостевой `/usr` возвращается как `/mnt/usr`
    /// (`selftest_lx::check_cwd_relative_create`, `docs/procfs.md` §1.2).
    pub cwd: String,
    /// Уже залогированные неподдерживаемые номера syscall'ов, чтобы диагностика
    /// `nosys` выдавалась не чаще одного раза на номер на процесс (R12.2).
    pub nosys_logged: BTreeSet<u64>,
    /// Находится ли управляющий tty в raw-режиме (ICANON снят через TCSETS).
    pub raw_mode: bool,
    /// Эхо-печатает ли управляющий tty введённые символы (ECHO в c_lflag).
    /// Настоящий tty эхо-печатает и в raw-режиме; снимают бит только программы,
    /// сами рисующие ввод (nvim, полный readline).
    pub echo: bool,
    /// Нормализованный код выхода (младший байт запрошенного кода) после
    /// завершения процесса (R12.3).
    pub exit_code: Option<u8>,
    /// Абсолютный путь VFS exec'нутого образа, сообщаемый через
    /// `readlink("/proc/self/exe")` (uv_exepath libuv / progpath nvim).
    pub exe_path: String,
    /// `/proc/self/cmdline`: argv процесса, каждый аргумент с NUL на конце
    /// (формат Linux). Снимается при запуске/exec из того же среза `argv`, что
    /// строит начальный стек — сам стек живёт в памяти пользователя, и прочитать
    /// его из узла VFS нельзя (`docs/procfs.md` §4.4). `fork`/`clone` наследуют,
    /// `execve` заменяет.
    pub cmdline: Vec<u8>,
    /// Отображённые `PT_LOAD` сегменты исполняемого файла для `/proc/self/maps`.
    pub image_segments: Vec<LoadSegment>,
    /// `st_ino` узла VFS исполняемого файла, как его сообщает `stat(2)`. Файл
    /// maps печатает его в строках сегментов образа, чтобы file-backed
    /// отображение было опознаваемо (0, когда у образа нет идентичности VFS).
    pub image_ino: u64,
    /// Отображённые `PT_LOAD` сегменты ELF-интерпретатора (пусто для статических образов).
    pub interp_segments: Vec<LoadSegment>,
    /// `st_ino` узла VFS интерпретатора (0 для статических образов) для
    /// `/proc/self/maps`; сам путь лежит в [`CompatState::interp_path`].
    pub interp_ino: u64,
    /// Путь ELF-интерпретатора, если он загружался (для `/proc/self/maps`).
    pub interp_path: String,
    /// Что было на fd 0/1/2 НА МОМЕНТ execve (после вычистки close-on-exec).
    /// Строки [DIAG] времени spawn стираются очисткой экрана TUI, поэтому
    /// watchdog печатает этот снимок заново.
    pub exec_stdio: [&'static str; 3],
    /// Переопределения rlimit на процесс из `prlimit64`/`setrlimit`
    /// (resource id -> (soft, hard)). Отсутствующие ресурсы берут умолчания ядра
    /// (`misc::default_rlimit`); `fork`/`clone` наследуют таблицу.
    pub rlimits: BTreeMap<u64, (u64, u64)>,
    /// Маска создания режима файлов процесса (`umask(2)`); применяется к битам
    /// прав свежесозданных файлов/каталогов.
    pub umask: u32,
    /// Креденшелы процесса (real/effective/saved id пользователя и группы).
    ///
    /// [`CompatState::new`] ставит [`Creds::ROOT`] как **дефолт** (ядро и
    /// boot-selftest'ы запускают процессы от root), но каждый боевой spawn-путь
    /// задаёт идентичность явно: `run_linux_binary_as` кладёт снимок сессии
    /// shell (`su`/`login`/`/etc/pagh-session`), а `execve` применяет
    /// `S_ISUID`/`S_ISGID` цели после успешной загрузки образа
    /// (`Creds::exec_setid`). `fork`/`clone` копируют состояние, поэтому ids и
    /// дополнительные группы наследуются. Все проверки прав на границе syscall
    /// читают это; VFS креденшелов не видит (AGENTS.md §11).
    pub creds: Creds,
    /// Переопределения битов прав по идентичности inode VFS (`fs_ino` или
    /// FNV-запасной для синтетических узлов). Пишутся `chmod` и при создании
    /// (режим по умолчанию, урезанный `umask`); читаются обработчиками семейства
    /// `stat` при выводе `st_mode`. Форкнутые/клонированные дети наследуют.
    pub mode_overrides: BTreeMap<u64, u32>,
    /// Общая таблица диспозиций сигналов (`sigaction`). В `Arc`, чтобы клоны
    /// потоков (`CLONE_VM|CLONE_THREAD`, как Linux `CLONE_SIGHAND`) ДЕЛИЛИ одну
    /// таблицу, а `fork` глубоко копировал её.
    pub sig: Arc<Spinlock<SignalTable>>,
    /// Битсет ожидающих сигналов потока (в Linux pending-сигналы принадлежат
    /// потоку, если не посланы групповыми путями вроде `kill_pgrp`, которых здесь
    /// нет).
    pub sig_pending: u64,
    /// Маска заблокированных сигналов потока (`rt_sigprocmask`). Биты
    /// `SIGKILL`/`SIGSTOP` никогда не ставятся (соблюдает
    /// `signal_frame::apply_mask_op`).
    pub sig_blocked: u64,
    /// Альтернативный стек сигналов, зарегистрированный `sigaltstack(2)`.
    pub sig_altstack: SigAltStack,
    /// Ставится, когда процесс замечен входящим в syscall через гейт `int 0x80`
    /// (`linux::INT80_ENTRY`). Инвариант 2 из AGENTS.md запрещает такой вход для
    /// Compat_Process: сохранённые `rcx`/`r11` там — обычные GPR, а слот `+120` —
    /// положенный CPU user RIP, поэтому смысл регистров, на который опираются
    /// `execve`/`clone`/доставка сигналов, не выполняется. Сигнальный путь
    /// отказывается доставлять фрейм такому процессу, вместо того чтобы писать
    /// `rt_sigframe` поверх кода пользователя (см.
    /// `signal::deliver_one_pending_syscall`). Сбрасывается в `false` у
    /// форкнутых/клонированных детей (их первый syscall поставит его снова, если
    /// они пойдут этим путём).
    pub int80_entry: bool,
    /// Сообщён ли уже отказ доставки при входе через int80 для этого процесса
    /// (диагностика не чаще строки на процесс).
    pub int80_refused_logged: bool,
}

/// Таблица диспозиций, общая для группы потоков.
#[derive(Clone)]
pub struct SignalTable {
    /// Диспозиция сигналов 1..=64 (индекс `sig - 1`); при создании процесса все
    /// `SIG_DFL`, как в Linux.
    pub handlers: [SignalAction; SIGNAL_COUNT],
}

impl CompatState {
    /// Построить начальное compat-состояние свежезапущенного Linux-бинаря:
    /// переданная таблица дескрипторов и учёт VM, `FS.base` = 0, заданный thread
    /// id, cwd `/mnt` (записываемое дерево ext2; у `/` нет `create_dir`), пустой
    /// лог `nosys` и кода выхода ещё нет.
    pub fn new(fds: FdTable, vm: Arc<Spinlock<VmRegionSet>>, tid: u64) -> Self {
        Self::new_with_parent(fds, vm, tid, 1)
    }
    pub fn new_with_parent(
        fds: FdTable,
        vm: Arc<Spinlock<VmRegionSet>>,
        tid: u64,
        ppid: u64,
    ) -> Self {
        Self {
            fds,
            vm,
            fs_base: 0,
            tid,
            ppid,
            tgid: tid,
            pgid: tid,
            tty_job: false,
            clear_child_tid: 0,
            waitable: true,
            robust_head: 0,
            robust_len: 0,
            cwd: "/mnt".to_string(),
            nosys_logged: BTreeSet::new(),
            raw_mode: false,
            echo: true,
            exit_code: None,
            exe_path: String::new(),
            cmdline: Vec::new(),
            image_segments: Vec::new(),
            image_ino: 0,
            interp_segments: Vec::new(),
            interp_ino: 0,
            interp_path: String::new(),
            exec_stdio: ["?", "?", "?"],
            rlimits: BTreeMap::new(),
            umask: 0o022,
            creds: Creds::ROOT,
            mode_overrides: BTreeMap::new(),
            sig: Arc::new(Spinlock::new(SignalTable {
                handlers: [SignalAction::default(); SIGNAL_COUNT],
            })),
            sig_pending: 0,
            sig_blocked: 0,
            sig_altstack: SigAltStack::default(),
            int80_entry: false,
            int80_refused_logged: false,
        }
    }
}

// ─── реестр CompatState текущего процесса ────────────────────────────────────
//
// АРХИТЕКТУРНАЯ ЗАМЕТКА (единственный источник правды для РАБОТАЮЩЕГО
// Compat_Process).
//
// Планировщик НЕ держит постоянного `Tcb` для текущей задачи — он хранит только
// `CURRENT_PID` и пересобирает `Tcb` из RSP ядра на каждом тике (см.
// `scheduler::scheduler_tick_irq`), отбрасывая поле `Tcb.compat` при requeue.
// Эффектным обработчикам syscall'ов нужен изменяемый доступ к `CompatState`
// *работающего* процесса (его `FdTable`, `VmRegionSet`, `fs_base`, `tid` и
// `nosys_logged`). Поле в транзиентном `Tcb` поэтому не может владеть этим
// состоянием.
//
// Решение — глобальный реестр по pid. Именно он, а НЕ любое поле транзиентного
// `Tcb` (поле удалено: `Tcb` пересобирается из сохранённого RSP на каждом тике и
// не может нести авторитетное состояние), — авторитетный дом Linux-состояния
// Compat_Process, пока тот работает. Доступ — через [`with_current_compat`].
//
// `run_linux_binary` зовёт [`install_compat`] для регистрации состояния свежего
// процесса; [`remove_compat`] сносит его. `exit_current` уже удаляет запись
// вызывающего pid (связано в `scheduler::exit_current`), поэтому выходящий
// Compat_Process теряет запись реестра как часть завершения.

/// Авторитетный реестр [`CompatState`] по pid.
///
/// Защищён [`Spinlock`] (выключает прерывания на время удержания). Обработчики
/// НЕ должны держать этот лок через операции, блокирующиеся в ожидании
/// прерывания устройства (дисковый I/O ext2/VFS): они извлекают нужное под
/// локом, отпускают его, выполняют блокирующую работу, затем ненадолго берут
/// снова для фиксации результатов (шаблон обработчиков `io`). Работа с
/// таблицами страниц/PMM (`brk`/`mmap`/`munmap`/`mprotect`) не ждёт прерываний,
/// поэтому может идти внутри замыкания [`with_current_compat`].
static COMPAT_STATES: Spinlock<BTreeMap<u64, CompatState>> = Spinlock::new(BTreeMap::new());
static EXITED_CHILDREN: Spinlock<BTreeMap<(u64, u64), u8>> = Spinlock::new(BTreeMap::new());

/// Зарегистрировать `state` как [`CompatState`] процесса `pid`, заменив прежнюю
/// запись. Вызывается `run_linux_binary` при запуске Compat_Process.
pub fn install_compat(pid: u64, state: CompatState) {
    COMPAT_STATES.lock().insert(pid, state);
}

/// Удалить и вернуть [`CompatState`] процесса `pid`, если он есть. Вызывается
/// при завершении процесса (`scheduler::exit_current`).
/// Находится ли stdin процесса `pid` сейчас в raw-режиме? Путь убийства по ^C
/// смотрит сюда, чтобы полноэкранные программы, трактующие ^C как клавишу
/// (nvim), сохраняли привязку, а программы в canonical-режиме (python, bash)
/// получали классическое завершение по SIGINT.
/// Содержит ли mmap-учёт ТЕКУЩЕЙ задачи `addr` внутри региона, созданного с
/// PROT_WRITE? Нужно обработчику page fault, чтобы решить, является ли отказ
/// записи в RO-страницу исправимой несогласованностью отображений.
pub fn current_addr_in_writable_mmap(addr: u64) -> bool {
    with_current_compat(|cs| {
        cs.vm
            .lock()
            .mmaps
            .iter()
            .any(|m| addr >= m.base && addr < m.base + m.pages * 4096 && m.writable)
    })
    .unwrap_or(false)
}

pub fn compat_is_raw(pid: u64) -> bool {
    COMPAT_STATES
        .lock()
        .get(&pid)
        .map(|cs| cs.raw_mode)
        .unwrap_or(false)
}

pub fn remove_compat(pid: u64) -> Option<CompatState> {
    COMPAT_STATES.lock().remove(&pid)
}
/// Существует ли ещё compat-процесс/поток с этим pid (используется foreground-
/// ожиданием шелла после `lxrun`).
pub fn compat_exists(pid: u64) -> bool {
    COMPAT_STATES.lock().contains_key(&pid)
}

/// [`compat_exists`] для kill-путей обработчиков исключений: не входит в реестр,
/// если его лок уже удерживает ЭТО ядро.
///
/// Контракт `with_current_compat`: замыкание исполняется только когда у текущего
/// pid есть compat-состояние, поэтому `compat_lock_held()` здесь — достаточное
/// доказательство существования. Без этого отказ внутри замыкания (исторически:
/// store в read-only user-страницу) звал `compat_exists` → повторный
/// `COMPAT_STATES.lock()` с уже снятым IF → вечный спин (F-r4-arch-1).
/// `compat_lock_held()` отвечает про ТЕКУЩУЮ задачу, поэтому глубина, поднятая
/// вытесненной задачей, не выдаётся за существование состояния у этой.
pub fn compat_exists_for_exit(pid: u64) -> bool {
    if compat_lock_held() {
        true
    } else {
        compat_exists(pid)
    }
}

/// Аварийно отпустить [`COMPAT_STATES`], удержанный прерванным замыканием
/// [`with_current_compat`], которое уже никогда не вернётся (задачу убивает
/// обработчик исключения).
///
/// Вызывающий обязан затем завершить текущую задачу через
/// [`crate::task::scheduler::exit_current`], который снимет её состояние из
/// реестра. Возвращает `false`, когда лок не удерживался (обычный kill-путь).
///
/// Отпускает лок только НАСТОЯЩИЙ владелец учёта. Вытесненная задача может
/// оставить глубину >0, вытеснившись между `enter` и `lock()`, — такая запись
/// принадлежит не текущей задаче, и сбрасывать её (вместе с Depth) нельзя:
/// возобновившийся владелец сделал бы `fetch_sub` из нуля, счётчик перевернулся
/// бы в `usize::MAX`, и guard отказывал бы уже всем отказам подряд. См.
/// [`crate::arch::x86_64::linux::compat_guard`].
///
/// # Safety
/// Покинутое замыкание не должно продолжать мутировать защищаемую карту: путь
/// существует для отказов на пользовательской памяти (store в RO-страницу),
/// которые случаются после изменения compat-состояния. Это последний рубеж
/// «машина живёт», а не общий механизм отмены.
pub fn abandon_held_registry() -> bool {
    let pid = super::scheduler::current_pid();
    if !COMPAT_ENTRY.abandon(pid) {
        return false;
    }
    // SAFETY: прерванный кадр задачи (владелец guard'а) не будет возобновлён:
    // обработчик исключения немедленно завершает задачу через exit_current, а
    // её стек ядра освобождает reaper. Глубже вложенных удержаний
    // COMPAT_STATES не бывает (лок нереентерабелен и берётся только здесь).
    unsafe {
        COMPAT_STATES.force_unlock();
    }
    crate::error!(
        "[COMPAT] abandoned COMPAT_STATES held by interrupted with_current_compat (exit recovery)"
    );
    true
}
/// Уведомить `parent`, что `child` сменил состояние, как SIGCHLD в Linux.
///
/// Очередь best-effort: ушедший (или не-compat) родитель просто не получает
/// сигнал. Вызов идёт после отпускания локов реестра, поэтому `send_signal` может
/// безопасно войти в `COMPAT_STATES` повторно.
fn notify_parent_sigchld(parent: u64) {
    if parent == 0 {
        return;
    }
    let _ = crate::arch::x86_64::linux::signal::send_signal(
        parent,
        crate::arch::x86_64::linux::signal_frame::SIGCHLD,
    );
}

pub fn finish_compat_exit(pid: u64) {
    if let Some(state) = remove_compat(pid) {
        if state.waitable && state.ppid != 0 {
            EXITED_CHILDREN
                .lock()
                .insert((state.ppid, pid), state.exit_code.unwrap_or(0));
        }
        // Выход ПЕРЕКРЫВАЕТ любой ожидающий отчёт о stop/continue для этого
        // ребёнка: следующий `wait4` родителя должен увидеть выход, а не
        // устаревшее событие «остановлен».
        clear_child_state_events(state.ppid, pid);
        notify_parent_sigchld(state.ppid);
    }
}
/// Потребить один ожидающий сигнал по `mask` для signalfd.
///
/// Signalfd читает pending-набор процесса в порядке маски и очищает бит,
/// который вернул, точно как `read(2)` на signalfd: сигнал не доставляется через
/// обычный путь обработчика. `None` — ничего подходящего не ожидает.
pub fn take_signalfd_signal(mask: u64) -> Option<u64> {
    let mut states = COMPAT_STATES.lock();
    let state = states.get_mut(&crate::task::scheduler::current_pid())?;
    let matching = state.sig_pending & mask & !state.sig_blocked;
    if matching == 0 {
        return None;
    }
    let signal = matching.trailing_zeros() as u64 + 1;
    state.sig_pending &= !(1u64 << (signal - 1));
    Some(signal)
}

/// Читаем ли signalfd с `mask` прямо сейчас.
///
/// ОБЯЗАН вызываться без удержания `COMPAT_STATES`. `with_current_compat`
/// нереентерабелен: вызывающий, уже находящийся внутри реестра (например,
/// проверка готовности fd под локом), должен читать `cs.sig_pending`/
/// `cs.sig_blocked` из уже имеющегося состояния, а не звать этот хелпер.
/// Повторный вход именно через signalfd-ветку `epoll_wait` давал тихое
/// зависание машины; debug-assert ниже превращает любое повторное появление в
/// громкий отказ debug-сборки.
pub fn signalfd_readable(mask: u64) -> bool {
    debug_assert!(
        !compat_lock_held(),
        "signalfd_readable re-entered COMPAT_STATES (non-reentrant spinlock)"
    );
    with_current_compat(|cs| cs.sig_pending & mask & !cs.sig_blocked != 0).unwrap_or(false)
}

pub fn current_ppid() -> u64 {
    with_current_compat(|s| s.ppid).unwrap_or(1)
}
fn child_matches(wanted: i64, child: u64) -> bool {
    wanted == -1 || (wanted > 0 && child == wanted as u64)
}
pub fn reap_child(parent: u64, wanted: i64) -> Option<(u64, u8)> {
    let mut z = EXITED_CHILDREN.lock();
    let k = z
        .keys()
        .find(|(p, c)| *p == parent && child_matches(wanted, *c))
        .copied()?;
    z.remove(&k).map(|v| (k.1, v))
}
pub fn has_child(parent: u64, wanted: i64) -> bool {
    if COMPAT_STATES
        .lock()
        .iter()
        .any(|(p, s)| s.ppid == parent && child_matches(wanted, *p))
    {
        return true;
    }
    EXITED_CHILDREN
        .lock()
        .keys()
        .any(|(p, c)| *p == parent && child_matches(wanted, *c))
}

// ─── события stop/continue детей (`wait4` WUNTRACED / WCONTINUED) ────────────
//
// `EXITED_CHILDREN` выше несёт только коды выхода; два реестра здесь несут два
// других изменения состояния, о которых сообщает `wait(2)`. Каждое событие
// сообщается НЕ БОЛЕЕ ОДНОГО РАЗА, и отчёт его потребляет — именно поэтому цикл
// `wait4(WUNTRACED)` завершается, а не повторяет один и тот же stop вечно.
// Порядок локов для всех четырёх хелперов ниже: STOPPED_CHILDREN, затем
// CONTINUED_CHILDREN (никогда наоборот), и никогда внутри guard'а
// `COMPAT_STATES`.

/// Дети в состоянии остановки: `(parent, child) -> сигнал остановки`.
static STOPPED_CHILDREN: Spinlock<BTreeMap<(u64, u64), u8>> = Spinlock::new(BTreeMap::new());
/// Дети, возобновлённые `SIGCONT` после последнего запроса родителя: `(parent, child)`.
static CONTINUED_CHILDREN: Spinlock<BTreeSet<(u64, u64)>> = Spinlock::new(BTreeSet::new());

/// Родитель, которому принадлежит отчёт stop/continue (`None`, когда у ребёнка
/// нет compat-состояния или он не waitable-ребёнок).
fn reporting_parent(child: u64) -> Option<u64> {
    COMPAT_STATES
        .lock()
        .get(&child)
        .filter(|cs| cs.waitable && cs.ppid != 0)
        .map(|cs| cs.ppid)
}

/// Записать, что `child` остановился по `sig`: следующий `wait4(WUNTRACED)`
/// родителя сообщит `wait_status_stopped(sig)`.
pub fn note_child_stopped(child: u64, sig: u64) {
    let Some(parent) = reporting_parent(child) else {
        return;
    };
    STOPPED_CHILDREN.lock().insert((parent, child), sig as u8);
    CONTINUED_CHILDREN.lock().remove(&(parent, child));
    notify_parent_sigchld(parent);
}

/// Записать, что `child` возобновлён: следующий `wait4(WCONTINUED)` родителя
/// сообщит `0xffff`.
///
/// Вызывающий делает это только для ребёнка, которого только что вывел из
/// припаркованного состояния (см. `signal::resume_group`), поэтому проверки «а
/// был ли он остановлен?» здесь нет — СОСТОЯНИЕ остановки ребёнка живёт в
/// планировщике (`STOPPED_TASKS`), а этот реестр несёт только *несообщённое
/// событие*.
pub fn note_child_continued(child: u64) {
    let Some(parent) = reporting_parent(child) else {
        return;
    };
    STOPPED_CHILDREN.lock().remove(&(parent, child));
    CONTINUED_CHILDREN.lock().insert((parent, child));
    notify_parent_sigchld(parent);
}

/// Убрать все ожидающие отчёты stop/continue для `(parent, child)`: ребёнок
/// вышел, и его код выхода перекрывает любое ранее сообщённое изменение
/// состояния.
fn clear_child_state_events(parent: u64, child: u64) {
    STOPPED_CHILDREN.lock().remove(&(parent, child));
    CONTINUED_CHILDREN.lock().remove(&(parent, child));
}

/// Извлечь один ожидающий отчёт об остановке для `parent`, подходящий под
/// `wanted`, и потребить его.
pub fn reap_child_stop(parent: u64, wanted: i64) -> Option<(u64, u8)> {
    let mut m = STOPPED_CHILDREN.lock();
    let k = m
        .keys()
        .find(|(p, c)| *p == parent && child_matches(wanted, *c))
        .copied()?;
    m.remove(&k).map(|sig| (k.1, sig))
}

/// Извлечь один ожидающий отчёт о продолжении для `parent`, подходящий под
/// `wanted`, и потребить его.
pub fn reap_child_continued(parent: u64, wanted: i64) -> Option<u64> {
    let mut s = CONTINUED_CHILDREN.lock();
    let k = s
        .iter()
        .find(|(p, c)| *p == parent && child_matches(wanted, *c))
        .copied()?;
    s.remove(&k);
    Some(k.1)
}

/// Сообщается ли `child` сейчас как остановленный (по реестру событий)?
pub fn child_is_stopped(child: u64) -> bool {
    STOPPED_CHILDREN.lock().keys().any(|(_, c)| *c == child)
}

// ─── правка pending-битов для магии stop/continue ───────────────────────────

/// Снять все ожидающие биты класса остановки у `pid` (это делает генерация
/// `SIGCONT`). Возвращает `false`, когда процесса нет.
///
/// `PENDING_APPROX` намеренно НЕ уменьшается: счётчик документированно завышает
/// (это оптимистичный заслон «есть ли что доставить»), а завышение стоит лишь
/// реальной проверки на следующем возврате из syscall.
pub fn clear_stop_pending(pid: u64) -> bool {
    COMPAT_STATES
        .lock()
        .get_mut(&pid)
        .map(|cs| cs.sig_pending = flush_stop_bits(cs.sig_pending))
        .is_some()
}

/// Снять ожидающий бит `SIGCONT` у `pid` (это делает генерация stop-сигнала).
pub fn clear_cont_pending(pid: u64) -> bool {
    COMPAT_STATES
        .lock()
        .get_mut(&pid)
        .map(|cs| cs.sig_pending = flush_cont_bits(cs.sig_pending))
        .is_some()
}

/// Битсет ожидающих сигналов `pid` (диагностика + in-QEMU тесты).
pub fn pending_of(pid: u64) -> u64 {
    COMPAT_STATES
        .lock()
        .get(&pid)
        .map(|cs| cs.sig_pending)
        .unwrap_or(0)
}

/// Клонировать compat-состояние текущего процесса для нового потока `child`.
/// Его `tid` — новый pid, `tgid` — общий с группой, таблица диспозиций сигналов
/// разделяется по `Arc` (Linux `CLONE_SIGHAND`), а fd-таблица и адресное
/// пространство наследуются по семантике clone. Новый поток не наследует
/// очередь сигналов: pending-набор начинается пустым.
pub fn clone_current_compat(child: u64, tls: Option<u64>, clear_child_tid: u64) -> bool {
    let parent = super::scheduler::current_pid();
    let mut states = COMPAT_STATES.lock();
    let Some(mut child_state) = states.get(&parent).cloned() else {
        return false;
    };
    child_state.tid = child;
    child_state.ppid = parent;
    child_state.waitable = false;
    child_state.clear_child_tid = clear_child_tid;
    child_state.exit_code = None;
    // У нового потока нет сигналов в очереди (в Linux pending-набор ребёнка
    // стартует пустым; с группой разделяется только таблица диспозиций).
    child_state.sig_pending = 0;
    // Входной путь у каждого потока свой: его определяет первый syscall нового потока.
    child_state.int80_entry = false;
    child_state.int80_refused_logged = false;
    if let Some(base) = tls {
        child_state.fs_base = base;
    }
    states.insert(child, child_state);
    true
}

/// Клонировать полное compat-состояние родителя для форкнутого ребёнка.
/// Ребёнок становится лидером собственной группы потоков (tid = tgid = pid
/// ребёнка), waitable (будущий зомби wait4 для родителя) и наследует fd-таблицу
/// (Arc-разделяемые концы pipe/сокетов — семантика Linux dup), cwd, fs_base,
/// raw_mode, регистрацию robust-списка и exe_path.
pub fn fork_current_compat(child: u64, clear_child_tid: u64) -> bool {
    let parent = super::scheduler::current_pid();
    let mut states = COMPAT_STATES.lock();
    let Some(parent_state) = states.get(&parent) else {
        return false;
    };
    let parent_tgid = parent_state.tgid;
    let mut child_state = parent_state.clone();
    // Fork: ребёнок владеет приватным адресным пространством (глубоко
    // скопированным `clone_user_space`), поэтому получает приватную копию учёта
    // mmap/brk, а не общий VmRegionSet родителя.
    {
        let vm_copy = child_state.vm.lock().clone();
        child_state.vm = Arc::new(Spinlock::new(vm_copy));
    }
    // Fork глубоко копирует и таблицу диспозиций сигналов (в Linux ребёнок
    // получает свой `struct sighand_struct`, в отличие от thread-клонов
    // CLONE_SIGHAND, делящих `Arc` родителя через клон `CompatState` выше).
    {
        let sig_copy = child_state.sig.lock().clone();
        child_state.sig = Arc::new(Spinlock::new(sig_copy));
    }
    child_state.tid = child;
    child_state.tgid = child;
    child_state.ppid = parent_tgid;
    child_state.waitable = true;
    child_state.clear_child_tid = clear_child_tid;
    child_state.exit_code = None;
    // Fork не наследует поставленные в очередь сигналы (в Linux pending-набор
    // ребёнка пуст, хотя диспозиции и маску блокировки он наследует), и его
    // собственный первый syscall определяет наблюдение входного пути.
    child_state.sig_pending = 0;
    child_state.int80_entry = false;
    child_state.int80_refused_logged = false;
    states.insert(child, child_state);
    true
}
pub fn fs_base_for(pid: u64) -> Option<u64> {
    COMPAT_STATES.lock().get(&pid).map(|s| s.fs_base)
}

/// Снимок креденшелов ТЕКУЩЕГО процесса или [`Creds::ROOT`] для нативной
/// задачи (у неё нет compat-состояния и видимой пользователю идентичности).
///
/// Это то, что зовут проверки прав слоя syscall: креденшелы — свойство
/// `Compat_Process`, никогда не узла VFS.
pub fn current_creds() -> Creds {
    with_current_compat(|cs| cs.creds).unwrap_or(Creds::ROOT)
}

/// Заменить креденшелы ТЕКУЩЕГО процесса. Возвращает `false`, когда у
/// вызывающего нет compat-состояния (нативная задача не может сменить
/// идентичность, которой у неё нет).
pub fn set_current_creds(creds: Creds) -> bool {
    with_current_compat(|cs| {
        cs.creds = creds;
    })
    .is_some()
}

/// Креденшелы произвольного зарегистрированного процесса (используются `kill`,
/// чтобы решить, может ли вызывающий сигнализировать каждую цель). `None`, когда
/// `pid` — не зарегистрированный compat-процесс/поток.
pub fn creds_of(pid: u64) -> Option<Creds> {
    COMPAT_STATES.lock().get(&pid).map(|s| s.creds)
}
pub fn current_tgid() -> u64 {
    with_current_compat(|s| s.tgid).unwrap_or_else(super::scheduler::current_pid)
}
pub fn current_clear_child_tid() -> u64 {
    with_current_compat(|s| s.clear_child_tid).unwrap_or(0)
}
pub fn current_robust_list() -> (u64, u64) {
    with_current_compat(|s| (s.robust_head, s.robust_len)).unwrap_or((0, 0))
}
/// Восстановлено при слиянии origin/main: очистке futex при выходе потока это
/// нужно по pid (не только для текущей задачи).
pub fn clear_tid_for(pid: u64) -> u64 {
    COMPAT_STATES
        .lock()
        .get(&pid)
        .map(|s| s.clear_child_tid)
        .unwrap_or(0)
}
pub fn robust_for(pid: u64) -> (u64, u64) {
    COMPAT_STATES
        .lock()
        .get(&pid)
        .map(|s| (s.robust_head, s.robust_len))
        .unwrap_or((0, 0))
}
pub fn group_member_pids(tgid: u64, except: u64) -> alloc::vec::Vec<u64> {
    COMPAT_STATES
        .lock()
        .iter()
        .filter_map(|(pid, s)| (s.tgid == tgid && *pid != except).then_some(*pid))
        .collect()
}

/// Есть ли в таблицах fd любого процесса открытый файл или каталог под
/// `prefix` (`/usb0p1` или `/usb0p1/...`).
///
/// `umount2` обязан ответить `EBUSY`, а не оторвать ФС у живого писателя:
/// `Ext2Fs` не имеет `Drop`, и после снятия mount-узла открытый fd продолжил бы
/// писать через `Arc`. Скан короткий (без I/O), реестр не держится через
/// блокирующую работу — инвариант 4 в `AGENTS.md`.
pub fn fds_open_under(prefix: &str) -> bool {
    let states = COMPAT_STATES.lock();
    let under = |path: &str| {
        path == prefix
            || path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/')
    };
    for state in states.values() {
        for fd in 0..crate::task::fd_alloc::FD_TABLE_LIMIT {
            match state.fds.get(fd as u32) {
                Some(crate::task::fd::OpenObject::File { path, .. })
                | Some(crate::task::fd::OpenObject::Dir { path, .. }) => {
                    if under(path) {
                        return true;
                    }
                }
                _ => {}
            }
        }
    }
    false
}

/// Все зарегистрированные задачи группы потоков `tgid`, включая лидера, по
/// возрастанию (реестр — `BTreeMap`, порядок обхода = порядок pid).
///
/// Это живой список потоков для `/proc/<tgid>/task` (и `/proc/self/task`): pagh
/// моделирует поток как зарегистрированный pid, чей [`CompatState::tgid`] равен
/// id процесса, поэтому «потоки p» — это ровно «pid, у которых `tgid == p`».
/// Результат — СНИМОК: лок реестра отпускается до того, как вызывающий
/// превращает pid в узлы VFS (AGENTS.md, инвариант 4).
pub fn thread_group_pids(tgid: u64) -> alloc::vec::Vec<u64> {
    COMPAT_STATES
        .lock()
        .iter()
        .filter_map(|(pid, s)| (s.tgid == tgid).then_some(*pid))
        .collect()
}

/// Все зарегистрированные pid группы процессов `pgid`, по возрастанию.
///
/// `kill(-pgid)`/`kill(0)` находят цели через это. Результат — СНИМОК:
/// вызывающие отпускают лок реестра, затем доставляют сигналы (инвариант 4).
pub fn group_pids(pgid: u64) -> alloc::vec::Vec<u64> {
    COMPAT_STATES
        .lock()
        .iter()
        .filter_map(|(pid, s)| (s.pgid == pgid).then_some(*pid))
        .collect()
}

/// Группа процессов вызывающего (с откатом к его tgid).
pub fn current_pgid() -> u64 {
    with_current_compat(|cs| cs.pgid).unwrap_or_else(current_tgid)
}

/// Задать группу процессов `pid`.
pub fn set_pgid(pid: u64, pgid: u64) {
    if let Some(state) = COMPAT_STATES.lock().get_mut(&pid) {
        state.pgid = pgid;
    }
}

/// Группа процессов `pid` или `None`, когда у него нет compat-состояния.
pub fn pgid_of(pid: u64) -> Option<u64> {
    COMPAT_STATES.lock().get(&pid).map(|s| s.pgid)
}

/// Пометить `pid` как терминальный JOB: шелл передал ему управляющий терминал
/// (`lxrun`, `fg`). Управлению доступа к терминалу подлежат только job'ы
/// ([`crate::drivers::tty::check_access`]).
pub fn mark_tty_job(pid: u64) {
    if let Some(state) = COMPAT_STATES.lock().get_mut(&pid) {
        state.tty_job = true;
    }
}

/// Является ли `pid` терминальным job'ом? `false` для неизвестного pid.
pub fn is_tty_job(pid: u64) -> bool {
    COMPAT_STATES
        .lock()
        .get(&pid)
        .map(|s| s.tty_job)
        .unwrap_or(false)
}

/// Снимок `(pid, tgid)` всех зарегистрированных compat-процессов/потоков по
/// возрастанию pid. Одно взятие лока на весь поиск целей для широковещательного
/// `kill(-1)`, который не должен обходить реестр, пока в него доставляются
/// сигналы.
pub fn compat_pid_tgid_snapshot() -> alloc::vec::Vec<(u64, u64)> {
    COMPAT_STATES
        .lock()
        .iter()
        .map(|(pid, s)| (*pid, s.tgid))
        .collect()
}

/// `tgid` процесса `pid` (с откатом к самому `pid`, когда compat-состояния нет).
pub fn tgid_of(pid: u64) -> u64 {
    COMPAT_STATES
        .lock()
        .get(&pid)
        .map(|s| s.tgid)
        .unwrap_or(pid)
}

/// Перезаписать записанный код выхода ещё зарегистрированного процесса
/// (используется путём принудительного завершения по сигналу, чтобы `wait4`
/// сообщил `128 + sig`).
pub fn set_exit_code_of(pid: u64, code: u8) -> bool {
    COMPAT_STATES
        .lock()
        .get_mut(&pid)
        .map(|cs| {
            cs.exit_code = Some(code);
        })
        .is_some()
}

/// Пометить сигнал `sig` ожидающим для процесса `pid`. Возвращает false, когда
/// процесса нет (вызывающий отображает в `ESRCH`).
pub fn set_pending(pid: u64, sig: u64) -> bool {
    COMPAT_STATES
        .lock()
        .get_mut(&pid)
        .map(|cs| {
            cs.sig_pending |= sigbit(sig);
        })
        .is_some()
}

/// Атомарно выбрать доставимый сигнал с наименьшим номером для ТЕКУЩЕГО процесса
/// (ожидает И не заблокирован), очистив его pending-бит.
///
/// Возвращает номер сигнала, его диспозицию, текущую маску блокировки
/// (вызывающий добавит в неё `sa_mask` действия на время работы обработчика) и
/// зарегистрированный альтернативный стек (для размещения `SA_ONSTACK`).
pub fn pick_pending_signal() -> Option<(u64, SignalAction, u64, SigAltStack)> {
    let pid = super::scheduler::current_pid();
    let mut states = COMPAT_STATES.lock();
    let cs = states.get_mut(&pid)?;
    let deliverable = cs.sig_pending & !cs.sig_blocked;
    if deliverable == 0 {
        return None;
    }
    // `sigbit(N)` — это `1 << (N - 1)`, поэтому номер сигнала — индекс младшего
    // установленного бита ПЛЮС ОДИН. Без +1 доставлялся сигнал N-1 с
    // диспозицией N-1 (настоящий бит оставался ожидающим и выбирался заново на
    // каждом возврате из syscall), а ожидающий SIGHUP заворачивал `sig - 1` в
    // индекс таблицы диспозиций вне диапазона — паника ядра при обычном
    // self-signal.
    let sig = deliverable.trailing_zeros() as u64 + 1;
    cs.sig_pending &= !(1u64 << (sig - 1));
    let action = cs.sig.lock().handlers[(sig - 1) as usize];
    Some((sig, action, cs.sig_blocked, cs.sig_altstack))
}

/// Выполнить `f` над [`CompatState`] процесса `pid` (любого, не только текущего),
/// вернув `None`, когда у этого pid нет compat-состояния.
///
/// Лок держится всё время `f` (то же правило, что у [`with_current_compat`]: `f`
/// не должен блокироваться или повторно входить в реестр).
pub fn with_pid_compat<R>(pid: u64, f: impl FnOnce(&mut CompatState) -> R) -> Option<R> {
    COMPAT_STATES.lock().get_mut(&pid).map(f)
}

/// Доставимый сигнал с наименьшим номером для ТЕКУЩЕГО процесса БЕЗ потребления:
/// `(сигнал, диспозиция, маска блокировки, altstack)`.
///
/// Доставке с тика это нужно, чтобы решить, что ожидающий сигнал СДЕЛАЕТ
/// (завершит? остановит? доставит фрейм?), до потребления бита: бит, потреблённый
/// на фрейме, в который нельзя записать (задача прервана внутри syscall), был бы
/// потерян навсегда — сигнал никто не переставит в очередь.
///
/// Номер сигнала — индекс младшего установленного бита ПЛЮС ОДИН: `sigbit(N)` —
/// `1 << (N - 1)`.
pub fn peek_deliverable_signal() -> Option<(u64, SignalAction, u64, SigAltStack)> {
    let pid = super::scheduler::current_pid();
    let states = COMPAT_STATES.lock();
    let cs = states.get(&pid)?;
    let deliverable = cs.sig_pending & !cs.sig_blocked;
    if deliverable == 0 {
        return None;
    }
    let sig = deliverable.trailing_zeros() as u64 + 1;
    let action = cs.sig.lock().handlers[(sig - 1) as usize];
    Some((sig, action, cs.sig_blocked, cs.sig_altstack))
}

/// Заблокировать `mask` в ТЕКУЩЕМ процессе на время работы обработчика
/// (маска до доставки уже сохранена во фрейме; `rt_sigreturn` её восстановит).
pub fn block_during_handler(mask: u64) {
    with_current_compat(|cs| {
        cs.sig_blocked |= mask & !crate::arch::x86_64::linux::signal_frame::UNBLOCKABLE_MASK
    });
}

/// Диспозиция, установленная сейчас для `sig` в ТЕКУЩЕМ процессе (`None`, когда
/// у процесса нет compat-состояния).
pub fn current_action(sig: u64) -> Option<SignalAction> {
    COMPAT_STATES
        .lock()
        .get(&super::scheduler::current_pid())
        .map(|cs| cs.sig.lock().handlers[(sig - 1) as usize])
}

pub fn current_has_compat() -> bool {
    let pid = super::scheduler::current_pid();
    COMPAT_STATES.lock().contains_key(&pid)
}

/// Записать, что ТЕКУЩИЙ процесс вошёл в syscall через `int 0x80` (нарушение
/// инварианта 2 из AGENTS.md для Compat_Process). No-op для нативной задачи — у
/// неё нет compat-состояния, чтобы пометить.
pub fn note_int80_entry() {
    with_current_compat(|cs| cs.int80_entry = true);
}

/// Замечен ли ТЕКУЩИЙ процесс на входном пути `int 0x80`?
pub fn current_int80_entry() -> bool {
    with_current_compat(|cs| cs.int80_entry).unwrap_or(false)
}

/// Пометить отказ доставки при входе через int80 как сообщённый для ТЕКУЩЕГО
/// процесса. Возвращает `true` при первом вызове, чтобы диагностика логировалась
/// раз на процесс, а не на каждом возврате из syscall.
pub fn note_int80_refused() -> bool {
    with_current_compat(|cs| {
        let first = !cs.int80_refused_logged;
        cs.int80_refused_logged = true;
        first
    })
    .unwrap_or(false)
}

/// Выполнить `f` над [`CompatState`] текущего процесса, вернув `Some(f(..))`,
/// когда у процесса зарегистрировано compat-состояние, и `None` иначе (например,
/// нативная задача или состояние до `install_compat`).
///
/// Лок `COMPAT_STATES` держится всё время `f`, поэтому `f` не должен
/// блокироваться на прерывании устройства (см. заметку о [`COMPAT_STATES`]) и не
/// должен звать [`with_current_compat`] повторно (дедлок на том же локе).
pub fn with_current_compat<R>(f: impl FnOnce(&mut CompatState) -> R) -> Option<R> {
    let pid = super::scheduler::current_pid();
    COMPAT_ENTRY.enter(pid);
    let out = {
        let mut guard = COMPAT_STATES.lock();
        guard.get_mut(&pid).map(f)
    };
    COMPAT_ENTRY.exit(pid);
    out
}

/// Кто и насколько глубоко вошёл в [`with_current_compat`] — pid владельца и
/// глубина его замыканий.
///
/// Её смотрит обработчик page fault: отказ, случившийся ВНУТРИ замыкания
/// `with_current_compat` (код ядра трогает память пользователя под локом), не
/// должен повторно входить в реестр — спинлок нереентерабелен, и второй
/// `lock()` заблокировал бы CPU. Ответ обязан быть про ТЕКУЩУЮ задачу: голый
/// счётчик глубины, поднятый вытесненной задачей (тик между `enter` и `lock()`),
/// отказывал бы законным отказам посторонних процессов и убивал их — ровно этот
/// дефект и убирает [`crate::arch::x86_64::linux::compat_guard`].
static COMPAT_ENTRY: CompatEntry = CompatEntry::new();

/// Находится ли kernel-контекст ТЕКУЩЕЙ задачи уже внутри [`with_current_compat`].
///
/// Не «лок кем-то удерживается»: вытесненный владелец лок не держит (спинлок
/// маскирует IF, поэтому удержание исключает переключение на этом CPU), и
/// посторонняя задача имеет право войти.
pub fn compat_lock_held() -> bool {
    COMPAT_ENTRY.is_inside(super::scheduler::current_pid())
}
