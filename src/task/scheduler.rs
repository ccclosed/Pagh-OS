use crate::memory::{pmm, vmm};
use crate::sync::spinlock::Spinlock;
use crate::task::as_refs;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::structures::paging::PageTableFlags;

/// Блок управления задачей (TCB), сведённый ровно к состоянию, из которого
/// восстанавливается переключение контекста на RSP (Requirement 11.3).
/// Рудиментарные поля `rip`/`rflags`/`regs`/signal удалены: точка входа и
/// начальные значения регистров закодированы во фрейме стека ядра, который
/// строит `kernel_thread_spawn`, а не хранятся здесь.
///
/// Состояние совместимости с Linux здесь НЕ носится: планировщик пересобирает
/// `Tcb` из `current_rsp` на каждом тике, поэтому `CompatState` в куче на `Tcb`
/// никогда не смог бы остаться авторитетным. Реестр по pid в `task::compat`
/// (`COMPAT_STATES`) — единственный источник правды для работающего
/// Compat_Process. `Tcb` перемещается по очередям планировщика по значению, что
/// существующие вызывающие уже делают.
/// Человекочитаемые имена ядерных потоков (pid -> имя), для `ps`.
///
/// `Tcb` планировщика намеренно не несёт имени: это побайтно точное состояние,
/// которое восстанавливает переключение контекста, и добавление полей туда
/// меняет несущую структуру. `ps` всё равно должен показывать, что машина что-то
/// выполняет — сессия шелла без Linux-процессов раньше печатала голый заголовок,
/// что читается как «ядро вообще не видит процессов». Боковая таблица,
/// наполняемая [`kernel_thread_spawn_named`], разделяет две задачи: путь
/// переключения отсюда никогда не читает.
static TASK_NAMES: Spinlock<alloc::collections::BTreeMap<u64, &'static str>> =
    Spinlock::new(alloc::collections::BTreeMap::new());

/// Записать/обновить отображаемое имя задачи.
pub fn set_task_name(pid: u64, name: &'static str) {
    TASK_NAMES.lock().insert(pid, name);
}

/// Забыть отображаемое имя задачи (вызывается при удалении её фрейма).
pub fn forget_task_name(pid: u64) {
    TASK_NAMES.lock().remove(&pid);
}

/// Все именованные ядерные задачи как `(pid, name)`, по возрастанию pid.
pub fn task_names_snapshot() -> alloc::vec::Vec<(u64, &'static str)> {
    TASK_NAMES.lock().iter().map(|(p, n)| (*p, *n)).collect()
}

/// Отображаемое имя `pid`, если оно есть.
pub fn task_name(pid: u64) -> Option<&'static str> {
    TASK_NAMES.lock().get(&pid).copied()
}

pub struct Tcb {
    pub pid: u64,
    /// Единственное состояние, из которого восстанавливает переключение: сохранённый стек ядра.
    pub kernel_rsp: u64,
    /// Физический адрес PML4 этой задачи (перезагружается в CR3 при переключении).
    pub cr3: u64,
}

impl Tcb {
    /// Построить готовую задачу. Точка входа не хранится в `Tcb`; она зашита в
    /// построенный фрейм стека ядра, на который указывает `kernel_rsp` (см.
    /// `kernel_thread_spawn`).
    pub fn new(pid: u64, kernel_rsp: u64, cr3: u64) -> Self {
        Tcb {
            pid,
            kernel_rsp,
            cr3,
        }
    }
}

static READY_QUEUE: Spinlock<VecDeque<Tcb>> = Spinlock::new(VecDeque::new());

// Реестр фреймов — ловит устаревшие/двойные восстановления в момент их
// появления, а не на фатальном iretq (apt #GP: iretq потребил область стека pid 1,
// где больше не было сохранённого фрейма). Каждое сохранение ставит штамп
// (pid -> rsp, live); каждое восстановление обязано найти соответствующий live-
// штамп и потребить его.
static FRAME_LEDGER: Spinlock<alloc::collections::BTreeMap<u64, (u64, bool)>> =
    Spinlock::new(alloc::collections::BTreeMap::new());
static KFRAME_ERRS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn stamp_save(pid: u64, rsp: u64) {
    FRAME_LEDGER.lock().insert(pid, (rsp, true));
}

fn stamp_restore(pid: u64, rsp: u64) {
    let prev = {
        let mut led = FRAME_LEDGER.lock();
        let prev = led.get(&pid).copied();
        led.insert(pid, (rsp, false));
        prev
    };
    if let Some((saved, live)) = prev {
        if saved != rsp {
            crate::error!(
                "[SCHED] STALE RESTORE pid={} rsp=0x{:x} but last saved rsp=0x{:x} (live={})",
                pid,
                rsp,
                saved,
                live
            );
        } else if !live {
            crate::error!(
                "[SCHED] DOUBLE RESTORE pid={} rsp=0x{:x} (frame already consumed once)",
                pid,
                rsp
            );
        }
    }
}
static NEXT_PID: AtomicU64 = AtomicU64::new(1);
static TICK_COUNT: AtomicU64 = AtomicU64::new(0);
static CURRENT_PID: Spinlock<u64> = Spinlock::new(0);

/// Pid'ы, запросившие выход, но ещё не снятые тиком.
///
/// Планировщик хранит текущую задачу только как `CURRENT_PID` и пересобирает её
/// `Tcb` из `current_rsp` на каждом тике, поэтому «убить» задачу — значит
/// записать pid, который обработчик тика НЕ должен возвращать в очередь. Это
/// *множество*, а не один слот: две задачи могут запросить выход между тиками, и
/// однослотовый сентинел дал бы второй записи стереть первую — оставив ту задачу
/// навсегда в цикле halt (неубиваемый зомби). Когда тик собирается вернуть
/// текущую задачу в очередь, он удаляет pid из множества: удаление и проверяет,
/// и очищает флаг, поэтому каждый ожидающий выход соблюдается ровно один раз
/// (Requirement 12.4).
static EXITING_PIDS: Spinlock<BTreeSet<u64>> = Spinlock::new(BTreeSet::new());

/// Пометить `pid` как выходящий: следующий тик, увидевший его текущим, снимает
/// его с ротации вместо возврата в очередь.
fn mark_exiting(pid: u64) {
    EXITING_PIDS.lock().insert(pid);
}

/// Атомарно проверить и очистить метку выхода `pid`. Возвращает `true`, когда
/// pid ожидал выхода (и вызывающий обязан снять его с ротации).
fn take_exiting(pid: u64) -> bool {
    EXITING_PIDS.lock().remove(&pid)
}

/// Pid'ы с ожидающим запросом STOP: когда фрейм этой задачи в следующий раз
/// сохраняется для решения о requeue — тик таймера или кооперативный
/// `yield_switch`, — фрейм паркуется вместо возврата в ротацию.
///
/// Остановка — действие времени *доставки* (Linux: «сама остановка ... выполняется
/// как действие сигнала для `SIG_DFL`»), а возобновление `SIGCONT` выполняется на
/// времени *генерации* отправителем. Поэтому остановленную задачу может
/// припарковать только один из двух путей, сохраняющих фрейм; сигнальный код
/// сам этого сделать не может, потому что сохранённый RSP задачи существует
/// только в ready-очереди, и его удаление потеряло бы задачу.
static STOP_REQUESTED: Spinlock<BTreeSet<u64>> = Spinlock::new(BTreeSet::new());

