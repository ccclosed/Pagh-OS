//! Механизм доставки сигналов — ядерный клей между чистым ABI фрейма
//! ([`super::signal_frame`]), чистой моделью аргументов/errno `kill(2)`
//! ([`super::kill`]), per-process сигнальным состоянием в
//! [`crate::task::compat`] и путём входа syscall.
//!
//! ## Где доставляются сигналы
//!
//! Доставка происходит ровно в ОДНОЙ точке: в хвосте
//! [`super::linux_dispatch`], после того как `Result` syscall'а сложен, но до
//! того как входной стаб запишет возвращаемое значение и снимет фрейм
//! `SavedRegs`. Ожидающий незаблокированный сигнал с пользовательским
//! обработчиком получает `rt_sigframe` на пользовательском стеке, а сохранённые
//! регистры переписываются так, что `sysretq` попадает в обработчик; сигнал с
//! завершением по умолчанию убивает процесс прямо здесь. Блокирующие ожидания в
//! ядре прерываются проверками `EINTR`, которые циклы ожидания выполняют через
//! [`has_deliverable_current`].
//!
//! Доставка также бывает на пути возврата тика (`tick_action`, для CPU-bound
//! задачи, никогда не входящей в syscall), а stop/continue — настоящее состояние
//! планировщика (остановленная задача сохраняет фрейм и уходит из ротации),
//! поэтому единственное оставшееся ограничение — сигнал наблюдается на границе
//! syscall или тика, никогда посреди вычисления в ядре.
//!
//! ## Отправка: `kill(2)`, `tgkill` и групповая рассылка
//!
//! [`send_signal`] ставит сигнал ОДНОМУ потоку. [`sys_kill`] (`kill`, nr 62)
//! декодирует аргументы `pid_t`/`int`, классифицирует цель (`pid > 0`, `0`, `-1`,
//! `-pgid`) через [`super::kill`], сводит её к СНИМКУ pid и ставит сигнал один раз
//! на адресованную группу процессов (`kill::pick_group_target`) — это то, чего
//! Linux достигает общим group-pending списком. [`send_signal_to_pgid_all`] —
//! вариант на каждую ГРУППУ ПОТОКОВ, нужный путям stop/continue терминала и
//! шелла (групповой stop паркует одну группу потоков за раз). Решения errno
//! (EINVAL до любого поиска, `ESRCH` для `INT_MIN` и пустых групп, `EPERM`,
//! когда ни одну цель нельзя сигнализировать по effective uid вызывающего)
//! документированы в `kill::kill_args`.
//!
//! ## Предположение о пути: compat ⇒ `syscall_entry`
//!
//! Доставка сигналов идёт только для Compat_Process (у нативных задач нет
//! сигнального состояния), а Compat_Process входит в syscall исключительно через
//! стаб инструкции `syscall`. На этом пути per-task слот user-RSP по
//! `SavedRegs + 120` хранит живой user RSP (тот же слот, который уже читают/пишут
//! `execve` и `clone`), сохранённый `rcx` — user RIP, а сохранённый `r11` — user
//! RFLAGS. У compat-процесса, вошедшего через `int 0x80`, эти слоты значат
//! другое — положенный CPU user RIP по `+120` — поэтому фрейм никогда не
//! интерпретируется вслепую. Это обеспечивают две проверки по порядку:
//!
//!   1. **Точная**: `linux_dispatch` записывает каждый вход `int 0x80` на
//!      процесс (`linux::INT80_ENTRY`, `compat::current_int80_entry`). Процесс,
//!      замеченный на этом пути, фрейма не получает: сигнал потребляется,
//!      печатается одна диагностика `[signal] ... refused: int 0x80 entry`, а
//!      фатальный по умолчанию сигнал исполняется через
//!      [`force_terminate_group`] вместо записи в память пользователя.
//!   2. **Последняя линия**: [`frame_carries_syscall_view`] отвергает фрейм, чей
//!      сохранённый `r11` не имеет бита 1 RFLAGS (архитектурно всегда
//!      установленного) — вход `syscall` не может дать такой фрейм, поэтому
//!      неизвестный путь входа деградирует в «сигнал потреблён, ничего не
//!      записано», а не в испорченное адресное пространство пользователя.
//!
//! ## Кратко о контракте регистров
//!
//! * Доставка: `RSP=frame`, `RIP=sa_handler`, `RDI=signo`, `RSI=&siginfo`,
//!   `RDX=&ucontext`, `RAX=результат syscall`, чистый пользовательский RFLAGS.
//!   `sigcontext` хранит `cx = ip` и `r11 = flags` (clobber инструкции `syscall`
//!   — идентично представлению доставки на возврате из syscall в Linux).
//! * `rt_sigreturn`: GPR mcontext восстанавливаются во фрейм `SavedRegs`;
//!   `rcx ← sigcontext.ip` (RIP sysret), `r11 ← sigcontext.flags`, слот
//!   `+120 ← sigcontext.sp`, а возвращаемое функцией значение становится
//!   восстановленным `rax` (стаб пишет возврат диспетчера в слот `rax` после
//!   нашего возврата).
#![allow(dead_code)]

