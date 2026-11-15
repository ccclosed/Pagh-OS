//! Минимальная таблица заданий для foreground-программ shell.
//!
//! `lxrun` кладёт сюда остановленную программу, когда Ctrl+Z паркует её группу
//! процессов; `jobs` перечисляет её, `fg` возобновляет группу и ждёт, `bg`
//! возобновляет в фоне. Таблица заданий одна на загрузку, и для нативных
//! потоков ядра управления заданиями нет.

use alloc::string::String;
use alloc::vec::Vec;

use crate::sync::spinlock::Spinlock;

/// Одно запомненное задание.
pub struct Job {
    pub pid: u64,
    pub pgid: u64,
    pub command: String,
    pub stopped: bool,
}

static JOBS: Spinlock<Vec<Job>> = Spinlock::new(Vec::new());

/// Записать (или заменить) задание по pid.
pub fn record(pid: u64, pgid: u64, command: String, stopped: bool) {
    let mut jobs = JOBS.lock();
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

/// Убрать задание (оно вышло).
pub fn remove(pid: u64) {
    JOBS.lock().retain(|job| job.pid != pid);
}

/// Снимок для `jobs`.
pub fn snapshot() -> Vec<(u64, u64, String, bool)> {
    JOBS.lock()
        .iter()
        .map(|job| (job.pid, job.pgid, job.command.clone(), job.stopped))
        .collect()
}

/// Самое недавно записанное задание, если есть.
pub fn last() -> Option<(u64, u64, String)> {
    JOBS.lock()
        .last()
        .map(|job| (job.pid, job.pgid, job.command.clone()))
}

/// Поставить флаг stopped для `pid`.
pub fn set_stopped(pid: u64, stopped: bool) {
    if let Some(job) = JOBS.lock().iter_mut().find(|job| job.pid == pid) {
        job.stopped = stopped;
    }
}
