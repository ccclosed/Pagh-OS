//! Эффектные misc-обработчики syscall'ов Linux:
//! `getpid`/`uname`/`arch_prctl`/`set_tid_address`/`clock_gettime`/`getrandom`/
//! `exit`/`exit_group`.
//!
//! Модуль **только для ядра** (не включается в `host-tests` через `#[path]`),
//! поэтому свободно использует планировщик, per-process [`CompatState`], MSR
//! FS-base и фасад логирования ядра. Переиспользует чистые планировщики
//! [`super::rand_clock`] (`getrandom_plan`/`ticks_to_timespec`) и
//! диагностические хелперы [`super::diag`] (`exit_code_byte`).
//!
//! Каждый пользовательский указатель проверяется через единую точку
//! [`super::check_user_ptr`] до разыменования.
#![allow(dead_code)]

use x86_64::registers::model_specific::FsBase;
use x86_64::VirtAddr;

use crate::task::compat;
use crate::task::scheduler;

use super::cred::Creds;
use super::diag::exit_code_byte;
use super::errno::Errno;
use super::rand_clock::{getrandom_plan, ticks_to_timespec, Timespec, CLOCK_REALTIME};
use super::rtc;
use super::timeconv::{encode_timeval, Timeval};
use super::{check_user_ptr, check_user_ptr_writable};

/// Частота тика периодического таймера LAPIC (см. `arch::x86_64::apic`).
/// Передаётся чистому [`ticks_to_timespec`], чтобы `clock_gettime` сообщал
/// близкое к стенному время из счётчика тиков планировщика.
use crate::arch::x86_64::apic::TICK_HZ;

/// Подфункция `arch_prctl`: записать регистр `FS.base`.
const ARCH_SET_FS: u64 = 0x1002;
/// Подфункция `arch_prctl`: прочитать регистр `FS.base` в пользовательский
/// `u64`.
const ARCH_GET_FS: u64 = 0x1003;

/// `getpid` (39): вернуть pid вызывающего процесса.
pub fn sys_getpid() -> Result<u64, Errno> {
    Ok(compat::current_tgid())
}

/// Linux x86_64 `struct utsname`: шесть полей фиксированного размера с NUL.
#[repr(C)]
struct Utsname {
    sysname: [u8; 65],
    nodename: [u8; 65],
    release: [u8; 65],
    version: [u8; 65],
    machine: [u8; 65],
    domainname: [u8; 65],
}

/// Скопировать `s` в 65-байтное поле с паддингом NUL (усечь, если длиннее 64).
fn field(s: &str) -> [u8; 65] {
    let mut out = [0u8; 65];
    let bytes = s.as_bytes();
    let n = core::cmp::min(bytes.len(), 64);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

/// `uname` (63): заполнить пользовательский `struct utsname` фиксированными
/// строками идентификации и вернуть 0 (R2.11).
pub fn sys_uname(buf: u64) -> Result<u64, Errno> {
    check_user_ptr_writable(buf, core::mem::size_of::<Utsname>() as u64)?;
    let uts = Utsname {
        sysname: field("Linux"),
        nodename: field("pagh"),
        release: field("6.1.0-pagh"),
        version: field("#1 pagh compat"),
        machine: field("x86_64"),
        domainname: field("(none)"),
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &uts as *const Utsname as *const u8,
            core::mem::size_of::<Utsname>(),
        )
    };
    // SAFETY: `buf` проверен на полную длину структуры выше; активный CR3 —
    // пользовательский PML4 вызывающего процесса.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, bytes.len());
    }
    Ok(0)
}

/// `arch_prctl` (158): `ARCH_SET_FS` задаёт `FS.base` вызывающего процесса (и
/// запоминает его в [`CompatState`]) и возвращает 0 (R2.9); `ARCH_GET_FS` пишет
/// запомненную базу в пользовательский указатель. Прочие подфункции — `EINVAL`.
pub fn sys_arch_prctl(code: u64, addr: u64) -> Result<u64, Errno> {
    match code {
        ARCH_SET_FS => {
            // Задать архитектурный FS.base выполняющегося потока...
            FsBase::write(VirtAddr::new(addr));
            // ...и запомнить его в compat-состоянии процесса, чтобы он жил в
            // per-process модели (R2.9).
            compat::with_current_compat(|cs| cs.fs_base = addr);
            Ok(0)
        }
        ARCH_GET_FS => {
            check_user_ptr_writable(addr, 8)?;
            let base = compat::with_current_compat(|cs| cs.fs_base).unwrap_or(0);
            // SAFETY: `addr` проверен на 8 байт выше.
            unsafe {
                *(addr as *mut u64) = base;
            }
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

/// `set_tid_address` (218): вернуть tid вызывающего потока (R2.10). Переданный
/// `clear_child_tid` принимается, но не используется (сноса потоков здесь нет).
pub fn sys_set_tid_address(tidptr: u64) -> Result<u64, Errno> {
    // clear_child_tid ядро очищает при выходе потока, поэтому это out-указатель:
    // read-only страница дала бы store из kill/exit-пути.
    if tidptr != 0 {
        check_user_ptr_writable(tidptr, 4)?;
    }
    let tid = compat::with_current_compat(|cs| {
        cs.clear_child_tid = tidptr;
        cs.tid
    })
    .unwrap_or_else(scheduler::current_pid);
    Ok(tid)
}

/// `clock_gettime` (228): заполнить пользовательский `struct timespec` из
/// счётчика тиков ядра для `CLOCK_MONOTONIC`/`CLOCK_REALTIME` и вернуть 0
/// (R2.13); `EINVAL` для неподдерживаемого id часов, оставляя буфер неизменным
/// (R2.16).
pub fn sys_clock_gettime(clock_id: u64, tsptr: u64) -> Result<u64, Errno> {
    // Проверить id часов ДО касания буфера пользователя, чтобы
    // неподдерживаемый id оставил его неизменным (R2.16).
    let mut ts = ticks_to_timespec(scheduler::ticks(), clock_id as u32, TICK_HZ)?;
    // CLOCK_REALTIME — стенные часы: целые секунды берём с CMOS RTC, а
    // производные от тиков наносекунды оставляем. CLOCK_MONOTONIC остаётся на
    // тиках.
    if clock_id as u32 == CLOCK_REALTIME || clock_id as u32 == 5 {
        // 5 = CLOCK_REALTIME_COARSE — те же стенные часы, ниже точность.
        ts.tv_sec = rtc::now_unix() as i64;
    }
    check_user_ptr_writable(tsptr, core::mem::size_of::<Timespec>() as u64)?;
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &ts as *const Timespec as *const u8,
            core::mem::size_of::<Timespec>(),
        )
    };
    // SAFETY: `tsptr` проверен на длину timespec выше.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), tsptr as *mut u8, bytes.len());
    }
    Ok(0)
}

/// `getrandom` (318): заполнить пользовательский буфер из аппаратного API
/// энтропии. Падает с `EAGAIN`, а не молча возвращает предсказуемые байты на
/// основе времени, когда RDSEED/RDRAND недоступны.
pub fn sys_getrandom(buf: u64, count: u64, _flags: u64) -> Result<u64, Errno> {
    let n = getrandom_plan(count, count)?;
    if n == 0 {
        return Ok(0);
    }
    check_user_ptr_writable(buf, n)?;
    let out = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, n as usize) };
    crate::security::entropy::fill(out).map_err(|_| Errno::EAGAIN)?;
    Ok(n)
}