use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use super::check_user_ptr;
use super::errno::Errno;
use super::regs::SavedRegs;
use super::signal_frame::{
    decode_rt_sigframe, default_action, encode_rt_sigframe, frame_location, is_user_handler,
    DefaultAction, RestoredFrame, RT_SIGFRAME_SIZE, SIGINFO_OFFSET, SIGKILL, SIG_DFL, SIG_IGN,
    UC_OFFSET, UNBLOCKABLE_MASK, USER_RFLAGS,
};
use super::trap_frame::{plan_irq_delivery, IrqFrame};

use crate::task::compat;
use crate::task::scheduler;

/// Консервативный счётчик поставленных в очередь сигналов по всем процессам.
/// Проверка доставки на КАЖДОМ возврате из syscall — `load(==0) → пропуск`, чистое
/// атомарное чтение; значение может завышать (снятые pending, вышедшие процессы),
/// но никогда не занижает реально доставимый сигнал, поэтому оптимистичный пропуск
/// всегда безопасен.
static PENDING_APPROX: AtomicU64 = AtomicU64::new(0);

// ─── Отправка ────────────────────────────────────────────────────────────────

/// Поставить сигнал `sig` процессу `pid` (примитив на один поток):
///
/// * `sig == 0`: проба существования (семантика `kill(pid, 0)`).
/// * `SIGKILL`: немедленное внешнее убийство — compat-состояние цели сносится с
///   кодом выхода `128 + 9` для `wait4`, а задача помечается exiting (она заметит
///   на следующем тике/уступке). ПРИПАРКОВАННАЯ (остановленная) задача снимается
///   из `STOPPED_TASKS` и собирается со своим cr3 (см.
///   `scheduler::request_exit`).
/// * всё остальное: добавляется в pending-битсет цели и доставляется на её
///   следующем возврате из syscall (или проверками `EINTR` её блокирующих
///   ожиданий).
///
/// ## Магия stop/continue происходит ЗДЕСЬ, на времени генерации
///
/// Linux делит job control на две половины (`prepare_signal`): *stop* выполняется
/// как действие сигнала при доставке, а *continue* — сразу при генерации
/// `SIGCONT`, «независимо от блокировки, игнорирования или обработки», потому что
/// у остановленной задачи нет контекста исполнения, который мог бы увидеть
/// pending-бит. Поэтому функция для ГРУППЫ ПОТОКОВ цели выполняет:
///
/// * `SIGCONT`: возобновляет каждого припаркованного члена
///   ([`scheduler::resume_stopped`]) и отбрасывает все ожидающие биты класса
///   остановки (POSIX 2.4.1). Безусловно — заблокированный или `SIG_IGN`-нутый
///   `SIGCONT` всё равно возобновляет; уже потом доставка самого сигнала идёт по
///   обычным правилам маски/диспозиции.
/// * stop-сигнал (`SIGSTOP`/`SIGTSTP`/`SIGTTIN`/`SIGTTOU`): отбрасывает ожидающий
///   `SIGCONT` (POSIX: генерация stop-сигнала его отбрасывает), а если группа
///   УЖЕ остановлена, потребляет сигнал, не ставя его в очередь — иначе
///   возобновление тут же снова остановило бы группу.
///
/// Pending-сигналы per-thread: `pid` адресуется точно. Групповые вызовы
/// (`kill(0|-1|-pgid)`) идут через [`sys_kill`], который сводит группу к одному
/// потоку на группу перед вызовом (Linux ставит групповой сигнал в общий
/// pending-список группы, поэтому обработчик выполняется раз на группу).
pub fn send_signal(pid: u64, sig: u64) -> Result<(), Errno> {
    if sig == 0 {
        return if compat::compat_exists(pid) {
            Ok(())
        } else {
            Err(Errno::ESRCH)
        };
    }
    if sig > super::signal_frame::SIGNAL_COUNT as u64 {
        return Err(Errno::EINVAL);
    }
    if !compat::compat_exists(pid) {
        return Err(Errno::ESRCH);
    }
    if sig == SIGKILL {
        force_terminate_group(pid, SIGKILL);
        return Ok(());
    }
    // ── эффекты stop/continue на времени генерации (см. doc выше) ──────────
    let tgid = compat::tgid_of(pid);
    if sig == super::signal_frame::SIGCONT {
        resume_group(tgid);
    } else if super::signal_frame::is_stop_signal(sig) {
        for member in compat::group_pids(tgid) {
            compat::clear_cont_pending(member);
        }
        if group_is_stopped(tgid) {
            crate::info!(
                "[signal] pid={} sig={} ignored: thread group {} is already stopped",
                pid,
                sig,
                tgid
            );
            return Ok(());
        }
    }
    if !compat::set_pending(pid, sig) {
        return Err(Errno::ESRCH);
    }
    PENDING_APPROX.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Есть ли среди членов группы потоков `tgid` припаркованные в состоянии
/// остановки?
fn group_is_stopped(tgid: u64) -> bool {
    compat::group_pids(tgid)
        .into_iter()
        .any(scheduler::is_stopped)
}

/// Генерация `SIGCONT` для группы потоков `tgid`: отбросить каждый ожидающий бит
/// класса остановки и вернуть каждого припаркованного члена в ротацию.
///
/// Один снимок реестра, затем вызовы планировщика по членам — `COMPAT_STATES`
/// никогда не держится между ними (AGENTS.md, инвариант 4).
fn resume_group(tgid: u64) {
    for member in compat::group_pids(tgid) {
        compat::clear_stop_pending(member);
        // Снять снимок состояния парковки ДО возобновления: только член,
        // который действительно был припаркован, даёт отчёт WCONTINUED (SIGCONT
        // работающему процессу просто отбрасывает stop-биты).
        let was_parked = scheduler::is_stopped(member);
        if scheduler::resume_stopped(member) && was_parked {
            compat::note_child_continued(member);
            // Часы зависшего syscall перезапускаются: пауза не должна считаться
            // «застреванием в syscall» для watchdog'а (см. `watchdog_tick`).
            super::inflight_refresh(member);
            crate::info!("[signal] pid={} resumed by SIGCONT", member);
        }
    }
}

/// Применить ДОСТАВЛЕННЫЙ stop-сигнал с диспозицией `SIG_DFL`: останавливается вся
/// группа потоков (групповой stop Linux).
///
/// Получатель паркуется в ЭТОЙ точке доставки — pending-бит уже потреблён
/// вызывающим, — а члены, чьи фреймы уже сохранены, немедленно уводятся из
/// ротации. `mark_stop_requested` заставляет следующее решение о requeue (эта
/// уступка или тик, сработавший первым) перевести фрейм в `STOPPED_TASKS`; в ring 3
/// с ожидающим stop мы не возвращаемся никогда.
fn begin_group_stop(sig: u64) {
    let pid = scheduler::current_pid();
    let tgid = compat::tgid_of(pid);
    let siblings: alloc::vec::Vec<u64> = compat::group_pids(tgid)
        .into_iter()
        .filter(|m| *m != pid)
        .collect();
    let parked = scheduler::stop_ready_pids(&siblings);
    compat::note_child_stopped(pid, sig);
    crate::info!(
        "[signal] pid={} sig={} stopping thread group {} ({} sibling(s) parked now, receiver parks at this return point)",
        pid,
        sig,
        tgid,
        parked
    );
    scheduler::mark_stop_requested(pid);
}

/// Доставка stop-сигнала на возврате из syscall: пометить группу остановленной,
/// затем припарковать получателя уступкой CPU — решение о requeue внутри
/// `yield_switch` видит запрос остановки и паркует этот фрейм вместо ротации.
fn stop_current_group(sig: u64) {
    begin_group_stop(sig);
    scheduler::yield_current();
}

/// Внешнее убийство группы потоков, которой принадлежит `pid`, с записью статуса
/// выхода `128 + sig` для `wait4` (обычный `request_exit` снёс бы compat-состояние,
/// так и не вставив запись зомби).
///
/// Публичный двойник пути отправителя SIGKILL для вызывающих в стиле `tgkill`,
/// адресующих не себя.
pub fn terminate_group(pid: u64, sig: u64) {
    force_terminate_group(pid, sig);
}

/// Установлен ли пользовательский обработчик для `sig` в группе потоков `pid` (а
/// не вызывающего)? `true`/`false`, когда у `pid` есть compat-состояние.
pub fn has_handler_for(pid: u64, sig: u64) -> bool {
    compat::with_pid_compat(pid, |cs| {
        is_user_handler(&cs.sig.lock().handlers[(sig - 1) as usize])
    })
    .unwrap_or(false)
}

/// Внешнее убийство группы потоков, которой принадлежит `pid`, с записью статуса
/// выхода `128 + sig` для `wait4` (обычный `request_exit` снёс бы compat-состояние,
/// так и не вставив запись зомби).
fn force_terminate_group(pid: u64, sig: u64) {
    let tgid = compat::tgid_of(pid);
    for member in compat::group_member_pids(tgid, pid) {
        scheduler::request_exit(member);
    }
    let code = (128u64 + sig) as u8;
    compat::set_exit_code_of(pid, code);
    // Удаляет compat-состояние и вставляет зомби wait4 с записанным выше кодом.
    compat::finish_compat_exit(pid);
    // Теперь no-op-удаление + фактическая пометка exiting.
    scheduler::request_exit(pid);
}

// ─── kill(2) ─────────────────────────────────────────────────────────────────

/// `kill(2)` (62): послать `sig` процессу, группе процессов или каждому процессу,
/// которому вызывающий имеет право сигнализировать.
///
/// Чистая половина живёт в [`super::kill`]: декодирование аргументов (оба
/// параметра — C `int`, поэтому усекаются до 32 бит и знаково расширяются — от
/// этого зависит `kill(-pgid)`), классификация цели и матрица errno. См.
/// `kill::kill_args`, почему каждая ошибка возвращается там, где возвращается:
/// `EINVAL` для неверного сигнала ДО любого поиска цели, `ESRCH` для `INT_MIN` и
/// группы без живых членов, `EPERM`, когда effective uid вызывающего не может
/// сигнализировать ни одной из найденных целей.
///
/// Локи (AGENTS.md, инвариант 4): [`resolve_kill_targets`] берёт один короткий
/// снимок реестра и отпускает его; отправки идут после, никогда под
/// `COMPAT_STATES` (каждый `send_signal` сам входит в реестр).
/// Послать `sig` группе процессов `pgid`, поставив его один раз на её лидера.
pub fn send_signal_to_pgid(pgid: u64, sig: u64) -> Result<(), Errno> {
    let targets = super::kill::pick_group_target(&compat::group_pids(pgid), pgid);
    match targets {
        Some(leader) => send_signal(leader, sig),
        None => Err(Errno::ESRCH),
    }
}

/// Послать `sig` КАЖДОЙ группе потоков в группе процессов `pgid`.
///
/// [`send_signal_to_pgid`] ставит одну копию на лидера группы, что и нужно
/// `kill(-pgid)`/`kill(0)` (общий group-pending список Linux выполняет обработчик
/// раз на процесс). Контроль доступа к терминалу иной: `SIGTTIN`, `SIGTTOU` и
/// `SIGTSTP` останавливают всю foreground-группу, а группа процессов pagh может
/// содержать НЕСКОЛЬКО групп потоков (job, чей процесс форкнулся), поэтому каждая
/// должна получить свою копию — групповой stop в планировщике per thread group
/// ([`begin_group_stop`]), и сигнал только лидеру оставил бы его соседей
/// работающими (ровно отказ «останавливает только последний pid», который это
/// заменяет). Один представитель на группу потоков сохраняет свойство «один запуск
/// обработчика на процесс».
pub fn send_signal_to_pgid_all(pgid: u64, sig: u64) -> Result<(), Errno> {
    let members = compat::group_pids(pgid);
    if members.is_empty() {
        return Err(Errno::ESRCH);
    }
    // Один снимок реестра, затем группировка в памяти: `tgid_of` брал бы лок
    // заново на каждого члена, а инвариант 4 из AGENTS.md не пускает доставку под
    // локом.
    let mut groups: alloc::collections::BTreeMap<u64, alloc::vec::Vec<u64>> =
        alloc::collections::BTreeMap::new();
    for pid in members {
        groups.entry(compat::tgid_of(pid)).or_default().push(pid);
    }
    let mut sent = 0usize;
    for (tgid, pids) in groups {
        if let Some(target) = super::kill::pick_group_target(&pids, tgid) {
            if send_signal(target, sig).is_ok() {
                sent += 1;
            }
        }
    }
    if sent == 0 {
        Err(Errno::ESRCH)
    } else {
        Ok(())
    }
}

pub fn sys_kill(pid_raw: u64, sig_raw: u64) -> Result<u64, Errno> {
    let (target, sig) = super::kill::kill_args(
        super::kill::decode_pid(pid_raw),
        super::kill::decode_sig(sig_raw),
    )?;
    let targets = resolve_kill_targets(target);
    deliver_to_targets(&targets, sig)
}

/// Свести классифицированную цель `kill(2)` к точным pid, на которые ставится
/// сигнал: ОДИН поток на адресованную группу процессов
/// (`kill::pick_group_target`), а не каждый член — эквивалент постановки в общий
/// pending-список группы Linux, поэтому обработчик, установленный многопоточным
/// процессом, выполняется один раз.
///
/// ИЗВЕСТНЫЙ ПРОБЕЛ (назван в `AGENTS.md` → Known open gaps): с тех пор как `pgid`
/// стал независим от группы потоков (2.16.0), сигнал группе процессов, содержащей
/// НЕСКОЛЬКО групп потоков, доходит только до выбранной, поэтому
/// `kill(-pgid, SIGCONT)` возобновляет лишь её. Пути stop/continue терминала и
/// шелла эту функцию не используют — они берут [`send_signal_to_pgid_all`], который
/// ставит копию на каждую группу потоков.
///
/// Пустой результат означает «нет такого процесса/группы» и превращается в
/// `ESRCH` в [`deliver_to_targets`]. Весь обход реестра идёт внутри хелперов
/// снимка здесь; ничего не доставляется, пока их лок удерживается.
pub fn resolve_kill_targets(target: super::kill::KillTarget) -> alloc::vec::Vec<u64> {
    use super::kill::KillTarget;
    match target {
        // Положительный pid: адресуется точно (per-thread семантика — doc модуля
        // фиксирует это отклонение от групповой доставки Linux). Обхода реестра
        // нет: существование проверяет сам `send_signal`.
        KillTarget::Pid(pid) => alloc::vec![pid],
        // pid == 0: собственная группа процессов вызывающего.
        KillTarget::OwnGroup => {
            let pgid = compat::current_pgid();
            super::kill::pick_group_target(&compat::group_pids(pgid), pgid)
                .into_iter()
                .collect()
        }
        // pid < -1: группа процессов `-pid`.
        KillTarget::Group(pgid) => super::kill::pick_group_target(&compat::group_pids(pgid), pgid)
            .into_iter()
            .collect(),
        // pid == -1: каждый сигнализируемый процесс, кроме всей группы потоков
        // вызывающего и pid 1 (`kill::INIT_PID`; man7 kill(2) NOTES). Один
        // снимок, затем группировка по tgid, чтобы каждый процесс всё равно
        // получил ровно одну копию.
        KillTarget::All => {
            let own = compat::current_tgid();
            let mut groups: alloc::collections::BTreeMap<u64, alloc::vec::Vec<u64>> =
                alloc::collections::BTreeMap::new();
            for (pid, tgid) in compat::compat_pid_tgid_snapshot() {
                if tgid == own || pid == super::kill::INIT_PID {
                    continue;
                }
                groups.entry(tgid).or_default().push(pid);
            }
            groups
                .iter()
                .filter_map(|(tgid, members)| super::kill::pick_group_target(members, *tgid))
                .collect()
        }
    }
}

/// Политика прав: отправитель — effective uid ТЕКУЩЕГО процесса, а креденшелы
/// каждой цели ищутся в compat-реестре. Цель, уже не зарегистрированная (вышла
/// между снимком и этим вызовом), здесь не фильтруется — `send_signal` сообщает
/// для неё `ESRCH`, как Linux. Если все найденные цели несигнализируемы,
/// результат — `EPERM`; иначе доставка идёт разрешённому подмножеству и
/// применяется обычное складывание errno.
///
/// Складывание errno (Linux: групповой send успешен, когда достигнута хотя бы одна
/// цель — `__kill_pgrp_info`): `Ok(0)`, как только одна доставка удалась, иначе
/// последняя ошибка по цели. Для цели, существовавшей на момент снимка, `ESRCH` —
/// единственная ошибка, которую может дать `send_signal` (цель вышла между
/// вызовами).
fn deliver_to_targets(targets: &[u64], sig: u64) -> Result<u64, Errno> {
    if targets.is_empty() {
        return Err(Errno::ESRCH);
    }
    let sender_euid = compat::current_creds().euid;
    let permitted: alloc::vec::Vec<u64> = targets
        .iter()
        .copied()
        .filter(|pid| match compat::creds_of(*pid) {
            Some(target) => super::kill::kill_permits(sender_euid, target.uid, target.euid),
            // Уже нет: пусть `send_signal` ответит ESRCH, а не переврал отказ прав
            // для процесса, которого больше нет.
            None => true,
        })
        .collect();
    if permitted.is_empty() {
        return Err(Errno::EPERM);
    }
    let mut delivered = 0usize;
    let mut last_err = Errno::ESRCH;
    for pid in permitted {
        match send_signal(pid, sig) {
            Ok(()) => delivered += 1,
            Err(e) => last_err = e,
        }
    }
    if delivered == 0 {
        Err(last_err)
    } else {
        Ok(0)
    }
}

// ─── Доставка ────────────────────────────────────────────────────────────────

/// Несёт ли фрейм сохранённых регистров смысл входа `syscall`, которого требует
/// контракт доставки?
///
/// На пути `syscall` CPU кладёт пользовательский RFLAGS в `r11`, а бит 1 RFLAGS
/// архитектурно всегда установлен; на любом другом входе в слоте лежит обычный
/// пользовательский GPR. Фрейм, проваливающий проверку, не мог прийти из
/// `syscall`, и слово `+120` в нём — НЕ user RSP: запись `rt_sigframe` «под ним»
/// попала бы туда, куда указывает это значение (для `int 0x80` — в собственную
/// кодовую страницу пользователя).
///
/// Это последняя линия защиты инварианта 2: точная половина — наблюдение
/// `int 0x80` на процесс (`compat::current_int80_entry`), проверяемое первым.
/// Дёшево, без лока и не может отвергнуть настоящий фрейм `syscall`.
fn frame_carries_syscall_view(regs: &SavedRegs) -> bool {
    regs.r11 & 0x2 != 0
}

/// Доставить ОДИН ожидающий сигнал текущего процесса, если какой-то доставим
/// (pending ∧ ¬blocked), в точке возврата из syscall. Вызывается из эпилога
/// [`super::linux_dispatch`] с `result` = сложенное возвращаемое значение syscall
/// (записывается в сохранённый `rax` фрейма, поэтому обработчик, вернувшийся позже
/// через `rt_sigreturn`, возобновляется с правильным результатом syscall).
///
/// Действия по умолчанию без пользовательского обработчика:
///   * `Term` → процесс выходит со статусом `128 + sig` (расходится, никогда не
///     возвращается — зеркалит фатальную семантику `tgkill`);
///   * `Ignore`/`Stop`/`Cont` → отбрасывается (у планировщика пока нет
///     stop/continue — у планировщика пока нет пути тика, это единственная точка
///     доставки, поэтому CPU-bound цикл без syscall'ов сигнала всё ещё не видит).
///
/// Ошибка размещения фрейма (регион ниже RSP не отображён — guard-страница или
/// почти исчерпанный стек) трактуется как `SIGSEGV`: процесс выходит с `128 + 11`
/// (139). Linux вырастил бы стек; наш путь роста стека отсюда пока недостижим.
///
/// Сигнал ВСЕГДА потребляется до любого из путей отказа ниже (pending-бит
/// очищается, `PENDING_APPROX` уменьшается), поэтому отвергнутая доставка не может
/// зациклиться: она либо превращается в действие сигнала по умолчанию, либо
/// отбрасывается с диагностикой.
pub fn deliver_one_pending_syscall(regs: &mut SavedRegs, result: u64) {
    if PENDING_APPROX.load(Ordering::Relaxed) == 0 {
        return;
    }
    let Some((sig, action, old_blocked, altstack)) = compat::pick_pending_signal() else {
        return;
    };
    PENDING_APPROX.fetch_sub(1, Ordering::Relaxed);

    if action.handler == SIG_IGN {
        return;
    }
    // Точная половина защиты инварианта 2: этот процесс хотя бы раз вошёл в
    // syscall через `int 0x80`, поэтому его сохранённые регистры значат не то,
    // что требует контракт фрейма выше. Никогда не писать для него `rt_sigframe`;
    // выполнить действие по умолчанию и сказать об этом один раз на процесс.
    if compat::current_int80_entry() {
        if compat::note_int80_refused() {
            crate::warn!(
                "[signal] pid={} sig={} refused: int 0x80 entry path \
                 (AGENTS.md invariant 2) - no rt_sigframe written",
                scheduler::current_pid(),
                sig
            );
        }
        if action.handler == SIG_DFL && default_action(sig) == DefaultAction::Term {
            force_terminate_group(scheduler::current_pid(), sig);
        }
        return;
    }
    if action.handler == SIG_DFL {
        match default_action(sig) {
            DefaultAction::Term => {
                crate::info!(
                    "[signal] pid={} sig={} default-terminate (no handler)",
                    scheduler::current_pid(),
                    sig
                );
                // Расходится: завершает группу потоков с 128 + sig.
                super::misc::sys_exit_group(128 + sig);
            }
            DefaultAction::Stop => {
                // Stop — это ДЕЙСТВИЕ (Linux: «сама остановка ... выполняется как
                // действие сигнала для SIG_DFL»): припарковать эту задачу и её
                // соседей. Не возвращается, пока SIGCONT не возобновит нас;
                // pending-бит уже потреблён выше, поэтому stop не наблюдается
                // повторно.
                stop_current_group(sig);
                return;
            }
            _ => return,
        }
    }

    // Последняя линия защиты инварианта 2 (см. doc модуля): фрейм, который не мог
    // прийти из `syscall`, нельзя интерпретировать как таковой.
    if !frame_carries_syscall_view(regs) {
        crate::warn!(
            "[signal] pid={} sig={} refused: saved RFLAGS={:#x} lacks bit 1 - \
             not a syscall-entry frame, no rt_sigframe written",
            scheduler::current_pid(),
            sig,
            regs.r11
        );
        return;
    }

    // Пользовательский обработчик: построить фрейм ниже прерванного RSP.
    let user_rsp = unsafe { ((regs as *mut SavedRegs as *const u64).add(15)).read() };
    let frame = frame_location(user_rsp, &action, &altstack);
    if check_user_ptr(frame, RT_SIGFRAME_SIZE).is_err() {
        crate::warn!(
            "[signal] pid={} sig={} frame {:#x} unmapped -> SIGSEGV exit",
            scheduler::current_pid(),
            sig,
            frame
        );
        super::misc::sys_exit_group(128 + crate::arch::x86_64::linux::signal_frame::SIGSEGV);
    }

    let saved = crate::arch::x86_64::linux::signal_frame::SigFrameRegs {
        r8: regs.r8,
        r9: regs.r9,
        r10: regs.r10,
        r11: regs.r11,
        r12: regs.r12,
        r13: regs.r13,
        r14: regs.r14,
        r15: regs.r15,
        di: regs.rdi,
        si: regs.rsi,
        bp: regs.rbp,
        bx: regs.rbx,
        dx: regs.rdx,
        ax: result,
        cx: regs.rcx,
        sp: user_rsp,
        ip: regs.rcx,
        flags: regs.r11,
    };
    let newmask = (old_blocked | action.mask) & !UNBLOCKABLE_MASK;
    let mut buf = [0u8; RT_SIGFRAME_SIZE as usize];
    encode_rt_sigframe(&mut buf, action.restorer, sig, &saved, newmask, &altstack);
    // SAFETY: весь диапазон фрейма проверен как отображённый и доступный
    // пользователю через `check_user_ptr` выше, а задача эксклюзивно владеет своим
    // пользовательским стеком, пока выполняется.
    unsafe {
        ptr::copy_nonoverlapping(buf.as_ptr(), frame as *mut u8, buf.len());
        // Новый user RSP для стаба выхода syscall: база фрейма.
        ((regs as *mut SavedRegs as *mut u64).add(15)).write(frame);
    }
    regs.rdi = sig;
    regs.rsi = frame + SIGINFO_OFFSET;
    regs.rdx = frame + UC_OFFSET;
    regs.rcx = action.handler; // цель RIP для sysretq
    regs.r11 = USER_RFLAGS;
    regs.rax = result;
    compat::block_during_handler(action.mask);
    crate::info!(
        "[signal] pid={} delivered sig={} handler={:#x} frame={:#x} (restorer={:#x})",
        scheduler::current_pid(),
        sig,
        action.handler,
        frame,
        action.restorer
    );
}

// ─── Доставка с тика таймера ─────────────────────────────────────────────────

/// Что путь возврата тика сделал с прерванной задачей.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TickAction {
    /// Делать нечего: нет доставимого сигнала либо прерванный фрейм — ядерный,
    /// чей пользовательский контекст не здесь (вернёт эпилог возврата syscall или
    /// проверки `EINTR` блокирующих циклов ожидания).
    None,
    /// Записан `rt_sigframe` и iret-фрейм переписан на месте; вызывающий обязан
    /// вернуть задачу в очередь с этим фреймом.
    Delivered,
    /// Действие сигнала по умолчанию остановило задачу: решение о requeue должно
    /// припарковать её (запрос остановки уже помечен).
    Park,
    /// Задача завершена (фатальное действие по умолчанию или `rt_sigframe`,
    /// который не удалось разместить): вызывающий обязан снять её — никогда не
    /// возвращать в очередь — и собрать с её собственным адресным пространством.
    Kill,
}