/// Фреймы задач, припаркованных в состоянии остановки: `pid -> (сохранённый
/// RSP ядра, cr3)`.
///
/// Остановленная задача НЕ снимается — для этого есть [`EXITING_PIDS`]. Её
/// сохранённый фрейм уходит из ready-очереди в эту карту, чтобы `SIGCONT` мог
/// вернуть его с точным instruction pointer, stack pointer и адресным
/// пространством.
///
/// Инварианты:
///   * pid находится не более чем в ОДНОМ из {ready-очередь, `STOPPED_TASKS`,
///     `EXITING_PIDS`} — три взаимоисключающих состояния ротации;
///   * парковка проверяет фрейм (`check_frame`) и ставит штамп реестра
///     (`stamp_save`), как `requeue`, поэтому возобновлённый фрейм снова
///     проверяется обычным путём восстановления;
///   * адресное пространство запоминается на припаркованную задачу, потому что
///     reaper выхода обязан использовать ИМЕННО этот cr3 (см. [`pend_reap_with`]).
static STOPPED_TASKS: Spinlock<BTreeMap<u64, (u64, u64)>> = Spinlock::new(BTreeMap::new());

/// Попросить припарковать `pid` при следующем сохранении его фрейма. Идемпотентно.
pub fn mark_stop_requested(pid: u64) {
    STOP_REQUESTED.lock().insert(pid);
}

/// Атомарно проверить и очистить запрос остановки `pid`.
fn take_stop_requested(pid: u64) -> bool {
    STOP_REQUESTED.lock().remove(&pid)
}

/// Припаркован ли `pid` сейчас в состоянии остановки?
pub fn is_stopped(pid: u64) -> bool {
    STOPPED_TASKS.lock().contains_key(&pid)
}

/// Припарковать только что сохранённый фрейм задачи: проверить + поставить
/// штамп, затем увести его с ротации в [`STOPPED_TASKS`].
fn park_stopped(pid: u64, rsp: u64, cr3: u64) {
    // Запрос, приведший сюда — и любой запрос, всё ещё стоящий для задачи, чей
    // фрейм уже припарковал другой вызывающий (`stop_ready_pids`), — этим
    // парком удовлетворён: удаляем его, иначе остаток заново припарковал бы задачу
    // на следующем тике ПОСЛЕ того, как SIGCONT её возобновил (задача выглядела
    // бы остановленной, будучи работающей).
    STOP_REQUESTED.lock().remove(&pid);
    check_frame("park-stopped", pid, rsp);
    stamp_save(pid, rsp);
    STOPPED_TASKS.lock().insert(pid, (rsp, cr3));
}

/// Припарковать задачи, чьи фреймы УЖЕ сохранены: вычерпать их из
/// ready-очереди в [`STOPPED_TASKS`]. Используется, чтобы остановить каждый поток
/// группы, кроме текущего (получатель паркуется сам в точке доставки).
///
/// Только один порядок локов (READY_QUEUE → STOPPED_TASKS), и первый лок
/// маскирует прерывания, поэтому тик не увидит фрейм ни в одном из двух мест.
/// Возвращает число припаркованных задач.
pub fn stop_ready_pids(pids: &[u64]) -> usize {
    let mut frames: alloc::vec::Vec<(u64, u64, u64)> = alloc::vec::Vec::new();
    {
        let mut q = READY_QUEUE.lock();
        let mut i = 0;
        while i < q.len() {
            if pids.contains(&q[i].pid) {
                if let Some(t) = q.remove(i) {
                    frames.push((t.pid, t.kernel_rsp, t.cr3));
                }
            } else {
                i += 1;
            }
        }
    }
    let n = frames.len();
    for (pid, rsp, cr3) in frames {
        park_stopped(pid, rsp, cr3);
    }
    n
}

/// Вернуть припаркованную задачу в ротацию с её точным сохранённым фреймом,
/// проверенным и со штампом обычного `requeue`. Возвращает `false`, когда `pid`
/// не был припаркован (нечего возобновлять), так что `SIGCONT` работающему
/// процессу — no-op.
pub fn resume_stopped(pid: u64) -> bool {
    // Возобновлённая задача не остановлена: ещё не исполненный запрос остановки
    // не должен парковать её на следующем тике (POSIX: SIGCONT отбрасывает
    // ожидающие stop-сигналы).
    STOP_REQUESTED.lock().remove(&pid);
    let Some((rsp, cr3)) = STOPPED_TASKS.lock().remove(&pid) else {
        return false;
    };
    requeue(Tcb {
        pid,
        kernel_rsp: rsp,
        cr3,
    });
    true
}

/// Забыть запрос остановки, который перекрыло внешнее событие (задачу убивают
/// или тест отменил остановку до того, как тик успел её исполнить). У множества
/// запросов нет другого потребителя: никогда не исполненный запрос припарковал бы
/// задачу на следующем тике, хотя причина остановки уже исчезла.
pub fn cancel_stop_request(pid: u64) {
    STOP_REQUESTED.lock().remove(&pid);
}

/// Удалить и вернуть сохранённый фрейм припаркованной задачи. Вызывающий
/// завершает задачу: фрейм больше никогда не восстановится, поэтому собрать его
/// нужно с возвращённым здесь cr3 (никогда с текущим загруженным).
fn take_stopped_frame(pid: u64) -> Option<(u64, u64)> {
    STOPPED_TASKS.lock().remove(&pid)
}

/// Отложенная работа очистки выхода для одной снятой задачи (реестр reap).
struct ExitReap {
    /// Адресное пространство, в котором работала задача (`current_pml4_phys()`,
    /// снят тиком, который её снял). Освобождается здесь, когда это *приватный*
    /// пользовательский PML4 — то есть не PML4 ядра и не разделённый с другой
    /// задачей.
    cr3: u64,
}

/// Pid'ы, чей выход исполнен тиком, но чья память ещё не возвращена. Задача
/// записывается сюда тем самым тиком, который её снимает — а он всё ещё
/// выполняется НА стеке ядра этой задачи, поэтому кадры стека можно освободить
/// только из более позднего, постороннего контекста. За один тик
/// [`reap_exited_tasks`] обрабатывает одну запись, ограничивая окно с
/// выключенными прерываниями.
static PENDING_REAPS: Spinlock<BTreeMap<u64, ExitReap>> = Spinlock::new(BTreeMap::new());

/// Поставить память снятой задачи в очередь на возврат и отпустить её штамп
/// реестра фреймов (её сохранённый фрейм больше не восстановится). Вызывается из
/// тика, снимающего задачу.
fn pend_reap(pid: u64) {
    pend_reap_with(pid, vmm::current_pml4_phys());
}

/// Поставить память НЕ-текущей задачи в очередь на возврат вместе с адресным
/// пространством, в котором она реально работала.
///
/// `current_pml4_phys()` из [`pend_reap`] корректен только для задачи, которую
/// снимает текущий тик, потому что вызов идёт на стеке самой снимаемой задачи.
/// Припаркованная (остановленная) задача, принудительно убитая снаружи, НЕ
/// текущая, поэтому повторное использование текущего CR3 заставило бы reaper
/// проверить — и, возможно, освободить — чужое адресное пространство, а
/// собственный пользовательский PML4 припаркованной задачи утёк бы. Штамп фрейма
/// снимается здесь по той же причине, что и в [`pend_reap`].
fn pend_reap_with(pid: u64, cr3: u64) {
    PENDING_REAPS.lock().insert(pid, ExitReap { cr3 });
    FRAME_LEDGER.lock().remove(&pid);
    // Задача вне ротации: удалить и её отображаемое имя, иначе `ps` продолжал бы
    // перечислять pid, которого больше нет.
    forget_task_name(pid);
}