/// Построить ELF-блок `AT_RANDOM`. Предпочтительна аппаратная энтропия
/// (RDSEED/RDRAND). Когда платформа не даёт ни одной, блок выводится из
/// **смешанного boot-seed** ([`crate::security::entropy::mixed_fill`]):
/// несколько независимых источников времени загрузки, свёрнутых через SHA-256,
/// плюс счётчик на вызов и наблюдаемые входы.
///
/// ПОЧЕМУ НЕ FALLBACK-PRNG: прежняя реализация смешивала тиковые часы, RTC, pid и
/// фиксированную константу (аппаратный терм — `secure_u64().unwrap_or(0)` — на
/// этом пути необходимо нулевой, а свободно бегущего счётчика не было) через
/// 64-битный xorshift. Это *линейный* поток по наблюдаемым или угадываемым
/// значениям, поэтому ключ stack-canary/pointer-mangling каждой программы был
/// предсказуем при отсутствии RDSEED/RDRAND, и даже ненулевой блок не
/// гарантировался (ноль достижим для отдельных значений тика). Boot-seed из этих
/// наблюдаемых не выводится, а деградация сообщается один раз при загрузке
/// (`stage=degraded` в предупреждении энтропии), а не молча. `sys_getrandom`
/// по-прежнему отказывает с `EAGAIN`; эти байты он не отдаёт никогда.
///
/// Никогда не паникует и не отказывает: glibc читает `AT_RANDOM` безусловно при
/// старте процесса, поэтому отказ произвести его означал бы отказ запускать
/// процесс.
pub fn random_bytes_16() -> [u8; 16] {
    let mut out = [0u8; 16];
    if crate::security::entropy::fill(&mut out).is_ok() {
        return out;
    }
    crate::security::entropy::mixed_fill(&mut out);
    out
}

/// `exit` (60) / `exit_group` (231): записать нормализованный код выхода
/// (младший байт, R12.3), залогировать pid + код (R12.3/R12.5), затем завершить
/// только вызывающий процесс и навсегда уступить планировщику (R7.2). Не
/// возвращается.
pub fn sys_exit(code: u64) -> ! {
    super::process_sys::cleanup_current_thread_exit();
    let byte = exit_code_byte(code);
    compat::with_current_compat(|cs| cs.exit_code = Some(byte));
    let pid = scheduler::current_pid();
    crate::info!(
        "[linux] Compat_Process pid={} exited with code {}",
        pid,
        byte
    );
    // Завершает только эту задачу; планировщик продолжает другие (R7.2).
    scheduler::exit_current()
}

pub fn sys_exit_group(code: u64) -> ! {
    let byte = exit_code_byte(code);
    let current = scheduler::current_pid();
    let tgid = compat::current_tgid();
    let others = compat::group_member_pids(tgid, current);
    for pid in others.iter().copied() {
        super::process_sys::cleanup_thread_exit(pid);
        compat::remove_compat(pid);
    }
    scheduler::remove_ready_pids(&others);
    compat::with_current_compat(|cs| cs.exit_code = Some(byte));
    super::process_sys::cleanup_current_thread_exit();
    crate::debug!("[linux] thread-group {} exited with code {}", tgid, byte);
    scheduler::exit_current()
}

/// `tgkill` (234): послать сигнал потоку ГРУППЫ ПОТОКОВ вызывающего.
/// Семантика:
///   * при установленном пользовательском обработчике для `sig` (через
///     `rt_sigaction`): сигнал ставится ожидающим и доставляется в точке возврата
///     этого syscall'а ([`super::signal::deliver_one_pending_syscall`]) — так
///     self-сигналы в стиле `raise()`/`abort()` запускают обработчик раньше всего
///     остального;
///   * без обработчика и с фатальным по умолчанию `sig`: завершить группу
///     потоков с `128 + sig` (поведение сохранено: `ENOSYS` здесь заставлял
///     `abort()` из glibc проваливаться в ring-3 `hlt`, что проявлялось ложным
///     #GP после каждого «Fatal Python error»);
///   * без обработчика и с нефатальным `sig`: принять и игнорировать
///     (`SIGCHLD`, `SIGCONT`, `SIGWINCH`, ... не должны самоуничтожаться).
pub fn sys_tgkill(tgid: u64, tid: u64, sig: u64) -> Result<u64, Errno> {
    // ВСЕ ТРИ аргумента — C `int`: усечь каждый до 32 бит и расширить знаково до
    // любых действий. `sig` ещё и индексирует таблицу диспозиций по `sig - 1`,
    // поэтому «отрицательный» сигнал (0xFFFF_FFFF как `int`) переполнил бы индекс
    // вниз и паниковал ядро (`panic = "abort"` → машина умирает от недоверенного
    // аргумента syscall) — проверка общая `kill::kill_sig_valid`, одна
    // реализация на всё семейство.
    let sig = super::kill::decode_sig(sig);
    let tgid = super::kill::decode_pid(tgid);
    let tid = super::kill::decode_pid(tid);
    if !super::kill::kill_sig_valid(sig) {
        return Err(Errno::EINVAL);
    }
    // ESRCH: `tgkill` адресует ПАРУ (tgid, tid), и `do_tkill` в Linux отвергает
    // её, если поток не существует ИЛИ не принадлежит этой группе
    // (`task_tgid_vnr(p) != tgid` → ESRCH). Без этого `tgkill(self, 999999, 0)`
    // «успешно» действовал на несуществующий thread id. Несуществующий `tgid`
    // тоже не совпадёт и попадёт сюда; id группы остаётся валидным, пока жив
    // ХОТЬ ОДИН её поток, даже после выхода лидера (реестр держит `tgid` на
    // поток).
    if tid <= 0 {
        return Err(Errno::ESRCH);
    }
    let tid = tid as u64;
    if !compat::compat_exists(tid) {
        return Err(Errno::ESRCH);
    }
    if tgid <= 0 || tgid as u64 != compat::tgid_of(tid) {
        return Err(Errno::ESRCH);
    }
    // То же правило прав, что у kill(2), применённое к адресованному потоку
    // (`do_tkill` в Linux вызывает `check_kill_permission` до доставки, включая
    // пробу существования `sig == 0`).
    if let Some(target) = compat::creds_of(tid) {
        if !super::kill::kill_permits(compat::current_creds().euid, target.uid, target.euid) {
            return Err(Errno::EPERM);
        }
    }
    if sig == 0 {
        // Проба существования: пара валидна, ничего не посылается.
        return Ok(0);
    }
    let sig = sig as u64;
    // Таблица диспозиций общая для всей группы потоков (CLONE_THREAD делит её),
    // поэтому спросить действие ВЫЗЫВАЮЩЕГО — то же, что спросить адресованный
    // поток, когда они в одной группе; для чужого потока поиск ниже просто не
    // найдёт обработчика, и применяется действие по умолчанию.
    let has_handler = if tid == scheduler::current_pid() {
        super::signal::current_has_handler(sig)
    } else {
        // Чужая цель: решает её собственная таблица. `None` (нет compat-состояния)
        // здесь невозможен — проверка пары выше доказала существование.
        Some(super::signal::has_handler_for(tid, sig))
    };
    match has_handler {
        Some(true) => {
            super::signal::send_signal(tid, sig)?;
            Ok(0)
        }
        Some(false) => {
            if crate::arch::x86_64::linux::signal_frame::default_is_fatal(sig) {
                if tid == scheduler::current_pid() {
                    // Поведение сохранено: фатальный self-сигнал (`abort()` из
                    // glibc) завершает группу потоков вызывающего отсюда.
                    crate::info!("[linux] tgkill: fatal signal {} to self - exiting", sig);
                    sys_exit_group(128 + sig)
                } else {
                    // Фатальный сигнал, нацеленный на другой поток, обязан убить
                    // ТУ группу, а не группу вызывающего.
                    crate::info!(
                        "[linux] tgkill: fatal signal {} to tid={} - terminating its group",
                        sig,
                        tid
                    );
                    super::signal::terminate_group(tid, sig);
                    Ok(0)
                }
            } else {
                Ok(0)
            }
        }
        // Нативная задача (нет compat-состояния): сигнализировать некого.
        None => Ok(0),
    }
}