/// Обработать один ожидающий сигнал ТЕКУЩЕЙ задачи в точке возврата тика
/// таймера, где `cur_rsp` — фрейм, только что сохранённый `irq32_stub` (см.
/// [`super::trap_frame`]).
///
/// ## Почему это отдельный путь от [`deliver_one_pending_syscall`]
///
/// Фреймы разные: по `cur_rsp` слово `+120` — прерванный `rax`, а прерванный
/// пользовательский контекст живёт в положенных CPU iret-словах, поэтому код
/// syscall-пути (переписывающий `rcx`/`r11` для `sysretq` и per-task слот
/// user-RSP) испортил бы задачу. Здесь доставка переписывает iret-фрейм:
/// `RIP` = обработчик, `RSP` = база фрейма, `RFLAGS` = [`USER_RFLAGS`],
/// `RDI/RSI/RDX` = signo/`&siginfo`/`&ucontext`; `CS`, `SS`, `rax` и слово для
/// `popfq` не трогаются.
///
/// ## Порядок проверок (все несущие)
///
///   1. **Сначала фатальное действие** (любой фрейм): сигналу `SIG_DFL` с
///      завершением не нужен пользовательский фрейм, и задача должна умереть, была
///      ли она в ring 3 или внутри syscall. Завершает БЕЗ расхождения —
///      `sys_exit_group` нельзя звать из контекста IRQ.
///   2. **Затем stop** (любой фрейм): остановка паркует задачу на решении о
///      requeue этого тика; в отличие от syscall-пути, уступать CPU из контекста
///      IRQ нельзя.
///   3. **Проверка ring 3 ДО выбора**: у ядерного фрейма нет пользовательского
///      контекста, и потребление бита потеряло бы сигнал навсегда (никто не
///      переставит сигнал в очередь). Это единственное правило порядка, от
///      которого зависит весь путь.
///   4. Затем (и только затем) потребить бит и построить фрейм. Выбранный сигнал
///      сверяется с подсмотренным, и фрейм строится из реально взятого значения,
///      поэтому доставленный сигнал никогда не может быть соседом pending-бита
///      (класс off-by-one).
pub fn tick_action(cur_rsp: u64) -> TickAction {
    if cur_rsp == 0 || PENDING_APPROX.load(Ordering::Relaxed) == 0 {
        return TickAction::None;
    }
    let pid = scheduler::current_pid();
    if !compat::compat_exists(pid) {
        return TickAction::None;
    }
    // SAFETY: единственный вызывающий — `scheduler_tick_irq`, передающий свой
    // `current_rsp` — фрейм, который `irq32_stub` положил на стек ядра ТЕКУЩЕЙ
    // задачи. Фрейм занимает 168 байт, отображён и не алиасится на время вызова
    // (у каждой задачи свой стек ядра, а тик идёт с замаскированными прерываниями,
    // так что ничто не может вытеснить или алиасить его).
    let frame = unsafe { &mut *(cur_rsp as *mut IrqFrame) };

    // (1) Что сделал бы ожидающий сигнал? Подсмотреть БЕЗ потребления.
    let Some((sig, action, _blocked, _alt)) = compat::peek_deliverable_signal() else {
        return TickAction::None;
    };

    if action.handler == SIG_DFL && default_action(sig) == DefaultAction::Term {
        let _ = compat::pick_pending_signal();
        PENDING_APPROX.fetch_sub(1, Ordering::Relaxed);
        crate::info!(
            "[signal] pid={} sig={} tick-delivered default-terminate (no handler)",
            pid,
            sig
        );
        // Записывает 128 + sig для wait4 и помечает задачу exiting; вызывающий
        // снимает её. Расхождение здесь (exit_group) было бы циклом halt внутри
        // IRQ.
        force_terminate_group(pid, sig);
        return TickAction::Kill;
    }
    if action.handler == SIG_DFL && default_action(sig) == DefaultAction::Stop {
        let _ = compat::pick_pending_signal();
        PENDING_APPROX.fetch_sub(1, Ordering::Relaxed);
        begin_group_stop(sig);
        return TickAction::Park;
    }
    // (3) Ядерный фрейм: пользовательского контекста здесь нет, поэтому НИЧЕГО
    // не потребляем.
    if !frame.is_user_frame() {
        return TickAction::None;
    }
    if action.handler == SIG_IGN {
        let _ = compat::pick_pending_signal();
        PENDING_APPROX.fetch_sub(1, Ordering::Relaxed);
        return TickAction::None;
    }

    // (4) Потребить бит и доставить пользовательскому обработчику.
    let Some((taken, action, old_blocked, altstack)) = compat::pick_pending_signal() else {
        return TickAction::None;
    };
    PENDING_APPROX.fetch_sub(1, Ordering::Relaxed);
    if taken != sig {
        // Невозможно при замаскированных прерываниях (ничто другое не может
        // тронуть pending-набор процесса), но если случится, фрейм обязан строиться
        // из сигнала, который РЕАЛЬНО взят, — никогда из соседа.
        crate::error!(
            "[signal] pid={} tick delivery: peeked sig={} but picked sig={} - delivering the picked one",
            pid,
            sig,
            taken
        );
    }
    let user_rsp = frame.rsp;
    let frame_addr = frame_location(user_rsp, &action, &altstack);
    if check_user_ptr(frame_addr, RT_SIGFRAME_SIZE).is_err() {
        crate::warn!(
            "[signal] pid={} sig={} tick delivery: frame {:#x} unmapped -> SIGSEGV exit",
            pid,
            taken,
            frame_addr
        );
        force_terminate_group(pid, super::signal_frame::SIGSEGV);
        return TickAction::Kill;
    }
    let saved = frame.user_context();
    let newmask = (old_blocked | action.mask) & !UNBLOCKABLE_MASK;
    let mut buf = [0u8; RT_SIGFRAME_SIZE as usize];
    encode_rt_sigframe(&mut buf, action.restorer, taken, &saved, newmask, &altstack);
    // SAFETY: `check_user_ptr` выше доказал, что весь регион RT_SIGFRAME_SIZE
    // отображён и доступен пользователю, а задача эксклюзивно владеет своим
    // пользовательским стеком, пока она текущая (тик идёт на её стеке ядра).
    unsafe {
        ptr::copy_nonoverlapping(buf.as_ptr(), frame_addr as *mut u8, buf.len());
    }
    frame.enter_handler(&plan_irq_delivery(
        frame_addr,
        taken,
        action.handler,
        USER_RFLAGS,
    ));
    compat::block_during_handler(action.mask);
    crate::info!(
        "[signal] pid={} tick-delivered sig={} handler={:#x} frame={:#x} (interrupted rip={:#x})",
        pid,
        taken,
        action.handler,
        frame_addr,
        saved.ip
    );
    TickAction::Delivered
}

