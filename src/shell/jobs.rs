//! Минимальная таблица заданий для foreground-программ shell — **на сессию**.
//!
//! `lxrun` кладёт сюда остановленную программу, когда Ctrl+Z паркует её группу
//! процессов; `jobs` перечисляет её, `fg` возобновляет группу и ждёт, `bg`
//! возобновляет в фоне. Таблица — своя у каждой консольной сессии
//! (`drivers::session`): `jobs`/`bg`/`fg` в окне не должны видеть и тем более
//! трогать задание boot-shell'а (и наоборот). Для нативных потоков ядра
//! управления заданиями нет.

use alloc::string::String;
use alloc::vec::Vec;

use crate::drivers::session_route::MAX_SESSIONS;
use crate::sync::spinlock::Spinlock;

/// Одно запомненное задание.
pub struct Job {
    pub pid: u64,
    pub pgid: u64,
    pub command: String,
    pub stopped: bool,
}

/// Таблицы заданий по консольным сессиям (индекс — сессия).
static JOBS: [Spinlock<Vec<Job>>; MAX_SESSIONS] =
    [const { Spinlock::new(Vec::new()) }; MAX_SESSIONS];

fn slot(session: usize) -> usize {
    if session < MAX_SESSIONS {
        session
    } else {
        crate::drivers::session_route::BOOT_SESSION
    }
}

/// Таблица заданий текущей консольной сессии.
fn current_table() -> &'static Spinlock<Vec<Job>> {
    &JOBS[slot(crate::drivers::session::current())]
}

/// Записать (или заменить) задание по pid в таблице текущей сессии.
pub fn record(pid: u64, pgid: u64, command: String, stopped: bool) {
    let mut jobs = current_table().lock();
    if let Some(job) = jobs.iter_mut().find(|job| job.pid == pid) {
        job.pgid = pgid;
        job.command = command;
        job.stopped = stopped;
        return;
    }
    jobs.push(Job {
        pid,
        pgid,
        command,
        stopped,
    });
}

/// Убрать задание (оно вышло) из таблицы текущей сессии.
pub fn remove(pid: u64) {
    current_table().lock().retain(|job| job.pid != pid);
}

/// Снимок для `jobs` текущей сессии.
pub fn snapshot() -> Vec<(u64, u64, String, bool)> {
    current_table()
        .lock()
        .iter()
        .map(|job| (job.pid, job.pgid, job.command.clone(), job.stopped))
        .collect()
}

/// Самое недавно записанное задание текущей сессии, если есть.
pub fn last() -> Option<(u64, u64, String)> {
    current_table()
        .lock()
        .last()
        .map(|job| (job.pid, job.pgid, job.command.clone()))
}

/// Поставить флаг stopped для `pid` в таблице текущей сессии.
pub fn set_stopped(pid: u64, stopped: bool) {
    if let Some(job) = current_table().lock().iter_mut().find(|job| job.pid == pid) {
        job.stopped = stopped;
    }
}

/// Забыть все задания сессии (выход сессии/переоткрытие окна).
pub fn clear_session(session: usize) {
    JOBS[slot(session)].lock().clear();
}