// ─────────────── идентичность / информация / время / сон / планирование / сигналы ───────────────
// Креденшелы живут в `CompatState` и по умолчанию root для каждого запущенного
// ядром процесса, поэтому шелл и существующий userland сохраняют прежнее
// поведение, а семейство `setuid` может создать настоящий не-root процесс для
// тестов.

/// Микросекунд в секунде.
const USEC_PER_SEC: u64 = 1_000_000;
/// Один тик планировщика — `1/TICK_HZ` секунд (1 мс при 1000 Гц).
const NS_PER_TICK: u64 = 1_000_000_000 / TICK_HZ;

/// Усечь регистр syscall до `uid_t`/`gid_t` (`u32`). Старшая половина 64-битного
/// регистра несёт то, что оставил кодогенератор вызывающего; Linux усекает
/// C-аргумент, и мы обязаны так же.
#[inline]
fn id_arg(raw: u64) -> u32 {
    raw as u32
}

/// Декодировать аргумент `uid_t`/`gid_t`, где `(u32)-1` означает «не менять»
/// (соглашение `setreuid`/`setresuid`).
#[inline]
fn optional_id_arg(raw: u64) -> Option<u32> {
    let v = raw as u32;
    if v == u32::MAX {
        None
    } else {
        Some(v)
    }
}

/// Записать один `u32` в проверенный пользовательский указатель.
fn put_u32(ptr: u64, value: u32) -> Result<(), Errno> {
    check_user_ptr_writable(ptr, 4)?;
    // SAFETY: `ptr` проверен на четыре байта выше; `write_unaligned` не требует
    // выравнивания, а обход присутствия страниц уже выполнен.
    unsafe {
        core::ptr::write_unaligned(ptr as *mut u32, value);
    }
    Ok(())
}

/// Заменить креденшелы текущего процесса, отображая «нет compat-состояния» в
/// `EPERM` — у нативной задачи нет видимой пользователю идентичности для
/// смены.
fn commit_creds(next: Creds) -> Result<u64, Errno> {
    if compat::set_current_creds(next) {
        Ok(0)
    } else {
        Err(Errno::EPERM)
    }
}

/// `getuid` (102): real user id.
pub fn sys_getuid() -> Result<u64, Errno> {
    Ok(compat::current_creds().uid as u64)
}

/// `getgid` (104): real group id.
pub fn sys_getgid() -> Result<u64, Errno> {
    Ok(compat::current_creds().gid as u64)
}

/// `geteuid` (107): effective user id (то, что используют проверки прав).
pub fn sys_geteuid() -> Result<u64, Errno> {
    Ok(compat::current_creds().euid as u64)
}

/// `getegid` (108): effective group id.
pub fn sys_getegid() -> Result<u64, Errno> {
    Ok(compat::current_creds().egid as u64)
}

/// `getresuid` (118): записать `(ruid, euid, suid)` в три пользовательских
/// указателя.
///
/// Все три указателя обязательны (Linux даёт `EFAULT` на плохой), и все
/// проверяются до первой записи, поэтому частичный результат ненаблюдаем.
pub fn sys_getresuid(ruid: u64, euid: u64, suid: u64) -> Result<u64, Errno> {
    let c = compat::current_creds();
    check_user_ptr_writable(ruid, 4)?;
    check_user_ptr_writable(euid, 4)?;
    check_user_ptr_writable(suid, 4)?;
    put_u32(ruid, c.uid)?;
    put_u32(euid, c.euid)?;
    put_u32(suid, c.suid)?;
    Ok(0)
}

/// `getresgid` (120): записать `(rgid, egid, sgid)` в три пользовательских
/// указателя.
pub fn sys_getresgid(rgid: u64, egid: u64, sgid: u64) -> Result<u64, Errno> {
    let c = compat::current_creds();
    check_user_ptr_writable(rgid, 4)?;
    check_user_ptr_writable(egid, 4)?;
    check_user_ptr_writable(sgid, 4)?;
    put_u32(rgid, c.gid)?;
    put_u32(egid, c.egid)?;
    put_u32(sgid, c.sgid)?;
    Ok(0)
}

/// `setuid` (105): [`Creds::setuid`], `EPERM`, когда переход отвергнут.
pub fn sys_setuid(uid: u64) -> Result<u64, Errno> {
    let c = compat::current_creds();
    match c.setuid(id_arg(uid)) {
        Some(next) => commit_creds(next),
        None => Err(Errno::EPERM),
    }
}

/// `setgid` (106): групповой аналог [`sys_setuid`].
pub fn sys_setgid(gid: u64) -> Result<u64, Errno> {
    let c = compat::current_creds();
    match c.setgid(id_arg(gid)) {
        Some(next) => commit_creds(next),
        None => Err(Errno::EPERM),
    }
}

/// `setreuid` (113): `(u32)-1` оставляет id без изменений.
pub fn sys_setreuid(uid: u64, euid: u64) -> Result<u64, Errno> {
    let c = compat::current_creds();
    match c.setreuid(optional_id_arg(uid), optional_id_arg(euid)) {
        Some(next) => commit_creds(next),
        None => Err(Errno::EPERM),
    }
}

/// `setregid` (114): групповой аналог [`sys_setreuid`].
pub fn sys_setregid(gid: u64, egid: u64) -> Result<u64, Errno> {
    let c = compat::current_creds();
    match c.setregid(optional_id_arg(gid), optional_id_arg(egid)) {
        Some(next) => commit_creds(next),
        None => Err(Errno::EPERM),
    }
}

/// `setresuid` (117): задать любое подмножество трёх user id.
pub fn sys_setresuid(uid: u64, euid: u64, suid: u64) -> Result<u64, Errno> {
    let c = compat::current_creds();
    match c.setresuid(
        optional_id_arg(uid),
        optional_id_arg(euid),
        optional_id_arg(suid),
    ) {
        Some(next) => commit_creds(next),
        None => Err(Errno::EPERM),
    }
}

/// `setresgid` (119): задать любое подмножество трёх group id.
pub fn sys_setresgid(gid: u64, egid: u64, sgid: u64) -> Result<u64, Errno> {
    let c = compat::current_creds();
    match c.setresgid(
        optional_id_arg(gid),
        optional_id_arg(egid),
        optional_id_arg(sgid),
    ) {
        Some(next) => commit_creds(next),
        None => Err(Errno::EPERM),
    }
}

