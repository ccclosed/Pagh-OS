// log.rs — Leveled logging facade over the active Console sinks
// 64-bit x86_64 OS kernel in Rust (#![no_std])
//
// Provides `error!`/`warn!`/`info!`/`debug!`/`trace!` macros plus a runtime
// level filter (`set_level`/`level`). The facade fans formatted output out to
// the active Console sinks (serial always, framebuffer when initialized) so
// callers do not invoke each sink separately.
//
// The raw `kprint!`/`kprintln!` macros (owned by `drivers::serial`) remain the
// unconditional serial print path for the shell and are intentionally NOT
// gated by this facade.

use core::fmt;
use core::sync::atomic::{AtomicU8, Ordering};

/// Severity levels, ordered from most severe (`Error`) to most verbose
/// (`Trace`). The discriminants encode the ordering used by the runtime
/// filter: a more verbose level has a numerically higher value.
///
/// With this numbering a message at level `M` is shown iff `M <= active`.
/// For the default active level `Info(2)`, `Error`/`Warn`/`Info` are shown
/// while `Debug`/`Trace` are filtered out.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    /// Reconstruct a `Level` from its stored `u8` discriminant. Out-of-range
    /// values saturate to `Trace` (the most verbose level).
    fn from_u8(v: u8) -> Level {
        match v {
            0 => Level::Error,
            1 => Level::Warn,
            2 => Level::Info,
            3 => Level::Debug,
            _ => Level::Trace,
        }
    }

    /// Human-readable prefix tag emitted before each formatted message.
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

/// The runtime-settable active level. Defaults to `Info`.
static ACTIVE_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// Set the active log level. Messages strictly more verbose than `level`
/// (numerically greater) are dropped before formatting.
pub fn set_level(level: Level) {
    ACTIVE_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Return the current active log level.
pub fn level() -> Level {
    Level::from_u8(ACTIVE_LEVEL.load(Ordering::Relaxed))
}

/// Whether a message at `level` would be emitted under the active filter.
///
/// Property 9 (level filter monotonicity): more verbose = higher number, so a
/// message at level `M` is enabled iff `M <= active`.
pub fn enabled(level: Level) -> bool {
    (level as u8) <= ACTIVE_LEVEL.load(Ordering::Relaxed)
}

/// Adapter that streams `core::fmt` output into a `Console` sink one chunk at
/// a time, so no intermediate heap `String` is ever built on the log path.
struct ConsoleWriter {
    sink: &'static dyn crate::drivers::Console,
}

impl fmt::Write for ConsoleWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.sink.write_str(s);
        Ok(())
    }
}

/// Stream `tag`, the formatted `args`, and a trailing newline into one sink.
fn write_to(sink: &'static dyn crate::drivers::Console, tag: &str, args: fmt::Arguments) {
    use core::fmt::Write;
    let mut w = ConsoleWriter { sink };
    // Errors are impossible (the sink swallows them) but ignore for safety.
    let _ = w.write_str(tag);
    let _ = w.write_fmt(args);
    let _ = w.write_str("\n");
}

/// Backing implementation for the logging macros.
///
/// Performs the level check FIRST and returns early when disabled, so the
/// formatting work (`write_fmt`) and any sink writes are skipped entirely for
/// filtered-out messages. No heap allocation occurs on this path: output is
/// streamed directly into each `Console` via [`ConsoleWriter`].
pub fn _log(level: Level, args: fmt::Arguments) {
    // Early-out before any formatting/sink work (Requirement 3.3, Property 9).
    if !enabled(level) {
        return;
    }

    let tag = level.tag();

    // Serial is the primary log sink and is always present.
    write_to(crate::drivers::serial::console(), tag, args);

    // Framebuffer sink: `Console::write_str` is a no-op until the framebuffer
    // has been initialized, so fanning out unconditionally is safe and only
    // produces output once the framebuffer console is live.
    //
    // Long-running background work (first-boot provisioning) can
    // pause the framebuffer mirror so its noisy per-chunk progress stays on
    // serial only and the interactive console is not flooded.
    // [WARN] chatter (watchdog / [DIAG] telemetry) stays off the
    // framebuffer unless explicitly enabled with the `warn on` shell command;
    // it was wrecking full-screen TUIs (nvim). Serial always logs everything.
    // [INFO] is muted from the framebuffer under the same toggle:
    // async "Compat_Process pid=N started" lines were interleaving with bash
    // output and garbling the screen. [ERROR] still always mirrors.
    let warn_muted = (level == Level::Warn || level == Level::Info)
        && !FB_WARN_MIRROR.load(core::sync::atomic::Ordering::Relaxed);
    if !FB_MIRROR_PAUSED.load(core::sync::atomic::Ordering::Relaxed) && !warn_muted {
        write_to(crate::drivers::framebuffer::console(), tag, args);
    }
}

/// When `false` (default), `warn!` lines are written to serial
/// only and skip the framebuffer console. Toggled by the `warn` shell command.
static FB_WARN_MIRROR: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Enable/disable mirroring of `[WARN]` log lines onto the framebuffer.
pub fn set_fb_warn_mirror(on: bool) {
    FB_WARN_MIRROR.store(on, core::sync::atomic::Ordering::Relaxed);
}

/// Whether `[WARN]` log lines are currently mirrored onto the framebuffer.
pub fn fb_warn_mirror() -> bool {
    FB_WARN_MIRROR.load(core::sync::atomic::Ordering::Relaxed)
}

/// When `true`, log macros skip the framebuffer sink (serial keeps everything).
/// Used by background tasks that must not spam the interactive console.
static FB_MIRROR_PAUSED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Pause/resume mirroring of log output onto the framebuffer console.
pub fn set_fb_mirror_paused(paused: bool) {
    FB_MIRROR_PAUSED.store(paused, core::sync::atomic::Ordering::Relaxed);
}

/// Whether the framebuffer log mirror is currently paused (see
/// [`set_fb_mirror_paused`]). Direct framebuffer printers that mirror
/// log-style chatter (e.g. the download progress bar) should check this.
pub fn fb_mirror_paused() -> bool {
    FB_MIRROR_PAUSED.load(core::sync::atomic::Ordering::Relaxed)
}

/// Log at `Error` severity.
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        $crate::log::_log($crate::log::Level::Error, format_args!($($arg)*))
    };
}

/// Log at `Warn` severity.
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        $crate::log::_log($crate::log::Level::Warn, format_args!($($arg)*))
    };
}

/// Log at `Info` severity.
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        $crate::log::_log($crate::log::Level::Info, format_args!($($arg)*))
    };
}

/// Log at `Debug` severity.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        $crate::log::_log($crate::log::Level::Debug, format_args!($($arg)*))
    };
}

/// Log at `Trace` severity.
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        $crate::log::_log($crate::log::Level::Trace, format_args!($($arg)*))
    };
}
