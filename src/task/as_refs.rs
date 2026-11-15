//! Чистый учёт ссылок на адресное пространство (CR3) для решений о сносе.
//!
//! Кадр пользовательского PML4 можно освобождать, только пока НИ ОДНА живая
//! задача не может на нём выполниться: освобождение под задачей, которая позже
//! проснётся, — use-after-free таблиц страниц, и каждая страница таблиц, которую
//! он содержал, может уже быть отдана под постороннее отображение.
//!
//! Планировщик ядра держит живые задачи в трёх отдельных реестрах, и путь сноса
//! обязан consult ВСЕ:
//!
//!   * [`Registry::Ready`] — ready-очередь. Спящего состояния TCB здесь нет:
//!     задача, блокированная в `nanosleep`/`futex`/`read`/`wait4`, кооперативно
//!     уступает CPU и остаётся RUNNABLE в этой очереди, поэтому «спящий сосед»
//!     находится и здесь.
//!   * [`Registry::Stopped`] — задачи, припаркованные SIGSTOP/SIGTSTP. Их
//!     сохранённый фрейм ВНЕ ready-очереди до SIGCONT, и именно это состояние
//!     позволяло `drop_exclusive_user_space` при `execve` освободить разделяемое
//!     адресное пространство остановленного потока-соседа.
//!   * [`Registry::PendingReap`] — задачи, снятые тиком, чьи ресурсы освободит
//!     более поздний тик. Их CR3 ещё во владении и будет освобождён reaper'ом,
//!     поэтому второе освобождение из другого пути — double free.
//!
//! Модуль только `core` и без зависимостей, поэтому `host-tests` включает тот же
//! исходник через `#[path]` и проверяет свойствами ту же политику, что исполняет
//! ядро (R11.6).

#![allow(dead_code)]

/// Одна регистрация живой задачи: её pid и физический PML4, на котором она работает.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsRef {
    /// Идентификатор задачи.
    pub pid: u64,
    /// Физический адрес пользовательского PML4 задачи (`Tcb::cr3`).
    pub cr3: u64,
}

/// В каком реестре планировщика найдена блокирующая ссылка. Сообщается в
/// строке лога, объясняющей, почему адресное пространство не освободили.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registry {
    /// Ready-очередь (включая кооперативно блокированные syscall'ы).
    Ready,
    /// `STOPPED_TASKS` (фреймы, припаркованные SIGSTOP/SIGTSTP).
    Stopped,
    /// `PENDING_REAPS` (сняты, ещё не собраны).
    PendingReap,
}

impl Registry {
    /// Короткое имя для строк лога ядра.
    pub fn name(self) -> &'static str {
        match self {
            Registry::Ready => "ready",
            Registry::Stopped => "stopped",
            Registry::PendingReap => "pending-reap",
        }
    }
}

/// Первая регистрация в снимке ОДНОГО реестра, которая ссылается на `cr3` и не
/// является `exclude_pid`.
///
/// `exclude_pid` — задача, которая спрашивает (exec'ающая задача не входит ни в
/// один реестр, но reaper ЕСТЬ в `PENDING_REAPS` со своим pid и не должен
/// считать себя).
pub fn referencing_pid<I>(cr3: u64, exclude_pid: u64, tasks: I) -> Option<u64>
where
    I: IntoIterator<Item = AsRef>,
{
    tasks
        .into_iter()
        .find(|t| t.pid != exclude_pid && t.cr3 == cr3)
        .map(|t| t.pid)
}

/// Полная политика сноса по снимкам всех трёх реестров: первая задача (и реестр,
/// откуда она взята), всё ещё ссылающаяся на `cr3`.
///
/// `None` означает, что адресное пространство принадлежит вызывающему
/// эксклюзивно и может быть освобождено. Реестры сканируются в порядке
/// захвата их локов ядром (`READY_QUEUE` → `STOPPED_TASKS` → `PENDING_REAPS`);
/// ядро применяет [`referencing_pid`] по одному локу за раз, а эта
/// объединённая форма нужна, чтобы host-property-тест закрепил полную политику.
pub fn referenced_by(
    cr3: u64,
    exclude_pid: u64,
    ready: &[AsRef],
    stopped: &[AsRef],
    pending: &[AsRef],
) -> Option<(u64, Registry)> {
    for (tasks, registry) in [
        (ready, Registry::Ready),
        (stopped, Registry::Stopped),
        (pending, Registry::PendingReap),
    ] {
        if let Some(pid) = referencing_pid(cr3, exclude_pid, tasks.iter().copied()) {
            return Some((pid, registry));
        }
    }
    None
}