/// `getgroups` (115): записать дополнительные группы в пользовательский массив.
///
/// Контракт Linux: `size == 0` — вернуть число групп и не трогать `list`;
/// `size < ngroups` — `EINVAL`; иначе записать ровно `ngroups` gid'ов (не `size`)
/// и вернуть их число. `list` проверяется на запись до первой записи, поэтому
/// частичный результат при плохом указателе ненаблюдаем.
pub fn sys_getgroups(size: u64, list: u64) -> Result<u64, Errno> {
    let creds = compat::current_creds();
    let groups = creds.groups();
    let n = groups.len() as u64;
    if size == 0 {
        return Ok(n);
    }
    if size < n {
        return Err(Errno::EINVAL);
    }
    check_user_ptr_writable(list, n * 4)?;
    for (i, gid) in groups.iter().enumerate() {
        let ptr = list + (i as u64) * 4;
        // SAFETY: весь диапазон `[list, list + n*4)` проверен на запись выше;
        // `write_unaligned` допускает невыровненный `gid_t*`, как и Linux.
        unsafe {
            core::ptr::write_unaligned(ptr as *mut u32, *gid);
        }
    }
    Ok(n)
}

/// `setgroups` (116): заменить дополнительные группы процесса.
///
/// Порядок проверок как в Linux: сначала привилегия (`CAP_SETGID` сведён к
/// `euid == 0`) → `EPERM`, затем предел `NGROUPS_MAX` (pagh:
/// [`super::cred::GROUPS_CAP`] = 32) → `EINVAL`, затем чтение массива
/// (`EFAULT`). `size == 0` разрешён и очищает набор, `list` при этом не читается.
/// Число gid'а не ограничивается: gid 0 разрешён, как и в Linux с `CAP_SETGID`.
pub fn sys_setgroups(size: u64, list: u64) -> Result<u64, Errno> {
    let creds = compat::current_creds();
    if !creds.is_root() {
        return Err(Errno::EPERM);
    }
    if size > super::cred::GROUPS_CAP as u64 {
        return Err(Errno::EINVAL);
    }
    let mut groups = [0u32; super::cred::GROUPS_CAP];
    if size > 0 {
        check_user_ptr(list, size * 4)?;
        for (i, slot) in groups.iter_mut().take(size as usize).enumerate() {
            let ptr = list + (i as u64) * 4;
            // SAFETY: диапазон `[list, list + size*4)` проверен на чтение выше;
            // `read_unaligned` допускает невыровненный `const gid_t*`.
            *slot = unsafe { core::ptr::read_unaligned(ptr as *const u32) };
        }
    }
    match creds.setgroups(&groups[..size as usize]) {
        Some(next) => commit_creds(next),
        // Недостижимо: euid и size проверены выше; страхует от дрейфа правил.
        None => Err(Errno::EINVAL),
    }
}

/// `getppid` (110): вернуть отслеживаемый pid родителя.
pub fn sys_getppid() -> Result<u64, Errno> {
    Ok(compat::current_ppid())
}

/// `gettid` (186): вернуть tid вызывающего потока (R2.10), откатываясь к pid для
/// контекста без compat-состояния.
pub fn sys_gettid() -> Result<u64, Errno> {
    let tid = compat::with_current_compat(|cs| cs.tid).unwrap_or_else(scheduler::current_pid);
    Ok(tid)
}

/// `gettimeofday` (96): заполнить пользовательский `struct timeval` стенным
/// временем (целые секунды с CMOS RTC, дробные микросекунды с тиковых часов) и
/// вернуть 0. Указатель таймзоны принимается и игнорируется.
pub fn sys_gettimeofday(tvptr: u64, _tzptr: u64) -> Result<u64, Errno> {
    if tvptr == 0 {
        return Ok(0);
    }
    check_user_ptr_writable(tvptr, core::mem::size_of::<Timeval>() as u64)?;
    let secs = rtc::now_unix() as i64;
    let usecs = ((scheduler::ticks() % TICK_HZ) * (NS_PER_TICK / 1000)) as i64;
    let tv = encode_timeval(secs, usecs);
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &tv as *const Timeval as *const u8,
            core::mem::size_of::<Timeval>(),
        )
    };
    // SAFETY: `tvptr` проверен на длину timeval выше.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), tvptr as *mut u8, bytes.len());
    }
    Ok(0)
}

/// `time` (201): вернуть стенные секунды, заодно записав их в необязательный
/// пользовательский указатель `time_t`, когда он не NULL.
pub fn sys_time(tptr: u64) -> Result<u64, Errno> {
    let secs = rtc::now_unix();
    if tptr != 0 {
        check_user_ptr_writable(tptr, 8)?;
        // SAFETY: `tptr` проверен на 8 байт выше.
        unsafe {
            *(tptr as *mut i64) = secs as i64;
        }
    }
    Ok(secs)
}

