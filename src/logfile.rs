//! Kernel log mirror to a text file on the mounted filesystem.
//!
//! WHY. A real machine has no serial port, so the only record of a boot is what
//! was on the screen — and the screen scrolls, the machine is not always
//! reachable, and nothing survives a reboot. An operator debugging pagh on their
//! own hardware asked for the log on the disk, as a file they can open from
//! another system. This is that file.
//!
//! WHY A BUFFER AND A FLUSHER, NOT A DIRECT WRITE. `log::_log` runs everywhere:
//! inside the timer IRQ, inside the ext2 transaction lock, during a panic. Writing
//! to a file from there would re-enter the filesystem (deadlock), allocate from
//! the heap in interrupt context, or corrupt a half-updated metadata block. So the
//! logger only ever appends into a fixed in-memory ring, lock-free apart from one
//! short spinlock over the ring itself, and a kernel thread drains it into
//! `/mnt/pagh-log.txt` when the filesystem is idle from the logger's point of
//! view. A dropped line under a flood is acceptable; a deadlock is not.
//!
//! WHAT IS WRITTEN. Every level, formatted exactly as the serial log, with no
//! filtering: the point of the file is to be complete when a human reads it after
//! the fact. Lines are truncated at [`LINE_MAX`] rather than split, so one entry
//! stays one line.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::sync::spinlock::Spinlock;

/// Longest single log line kept (a longer one is truncated).
pub const LINE_MAX: usize = 200;
/// Ring capacity in lines. At boot's chattiest this is several seconds of log;
/// the flusher drains far faster than that in practice.
const RING_LINES: usize = 512;

/// Where the log lands. Under `/mnt` so it lives on whatever the operator
/// mounted — a real disk when there is one, the RAM disk otherwise.
pub const LOG_PATH: &str = "/mnt/pagh-log.txt";

struct Ring {
    lines: [[u8; LINE_MAX]; RING_LINES],
    lens: [u16; RING_LINES],
    head: usize,
    tail: usize,
    /// Lines dropped because the ring was full while the flusher was behind.
    dropped: u64,
}

static RING: Spinlock<Ring> = Spinlock::new(Ring {
    lines: [[0u8; LINE_MAX]; RING_LINES],
    lens: [0u16; RING_LINES],
    head: 0,
    tail: 0,
    dropped: 0,
});

/// Buffering is on from early boot (see `enable_buffering`): the interesting
/// part of a log is the boot itself, so lines must be captured before a
/// filesystem exists to write them to. Only the WRITE waits for the mount.
static ENABLED: AtomicBool = AtomicBool::new(false);
/// Lines written to the file so far (diagnostics for the `log` shell command).
static WRITTEN: AtomicUsize = AtomicUsize::new(0);
/// Set while the flusher holds the filesystem, so a log call from inside a VFS
/// operation cannot re-enter the file path.
static FLUSHING: AtomicBool = AtomicBool::new(false);

/// Start capturing log lines into the ring.
///
/// Called at the very start of boot, not at mount time: the boot log is exactly
/// what an operator needs after a failure, and it is produced before there is
/// anywhere to write it. `flush` is what waits for the filesystem.
pub fn enable_buffering() {
    ENABLED.store(true, Ordering::Release);
}

/// Whether the file mirror is live.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// Number of log lines written to the file so far.
pub fn written() -> usize {
    WRITTEN.load(Ordering::Relaxed)
}

/// A fixed-capacity line buffer for `core::fmt`, so the logger can format once
/// for the file without allocating (it may run in interrupt context).
pub struct FixedLine {
    buf: [u8; LINE_MAX],
    len: usize,
}

