//! Учёт тиков и событий всей системы для `/proc/stat`.
//!
//! Тик таймера засчитывается РОВНО ОДНОЙ корзине в
//! [`crate::task::scheduler::scheduler_tick_irq`], один раз на прерывание, по
//! фрейму, который прервал CPU:
//!
//!   * ring 3 (`CS.RPL == 3`) -> user;
//!   * idle-задача или CPU, припаркованный в `hlt` -> idle;
//!   * любой другой ring 0 фрейм -> system.
//!
//! Классификация выполняется *до* решения о requeue/переключении, поэтому тик
//! принадлежит прерванному контексту, а не задаче, которую планировщик случайно
//! выберет следующей. Ни один путь не засчитывает второй корзине тот же тик.
//!
//! ## Единицы и CPU
//!
//! Счётчики — сэмплированные тики. `TICK_HZ == USER_HZ == 1000`
//! (`arch/x86_64/apic.rs`; в `src/vfs/procfs.rs` есть compile-time assertion),
//! поэтому сырые значения уже те числа, что ждёт `/proc/stat` Linux. Машина
//! однопроцессорная (`AGENTS.md`, инвариант 3): три счётчика — это набор одного
//! CPU, и рендерер печатает те же суммы и в агрегатной строке `cpu`, и в
//! `cpu0`, как Linux на однопроцессорной машине.
//!
//! ## Консистентность снимка
//!
//! Тик обновляет одну корзину под seqlock ([`SEQ`]), поэтому [`cpu_ticks`]
//! возвращает снимок, чьи три поля принадлежат одному набору завершённых тиков.
//! Это сохраняет осмысленность `user + system + idle`; каждое поле монотонно
//! само по себе и так. Путь тика без локов и аллокаций: три операции над
//! `AtomicU64` на 1000 Гц.
//!
//! Модуль только `core` и самодостаточен, поэтому `host-tests` включает этот
//! точный исходник (`task_stat`) и проверяет свойствами [`classify_tick`].

use core::sync::atomic::{AtomicU64, Ordering};

/// `USER_HZ` Linux / частота тика этого ядра. Рендерер ничего не делит: один
/// тик — одна единица `USER_HZ`.
pub const USER_HZ: u64 = 1000;

/// К какой корзине относится один тик таймера.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TickClass {
    /// Ring 3: CPU исполнял пользовательскую программу.
    User,
    /// Ring 0, но не idle: работа ядра (syscall'ы, драйверы, шелл, …).
    System,
    /// idle-задача или ожидание `hlt`: запускаемой работы нет.
    Idle,
}

/// Классифицировать один тик таймера по прерванному фрейму (`должно быть
/// значением, сохранённым CPU, а не мнением планировщика о следующей задаче`).
///
/// Сначала user: ring-3 фрейм не может быть idle-задачей или внутри `hlt`
/// (`hlt` привилегирован). Из оставшихся ring-0 фреймов idle-задача и явное
/// ожидание `hlt` — два способа этого ядра сказать «запускать нечего».
#[inline]
pub fn classify_tick(interrupted_cs: u64, idle_task: bool, in_halt: bool) -> TickClass {
    if interrupted_cs & 3 == 3 && interrupted_cs != 0 {
        TickClass::User
    } else if idle_task || in_halt {
        TickClass::Idle
    } else {
        TickClass::System
    }
}

static USER_TICKS: AtomicU64 = AtomicU64::new(0);
static SYSTEM_TICKS: AtomicU64 = AtomicU64::new(0);
static IDLE_TICKS: AtomicU64 = AtomicU64::new(0);
/// Нечётно, пока тик засчитывается, чётно — когда счётчики согласованы.
static SEQ: AtomicU64 = AtomicU64::new(0);

static CONTEXT_SWITCHES: AtomicU64 = AtomicU64::new(0);
static PROCESSES: AtomicU64 = AtomicU64::new(0);
static BOOT_UNIX: AtomicU64 = AtomicU64::new(0);

/// Согласованный снимок трёх временных корзин в тиках (`USER_HZ`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTicks {
    pub user: u64,
    pub system: u64,
    pub idle: u64,
}

/// Засчитать один прошедший тик ровно одной корзине. Вызывается только
/// обработчиком тика таймера; только атомики, без локов и аллокаций.
#[inline]
pub fn charge(class: TickClass) {
    SEQ.fetch_add(1, Ordering::Relaxed);
    match class {
        TickClass::User => USER_TICKS.fetch_add(1, Ordering::Relaxed),
        TickClass::System => SYSTEM_TICKS.fetch_add(1, Ordering::Relaxed),
        TickClass::Idle => IDLE_TICKS.fetch_add(1, Ordering::Relaxed),
    };
    // Release: приращение корзины выше видно до того, как читатель увидит чётный
    // номер последовательности, завершающий критическую секцию.
    SEQ.fetch_add(1, Ordering::Release);
}

/// Атомарно прочитать все три корзины из одного набора завершённых тиков.
pub fn cpu_ticks() -> CpuTicks {
    loop {
        let seq = SEQ.load(Ordering::Acquire);
        if seq & 1 == 0 {
            let user = USER_TICKS.load(Ordering::Relaxed);
            let system = SYSTEM_TICKS.load(Ordering::Relaxed);
            let idle = IDLE_TICKS.load(Ordering::Relaxed);
            if SEQ.load(Ordering::Acquire) == seq {
                return CpuTicks { user, system, idle };
            }
        }
        // Тик прервал читателя между полем и завершающей проверкой: повтор.
        // CPU один, поэтому писатель не может выполняться одновременно с этим
        // циклом — повтор достижим только через окно вытеснения.
        core::hint::spin_loop();
    }
}

/// Засчитать одно переключение контекста (вот-вот начнёт выполняться другая задача).
#[inline]
pub fn count_context_switch() {
    CONTEXT_SWITCHES.fetch_add(1, Ordering::Relaxed);
}

/// Число переключений контекста с загрузки.
#[inline]
pub fn context_switches() -> u64 {
    CONTEXT_SWITCHES.load(Ordering::Relaxed)
}

/// Засчитать одну новую задачу. Поле `processes` в Linux — это `total_forks`,
/// инкрементируемый при каждом `copy_process` (включая потоки и ядерные
/// потоки); аналог в этом ядре — `scheduler::spawn`.
#[inline]
pub fn count_process() {
    PROCESSES.fetch_add(1, Ordering::Relaxed);
}

/// Число созданных задач с загрузки (счётчик в стиле Linux `total_forks`).
#[inline]
pub fn processes() -> u64 {
    PROCESSES.load(Ordering::Relaxed)
}

/// Записать время загрузки по стенным часам (Unix-секунды), прочитанное с RTC
/// при загрузке. `0` означает, что RTC нечитаем/до эпохи; тогда `/proc/stat`
/// печатает `btime 0` — документированный пробел, а не выдуманная дата.
pub fn set_boot_unix(secs: u64) {
    BOOT_UNIX.store(secs, Ordering::Relaxed);
}

/// Время загрузки по стенным часам в Unix-секундах или `0`, если у RTC не было
/// валидного времени.
#[inline]
pub fn boot_unix() -> u64 {
    BOOT_UNIX.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_order() {
        assert_eq!(classify_tick(0x1b, false, false), TickClass::User);
        assert_eq!(classify_tick(0x1b, true, true), TickClass::User);
        assert_eq!(classify_tick(0x08, true, false), TickClass::Idle);
        assert_eq!(classify_tick(0x08, false, true), TickClass::Idle);
        assert_eq!(classify_tick(0x08, false, false), TickClass::System);
    }
}