/// Преобразовать длительность `(sec, nsec)` в число тиков при [`TICK_HZ`] с
/// округлением вверх, чтобы любая ненулевая длительность спала хотя бы один тик.
/// Везде saturating.
pub(super) fn duration_to_ticks(sec: i64, nsec: i64) -> u64 {
    if sec <= 0 && nsec <= 0 {
        return 0;
    }
    let total_ns = (sec.max(0) as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(nsec.max(0) as u64);
    if total_ns == 0 {
        return 0;
    }
    let ticks = total_ns / NS_PER_TICK;
    if ticks == 0 {
        1
    } else {
        ticks
    }
}

/// Прочитать `struct timespec` (два i64) из проверенного пользовательского
/// указателя.
fn read_timespec(ptr: u64) -> Result<(i64, i64), Errno> {
    check_user_ptr(ptr, 16)?;
    // SAFETY: 16 байт по `ptr` проверены выше.
    let sec = unsafe { *(ptr as *const i64) };
    let nsec = unsafe { *((ptr + 8) as *const i64) };
    if !(0..1_000_000_000).contains(&nsec) || sec < 0 {
        return Err(Errno::EINVAL);
    }
    Ok((sec, nsec))
}

/// `nanosleep` (35): спать запрошенную `struct timespec` длительность по счётчику
/// тиков планировщика, вернуть 0. Указатель остатка игнорируется (в этой модели
/// сон не прерывается сигналами).
pub fn sys_nanosleep(req: u64, _rem: u64) -> Result<u64, Errno> {
    let (sec, nsec) = read_timespec(req)?;
    let ticks = duration_to_ticks(sec, nsec);
    if ticks > 0 && !sleep_interruptible(ticks) {
        // Доставимый сигнал толкнул сон; эпилог диспетчеризации доставит его
        // сразу после возврата из обработчика. (`rem` остаётся незаписанным —
        // эта минимальная модель его никогда не заполняла.)
        return Err(Errno::EINTR);
    }
    Ok(0)
}

/// `clock_nanosleep` (230): относительный сон относительно запрошенных часов.
/// Абсолютный сон (`TIMER_ABSTIME`) и id часов в этой минимальной модели
/// игнорируются; спит запрошенную относительную длительность, как `nanosleep`, и
/// возвращает 0. Прерывается доставимым сигналом (`-EINTR`), как `nanosleep`.
pub fn sys_clock_nanosleep(_clock_id: u64, _flags: u64, req: u64, _rem: u64) -> Result<u64, Errno> {
    let (sec, nsec) = read_timespec(req)?;
    let ticks = duration_to_ticks(sec, nsec);
    if ticks > 0 && !sleep_interruptible(ticks) {
        return Err(Errno::EINTR);
    }
    Ok(0)
}

/// Проспать `total` тиков кусками по одному, прерываясь досрочно, когда у
/// текущего процесса есть доставимый сигнал (возвращает false). Проверка идёт до
/// каждого куска, поэтому задержка пробуждения в худшем случае — один тик (~1 мс
/// при 1000 Гц).
fn sleep_interruptible(total: u64) -> bool {
    let mut left = total;
    while left > 0 {
        if super::signal::has_deliverable_current() {
            return false;
        }
        scheduler::sleep_ticks(1);
        left -= 1;
    }
    true
}

/// Linux x86_64 `struct sysinfo` (поля заполнены правдоподобными значениями).
#[repr(C)]
struct Sysinfo {
    uptime: i64,
    loads: [u64; 3],
    totalram: u64,
    freeram: u64,
    sharedram: u64,
    bufferram: u64,
    totalswap: u64,
    freeswap: u64,
    procs: u16,
    pad: u16,
    totalhigh: u64,
    freehigh: u64,
    mem_unit: u32,
    // Хвостовые паддинг-байты (`_f`), чтобы структура совпадала с 64-битной
    // раскладкой; C-определение дополняется до фиксированного размера, что
    // воспроизводит выравнивание `#[repr(C)]`.
    _f: [u8; 0],
}

/// Средние нагрузки за 1/5/15 минут в 16.16 fixed-point форме `struct sysinfo`
/// (ABI Linux: `si_loads[i] = load * 65536`).
///
/// `sysinfo(2)` раньше заполнял `loads` жёстко зашитыми нулями, тогда как
/// `/proc/loadavg` печатал настоящие затухающие средние планировщика: одно ядро,
/// два ответа на один вопрос, поэтому userspace-инструмент, читающий `sysinfo`,
/// показывал `0.00 0.00 0.00` на загруженной машине. Оба представления теперь
/// берутся из [`crate::task::scheduler::load_averages`]; функция лишь переводит
/// сотые (`123` = 1.23) в fixed-point ABI ядра. Перевод закреплён in-QEMU
/// selftest'ом, потому что «сырые сотые там, где ABI требует 16.16» — ровно та
/// ошибка единиц, которая читается как правдоподобное число.
pub fn sysinfo_loads() -> [u64; 3] {
    let (one, five, fifteen) = scheduler::load_averages();
    let fixed = |hundredths: u32| -> u64 {
        (hundredths as u64).saturating_mul(SYSTEM_LOAD_FIXED_ONE) / 100
    };
    [fixed(one), fixed(five), fixed(fifteen)]
}

/// Одна единица нагрузки в 16.16 fixed-point `struct sysinfo`.
pub const SYSTEM_LOAD_FIXED_ONE: u64 = 1 << 16;

/// `sysinfo` (99): заполнить пользовательский `struct sysinfo` аптаймом (из
/// часов тиков), средними нагрузками (16.16 fixed point, тот же источник, что
/// `/proc/loadavg`) и общим/свободным объёмом RAM (из счётчиков кадров PMM),
/// вернув 0.
pub fn sys_sysinfo(info: u64) -> Result<u64, Errno> {
    check_user_ptr_writable(info, core::mem::size_of::<Sysinfo>() as u64)?;
    let uptime = (scheduler::ticks() / TICK_HZ) as i64;
    let totalram = crate::memory::pmm::total_frames() as u64 * 4096;
    let freeram = crate::memory::pmm::free_frames() as u64 * 4096;
    let si = Sysinfo {
        uptime,
        loads: sysinfo_loads(),
        totalram,
        freeram,
        sharedram: 0,
        bufferram: 0,
        totalswap: 0,
        freeswap: 0,
        procs: 1,
        pad: 0,
        totalhigh: 0,
        freehigh: 0,
        mem_unit: 1,
        _f: [],
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &si as *const Sysinfo as *const u8,
            core::mem::size_of::<Sysinfo>(),
        )
    };
    // SAFETY: `info` проверен на длину sysinfo выше.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), info as *mut u8, bytes.len());
    }
    Ok(0)
}

/// `sched_yield` (24): уступить CPU планировщику и вернуть 0.
pub fn sys_sched_yield() -> Result<u64, Errno> {
    scheduler::yield_current();
    Ok(0)
}

/// `rt_sigaction` (13): установить или прочитать диспозицию для `sig`.
///
/// Пользовательский ABI — `struct kernel_sigaction` (32 байта при
/// `sigsetsize == 8`): `{ handler, flags, restorer, mask }`. Адрес обработчика
/// хранится дословно — `SIG_DFL` (0) и `SIG_IGN` (1) — специальные значения, — а
/// доставка применяет `sa_mask` на время обработчика
/// ([`super::signal::deliver_one_pending_syscall`]).
///
/// `oldact`, если запрошен, получает ТЕКУЩУЮ диспозицию, поэтому
/// save-and-restore пользователи (bash, readline) делают точный круговой обход.
/// Диспозиции `SIGKILL` и `SIGSTOP` менять нельзя (`EINVAL`, как в Linux).
pub fn sys_rt_sigaction(sig: u64, act: u64, oldact: u64, sigsetsize: u64) -> Result<u64, Errno> {
    if sig == 0 || sig > super::signal_frame::SIGNAL_COUNT as u64 {
        return Err(Errno::EINVAL);
    }
    if (sig == crate::arch::x86_64::linux::signal_frame::SIGKILL
        || sig == crate::arch::x86_64::linux::signal_frame::SIGSTOP)
        && act != 0
    {
        return Err(Errno::EINVAL);
    }
    if act != 0 {
        check_user_ptr(act, 32)?;
        let mut b = [0u8; 32];
        // SAFETY: диапазон проверен выше.
        unsafe {
            core::ptr::copy_nonoverlapping(act as *const u8, b.as_mut_ptr(), 32);
        }
        let handler = u64::from_le_bytes(b[0..8].try_into().unwrap());
        let flags = u64::from_le_bytes(b[8..16].try_into().unwrap());
        let restorer = u64::from_le_bytes(b[16..24].try_into().unwrap());
        let mask = u64::from_le_bytes(b[24..32].try_into().unwrap());
        if flags & crate::arch::x86_64::linux::signal_frame::SA_RESTORER == 0 {
            // glibc всегда ставит SA_RESTORER; обработчик, возвращающийся без
            // него, не имеет пути назад в ядро (Linux дал бы page fault).
            crate::warn!(
                "[linux] rt_sigaction: sig={} without SA_RESTORER (handler return will fault)",
                sig
            );
        }
        compat::with_current_compat(|cs| {
            cs.sig.lock().handlers[(sig - 1) as usize] =
                crate::arch::x86_64::linux::signal_frame::SignalAction {
                    handler,
                    flags,
                    restorer,
                    mask,
                };
        })
        .ok_or(Errno::EINVAL)?;
    }
    if oldact != 0 {
        let cur = compat::current_action(sig).ok_or(Errno::EINVAL)?;
        check_user_ptr_writable(oldact, 32)?;
        let mut b = [0u8; 32];
        b[0..8].copy_from_slice(&cur.handler.to_le_bytes());
        b[8..16].copy_from_slice(&cur.flags.to_le_bytes());
        b[16..24].copy_from_slice(&cur.restorer.to_le_bytes());
        b[24..32].copy_from_slice(&cur.mask.to_le_bytes());
        // SAFETY: диапазон проверен выше.
        unsafe {
            core::ptr::copy_nonoverlapping(b.as_ptr(), oldact as *mut u8, 32);
        }
    }
    Ok(0)
}