/// Снять ТЕКУЩУЮ выполняющуюся задачу прямо сейчас (тик решил, что она больше не
/// должна выполняться: доставленный тиком фатальный сигнал или `rt_sigframe`,
/// который не удалось разместить).
///
/// Этот контекст всё ещё выполняется НА стеке ядра той задачи, поэтому память
/// может освободить только reaper более позднего тика ([`pend_reap`] снимает
/// адресное пространство, загруженное СЕЙЧАС — здесь это корректно, потому что
/// CR3 работающей задачи принадлежит ей). Метки ротации, которые мог поставить
/// путь убийства, очищаются: их потребляет только тик, видящий pid как CURRENT,
/// чего с этой задачей больше не случится, — остаток застрял бы в `EXITING_PIDS`
/// навсегда.
pub fn drop_current_now() {
    let pid = current_pid();
    let _ = take_exiting(pid);
    STOP_REQUESTED.lock().remove(&pid);
    pend_reap(pid);
}

/// Вернуть ресурсы ОДНОЙ снятой задачи (сначала самую старую): снять отображение
/// и освободить кадры её стека ядра, освободить её приватное пользовательское
/// адресное пространство (листовые кадры, промежуточные таблицы, PML4) и удалить
/// запись реестра фреймов.
///
/// Идёт в контексте тика таймера с замаскированными прерываниями, поэтому
/// обрабатывает не более одного pid за вызов. Освобождаемый стек всегда
/// принадлежит задаче, снятой на БОЛЕЕ РАННЕМ тике — сам обработчик тика
/// выполняется на стеке какой-то живой задачи, — поэтому ничего используемого не
/// освобождается; pid, на чьём стеке этот обработчик сейчас работает (только что
/// снятая задача, если есть), пропускается.
///
/// Пользовательское адресное пространство освобождается только при эксклюзивном
/// владении: пропускается, если PML4 — ядерный, всё ещё установлен в CR3 (потоки
/// того же процесса), на него ссылается другая задача в очереди или он разделён
/// с другим ожидающим reap (освобождает последний уходящий поток).
fn reap_exited_tasks() {
    let cur = current_pid();
    let Some((pid, cr3)) = PENDING_REAPS
        .lock()
        .iter()
        .find(|(p, _)| **p != cur)
        .map(|(k, v)| (*k, v.cr3))
    else {
        return;
    };

    // Стек ядра: реально отображённые страницы слота pid (guard-страница — нет).
    // Он есть и у ядерных потоков, и у процессов.
    let (_guard, stack_base, _top) = crate::memory::layout::kernel_stack_for_pid(pid);
    for page in 0..crate::memory::layout::KERNEL_STACK_PAGES {
        let vaddr = stack_base + page * crate::memory::layout::PAGE_SIZE;
        if let Some(phys) = vmm::virt_to_phys(vaddr) {
            if vmm::unmap(vaddr).is_ok() {
                pmm::free_frame(phys);
            }
        }
    }

    // Приватное пользовательское адресное пространство.
    if cr3 != vmm::kernel_pml4_phys() && cr3 != vmm::current_pml4_phys() {
        // Тот же учёт, что в `drop_exclusive_user_space`: живая задача в ЛЮБОМ
        // реестре держит пространство живым (припаркованный SIGSTOP поток-сосед
        // раньше был невидим и здесь и оставался со снятыми таблицами страниц), а
        // задача, сама ожидающая reap, запрещает второе освобождение.
        if let Some((other, registry)) = cr3_referenced_by(cr3, pid) {
            crate::trace!(
                "[SCHED] reap pid={}: user space 0x{:x} still used by {} task {}",
                pid,
                cr3,
                registry.name(),
                other
            );
        } else {
            vmm::drop_user_space(cr3);
        }
    }

    // Освободить FXSAVE-область задачи.
    crate::task::fpu::free(pid);

    PENDING_REAPS.lock().remove(&pid);
}

/// PID, зарезервированный за idle-задачей.
///
/// Idle-задача — основной поток загрузки: она исполняет `kernel_main` и
/// заканчивает циклом halt, всегда готова к запуску. Планировщик трактует её как
/// явную задачу (Requirement 11.4), а не разбрасывает проверки
/// `CURRENT_PID == 0` по обработчику тика. Она планируется, когда ready-очередь
/// пуста.
pub const IDLE_PID: u64 = 0;

/// Idle-задача, явно представленная настоящим `Tcb`. Во время работы обновляется
/// только `kernel_rsp` (сохраняется, когда idle-задачу вытесняет тик таймера);
/// `pid`/`cr3` фиксированы. Доступ только через хелперы ниже, чтобы у понятия
/// «idle-задача» был один владелец.
static IDLE_TASK: Spinlock<Tcb> = Spinlock::new(Tcb {
    pid: IDLE_PID,
    kernel_rsp: 0,
    cr3: 0,
});

/// `true`, когда `pid` — idle-задача.
#[inline]
pub fn is_idle(pid: u64) -> bool {
    pid == IDLE_PID
}

/// Сохранить указатель стека idle-задачи (вызывается при её вытеснении).
#[inline]
fn save_idle_rsp(rsp: u64) {
    stamp_save(IDLE_PID, rsp);
    IDLE_TASK.lock().kernel_rsp = rsp;
}

/// Сохранённый указатель стека idle-задачи (планируется, когда больше нечего).
#[inline]
fn idle_rsp() -> u64 {
    IDLE_TASK.lock().kernel_rsp
}

pub fn init() {
    crate::debug!("Scheduler initialized (Round Robin)");
}
pub fn tick() {
    TICK_COUNT.fetch_add(1, Ordering::Relaxed);
}
pub fn ticks() -> u64 {
    TICK_COUNT.load(Ordering::Relaxed)
}

