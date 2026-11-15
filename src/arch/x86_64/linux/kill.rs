//! Чистое декодирование аргументов `kill(2)`, классификация цели и матрица
//! errno — решения, которые `signal::sys_kill` принимает до обращения к
//! compat-реестру или планировщику.
//!
//! Намеренно только `core` и без зависимостей (R11.6): включается в `host-tests`
//! через `#[path]` (свойство `kill_target`), поэтому классификация, из которой
//! выводится каждый errno, проверяется на хосте для ВСЕГО пространства входов
//! `i64`, а не только для путей, куда случайно попадает гость.
//!
//! ## Зачем вообще декодировать аргументы
//!
//! Прототип `kill` — `int kill(pid_t pid, int sig)`, оба аргумента 32-битные. На
//! x86_64 диспетчер читает полные 64-битные регистры
//! (`linux::abi::marshal_args`), поэтому старшие половины несут то, что оставил
//! кодогенератор вызывающего. Linux усекает C-аргументы, и мы обязаны так же:
//! `pid = (raw as u32) as i32`, `sig = (raw as u32) as i32`. Без этого
//! отрицательный `sig` (`0xFFFF_FFFF` как `int` = -1) читается как огромное
//! положительное число, а отрицательный `pid` (`kill(-pgid)`) — как огромный
//! положительный pid; ровно этот класс ошибок и закрепляет свойство
//! `kill_target`.
//!
//! ## Матрица errno (man7 kill(2) + `kill_something_info`)
//!
//! Порядок важен: сначала проверяется валидность сигнала, затем находится цель,
//! затем права.
//!
//! | аргумент | смысл | результат |
//! |---|---|---|
//! | `sig == 0` | проба существования, сигнал не шлётся | `Ok`, если цель есть |
//! | `sig < 0` или `sig > 64` | неверный сигнал | `EINVAL` (до поиска цели) |
//! | `pid > 0` | тот процесс (поток) | `ESRCH`, если нет compat-состояния |
//! | `pid == 0` | все процессы группы вызывающего | `ESRCH`, если группа пуста |
//! | `pid == -1` | все процессы, которым можно, кроме своей группы потоков и pid 1 | `ESRCH`, если кандидатов нет |
//! | `pid < -1` | все процессы группы `-pid` | `ESRCH`, если группа пуста |
//! | `pid == INT_MIN` | `-INT_MIN` переполняется, поэтому Linux даёт | `ESRCH` (НЕ `EINVAL`) |
//! | любой | нет права сигнализировать НИ ОДНОЙ цели | `EPERM` |
//!
//! `EPERM` реален: сигнализировать цели можно, только когда **effective** uid
//! вызывающего равен real или effective uid цели, либо вызывающий — root
//! (`kill_permits`). Широковещательный `kill(-1)` применяет то же правило к
//! каждой цели и доставляет разрешённому подмножеству (как
//! `kill_something_info`), поэтому один несигнализируемый процесс не превращает
//! всю рассылку в `EPERM`. Обработчик снимает креденшелы из compat-реестра;
//! само решение — чистая [`kill_permits`], проверяемая свойством `kill_target`.
//!
//! ## Группы процессов в pagh
//!
//! `misc::sys_getpgid` сообщает `compat::current_tgid()`, а `sys_setpgid` —
//! документированный no-op, поэтому моделируемый pgid процесса ЕСТЬ его tgid.
//! `kill(-pgid)` выбирает зарегистрированные compat-потоки с `tgid == pgid` —
//! тот же набор, что даёт текущий шелл/job-control (`setpgid(0,0)` /
//! `setpgid(child, 0)` оба означают «свою группу»). Перевод процесса в ДРУГУЮ
//! группу не моделируется; если появится, менять нужно только поиск группы в
//! `signal::resolve_kill_targets`.
//!
//! ## Что считается «существующим»
//!
//! Существование — это compat-реестр (`compat::compat_exists`). Уже вышедший
//! ребёнок живёт там только как запись `wait4`, поэтому `kill(pid, 0)` на нём
//! сообщает `ESRCH`, тогда как Linux ещё принял бы pid, пока зомби существует.
//! Явно принятое отклонение: `kill(pid, 0)` отвечает «работает и сигнализируем»,
//! а не «слот pid занят» (именно это спрашивает шелл, пробуя job). Нативные и
//! ядерные задачи по той же причине несигнализируемы — у них нет
//! compat-состояния.
#![allow(dead_code)]

use super::errno::Errno;

/// Наибольший валидный номер сигнала (`_NSIG - 1` в Linux x86_64).
pub const NSIG_MAX: i64 = 64;
/// `INT_MIN`: `kill(INT_MIN, sig)` в Linux переполняет `-pid` и возвращает
/// `ESRCH` (`kill_something_info` явно оберегает этот случай).
pub const INT_MIN: i64 = i32::MIN as i64;
/// `init` (pid 1) исключён из широковещательного `kill(-1, ...)` (man7 kill(2)
/// NOTES: «единственные сигналы, которые можно послать процессу 1 ... — те, для
/// которых init явно установил обработчики»).
pub const INIT_PID: u64 = 1;

/// `pid_t pid` в том виде, как его видит ядро: усечь до 32 бит, затем расширить
/// знаково.
#[inline]
pub const fn decode_pid(raw: u64) -> i64 {
    (raw as u32) as i32 as i64
}

/// `int sig` в том виде, как его видит ядро: усечь до 32 бит, затем расширить
/// знаково.
#[inline]
pub const fn decode_sig(raw: u64) -> i64 {
    (raw as u32) as i32 as i64
}