/// `rt_sigprocmask` (14): прочитать/сменить маску заблокированных сигналов
/// вызывающего потока. `how` — `SIG_BLOCK`/`SIG_UNBLOCK`/`SIG_SETMASK`; биты
/// `SIGKILL` и `SIGSTOP` никогда не могут оказаться заблокированными
/// ([`apply_mask_op`](super::signal_frame::apply_mask_op) их вычищает).
/// Реализованы только младшие 64 бита sigset (сигналы 1..=64); больший
/// `sigsetsize` принимается, остальные байты игнорируются.
pub fn sys_rt_sigprocmask(how: u64, set: u64, oldset: u64, sigsetsize: u64) -> Result<u64, Errno> {
    let size = if sigsetsize != 0 { sigsetsize } else { 8 };
    if size < 8 {
        return Err(Errno::EINVAL);
    }
    // F-r4-arch-1: user-память не трогается под `COMPAT_STATES`. Входную маску
    // читаем и входной указатель проверяем ДО реестра; выходной указатель
    // проверяем на право записи тоже до него. Раньше store в present-read-only
    // `oldset` шёл ВНУТРИ замыкания, давал ring-0 #PF, а kill-путь повторно
    // брал нереентерабельный спинлок и вешал CPU.
    let requested = if set != 0 {
        check_user_ptr(set, 8)?;
        let mut b = [0u8; 8];
        // SAFETY: диапазон проверен выше.
        unsafe {
            core::ptr::copy_nonoverlapping(set as *const u8, b.as_mut_ptr(), 8);
        }
        Some(u64::from_le_bytes(b))
    } else {
        None
    };
    if oldset != 0 {
        check_user_ptr_writable(oldset, 8)?;
    }
    // Под локом — только чтение/запись compat-состояния: вычислить старую маску
    // и применить новую. `oldset` в Linux — ПРЕДЫДУЩАЯ маска, поэтому снимок
    // берётся до применения.
    let previous = compat::with_current_compat(|cs| {
        let previous = cs.sig_blocked;
        if let Some(mask) = requested {
            match crate::arch::x86_64::linux::signal_frame::apply_mask_op(how, cs.sig_blocked, mask)
            {
                Some(new) => cs.sig_blocked = new,
                None => return Err(Errno::EINVAL),
            }
        }
        Ok(previous)
    })
    .ok_or(Errno::EINVAL)??;
    if oldset != 0 {
        let le = previous.to_le_bytes();
        // SAFETY: указатель проверен (включая право записи) до входа в реестр;
        // COW-страница будет скопирована обработчиком #PF уже БЕЗ лока.
        unsafe {
            core::ptr::copy_nonoverlapping(le.as_ptr(), oldset as *mut u8, 8);
        }
    }
    Ok(0)
}

/// `sigaltstack` (131): зарегистрировать или прочитать альтернативный стек
/// сигналов (на нём выполняются обработчики
/// [`SA_ONSTACK`](super::signal_frame::SA_ONSTACK)). 24-байтный ABI `stack_t` —
/// `{ ss_sp, ss_flags(+pad), ss_size }`. Регистрация стека, когда мы уже
/// выполняемся НА альтернативном стеке, не детектируется (проверке нужен
/// прерванный RSP, которого этот обработчик не получает); glibc никогда не
/// переключает altstack изнутри обработчика, так что упущение безвредно.
pub fn sys_sigaltstack(ss: u64, old_ss: u64) -> Result<u64, Errno> {
    // F-r4-arch-1: см. `sys_rt_sigprocmask`. 24-байтный `old_ss` копировался в
    // user-память внутри реестра; вход разбирается до лока, выход пишется после.
    let requested = if ss != 0 {
        check_user_ptr(ss, 24)?;
        let mut b = [0u8; 24];
        // SAFETY: диапазон проверен выше.
        unsafe {
            core::ptr::copy_nonoverlapping(ss as *const u8, b.as_mut_ptr(), 24);
        }
        let sp = u64::from_le_bytes(b[0..8].try_into().unwrap());
        let flags = u32::from_le_bytes(b[8..12].try_into().unwrap());
        let size = u64::from_le_bytes(b[16..24].try_into().unwrap());
        if flags & crate::arch::x86_64::linux::signal_frame::SS_DISABLE == 0
            && size < crate::arch::x86_64::linux::signal_frame::MINSIGSTKSZ
        {
            return Err(Errno::EINVAL);
        }
        Some(crate::arch::x86_64::linux::signal_frame::SigAltStack { sp, flags, size })
    } else {
        None
    };
    if old_ss != 0 {
        check_user_ptr_writable(old_ss, 24)?;
    }
    let previous = compat::with_current_compat(|cs| {
        let previous = cs.sig_altstack;
        if let Some(next) = requested {
            cs.sig_altstack = next;
        }
        previous
    })
    .ok_or(Errno::EINVAL)?;
    if old_ss != 0 {
        let mut b = [0u8; 24];
        b[0..8].copy_from_slice(&previous.sp.to_le_bytes());
        b[8..12].copy_from_slice(&previous.flags.to_le_bytes());
        b[16..24].copy_from_slice(&previous.size.to_le_bytes());
        // SAFETY: указатель проверен (включая право записи) до входа в реестр.
        unsafe {
            core::ptr::copy_nonoverlapping(b.as_ptr(), old_ss as *mut u8, 24);
        }
    }
    Ok(0)
}