/// Заблокировать вызывающий поток примерно на `n` тиков таймера, засыпая между
/// тиками вместо busy-spin, чтобы не морить другие задачи (и CPU).
///
/// При текущей частоте LAPIC ([`crate::arch::x86_64::apic::TICK_HZ`]) это
/// ~`n / TICK_HZ` секунд. У вызывающего ОБЯЗАНЫ быть включены прерывания (у
/// потока шелла они включены); иначе счётчик тиков не растёт, и ожидание станет
/// вечным. Ожидание переживает (астрономическое) переполнение счётчика тиков
/// через `wrapping`/сравнение прошедшего.
pub fn sleep_ticks(n: u64) {
    let start = ticks();
    while ticks().wrapping_sub(start) < n {
        // `hlt` с замаскированным IF спит вечно (разбудит только NMI), а окно
        // syscall Linux ВХОДИТ с очищенным IF из-за SFMASK / шлюза прерывания.
        // Включаем прерывания перед остановкой — та же схема, что в
        // `exit_current`. Быть вытесненным здесь нормально.
        crate::arch::cpu::enable_interrupts();
        crate::arch::cpu::halt();
    }
}
/// Отладочная растяжка: проверить, что сохранённый фрейм контекста по `rsp`
/// выглядит как каноническая 21-словная раскладка ([+128] RIP канонический,
/// [+136] CS правдоподобный, [+144] RFLAGS с всегда-единичным битом), и, если
/// нет, напечатать его. Вызывается и при постановке фрейма в очередь, и прямо
/// перед восстановлением, поэтому порча фрейма ловится в момент появления, а не
/// на фатальном `iretq`.
pub fn check_frame(who: &str, pid: u64, rsp: u64) {
    if rsp == 0 {
        return;
    }
    // Растяжка не должна падать на собственном входе: испорченный фрейм может
    // нести произвольный rsp, а selftest'ы ставят в очередь синтетические Tcb с
    // сентинел-указателями стека (0x8000/0xDEAD). Проверяем только фреймы в
    // канонической верхней половине; всё прочее сообщается как есть.
    if (rsp as i64) >= 0 || rsp < 0xffff_8000_0000_0000 {
        crate::warn!(
            "[SCHED] BAD FRAME ({}) pid={} rsp=0x{:x}: non-canonical or user-space stack pointer, not probed",
            who,
            pid,
            rsp
        );
        return;
    }
    // Ядерные потоки (шелл, idle и т. д.) используют ядерный yield-фрейм, чья
    // раскладка отличается от ring-3 фрейма прерывания: тройки user CS/SS/RSP по
    // [+136..+160] нет, поэтому значение по [+136] — просто ядерный RSP, а не
    // селектор сегмента. Проверка его на ring-3 инварианты всегда даёт ложный BAD
    // FRAME, а затем iretq взрывается на этом RSP-как-CS (GP#err=rsp&~3). Гоняем
    // проверку только для задач, у которых реально есть compat-состояние Linux.
    let is_compat = crate::task::compat::compat_exists(pid);
    let rd = |off: u64| unsafe { core::ptr::read_volatile((rsp + off) as *const u64) };
    let rip = rd(128);
    let cs = rd(136);
    let rf = rd(144);
    let rip_canonical = (((rip as i64) << 16) >> 16) as u64 == rip;
    let rf_ok = rf & 0x2 != 0;
    // Ядерные потоки ВСЕГДА должны нести селектор кода ядра по [+136] и RIP
    // верхней половины; значение указателя стека в слоте CS — ровно подпись
    // apt iretq #GP (CS=0x...081238 -> #GP err=0x1238).
    let kcs = crate::arch::x86_64::gdt::Selectors::kernel_code().0 as u64;
    // Ring-3 фрейм (CS с RPL=3, правдоподобный селектор) валиден даже для задач
    // БЕЗ compat-состояния Linux — встроенный ring-3 тестовый процесс (pid 3)
    // создаётся «сырым», и его совершенно здоровый фрейм раньше засорял BAD FRAME
    // на каждом тике. Настоящая растяжка (значение указателя стека в слоте CS)
    // всё равно срабатывает: такие значения огромны и проваливают cs < 0x40.
    let ring3 = cs & 3 == 3 && cs != 0 && cs < 0x40;
    let cs_ok = if is_compat {
        cs != 0 && cs < 0x40
    } else {
        cs == kcs || ring3
    };
    let rip_ok = if is_compat || ring3 {
        rip_canonical
    } else {
        rip_canonical && rip >= 0xffff_8000_0000_0000
    };
    if !(rip_ok && cs_ok && rf_ok) {
        if !is_compat && KFRAME_ERRS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) >= 8 {
            return;
        }
        crate::error!(
            "[SCHED] BAD FRAME ({}) pid={} rsp=0x{:x} rip=0x{:x} cs=0x{:x} rflags=0x{:x}",
            who,
            pid,
            rsp,
            rip,
            cs,
            rf
        );
        let mut i = 0u64;
        while i < 21 {
            crate::error!("  frame[+0x{:02x}] 0x{:016x}", i * 8, rd(i * 8));
            i += 1;
        }
    }
}

pub fn spawn(tcb: Tcb) -> u64 {
    let pid = tcb.pid;
    let mut q = READY_QUEUE.lock();
    if q.iter().any(|t| t.pid == pid) {
        // Второй фрейм для pid, уже стоящего в очереди, НЕЛЬЗЯ класть: дубликат
        // прокручивал бы задачу дважды за раунд (каждая постановка исполняет свой
        // устаревший фрейм) и бесконечно рос бы в очереди. Вернуть pid
        // вызывающему, не ставя в очередь.
        crate::error!(
            "[SCHED] DOUBLE ENQUEUE (spawn) pid={} rsp=0x{:x} - duplicate not enqueued",
            pid,
            tcb.kernel_rsp
        );
        return pid;
    }
    check_frame("spawn", pid, tcb.kernel_rsp);
    stamp_save(pid, tcb.kernel_rsp);
    // Каждая задача получает FXSAVE-область сразу: будет ли она когда-либо
    // исполнять FP-код, на момент spawn неизвестно, а пути save/restore не могут
    // аллоцировать (они работают в контексте IRQ/тика).
    crate::task::fpu::area_for(pid);
    q.push_back(tcb);
    // `processes` в `/proc/stat`: каждая успешно поставленная в очередь новая
    // задача, то есть аналог Linux `total_forks` (он тоже считает потоки и
    // ядерные потоки, ведь все проходят через `copy_process`).
    crate::task::stat::count_process();
    pid
}
pub fn schedule() -> Option<Tcb> {
    READY_QUEUE.lock().pop_front()
}
pub fn requeue(tcb: Tcb) {
    let mut q = READY_QUEUE.lock();
    if q.iter().any(|t| t.pid == tcb.pid) {
        // См. `spawn`: дубликат, уже стоящий в очереди, отбрасывается, а не
        // кладётся. Этот фрейм бросается (он не штамповался, поэтому реестр
        // остаётся согласован с фреймом, уже стоящим в очереди).
        crate::error!(
            "[SCHED] DOUBLE ENQUEUE pid={} rsp=0x{:x} - duplicate dropped",
            tcb.pid,
            tcb.kernel_rsp
        );
        return;
    }
    check_frame("enqueue", tcb.pid, tcb.kernel_rsp);
    stamp_save(tcb.pid, tcb.kernel_rsp);
    q.push_back(tcb);
}
pub fn remove_ready_pids(pids: &[u64]) {
    READY_QUEUE.lock().retain(|t| !pids.contains(&t.pid));
}
pub fn next_pid() -> u64 {
    NEXT_PID.fetch_add(1, Ordering::Relaxed)
}
pub fn set_current_pid(pid: u64) {
    *CURRENT_PID.lock() = pid;
}
pub fn current_pid() -> u64 {
    *CURRENT_PID.lock()
}

/// Число задач, которые могут выполняться прямо сейчас: ready-очередь плюс
/// текущая задача, если это не idle-задача (Linux `procs_running` не считает
/// idle). Остановленные задачи припаркованы вне очереди и исключены, как
/// состояние stopped в Linux. `/proc/stat` рендерит это как `procs_running`.
///
/// Ненадолго берёт лок ready-очереди; вызывающие — обычный контекст
/// ядра/syscall, никогда обработчик тика (он должен оставаться безлоковым, кроме
/// собственных операций планировщика с очередью).
pub fn ready_count() -> u64 {
    let queued = READY_QUEUE.lock().len() as u64;
    queued + if is_idle(current_pid()) { 0 } else { 1 }
}