/// Что адресует `kill(pid, sig)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillTarget {
    /// `pid > 0`: ровно этот процесс/поток.
    Pid(u64),
    /// `pid == 0`: собственная группа процессов вызывающего.
    OwnGroup,
    /// `pid == -1`: каждый сигнализируемый процесс, кроме группы потоков вызывающего.
    All,
    /// `pid < -1`: группа процессов с id `-pid`.
    Group(u64),
}

/// Проверить `sig` и классифицировать `pid` (Linux: сначала валидность сигнала,
/// затем цель; `INT_MIN` — это `ESRCH`, см. [`INT_MIN`]).
pub fn kill_args(pid: i64, sig: i64) -> Result<(KillTarget, u64), Errno> {
    if !kill_sig_valid(sig) {
        return Err(Errno::EINVAL);
    }
    let target = kill_target(pid)?;
    Ok((target, sig as u64))
}

/// Принимает ли `kill(2)` такой номер сигнала? `0` — проба существования и
/// валиден; всё вне `0..=_NSIG-1` (включая отрицательные `int`) — `EINVAL`.
#[inline]
pub fn kill_sig_valid(sig: i64) -> bool {
    sig >= 0 && sig <= NSIG_MAX
}

/// Классифицировать `pid_t`, уже знаково расширенный до `i64`.
pub fn kill_target(pid: i64) -> Result<KillTarget, Errno> {
    match pid {
        INT_MIN => Err(Errno::ESRCH),
        0 => Ok(KillTarget::OwnGroup),
        -1 => Ok(KillTarget::All),
        p if p > 0 => Ok(KillTarget::Pid(p as u64)),
        p => Ok(KillTarget::Group((-p) as u64)),
    }
}

/// Может ли процесс с effective uid `sender_euid` сигнализировать цель с real uid
/// `target_uid` и effective uid `target_euid`?
///
/// Linux (`check_kill_permission`): можно, когда effective uid отправителя — root
/// либо равен real **или** effective uid цели. Дополнительные группы и
/// capabilities не моделируются, namespace `uids` нет.
#[inline]
pub fn kill_permits(sender_euid: u32, target_uid: u32, target_euid: u32) -> bool {
    sender_euid == 0 || sender_euid == target_uid || sender_euid == target_euid
}

/// Выбрать ЕДИНСТВЕННЫЙ поток, на который ставится групповой сигнал.
///
/// Linux ставит групповой сигнал в общий pending-список группы потоков, поэтому
/// обработчик многопоточного процесса выполняется ОДИН раз, а не на каждый
/// поток. В pagh pending-набор по-потоковый, поэтому групповой путь обязан
/// выбрать один поток: лидера группы (`tgid`, детерминированно и именно его
/// обычно имеет в виду вызывающий `kill(-pgid)`), если он ещё
/// зарегистрирован, иначе живой член с наименьшим pid (`members` приходят по
/// возрастанию pid из `BTreeMap` реестра).
///
/// `None` — у группы нет живых членов → `ESRCH` в точке вызова.
pub fn pick_group_target(members: &[u64], tgid: u64) -> Option<u64> {
    if members.contains(&tgid) {
        return Some(tgid);
    }
    members.iter().copied().min()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_truncates_to_32_bits_then_sign_extends() {
        assert_eq!(decode_pid(0x1_0000_0005), 5);
        // -5 как `int` живёт в младших 32 битах; старшая половина игнорируется.
        assert_eq!(decode_pid(0xFFFF_FFFF_FFFF_FFFB), -5);
        assert_eq!(decode_pid(0xFFFF_FFFB), -5);
        assert_eq!(decode_pid(0xFFFF_FFFF_8000_0000), INT_MIN);
    }

    #[test]
    fn sig_decoding_rejects_negative_ints() {
        assert_eq!(decode_sig(0xFFFF_FFFF), -1);
        assert!(!kill_sig_valid(decode_sig(0xFFFF_FFFF)));
        assert!(kill_sig_valid(0));
        assert!(kill_sig_valid(64));
        assert!(!kill_sig_valid(65));
    }

    #[test]
    fn target_classification_matches_linux() {
        assert_eq!(kill_target(7), Ok(KillTarget::Pid(7)));
        assert_eq!(kill_target(0), Ok(KillTarget::OwnGroup));
        assert_eq!(kill_target(-1), Ok(KillTarget::All));
        assert_eq!(kill_target(-7), Ok(KillTarget::Group(7)));
        assert_eq!(kill_target(INT_MIN), Err(Errno::ESRCH));
    }

    #[test]
    fn invalid_sig_wins_over_target_lookup() {
        // Linux проверяет сигнал до разрешения pid, поэтому плохой pid вместе с
        // плохим сигналом даёт EINVAL (а не ESRCH).
        assert_eq!(kill_args(0xDEAD, 65), Err(Errno::EINVAL));
        assert_eq!(kill_args(0xDEAD, 0), Ok((KillTarget::Pid(0xDEAD), 0)));
    }

    #[test]
    fn group_target_prefers_leader_then_lowest() {
        assert_eq!(pick_group_target(&[3, 5, 9], 5), Some(5));
        assert_eq!(pick_group_target(&[3, 5, 9], 4), Some(3));
        assert_eq!(pick_group_target(&[], 4), None);
    }

    #[test]
    fn permission_policy_uses_real_and_effective_target_ids() {
        assert!(kill_permits(0, 0, 0));
        assert!(kill_permits(0, 1000, 1001));
        assert!(kill_permits(1000, 1000, 1001));
        assert!(kill_permits(1000, 1001, 1000));
        assert!(!kill_permits(1000, 1001, 1002));
    }
}