/// `set_robust_list` (273): принять и вернуть 0 (поддержки futex/robust-списков
/// нет).
pub fn sys_set_robust_list(head: u64, len: u64) -> Result<u64, Errno> {
    const ROBUST_HEAD_SIZE: u64 = 24;
    if len != ROBUST_HEAD_SIZE {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(head, len)?;
    compat::with_current_compat(|cs| {
        cs.robust_head = head;
        cs.robust_len = len;
    })
    .ok_or(Errno::EINVAL)?;
    Ok(0)
}
pub fn sys_get_robust_list(pid: u64, head_ptr: u64, len_ptr: u64) -> Result<u64, Errno> {
    let current = scheduler::current_pid();
    if pid != 0 && pid != current {
        return Err(Errno::ESRCH);
    }
    check_user_ptr_writable(head_ptr, 8)?;
    check_user_ptr_writable(len_ptr, 8)?;
    let (head, len) = compat::current_robust_list();
    unsafe {
        core::ptr::write_unaligned(head_ptr as *mut u64, head);
        core::ptr::write_unaligned(len_ptr as *mut u64, len)
    }
    Ok(0)
}

/// `rseq` (334): restartable sequences не поддерживаются. Возврат 0 (а не
/// `-ENOSYS`) позволяет glibc-подобной инициализации не уходить в запасной путь;
/// rseq — чистая оптимизация, поэтому успех без регистрации безвреден.
pub fn sys_rseq() -> Result<u64, Errno> {
    Ok(0)
}

/// Linux `struct rlimit` / `rlimit64`: текущий и максимальный (жёсткий) лимит.
#[repr(C)]
struct Rlimit {
    rlim_cur: u64,
    rlim_max: u64,
}

/// `RLIM_INFINITY` — без лимита.
const RLIM_INFINITY: u64 = u64::MAX;
/// Id ресурса `RLIMIT_STACK`.
const RLIMIT_STACK: u64 = 3;
/// Id ресурса `RLIMIT_NOFILE`.
const RLIMIT_NOFILE: u64 = 7;
/// Число ресурсов rlimit, определённых Linux (`RLIMIT_NLIMITS`); большие id
/// отвергаются с `EINVAL`.
pub(crate) const RLIMIT_NLIMITS: u64 = 16;

/// Пара `(cur, max)` rlimit по умолчанию ядра для `resource`, когда у процесса нет
/// переопределения `prlimit64`/`setrlimit`. Жёсткий лимит `RLIMIT_NOFILE` — это
/// ёмкость таблицы дескрипторов (`io_sys::NOFILE_MAX`).
pub(crate) fn default_rlimit(resource: u64) -> (u64, u64) {
    match resource {
        RLIMIT_STACK => (8 * 1024 * 1024, RLIM_INFINITY), // мягкий лимит стека 8 МиБ
        RLIMIT_NOFILE => (1024, super::io_sys::NOFILE_MAX), // лимиты открытых файлов
        _ => (RLIM_INFINITY, RLIM_INFINITY),
    }
}

/// Эффективный `(cur, max)` rlimit текущего процесса для `resource`:
/// per-process переопределение из [`CompatState`](crate::task::compat::CompatState),
/// если оно есть, иначе умолчание ядра.
fn current_rlimit(resource: u64) -> (u64, u64) {
    compat::with_current_compat(|cs| match cs.rlimits.get(&resource) {
        Some(lim) => *lim,
        None => default_rlimit(resource),
    })
    .unwrap_or_else(|| default_rlimit(resource))
}

/// Записать эффективный `Rlimit` текущего процесса для `resource` в проверенный
/// пользовательский указатель. Неизвестные id ресурсов отвергаются (`EINVAL`).
fn write_rlimit(ptr: u64, resource: u64) -> Result<(), Errno> {
    if resource >= RLIMIT_NLIMITS {
        return Err(Errno::EINVAL);
    }
    check_user_ptr_writable(ptr, core::mem::size_of::<Rlimit>() as u64)?;
    let (cur, max) = current_rlimit(resource);
    let rl = Rlimit {
        rlim_cur: cur,
        rlim_max: max,
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &rl as *const Rlimit as *const u8,
            core::mem::size_of::<Rlimit>(),
        )
    };
    // SAFETY: `ptr` проверен на длину rlimit выше.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
    }
    Ok(())
}

/// `prlimit64` (302): сообщить эффективный лимит текущего процесса для
/// `resource` через `old_limit` (если не NULL); не NULL `new_limit`
/// (пользовательский указатель на `struct rlimit64`) обновляет per-process
/// переопределение для будущих запросов. Лимиты не принудительно
/// применяются. Неизвестные id ресурсов и `rlim_cur > rlim_max` дают `EINVAL`.
pub fn sys_prlimit64(
    _pid: u64,
    resource: u64,
    new_limit: u64,
    old_limit: u64,
) -> Result<u64, Errno> {
    if resource >= RLIMIT_NLIMITS {
        return Err(Errno::EINVAL);
    }
    if new_limit != 0 {
        check_user_ptr(new_limit, core::mem::size_of::<Rlimit>() as u64)?;
        // SAFETY: `new_limit` проверен на длину rlimit выше.
        let rl = unsafe { core::ptr::read_unaligned(new_limit as *const Rlimit) };
        if rl.rlim_cur > rl.rlim_max {
            return Err(Errno::EINVAL);
        }
        compat::with_current_compat(|cs| {
            cs.rlimits.insert(resource, (rl.rlim_cur, rl.rlim_max));
        });
    }
    if old_limit != 0 {
        write_rlimit(old_limit, resource)?;
    }
    Ok(0)
}

/// `getrlimit` (97): сообщить разумный лимит ресурса и вернуть 0.
pub fn sys_getrlimit(resource: u64, rlim: u64) -> Result<u64, Errno> {
    write_rlimit(rlim, resource)?;
    Ok(0)
}

/// `setsid` (112): начать новую сессию; вызывающий становится лидером сессии и
/// лидером новой группы процессов (сами сессии не моделируются).
pub fn sys_setsid() -> Result<u64, Errno> {
    let pid = scheduler::current_pid();
    compat::with_current_compat(|cs| {
        cs.pgid = cs.tgid;
    });
    Ok(pid)
}

/// `setpgid` (109): перевести вызывающего или одного из его детей в `pgid`.
///
/// `pid == 0` означает вызывающего, `pgid == 0` — собственный pid цели. Цель
/// должна быть вызывающим или прямым ребёнком.
pub fn sys_setpgid(pid: u64, pgid: u64) -> Result<u64, Errno> {
    let me = scheduler::current_pid();
    let target = if pid == 0 { me } else { pid };
    let new_pgid = if pgid == 0 { target } else { pgid };
    let Some((ppid, _current)) = compat::with_pid_compat(target, |cs| (cs.ppid, cs.pgid)) else {
        return Err(Errno::ESRCH);
    };
    if target != me && ppid != me {
        return Err(Errno::ESRCH);
    }
    compat::set_pgid(target, new_pgid);
    Ok(0)
}

/// `getpgid` (132): группа процессов `pid`, либо вызывающего при `pid == 0`.
pub fn sys_getpgid(pid: u64) -> Result<u64, Errno> {
    let target = if pid == 0 {
        scheduler::current_pid()
    } else {
        pid
    };
    compat::pgid_of(target).ok_or(Errno::ESRCH)
}

/// `getpgrp` (111): группа процессов вызывающего.
pub fn sys_getpgrp() -> Result<u64, Errno> {
    sys_getpgid(0)
}

/// `umask` (95): задать маску создания режима файлов процесса `mask & 0777` и
/// вернуть прежнюю маску. Маска применяется к битам прав свежесозданных
/// файлов/каталогов (см. обработчики `open`/`mkdir`).
pub fn sys_umask(mask: u64) -> Result<u64, Errno> {
    let old = compat::with_current_compat(|cs| {
        let old = cs.umask;
        cs.umask = (mask & 0o777) as u32;
        old
    })
    .unwrap_or(0o022);
    Ok(old as u64)
}

/// `flock` (73): рекомендательные блокировки бессмысленны при одном пользователе
/// и внутрипроцессном VFS; сообщить успех, чтобы блокировка swap-файла nvim
/// пошла дальше.
pub fn sys_flock(_fd: u64, _op: u64) -> Result<u64, Errno> {
    Ok(0)
}

/// Записать один `u32` в проверенный пользовательский указатель.
fn write_user_u32(ptr: u64, value: u32) -> Result<(), Errno> {
    check_user_ptr_writable(ptr, 4)?;
    super::io_sys::copy_out_pub(ptr, &value.to_le_bytes())
}

/// `getcpu` (309): pagh — один CPU, один NUMA-узел.
pub fn sys_getcpu(cpu: u64, node: u64, _tcache: u64) -> Result<u64, Errno> {
    if cpu != 0 {
        write_user_u32(cpu, 0)?;
    }
    if node != 0 {
        write_user_u32(node, 0)?;
    }
    Ok(0)
}