/// Счётчики живых задач плюс последний выданный pid, для `/proc/loadavg`.
///
/// `runnable` — [`ready_count`] (ready-очередь + текущая задача, idle исключён).
/// `total` добавляет припаркованные/остановленные задачи: в этом планировщике
/// нет sleeping или uninterruptible состояния — блокирующее ожидание уступает CPU
/// и остаётся runnable, а остановленная задача припаркована в [`STOPPED_TASKS`],
/// а не снята, — поэтому среди живых задач это единственные, кого нет в ready-
/// наборе. Pid'ы в [`EXITING_PIDS`] уже мертвы (ждут сбора) и не считаются.
///
/// `last_pid` выводится из счётчика pid: [`next_pid`] возвращает старое значение
/// и инкрементирует, поэтому наибольший выданный pid — `NEXT_PID - 1`, а `0`
/// означает, что pid ещё не выдавались.
///
/// Порядок локов как в остальном планировщике: `READY_QUEUE` (внутри
/// [`ready_count`]) отпускается до взятия `STOPPED_TASKS`, поэтому единственный
/// документированный порядок (`READY_QUEUE → STOPPED_TASKS`) соблюдён.
/// Вызывается из обычного контекста syscall/ядра (рендерер procfs), никогда из
/// тика.
pub fn loadavg_counts() -> (u64, u64, u64) {
    let runnable = ready_count();
    let stopped = STOPPED_TASKS.lock().len() as u64;
    let last_pid = NEXT_PID.load(Ordering::Relaxed).saturating_sub(1);
    (runnable, runnable + stopped, last_pid)
}
/// Экспоненциально затухающие средние нагрузки за 1/5/15 минут, fixed point
/// 16.16.
///
/// Средняя нагрузка Linux — `load = load * exp(-dt/window) + runnable *
/// (1 - exp(-dt/window))`. FPU-контекста в планировщике нет, поэтому та же
/// рекуррентность считается в целочисленном fixed point при каждом чтении
/// `/proc/loadavg` (Linux тоже обновляет лениво, из контекста таймера).
/// Интервал сэмплирования — дельта тиков стенного времени между чтениями;
/// долгий разрыв просто затухает к текущему runnable-счёту, что и есть верный
/// предел. `loadavg_counts` берёт локы ready/stopped, поэтому это должно
/// выполняться в обычном контексте ядра/syscall — никогда в обработчике тика.
static LOADAVG: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static LOADAVG_LAST_TICK: AtomicU64 = AtomicU64::new(0);
/// Окна в секундах: 1, 5 и 15 минут.
const LOADAVG_WINDOWS: [u64; 3] = [60, 300, 900];
const LOADAVG_ONE: u64 = 1 << 16;

/// Текущие средние нагрузки за 1/5/15 минут в сотых (`123` = `1.23`).
pub fn load_averages() -> (u32, u32, u32) {
    let now = ticks();
    let last = LOADAVG_LAST_TICK.swap(now, Ordering::Relaxed);
    let (runnable, _total, _last_pid) = loadavg_counts();
    let dt = now.saturating_sub(last);
    let tick_hz = crate::arch::x86_64::apic::TICK_HZ;
    if dt > 0 && tick_hz > 0 {
        for (index, seconds) in LOADAVG_WINDOWS.iter().enumerate() {
            let window = tick_hz.saturating_mul(*seconds);
            if window == 0 {
                continue;
            }
            let old = LOADAVG[index].load(Ordering::Relaxed);
            let (decay, add) = if dt >= window {
                // Прошло больше одного полного окна: старое значение полностью
                // затухло, начинаем от текущего runnable-счёта.
                (0, LOADAVG_ONE)
            } else {
                let decay = LOADAVG_ONE.saturating_sub(dt.saturating_mul(LOADAVG_ONE) / window);
                let add = runnable.saturating_mul(LOADAVG_ONE).saturating_mul(dt) / window;
                (decay, add)
            };
            let new = old.saturating_mul(decay) / LOADAVG_ONE + add;
            LOADAVG[index].store(new, Ordering::Relaxed);
        }
    }
    let hundredths = |index: usize| {
        let value = LOADAVG[index].load(Ordering::Relaxed);
        ((value.saturating_mul(100)) / LOADAVG_ONE).min(u32::MAX as u64) as u32
    };
    (hundredths(0), hundredths(1), hundredths(2))
}

fn activate_task(pid: u64) {
    if is_idle(pid) {
        return;
    }
    let top = crate::memory::layout::kernel_stack_for_pid(pid).2;
    crate::arch::x86_64::gdt::set_kernel_stack(top);
    crate::arch::x86_64::syscall::set_syscall_kernel_stack(top);
    if let Some(base) = crate::task::compat::fs_base_for(pid) {
        x86_64::registers::model_specific::FsBase::write(x86_64::VirtAddr::new(base));
    }
}

pub fn kernel_thread_spawn(entry: fn()) -> u64 {
    kernel_thread_spawn_named(entry, "kernel")
}

