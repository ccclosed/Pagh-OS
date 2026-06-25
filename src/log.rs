// log.rs — Leveled logging facade (ported from x86_64; routed to the riscv console).
//
// Provides `error!`/`warn!`/`info!`/`debug!`/`trace!` + a runtime level filter,
// streaming formatted output to the SBI/ns16550 console (`sbi::Console`). The raw
// `kprint!`/`kprintln!` macros (in `sbi`) remain the unconditional print path.
#![allow(dead_code)]

use core::fmt;
use core::sync::atomic::{AtomicU8, Ordering};

/// Severity levels (most severe = lowest number). A message at level `M` is
/// shown iff `M <= active`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Error => "[ERROR] ",
            Level::Warn => "[WARN] ",
            Level::Info => "[INFO] ",
            Level::Debug => "[DEBUG] ",
            Level::Trace => "[TRACE] ",
        }
    }
}

/// Active level, defaults to `Info`.
static ACTIVE_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

pub fn set_level(level: Level) {
    ACTIVE_LEVEL.store(level as u8, Ordering::Relaxed);
}

pub fn enabled(level: Level) -> bool {
    (level as u8) <= ACTIVE_LEVEL.load(Ordering::Relaxed)
}

/// Backing implementation for the logging macros. Level-checks first (skips all
/// formatting when filtered), then streams `tag` + `args` + newline to the
/// riscv console with no heap allocation.
pub fn _log(level: Level, args: fmt::Arguments) {
    if !enabled(level) {
        return;
    }
    use core::fmt::Write;
    let mut c = crate::sbi::Console;
    let _ = c.write_str(level.tag());
    let _ = c.write_fmt(args);
    let _ = c.write_str("\n");
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log::_log($crate::log::Level::Error, format_args!($($arg)*)) };
}
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log::_log($crate::log::Level::Warn, format_args!($($arg)*)) };
}
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log::_log($crate::log::Level::Info, format_args!($($arg)*)) };
}
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::log::_log($crate::log::Level::Debug, format_args!($($arg)*)) };
}
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => { $crate::log::_log($crate::log::Level::Trace, format_args!($($arg)*)) };
}