// ─── rt_sigreturn ────────────────────────────────────────────────────────────

/// `rt_sigreturn` (15): восстановить прерванный контекст из `rt_sigframe`, на
/// котором стоит restorer.
///
/// Restorer glibc работает с `RSP = frame + 8` (`pretcode` снят `ret` обработчика),
/// а это ровно начало `ucontext` — значит входной user RSP (слот `+120`) И ЕСТЬ
/// адрес ucontext.
///
/// Возвращает сохранённый `rax`; вызывающий [`super::linux_dispatch`] (входной
/// стаб) пишет его в сохранённый слот `rax`, и так восстановленный `rax` доходит до
/// ring 3 вопреки обычной обвязке возвращаемого значения.
pub fn sys_rt_sigreturn(regs: &mut SavedRegs) -> Result<u64, Errno> {
    let user_rsp = unsafe { ((regs as *mut SavedRegs as *const u64).add(15)).read() };
    check_user_ptr(user_rsp, 304)?;
    let mut uc = [0u8; 304];
    // SAFETY: диапазон проверен выше; фрейм записан нами (или подделан
    // пользователем — Linux тоже доверяет; фрейм с нулевым IP отвергается ниже).
    unsafe {
        ptr::copy_nonoverlapping(user_rsp as *const u8, uc.as_mut_ptr(), uc.len());
    }
    let Some(RestoredFrame { regs: fr, mask }) = decode_rt_sigframe(&uc) else {
        crate::warn!("[signal] rt_sigreturn: null-IP frame at {:#x}", user_rsp);
        return Err(Errno::EINVAL);
    };
    regs.r8 = fr.r8;
    regs.r9 = fr.r9;
    regs.r10 = fr.r10;
    regs.r12 = fr.r12;
    regs.r13 = fr.r13;
    regs.r14 = fr.r14;
    regs.r15 = fr.r15;
    regs.rdi = fr.di;
    regs.rsi = fr.si;
    regs.rbp = fr.bp;
    regs.rbx = fr.bx;
    regs.rdx = fr.dx;
    regs.rcx = fr.ip; // RIP для sysretq
                      // `decode_rt_sigframe` уже проверил ip/sp и вымаскировал флаги до
                      // пользовательски устанавливаемых битов: `sysretq` восстанавливает RFLAGS
                      // дословно, поэтому IOPL/NT/VM/RF/AC никогда не должны дойти до него из
                      // подделанного пользователем фрейма.
    regs.r11 = fr.flags; // RFLAGS для sysretq
                         // SAFETY: per-task слот user-RSP — собственное хранилище этой задачи на
                         // стеке ядра (тот же слот, который переписывают execve/clone).
    unsafe {
        ((regs as *mut SavedRegs as *mut u64).add(15)).write(fr.sp);
    }
    compat::with_current_compat(|cs| cs.sig_blocked = mask);
    Ok(fr.ax)
}

