//! Minimal job table for the shell's foreground programs.
//!
//! `lxrun` puts a stopped program here when Ctrl+Z parks its process group;
//! `jobs` lists it, `fg` resumes the group and waits, `bg` resumes it in the
//! background. There is one job table per boot and no job control for native
//! kernel threads.

use alloc::string::String;
use alloc::vec::Vec;

use crate::sync::spinlock::Spinlock;

/// One remembered job.
pub struct Job {
    pub pid: u64,
    pub pgid: u64,
    pub command: String,
    pub stopped: bool,
}

static JOBS: Spinlock<Vec<Job>> = Spinlock::new(Vec::new());

/// Record (or replace) a job by pid.
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

/// Drop a job (it exited).
pub fn remove(pid: u64) {
    JOBS.lock().retain(|job| job.pid != pid);
}

/// Snapshot for `jobs`.
pub fn snapshot() -> Vec<(u64, u64, String, bool)> {
    JOBS.lock()
        .iter()
        .map(|job| (job.pid, job.pgid, job.command.clone(), job.stopped))
        .collect()
}

/// The most recently recorded job, if any.
pub fn last() -> Option<(u64, u64, String)> {
    JOBS.lock()
        .last()
        .map(|job| (job.pid, job.pgid, job.command.clone()))
}

/// Set the stopped flag for `pid`.
pub fn set_stopped(pid: u64, stopped: bool) {
    if let Some(job) = JOBS.lock().iter_mut().find(|job| job.pid == pid) {
        job.stopped = stopped;
    }
}