/// Запустить ядерный поток и запомнить отображаемое имя (см. [`TASK_NAMES`]).
pub fn kernel_thread_spawn_named(entry: fn(), name: &'static str) -> u64 {
    let pid = next_pid();
    set_task_name(pid, name);
    let (_guard_base, stack_base, stack_top) = crate::memory::layout::kernel_stack_for_pid(pid);

    for page in 0..crate::memory::layout::KERNEL_STACK_PAGES {
        let vaddr = stack_base + page * crate::memory::layout::PAGE_SIZE;
        let frame = pmm::alloc_frame().expect("SCHED: PMM OOM");
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
        vmm::map(frame, vaddr, flags).expect("SCHED: VMM map fail");
    }

    let kernel_cs = crate::arch::x86_64::gdt::Selectors::kernel_code().0 as u64;
    let kernel_ss = crate::arch::x86_64::gdt::Selectors::kernel_data().0 as u64;

    unsafe {
        // Собрать начальный фрейм ядерного потока так, чтобы его байтовая
        // раскладка ТОЧНО совпадала с порядком, в котором
        // `irq32_stub`/`scheduler_tick_irq` восстанавливают регистры
        // (Requirement 11.1 / Property 7). Вытесняющий путь восстановления при
        // заданном новом RSP делает:
        //     mov rsp, new_rsp
        //     popfq                       ; снять слово RFLAGS для popfq
        //     pop r15; pop r14; ...; pop rax   ; 15 GPR (r15 первым, rax последним)
        //     iretq                       ; RIP, CS, RFLAGS, RSP, SS (long mode: всегда 5)
        //
        // Фрейм строим от СТАРШЕГО адреса вниз, поэтому итоговый (младший) RSP —
        // это то, с чего начинает снимать путь восстановления. Регистр `entry`
        // кладётся в слот *rdi*: после 15 pop'ов GPR rdi == entry, а `iretq`
        // ставит RSP = stack_top. Трамплин затем идёт с rdi = entry и чистым
        // стеком и просто `call rdi` (без pop). См. `kernel_thread_trampoline` в
        // switch.rs.
        //
        // Раскладка фрейма (младший адрес = итоговый kernel_rsp → старший):
        //   [kernel_rsp+0]   RFLAGS-для-popfq
        //   [+8]   r15   [+16]  r14   [+24]  r13   [+32]  r12   [+40]  r11
        //   [+48]  r10   [+56]  r9    [+64]  r8    [+72]  rbp   [+80]  rdi = entry
        //   [+88]  rsi   [+96]  rdx   [+104] rcx   [+112] rbx   [+120] rax
        //   [+128] RIP = трамплин  [+136] CS  [+144] RFLAGS  [+152] RSP  [+160] SS
        let mut rsp = stack_top;

        // Long-mode IRETQ ВСЕГДА снимает пять слов — RIP, CS, RFLAGS, RSP, SS —
        // даже при возврате на том же уровне привилегий (ring0 -> ring0).
        // Синтетический ring0-фрейм обязан поэтому давать и слова RSP/SS;
        // 3-словный фрейм заставлял iretq читать за stack_top в неотображённую
        // guard-область и падать page fault'ом на самом первом восстановлении
        // потока.
        rsp -= 8;
        (rsp as *mut u64).write(kernel_ss); // [+160] SS
        rsp -= 8;
        (rsp as *mut u64).write(stack_top); // [+152] RSP after iretq
        rsp -= 8;
        (rsp as *mut u64).write(0x202u64); // [+144] RFLAGS (IF set)
        rsp -= 8;
        (rsp as *mut u64).write(kernel_cs); // [+136] CS

        let trampoline = crate::task::switch::kernel_thread_trampoline as *const () as u64;
        rsp -= 8;
        (rsp as *mut u64).write(trampoline); // [+128] RIP -> trampoline

        // ── 15 слотов GPR, пишутся от старших к младшим под порядок pop ────
        // Старшие→младшие адреса соответствуют: rax (старший, снимается
        // последним) вниз до r15 (младший, снимается первым). `entry` идёт в слот
        // rdi.
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+120] rax
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+112] rbx
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+104] rcx
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+96]  rdx
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+88]  rsi
        rsp -= 8;
        (rsp as *mut u64).write(entry as u64); // [+80]  rdi = entry
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+72]  rbp
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+64]  r8
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+56]  r9
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+48]  r10
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+40]  r11
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+32]  r12
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+24]  r13
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+16]  r14
        rsp -= 8;
        (rsp as *mut u64).write(0); // [+8]   r15

        // ── Слово RFLAGS, снимаемое `popfq` (младший адрес = итоговый rsp) ──
        rsp -= 8;
        (rsp as *mut u64).write(0x002u64); // [+0] RFLAGS-for-popfq (IF=0 invariant)

        let tcb = Tcb {
            pid,
            kernel_rsp: rsp,
            cr3: vmm::current_pml4_phys(),
        };
        spawn(tcb);
        pid
    }
}

#[no_mangle]
pub extern "C" fn scheduler_tick_irq(current_rsp: u64) -> u64 {
    let tick = TICK_COUNT.fetch_add(1, Ordering::Relaxed);

    if tick % 100 == 0 {
        crate::trace!("Tick {} RSP=0x{:x}", tick, current_rsp);
    }

    let cur = current_pid();

    if (current_rsp & (1 << 47)) == 0 {
        crate::error!("[SCHED] RSP in user space! RSP=0x{:x}", current_rsp);
        crate::arch::cpu::halt_loop();
    }

    // Учёт тиков `/proc/stat`: засчитать ЭТОТ тик ровно одной корзине до любой
    // работы с сигналами/requeue, которая может переписать фрейм. Ring берётся
    // из прерванного CS, положенного CPU (смещение +136, `trap_frame::IrqFrame`),
    // а не из выбора следующей задачи планировщиком; `idle_task`/`in_halt` — два
    // способа этого ядра сказать «ничего не выполнялось». Ниже ничего не
    // засчитывается повторно — один `charge` на прерывание таймера.
    //
    // SAFETY: `current_rsp` — фрейм, который `irq32_stub` только что положил для
    // текущей задачи (168 байт на её приватном стеке ядра); проверка выше
    // доказала, что это указатель верхней половины, а прерывания замаскированы на
    // весь обработчик, поэтому фрейм нельзя заменить под нами.
    let interrupted_cs =
        unsafe { (*(current_rsp as *const crate::arch::x86_64::linux::trap_frame::IrqFrame)).cs };
    crate::task::stat::charge(crate::task::stat::classify_tick(
        interrupted_cs,
        is_idle(cur),
        crate::arch::cpu::in_halt(),
    ));

    if is_idle(cur) {
        // Idle-задача была вытеснена: сохранить указатель её стека в явной
        // idle-задаче, а не трактовать pid 0 как магический особый случай.
        save_idle_rsp(current_rsp);
    } else if take_exiting(cur) {
        // Текущая задача запросила выход (Requirement 12.4): НЕ возвращать её в
        // очередь, чтобы снять с ротации и больше не планировать. Память здесь
        // освободить нельзя — обработчик выполняется на стеке ядра этой задачи, —
        // поэтому она ставится в очередь reaper'а более позднего тика.
        crate::trace!("[SCHED] task {} exited", cur);
        pend_reap(cur);
    } else {
        // Сохранить FPU/SSE-состояние уходящей задачи до того, как его
        // перезапишет восстановление входящей ниже.
        crate::task::fpu::save_if_user(cur, current_rsp);
        let cr3 = vmm::current_pml4_phys();
        // Доставка сигнала с пути тика: ПОСЛЕДНИЙ шанс для задачи, которая
        // никогда не входит в syscall (CPU-bound цикл), увидеть сигнал. Доставка
        // идёт ЗДЕСЬ — до решения о requeue, на фрейме ТЕКУЩЕЙ задачи, с её
        // загруженным CR3, — чтобы вернувшийся в очередь фрейм был тем же, с
        // которым войдут в обработчик. Раскладка фрейма читается через
        // `trap_frame::IrqFrame`, никогда через приведение к syscall
        // `SavedRegs` (см. тот модуль).
        let tick = crate::arch::x86_64::linux::signal::tick_action(current_rsp);
        if tick == crate::arch::x86_64::linux::signal::TickAction::Kill {
            // Тик убил эту задачу (фатальное действие по умолчанию или её
            // rt_sigframe не удалось разместить): снять сейчас, с её собственным
            // адресным пространством, и никогда не возвращать в очередь.
            crate::trace!("[SCHED] task {} killed by tick-delivered signal", cur);
            drop_current_now();
        } else if take_stop_requested(cur) {
            // Задаче доставлен stop-сигнал (с пути тика выше или с пути возврата
            // из syscall, который уступил CPU): припарковать фрейм вместо возврата
            // в очередь. Фрейм остаётся в STOPPED_TASKS до SIGCONT, поэтому задача
            // возобновляется ровно в этой точке.
            crate::trace!("[SCHED] task {} parked (stopped)", cur);
            park_stopped(cur, current_rsp, cr3);
        } else {
            requeue(Tcb {
                pid: cur,
                kernel_rsp: current_rsp,
                cr3,
            });
        }
    }

    // Освободить память ранее снятых задач (сначала старых, не больше одной за
    // тик, чтобы ограничить окно с выключенными прерываниями). Идёт после решения
    // о requeue выше: pid, снятый ЭТИМ тиком (если есть), находится на стеке, на
    // котором мы исполняемся, и reaper его пропускает.
    reap_exited_tasks();

    crate::arch::x86_64::apic::send_eoi();

    let next = match schedule() {
        Some(tcb) => tcb,
        None => {
            // Ничего не готово: запустить idle-задачу.
            set_current_pid(IDLE_PID);
            if !is_idle(cur) {
                crate::task::stat::count_context_switch();
            }
            let rsp = idle_rsp();
            check_frame("restore-idle", IDLE_PID, rsp);
            stamp_restore(IDLE_PID, rsp);
            return rsp;
        }
    };

    // Переключение считается, только когда возобновляется ДРУГАЯ задача: при
    // одной готовой задаче round-robin снимает саму прерванную задачу, что
    // сохраняет и восстанавливает фрейм, но не является сменой контекста (Linux
    // `ctxt` тоже считает `nr_switches`, а не тики). `/proc/stat` рендерит
    // счётчик.
    if next.pid != cur {
        crate::task::stat::count_context_switch();
    }

    set_current_pid(next.pid);
    activate_task(next.pid);

    // Единая централизованная перезагрузка CR3 для вытесняющего пути
    // (Requirement 11.5). Делегирует `vmm::load_cr3` — ЕДИНСТВЕННОМУ месту,
    // которое пишет CR3 при переключении. Перезагрузка заодно сбрасывает TLB,
    // чтобы перезагрузились страницы стека следующей задачи. Никакое другое место
    // на этом пути CR3 не трогает.
    unsafe {
        vmm::load_cr3(next.cr3);
    }

    // Вернуть FPU/SSE-состояние входящей задачи до её возобновления.
    crate::task::fpu::restore_if_user(next.pid, next.kernel_rsp);

    check_frame("restore-tick", next.pid, next.kernel_rsp);
    stamp_restore(next.pid, next.kernel_rsp);
    next.kernel_rsp
}