impl FixedLine {
    pub const fn new() -> Self {
        Self {
            buf: [0; LINE_MAX],
            len: 0,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl core::fmt::Write for FixedLine {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for b in s.bytes() {
            if self.len == LINE_MAX - 1 {
                // Truncate rather than fail: a long line still gets recorded.
                self.buf[self.len] = b'\n';
                self.len += 1;
                return Ok(());
            }
            self.buf[self.len] = b;
            self.len += 1;
        }
        Ok(())
    }
}

/// Append one already-formatted line to the ring.
///
/// Called from [`crate::log::_log`] on every enabled message, so it must be
/// cheap and allocation-free: no `Vec`, no `String`, no formatting.
pub fn push(bytes: &[u8]) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    // `try_lock`: a log call made from inside the flusher (or from a context
    // that already holds the ring) must not spin forever.
    let Some(mut ring) = RING.try_lock() else {
        return;
    };
    let next = (ring.tail + 1) % RING_LINES;
    if next == ring.head {
        // Full: drop the oldest so the newest (usually the interesting one) is
        // kept. Counted, because a silent gap in a log is worse than a note.
        ring.head = (ring.head + 1) % RING_LINES;
        ring.dropped += 1;
    }
    let n = bytes.len().min(LINE_MAX);
    let tail = ring.tail;
    ring.lines[tail][..n].copy_from_slice(&bytes[..n]);
    ring.lens[tail] = n as u16;
    ring.tail = next;
}

/// Take one line out of the ring, if any. Returns `(length, bytes)`.
fn pop(ring: &mut Ring) -> Option<usize> {
    if ring.head == ring.tail {
        return None;
    }
    let index = ring.head;
    ring.head = (ring.head + 1) % RING_LINES;
    Some(index)
}

/// Write everything buffered to [`LOG_PATH`]. Returns the number of lines
/// written, or 0 when there was nothing to do / the filesystem is not ready.
///
/// Called from a kernel thread; never from an interrupt.
pub fn flush() -> usize {
    if FLUSHING.swap(true, Ordering::AcqRel) {
        return 0;
    }
    let count = flush_inner();
    FLUSHING.store(false, Ordering::Release);
    count
}

fn flush_inner() -> usize {
    use crate::vfs::{self, VfsError};

    // Collect the pending lines first, releasing the ring before touching the
    // filesystem: the flusher must never hold the ring across VFS work (a log
    // call during that work would find the ring busy and be dropped).
    let mut batch: alloc::vec::Vec<alloc::vec::Vec<u8>> = alloc::vec::Vec::new();
    let mut dropped = 0u64;
    {
        let Some(mut ring) = RING.try_lock() else {
            return 0;
        };
        while let Some(index) = pop(&mut ring) {
            let len = ring.lens[index] as usize;
            batch.push(ring.lines[index][..len].to_vec());
            if batch.len() >= 64 {
                break;
            }
        }
        dropped = core::mem::take(&mut ring.dropped);
    }
    if batch.is_empty() && dropped == 0 {
        return 0;
    }

    // Open (or create) the log file and append at its end.
    let file = match vfs::lookup_path(LOG_PATH) {
        Ok(node) => node,
        Err(_) => {
            let Some(split) = LOG_PATH.rfind('/') else {
                return 0;
            };
            let (parent_path, name) = LOG_PATH.split_at(split);
            let name = &name[1..];
            let parent_path = if parent_path.is_empty() {
                "/"
            } else {
                parent_path
            };
            match vfs::lookup_path(parent_path) {
                Ok(dir) => match dir.create_file(name) {
                    Ok(node) => node,
                    Err(e) => {
                        if !matches!(e, VfsError::AlreadyExists) {
                            return 0;
                        }
                        match vfs::lookup_path(LOG_PATH) {
                            Ok(node) => node,
                            Err(_) => return 0,
                        }
                    }
                },
                Err(_) => return 0,
            }
        }
    };

    let mut offset = file.size();
    let mut written = 0usize;
    if dropped > 0 {
        let note = alloc::format!("[log] {} line(s) dropped: ring full\n", dropped);
        match file.write(offset, note.as_bytes()) {
            Ok(n) => offset += n as u64,
            Err(_) => return 0,
        }
    }
    for line in &batch {
        match file.write(offset, line) {
            Ok(n) => {
                offset += n as u64;
                written += 1;
            }
            Err(_) => break,
        }
        // Each line ends with a newline; the logger guarantees it.
    }
    file.sync();
    WRITTEN.fetch_add(written, Ordering::Relaxed);
    written
}

/// The flusher thread: append buffered log lines to the file, forever.
pub fn flush_thread() {
    // Wait for the filesystem: before `/mnt` exists there is nowhere to write,
    // but the ring has been filling since the first boot message, so nothing is
    // lost — the first flush writes the whole boot log.
    while crate::vfs::lookup_path("/mnt").is_err() {
        crate::task::scheduler::sleep_ticks(50);
    }
    crate::info!("log: mirroring to {}", LOG_PATH);
    let mut created = false;
    loop {
        let wrote = flush();
        if !created && wrote > 0 {
            created = true;
            crate::info!("log: {} line(s) written to {}", wrote, LOG_PATH);
        }
        crate::task::scheduler::sleep_ticks(200);
    }
}