// ─── Интроспекция циклов ожидания ────────────────────────────────────────────

/// Есть ли у ТЕКУЩЕГО процесса сигнал, который был бы доставлен на следующем
/// возврате из syscall. Блокирующие циклы ожидания syscall'ов зовут это, чтобы
/// превратить доставленный сигнал в возврат `-EINTR`, а не проспать его.
pub fn has_deliverable_current() -> bool {
    if PENDING_APPROX.load(Ordering::Relaxed) == 0 {
        return false;
    }
    compat::with_current_compat(|cs| cs.sig_pending & !cs.sig_blocked != 0).unwrap_or(false)
}

/// Установлен ли у ТЕКУЩЕГО процесса ПОЛЬЗОВАТЕЛЬСКИЙ обработчик для `sig`?
/// `None`, когда compat-состояния нет (нативная задача).
pub fn current_has_handler(sig: u64) -> Option<bool> {
    compat::current_action(sig).map(|a| is_user_handler(&a))
}

/// Снимок диспозиции для `sig` в ТЕКУЩЕМ процессе (используется чтением oldact в
/// `rt_sigaction`).
pub fn current_action_clone(
    sig: u64,
) -> Option<crate::arch::x86_64::linux::signal_frame::SignalAction> {
    compat::current_action(sig)
}