/// Кооперативно уступить CPU следующей готовой задаче (если есть).
///
/// Сохранение/восстановление фрейма живёт в `switch::yield_switch`; ротация
/// очереди — в [`scheduler_yield_switch`]. Критический порядок
/// requeue-до-восстановления описан в обоих.
pub fn yield_current() {
    // Каждый блокирующий цикл в ядре сходится сюда, поэтому сами уступающие
    // задачи двигают скан watchdog'а зависших syscall'ов.
    crate::arch::x86_64::linux::watchdog_tick();
    // SAFETY: yield_switch сохраняет полный контекст этой задачи в канонической
    // раскладке фрейма, ставит её в очередь через `scheduler_yield_switch` и лишь
    // затем переключает стеки; позже задача возобновляется любым путём
    // восстановления.
    unsafe {
        crate::task::switch::yield_switch();
    }
}

/// Половина планировщика в кооперативной уступке. Вызывается из asm
/// `yield_switch` с замаскированными прерываниями; `current_rsp` — только что
/// сохранённый фрейм вызывающего (каноническая раскладка, младшее слово = RFLAGS
/// для popfq).
///
/// КРИТИЧЕСКИЙ ПОРЯДОК: уступающую задачу нужно поставить в очередь ЗДЕСЬ — между
/// сохранением фрейма и переключением стека, — потому что любой код после
/// переключения выполнится, лишь когда задачу уже перепланировали, чего не может
/// случиться, если она не попала в очередь.
///
/// Возвращает сохранённый RSP следующей задачи или `current_rsp` без изменений,
/// когда ready-очередь пуста (тогда восстанавливается собственный фрейм
/// вызывающего и уступка — no-op).
pub extern "C" fn scheduler_yield_switch(current_rsp: u64) -> u64 {
    let cur = current_pid();

    let next = match schedule() {
        Some(tcb) => tcb,
        // Больше нечего: возобновить собственный фрейм вызывающего — ЕСЛИ только
        // эта уступка не приводит в действие stop-сигнал. Остановленная задача не
        // должна восстанавливаться собственной уступкой (её фрейм теперь в
        // STOPPED_TASKS, и восстановление здесь выполнило бы задачу и оставило
        // дубликат фрейма), поэтому отдаём CPU idle-задаче ровно как путь тика с
        // пустой очередью.
        None => {
            if !is_idle(cur) && take_stop_requested(cur) {
                crate::task::fpu::save_if_user(cur, current_rsp);
                crate::trace!("[SCHED] task {} parked (stopped, empty queue)", cur);
                park_stopped(cur, current_rsp, vmm::current_pml4_phys());
                set_current_pid(IDLE_PID);
                // Остановленная задача отдала CPU idle-задаче: настоящее
                // переключение, считается как откат к idle в пути тика.
                crate::task::stat::count_context_switch();
                let rsp = idle_rsp();
                check_frame("restore-yield-idle", IDLE_PID, rsp);
                stamp_restore(IDLE_PID, rsp);
                return rsp;
            }
            return current_rsp;
        }
    };

    // Сохранить FPU/SSE-состояние уступающей задачи до того, как его перезапишет
    // восстановление входящей ниже. Compat-задача уступает внутри syscall, и её
    // пользовательское FP-состояние ещё живо в CPU — ядерный CS фрейма не значит,
    // что состоянием владеет ядро.
    crate::task::fpu::save_if_user(cur, current_rsp);

    // Поставить уступающую задачу в очередь ДО переключения стека (см. doc выше).
    // Idle-задача никогда не ставится в очередь: она паркует фрейм в выделенном
    // слоте, зеркаля вытесняющий путь. Запрос остановки паркует фрейм вместо
    // возврата в очередь (см. `STOPPED_TASKS`).
    if is_idle(cur) {
        save_idle_rsp(current_rsp);
    } else if take_stop_requested(cur) {
        crate::trace!("[SCHED] task {} parked (stopped)", cur);
        park_stopped(cur, current_rsp, vmm::current_pml4_phys());
    } else {
        requeue(Tcb {
            pid: cur,
            kernel_rsp: current_rsp,
            cr3: vmm::current_pml4_phys(),
        });
    }

    // Считать настоящее переключение задача-задача (счётчик, который сообщает
    // `/proc/stat`); уступка, возобновляющая собственный фрейм вызывающего, — нет.
    if next.pid != cur {
        crate::task::stat::count_context_switch();
    }

    set_current_pid(next.pid);
    activate_task(next.pid);

    // Централизованная перезагрузка CR3 для кооперативного пути
    // (Requirement 11.5): уступка перезагружает CR3 через тот же хелпер
    // `vmm::load_cr3`, что и тик, поэтому CR3 пишется ровно в одном месте. В этом
    // пути CR3 больше нигде не переписывается.
    unsafe {
        vmm::load_cr3(next.cr3);
    }

    // Вернуть FPU/SSE-состояние входящей задачи до её возобновления.
    crate::task::fpu::restore_if_user(next.pid, next.kernel_rsp);

    check_frame("restore-yield", next.pid, next.kernel_rsp);
    stamp_restore(next.pid, next.kernel_rsp);
    next.kernel_rsp
}