/// `sched_getaffinity` (204): маска сродства — всегда ровно CPU 0.
///
/// `pid` должен называть вызывающего (`0` или `getpid`); маска — одно 64-битное
/// слово, поэтому `cpusetsize < 8` — `EINVAL`. Возвращает число записанных
/// байт (8), как соглашение Linux «сколько байт скопировано».
pub fn sys_sched_getaffinity(pid: u64, cpusetsize: u64, mask: u64) -> Result<u64, Errno> {
    let me = compat::with_current_compat(|cs| cs.tgid).unwrap_or(0);
    if pid != 0 && pid != me {
        return Err(Errno::EINVAL);
    }
    if cpusetsize < 8 || mask == 0 {
        return Err(Errno::EINVAL);
    }
    check_user_ptr_writable(mask, 8)?;
    super::io_sys::copy_out_pub(mask, &1u64.to_le_bytes())?;
    Ok(8)
}

/// `sched_setaffinity` (203): принимать только маски, включающие CPU 0.
pub fn sys_sched_setaffinity(pid: u64, cpusetsize: u64, mask: u64) -> Result<u64, Errno> {
    let me = compat::with_current_compat(|cs| cs.tgid).unwrap_or(0);
    if pid != 0 && pid != me {
        return Err(Errno::EINVAL);
    }
    if cpusetsize < 8 || mask == 0 {
        return Err(Errno::EINVAL);
    }
    check_user_ptr(mask, 8)?;
    let raw = super::io_sys::copy_in_pub(mask, 8);
    let bits = u64::from_le_bytes(raw[..8].try_into().unwrap_or([0; 8]));
    if bits & 1 == 0 {
        return Err(Errno::EINVAL);
    }
    Ok(0)
}

/// `fadvise64` (221): совет рекомендателен; проверить дескриптор и принять.
pub fn sys_fadvise64(fd: u64, _offset: u64, _len: u64, advice: u64) -> Result<u64, Errno> {
    if advice > 5 {
        return Err(Errno::EINVAL);
    }
    let open = compat::with_current_compat(|cs| cs.fds.get(fd as u32).is_some()).unwrap_or(false);
    if !open {
        return Err(Errno::EBADF);
    }
    Ok(0)
}

/// `setrlimit` (160): `prlimit64` для вызывающего процесса, старый лимит не
/// нужен.
pub fn sys_setrlimit(resource: u64, new_limit: u64) -> Result<u64, Errno> {
    sys_prlimit64(0, resource, new_limit, 0)
}

/// `getrusage` (98): Linux `struct rusage` — 144 байта. pagh пока не учитывает
/// per-process CPU-время и faults, поэтому вся структура обнуляется — та же
/// честная подмена, что обнулённые поля в `/proc/<pid>/stat`, а не выдуманные
/// числа.
pub fn sys_getrusage(who: u64, usage: u64) -> Result<u64, Errno> {
    if who > 2 && who != u64::MAX {
        return Err(Errno::EINVAL);
    }
    if usage == 0 {
        return Err(Errno::EFAULT);
    }
    check_user_ptr_writable(usage, 144)?;
    super::io_sys::copy_out_pub(usage, &[0u8; 144])?;
    Ok(0)
}

/// `times` (100): вернуть счётчик тиков планировщика и обнулённый `struct tms`.
pub fn sys_times(tms: u64) -> Result<u64, Errno> {
    if tms != 0 {
        check_user_ptr_writable(tms, 32)?;
        super::io_sys::copy_out_pub(tms, &[0u8; 32])?;
    }
    Ok(scheduler::ticks())
}

/// `readahead` (187): рекомендательная предвыборка; проверить fd и принять.
pub fn sys_readahead(fd: u64, _offset: u64, _count: u64) -> Result<u64, Errno> {
    let open = compat::with_current_compat(|cs| cs.fds.get(fd as u32).is_some()).unwrap_or(false);
    if !open {
        return Err(Errno::EBADF);
    }
    Ok(0)
}

/// `syncfs` (306): путь записи VFS/ext2 синхронный, сбрасывать нечего; проверить
/// fd, как Linux.
pub fn sys_syncfs(fd: u64) -> Result<u64, Errno> {
    let open = compat::with_current_compat(|cs| cs.fds.get(fd as u32).is_some()).unwrap_or(false);
    if !open {
        return Err(Errno::EBADF);
    }
    Ok(0)
}

/// `getpriority` (140): nice 0 отображается в канонические 20.
pub fn sys_getpriority(_which: u64, _who: u64) -> Result<u64, Errno> {
    Ok(20)
}

/// `setpriority` (141): один CPU, классов приоритета планировщика нет;
/// принимается.
pub fn sys_setpriority(_which: u64, _who: u64, priority: u64) -> Result<u64, Errno> {
    if priority < 40 {
        Ok(0)
    } else {
        Err(Errno::EINVAL)
    }
}

/// `sched_getparam` (143): `struct sched_param { int sched_priority; }`.
pub fn sys_sched_getparam(_pid: u64, param: u64) -> Result<u64, Errno> {
    write_user_u32(param, 0)?;
    Ok(0)
}

/// `sched_setscheduler` (144): существует только SCHED_OTHER (0).
pub fn sys_sched_setscheduler(_pid: u64, policy: u64, _param: u64) -> Result<u64, Errno> {
    if policy == 0 {
        Ok(0)
    } else {
        Err(Errno::EINVAL)
    }
}

/// `sched_rr_get_interval` (148): нулевой timespec; SCHED_RR не реализован.
pub fn sys_sched_rr_get_interval(_pid: u64, interval: u64) -> Result<u64, Errno> {
    check_user_ptr_writable(interval, 16)?;
    super::io_sys::copy_out_pub(interval, &[0u8; 16])?;
    Ok(0)
}

/// `rt_sigpending` (127): ожидающие сигналы никогда не сообщаются.
pub fn sys_rt_sigpending(set: u64, sigsetsize: u64) -> Result<u64, Errno> {
    if sigsetsize != 8 {
        return Err(Errno::EINVAL);
    }
    check_user_ptr_writable(set, 8)?;
    super::io_sys::copy_out_pub(set, &0u64.to_le_bytes())?;
    Ok(0)
}

/// `sync_file_range` (277): обратной записи грязных страниц нет; проверить fd.
pub fn sys_sync_file_range(fd: u64, _offset: u64, _nbytes: u64, _flags: u64) -> Result<u64, Errno> {
    let open = compat::with_current_compat(|cs| cs.fds.get(fd as u32).is_some()).unwrap_or(false);
    if !open {
        return Err(Errno::EBADF);
    }
    Ok(0)
}

/// `fallocate` (285): режим 0 растягивает файл до `offset + len`; остальные режимы
/// (punch-hole, collapse, zero-range) не реализованы.
pub fn sys_fallocate(fd: u64, mode: u64, offset: u64, len: u64) -> Result<u64, Errno> {
    if mode != 0 {
        return Err(Errno::EINVAL);
    }
    let node = super::io_sys::file_node_for_fd(fd as u32).ok_or(Errno::EBADF)?;
    let end = offset.checked_add(len).ok_or(Errno::EINVAL)?;
    node.truncate(end).map_err(|_| Errno::EIO)?;
    Ok(0)
}