/// Завершить вызывающую задачу и навсегда уступить планировщику.
///
/// Requirement 12.4: `SYS_EXIT` должен завершить *вызывающую задачу*, а
/// планировщик продолжает другие, — он НЕ должен останавливать весь CPU. При
/// планировщике на RSP (не держащем постоянного `Tcb` текущей задачи) минимальный
/// надёжный механизм таков:
///
///   1. Записать текущий pid в [`EXITING_PIDS`].
///   2. Крутиться в цикле halt с **включёнными** прерываниями, чтобы
///      периодический тик мог нас вытеснить.
///   3. На следующем тике `scheduler_tick_irq` удалит `cur` из [`EXITING_PIDS`],
///      снимает задачу вместо возврата в очередь и переключится на следующую
///      готовую. Поскольку задача никогда не возвращается в очередь, управление
///      сюда не вернётся — отсюда тип возврата `-> !`.
///
/// Прерывания в цикле ОБЯЗАНЫ оставаться включёнными, иначе таймер не сработает и
/// задача (и CPU) заблокируются.
///
/// Idle-задача (`IDLE_PID`) никогда не является настоящей завершаемой задачей;
/// если `exit_current` всё же достигнут на ней, откатываемся к полному halt-циклу,
/// а не убираем всегда-готовую idle-задачу из ротации.
/// Убить задачу снаружи (^C на foreground-программе): пометить её как exiting,
/// чтобы следующий тик снял её вместо возврата в очередь, и сразу снести её
/// Linux-compat-состояние, чтобы ожидающие (`lxrun` foreground-цикл) увидели её
/// исчезновение. Сама задача ничего особенного не замечает — её следующая
/// блокирующая уступка не вернётся.
///
/// ПРИПАРКОВАННАЯ (остановленная) задача — исключение, и здесь закрыты два
/// дефекта: `EXITING_PIDS` потребляется тиком только для задачи, которая CURRENT,
/// а припаркованная задача больше никогда не станет текущей — метка застряла бы
/// там навсегда, пока фрейм стоит припаркованным. Поэтому её сохранённый фрейм
/// здесь вынимается из `STOPPED_TASKS` и ставится напрямую в очередь reaper'а — с
/// тем cr3, с которым она была припаркована (`pend_reap_with`), никогда с текущим
/// загруженным.
pub fn request_exit(pid: u64) {
    crate::task::compat::remove_compat(pid);
    if let Some((_rsp, cr3)) = take_stopped_frame(pid) {
        STOP_REQUESTED.lock().remove(&pid);
        crate::trace!("[SCHED] parked task {} killed", pid);
        pend_reap_with(pid, cr3);
        return;
    }
    mark_exiting(pid);
}

pub fn exit_current() -> ! {
    let pid = current_pid();
    if is_idle(pid) {
        crate::arch::cpu::halt_loop();
    }

    // Снести Linux-compat-состояние этого процесса. Для нативных задач (нет
    // зарегистрированного состояния) это no-op; для Compat_Process — освобождает
    // FdTable/VmRegionSet/набор nosys, чтобы реестр не тёк между выходами.
    crate::task::compat::finish_compat_exit(pid);

    mark_exiting(pid);

    // Ждать вытеснения и снятия. Держать прерывания включёнными, чтобы тик мог
    // сработать; как только тик снимет нас, мы больше не планируемся.
    loop {
        crate::arch::cpu::enable_interrupts();
        crate::arch::cpu::halt();
    }
}

/// Ядерный поток, чья функция входа вернулась, припарковался в
/// `switch::scheduler_exit_thread`. Пометить его exiting, чтобы следующий тик снял
/// его с ротации вместо возврата уже завершённого фрейма в очередь (иначе
/// припаркованный поток вечно сжигал один idle-слайс за раунд планирования).
pub fn kernel_thread_finished() {
    let pid = current_pid();
    if !is_idle(pid) {
        mark_exiting(pid);
    }
}

/// Освободить дерево фреймов за `cr3`, когда ТЕКУЩАЯ задача — его эксклюзивный
/// владелец: это не PML4 ядра, он не разделён ни с одной живой задачей (потоки
/// делят CR3: ready-очередь, припаркованные SIGSTOP задачи и ожидающие reap
/// выходы) и не упомянут в ожидающем reap выхода. COW-листья внутри делают unref
/// (`drop_user_space`), поэтому exec'нувшийся форк-ребёнок освобождает
/// разделяемые кадры родителя, а не вечно их держит. Возвращает `true`, если
/// освободил.
///
/// Используется путём execve сразу после переключения на свежезагруженный образ.
///
/// Предыдущая версия смотрела только `READY_QUEUE` и `PENDING_REAPS`, поэтому
/// поток-сосед, припаркованный SIGSTOP/SIGTSTP (`STOPPED_TASKS`, вне
/// ready-очереди до SIGCONT), был невидим, и его разделяемое адресное
/// пространство освобождалось под ним — page-table use-after-free, ждущий
/// SIGCONT. Теперь опрашиваются все реестры. `exclude_pid` — вызывающий (его
/// собственный Tcb не зарегистрирован, пока он работает).
pub fn drop_exclusive_user_space(cr3: u64) -> bool {
    if cr3 == 0 || cr3 == vmm::kernel_pml4_phys() {
        return false;
    }
    let me = current_pid();
    if let Some((pid, registry)) = cr3_referenced_by(cr3, me) {
        // Редко по построению (execve из CLONE_VM-группы потоков или припаркованный
        // SIGSTOP сосед): сказать, ПОЧЕМУ старый образ НЕ освобождён, а не оставлять
        // молчаливое `freed=false` в строке execve.
        crate::warn!(
            "[SCHED] user space 0x{:x} still referenced by {} task {} - not freeing (execve keeps it alive)",
            cr3,
            registry.name(),
            pid
        );
        return false;
    }
    vmm::drop_user_space(cr3);
    true
}

/// Первая живая задача (кроме `exclude_pid`), всё ещё работающая на `cr3`, и
/// реестр планировщика, где она найдена. Берёт три лока реестров ПО ОДНОМУ, в
/// документированном порядке (`READY_QUEUE` → `STOPPED_TASKS` → `PENDING_REAPS`),
/// потому что спинлоки планировщика нереентерабельны и маскируют прерывания.
///
/// «Заблокированные» задачи из этих реестров НЕ выпадают: syscall, припаркованный
/// в `nanosleep`/`futex`/`read`, кооперативно уступает и остаётся в ready-
/// очереди. Два состояния, действительно покидающих очередь, — припаркованные
/// SIGSTOP задачи (`STOPPED_TASKS`) и снятые, но ещё не собранные
/// (`PENDING_REAPS`); оба должны блокировать снос.
fn cr3_referenced_by(cr3: u64, exclude_pid: u64) -> Option<(u64, as_refs::Registry)> {
    use as_refs::{AsRef, Registry};
    {
        let q = READY_QUEUE.lock();
        if let Some(pid) = as_refs::referencing_pid(
            cr3,
            exclude_pid,
            q.iter().map(|t| AsRef {
                pid: t.pid,
                cr3: t.cr3,
            }),
        ) {
            return Some((pid, Registry::Ready));
        }
    }
    {
        let s = STOPPED_TASKS.lock();
        if let Some(pid) = as_refs::referencing_pid(
            cr3,
            exclude_pid,
            s.iter().map(|(pid, (_rsp, cr3))| AsRef {
                pid: *pid,
                cr3: *cr3,
            }),
        ) {
            return Some((pid, Registry::Stopped));
        }
    }
    let r = PENDING_REAPS.lock();
    as_refs::referencing_pid(
        cr3,
        exclude_pid,
        r.iter().map(|(pid, reap)| AsRef {
            pid: *pid,
            cr3: reap.cr3,
        }),
    )
    .map(|pid| (pid, Registry::PendingReap))
}
