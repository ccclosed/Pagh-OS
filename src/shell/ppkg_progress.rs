//! `ppkg install` progress model and renderers — **pure** (`core` + `alloc` only).
//!
//! This is the deterministic half of the package-manager progress UI (the
//! effectful console driver lives in [`super::progress`]). It is host-included by
//! `host-tests`, so every visual rule — bar fill, unknown-total animation
//! position, percent/speed/size/ETA widgets, the ANSI repaint sequence and the
//! non-interactive fallback — is pinned by deterministic snapshot tests rather
//! than by watching a screen.
//!
//! ## Behaviour (the DNF-style reference, behaviour only)
//!
//! * Known total: `[====    ]` — `=` cells proportional to `done/total`, real
//!   percent, speed, size and ETA.
//! * Unknown total: the bar carries an animated `<=>` marker whose position is
//!   `(elapsed_ms / 500) % (inner_width - 4)` and the percent reads `???%`.
//! * Multi-line frames are redrawn at most every 100 ms (10 FPS); a completed
//!   package bar is printed once and then removed from the live frame; the total
//!   bar is drawn after a divider.
//! * ANSI consoles repaint with `ESC[<n>A` + `\r` + `ESC[K` per line and hide/show
//!   the cursor; a dumb console (the Pagh framebuffer has no cursor addressing)
//!   degrades to one live line redrawn with `\r` plus plain completed lines; a
//!   non-interactive/serial capture gets plain lines only.
//!
//! Nothing here emits bytes itself: [`AnsiEmitter`]/[`CrEmitter`] return the
//! exact string a driver should write, which is what makes them testable.
#![allow(dead_code)]

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// How the console can repaint (selected by the effectful driver).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProgressMode {
    /// ANSI/VT100: multi-line frames, cursor hide/show, `ESC[nA\rESC[K` clearing.
    Ansi,
    /// Dumb console with `\r` but no cursor addressing: one live line, completed
    /// bars printed once. This is the Pagh framebuffer/VT interactive mode.
    CarriageReturn,
    /// Non-interactive console, serial capture or `--quiet`: plain phase lines.
    Plain,
}

impl ProgressMode {
    /// Parse a `--progress=<mode>` argument.
    pub fn parse(value: &str) -> Option<ProgressMode> {
        match value {
            "ansi" => Some(ProgressMode::Ansi),
            "cr" | "carriage-return" => Some(ProgressMode::CarriageReturn),
            "plain" | "quiet" => Some(ProgressMode::Plain),
            _ => None,
        }
    }

    /// True if this mode draws live bars at all.
    pub fn is_live(self) -> bool {
        !matches!(self, ProgressMode::Plain)
    }
}

/// One phase of installing a package.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Fetching the `.deb` (the transport draws its own byte bar).
    Download,
    /// Checking the signed index digest before unpacking.
    Verify,
    /// Planning and writing the package's files.
    Unpack,
}

impl Phase {
    /// Short label used in the live bar.
    pub fn label(self) -> &'static str {
        match self {
            Phase::Download => "download",
            Phase::Verify => "verify",
            Phase::Unpack => "unpack",
        }
    }
}

/// The redraw gate: at most one repaint per this many milliseconds (10 FPS).
pub const REDRAW_INTERVAL_MS: u64 = 100;
/// Animation step of the unknown-total marker: one cell per 500 ms.
pub const UNKNOWN_STEP_MS: u64 = 500;
/// Width of a progress bar including its brackets (`[====    ]`).
pub const BAR_WIDTH: usize = 28;

/// Human-readable byte count: `B`/`KiB`/`MiB` with one decimal (integer math).
pub fn human_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if n >= MIB {
        format!("{}.{} MiB", n / MIB, (n % MIB) * 10 / MIB)
    } else if n >= KIB {
        format!("{}.{} KiB", n / KIB, (n % KIB) * 10 / KIB)
    } else {
        format!("{} B", n)
    }
}

/// A known-total bar of exactly `width` cells: `[====    ]`.
///
/// `width` includes the two brackets; an all-complete bar is `[==========]` and a
/// zero-progress bar is enclosed spaces. `total == 0` renders as zero progress so
/// callers cannot divide by zero (that is the unknown-total case — use
/// [`unknown_bar`] instead).
pub fn known_bar(width: usize, done: u64, total: u64) -> String {
    let width = width.max(4);
    let inner = width - 2;
    let filled = if total == 0 {
        0
    } else {
        ((inner as u128 * done as u128) / total as u128).min(inner as u128) as usize
    };
    let mut out = String::with_capacity(width);
    out.push('[');
    for i in 0..inner {
        out.push(if i < filled { '=' } else { ' ' });
    }
    out.push(']');
    out
}

/// Where the animated `<=>` marker starts for an unknown total.
///
/// `(elapsed_ms / 500) % (inner_width - 4)`, exactly the reference rule; widths
/// too small to hold the marker clamp to 0.
pub fn unknown_marker_pos(inner_width: usize, elapsed_ms: u64) -> usize {
    if inner_width <= 4 {
        0
    } else {
        ((elapsed_ms / UNKNOWN_STEP_MS) as usize) % (inner_width - 4)
    }
}

/// An unknown-total bar with the animated `<=>` marker: `[  <=>       ]`.
pub fn unknown_bar(width: usize, elapsed_ms: u64) -> String {
    let width = width.max(6);
    let inner = width - 2;
    let marker = unknown_marker_pos(inner, elapsed_ms);
    let mut cells = Vec::with_capacity(inner);
    for _ in 0..inner {
        cells.push(' ');
    }
    cells[marker] = '<';
    cells[marker + 1] = '=';
    cells[marker + 2] = '>';
    let mut out = String::with_capacity(width);
    out.push('[');
    for c in cells {
        out.push(c);
    }
    out.push(']');
    out
}

/// The right-aligned percent field: `100%`, ` 42%`, `???%` (always 4 columns).
pub fn percent_field(done: u64, total: u64, known: bool) -> String {
    if !known || total == 0 {
        return String::from("???%");
    }
    let pct = ((done as u128 * 100) / total as u128).min(100) as u64;
    format!("{:>3}%", pct)
}

/// `(speed, eta)` for a known byte total over `elapsed_ms`.
///
/// Speed is the average completed-bytes rate (`1.2 MiB/s`); ETA is the remaining
/// time at that rate as `mm:ss` (or `--:--` while the rate is unknown/zero).
pub fn speed_and_eta(done: u64, total: u64, elapsed_ms: u64) -> (String, String) {
    if elapsed_ms == 0 || done == 0 {
        return (String::from("--.-- MiB/s"), String::from("--:--"));
    }
    // Integer-only: the kernel has no floating-point context. bytes/s and the
    // remaining time are both exact enough at u128 width.
    let bytes_per_second =
        ((done as u128 * 1000) / elapsed_ms as u128).min(u64::MAX as u128) as u64;
    let speed_txt = format!("{}/s", human_bytes(bytes_per_second));
    if total <= done || total == 0 {
        return (speed_txt, String::from("00:00"));
    }
    let remaining_bytes = (total - done) as u128;
    let remaining_ms =
        ((remaining_bytes * elapsed_ms as u128) / done as u128).min(u64::MAX as u128) as u64;
    (speed_txt, eta_field(remaining_ms))
}

/// Format a duration as `mm:ss` (hours roll into minutes), saturating.
pub fn eta_field(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// One package in the transaction.
#[derive(Clone, Debug)]
pub struct PackageBar {
    /// Package name as the resolver planned it.
    pub name: String,
    /// Declared `.deb` size (known from the signed index).
    pub size: u64,
    /// Current phase.
    pub phase: Phase,
    /// Bytes of unpacked content written so far.
    pub unpacked: u64,
    /// Total unpacked content the plan will write (0 = not known yet).
    pub unpack_total: u64,
    /// Files created for this package (filled on completion).
    pub files: usize,
    /// Completed packages leave the live frame and are printed once.
    pub finished: bool,
}

/// The deterministic transaction model: plan, per-package phase/progress, total.
#[derive(Clone, Debug)]
pub struct ProgressModel {
    /// Console mode (only affects which widgets are rendered).
    pub mode: ProgressMode,
    /// Packages in resolver order.
    pub packages: Vec<PackageBar>,
    /// Index of the package currently being installed.
    pub active: Option<usize>,
    /// Scheduler milliseconds when the transaction started.
    pub started_ms: u64,
    /// Last time a frame was emitted (10 FPS gate).
    pub last_redraw_ms: u64,
    /// Sum of declared `.deb` sizes.
    pub total_bytes: u64,
    /// Declared sizes of completed packages.
    pub done_bytes: u64,
}

impl ProgressModel {
    /// A fresh model for `mode` starting at `now_ms`.
    pub fn new(mode: ProgressMode, now_ms: u64) -> ProgressModel {
        ProgressModel {
            mode,
            packages: Vec::new(),
            active: None,
            started_ms: now_ms,
            last_redraw_ms: now_ms,
            total_bytes: 0,
            done_bytes: 0,
        }
    }

    /// Install the resolver's plan: `(name, declared .deb size)` pairs.
    pub fn plan(&mut self, packages: &[(String, u64)]) {
        self.packages = packages
            .iter()
            .map(|(name, size)| PackageBar {
                name: name.clone(),
                size: *size,
                phase: Phase::Download,
                unpacked: 0,
                unpack_total: 0,
                files: 0,
                finished: false,
            })
            .collect();
        self.total_bytes = packages.iter().map(|(_, s)| *s).sum();
        self.active = if packages.is_empty() { None } else { Some(0) };
    }

    /// Move to `phase` (starting the package's bar if needed).
    pub fn begin_phase(&mut self, pkg: &str, phase: Phase) {
        if let Some(bar) = self.packages.iter_mut().find(|p| p.name == pkg) {
            bar.phase = phase;
        }
    }

    /// Record unpack progress. `total == 0` means "still unknown" (the scan pass).
    pub fn unpack_progress(&mut self, pkg: &str, done: u64, total: u64) {
        if let Some(bar) = self.packages.iter_mut().find(|p| p.name == pkg) {
            bar.unpacked = done;
            if total > 0 {
                bar.unpack_total = total;
            }
        }
    }

    /// Mark a package complete; returns the permanent line to print once.
    pub fn complete_package(&mut self, pkg: &str, files: usize) -> Option<String> {
        let idx = self.packages.iter().position(|p| p.name == pkg)?;
        let bar = &mut self.packages[idx];
        if bar.finished {
            return None;
        }
        bar.finished = true;
        bar.files = files;
        self.done_bytes = self.done_bytes.saturating_add(bar.size);
        let line = format!(
            "  [ok] {} ({} files, {})",
            bar.name,
            files,
            human_bytes(bar.size)
        );
        // Advance the active pointer to the next unfinished package.
        self.active = self.packages.iter().position(|p| !p.finished);
        Some(line)
    }

    /// The 10 FPS gate: may a frame be emitted at `now_ms`?
    pub fn redraw_due(&mut self, now_ms: u64) -> bool {
        if now_ms.saturating_sub(self.last_redraw_ms) < REDRAW_INTERVAL_MS {
            return false;
        }
        self.last_redraw_ms = now_ms;
        true
    }

    /// Overall percent (0..100) by declared `.deb` bytes.
    pub fn percent_done(&self) -> u64 {
        if self.total_bytes == 0 {
            return 0;
        }
        ((self.done_bytes as u128 * 100) / self.total_bytes as u128).min(100) as u64
    }

    /// The live frame lines: the active package bar, a divider and the total bar.
    ///
    /// Empty in [`ProgressMode::Plain`] (the caller prints plain phase lines via
    /// [`plain_phase_line`] and [`plain_plan_line`] instead).
    pub fn frame(&self, now_ms: u64) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        if self.mode == ProgressMode::Plain {
            return lines;
        }
        let elapsed = now_ms.saturating_sub(self.started_ms);
        if let Some(idx) = self.active {
            let bar = &self.packages[idx];
            let known = bar.phase == Phase::Unpack && bar.unpack_total > 0;
            let (bar_txt, pct, size_txt) = if known {
                (
                    known_bar(BAR_WIDTH, bar.unpacked, bar.unpack_total),
                    percent_field(bar.unpacked, bar.unpack_total, true),
                    format!(
                        "{} / {}",
                        human_bytes(bar.unpacked),
                        human_bytes(bar.unpack_total)
                    ),
                )
            } else {
                (
                    unknown_bar(BAR_WIDTH, elapsed),
                    String::from("???%"),
                    format!("{} ({})", phase_size_hint(bar), bar.phase.label()),
                )
            };
            let (speed, eta) = if known {
                speed_and_eta(bar.unpacked, bar.unpack_total, elapsed)
            } else {
                (String::from("--.-- MiB/s"), String::from("--:--"))
            };
            lines.push(format!(
                "  Installing {:<20} {} {}  {}  {}  ETA {}",
                bar.name, bar_txt, pct, size_txt, speed, eta
            ));
        }
        // Total progress is by the signed index's declared `.deb` bytes.
        let total_known = self.total_bytes > 0;
        let total_bar = if total_known {
            known_bar(BAR_WIDTH, self.done_bytes, self.total_bytes)
        } else {
            unknown_bar(BAR_WIDTH, elapsed)
        };
        let total_pct = percent_field(self.done_bytes, self.total_bytes, total_known);
        let (total_speed, total_eta) = if total_known {
            speed_and_eta(self.done_bytes, self.total_bytes, elapsed)
        } else {
            (String::from("--.-- MiB/s"), String::from("--:--"))
        };
        lines.push(String::from("  ------------------------------"));
        lines.push(format!(
            "  Total      {:<12} {} {}  {} / {}  {}  ETA {}",
            "",
            total_bar,
            total_pct,
            human_bytes(self.done_bytes),
            human_bytes(self.total_bytes),
            total_speed,
            total_eta
        ));
        lines
    }
}

/// Size hint shown during the unknown phases: the declared `.deb` size for
/// download/verify, or the unpacked bytes known so far while the plan is built.
fn phase_size_hint(bar: &PackageBar) -> String {
    match bar.phase {
        Phase::Download | Phase::Verify => human_bytes(bar.size),
        Phase::Unpack => {
            if bar.unpacked > 0 {
                human_bytes(bar.unpacked)
            } else {
                human_bytes(0)
            }
        }
    }
}

impl ProgressModel {
    /// The single live line used by [`ProgressMode::CarriageReturn`]: the active
    /// package bar plus a compact total summary, sized for an 80-column console.
    ///
    /// The Pagh framebuffer console has no cursor addressing, so it cannot repaint
    /// a stable multi-line block; one line redrawn with `\r` is the honest
    /// degradation, and completed packages are printed once above it.
    pub fn cr_line(&self, now_ms: u64) -> String {
        let elapsed = now_ms.saturating_sub(self.started_ms);
        let active = self.active.and_then(|i| self.packages.get(i));
        let pkg_part = match active {
            Some(bar) => {
                let known = bar.phase == Phase::Unpack && bar.unpack_total > 0;
                if known {
                    format!(
                        "{} {} {}",
                        bar.name,
                        known_bar(20, bar.unpacked, bar.unpack_total),
                        percent_field(bar.unpacked, bar.unpack_total, true)
                    )
                } else {
                    format!("{} {} ???%", bar.name, unknown_bar(20, elapsed))
                }
            }
            None => String::from("finishing"),
        };
        let total_pct = percent_field(self.done_bytes, self.total_bytes, self.total_bytes > 0);
        let (_, eta) = speed_and_eta(self.done_bytes, self.total_bytes, elapsed);
        format!(
            "  {} | total {} {}/{} ETA {}",
            pkg_part,
            total_pct,
            human_bytes(self.done_bytes),
            human_bytes(self.total_bytes),
            eta
        )
    }
}

/// The plain (non-interactive) plan line: one line per transaction, no ANSI.
pub fn plain_plan_line(count: usize, total_bytes: u64) -> String {
    format!(
        "ppkg: installing {} package(s), {} to download",
        count,
        human_bytes(total_bytes)
    )
}

/// The plain (non-interactive) phase line: emitted only for the long unpack phase
/// (download already prints the transport's single-line bar, and `ppkg` logs its
/// own `Get`/`Unpacked` lines everywhere).
pub fn plain_phase_line(pkg: &str, phase: Phase, size: u64) -> Option<String> {
    match phase {
        Phase::Unpack => Some(format!("ppkg: unpacking {} ({})", pkg, human_bytes(size))),
        _ => None,
    }
}

/// ANSI multi-line repainter: cursor hide/show and `ESC[nA\rESC[K` clearing.
#[derive(Clone, Debug, Default)]
pub struct AnsiEmitter {
    /// Lines currently on screen from the previous frame.
    prev_lines: usize,
    /// Whether the cursor was hidden (so it is shown exactly once at the end).
    hidden: bool,
}

impl AnsiEmitter {
    /// A fresh emitter.
    pub fn new() -> AnsiEmitter {
        AnsiEmitter::default()
    }

    /// Number of lines currently drawn by this emitter.
    pub fn lines(&self) -> usize {
        self.prev_lines
    }

    /// Begin a live session: hide the cursor (`ESC[?25l`).
    pub fn begin(&mut self) -> String {
        if self.hidden {
            return String::new();
        }
        self.hidden = true;
        String::from("\x1b[?25l")
    }

    /// Render one frame: move up over the previous frame, clear and replace each
    /// line. A shorter new frame is padded with cleared lines so the cursor ends
    /// up below exactly `max(previous, new)` lines.
    pub fn frame(&mut self, lines: &[String]) -> String {
        let mut out = String::new();
        let prev = self.prev_lines;
        if prev > 0 {
            out.push_str(&format!("\x1b[{}A", prev));
        }
        let draw = prev.max(lines.len());
        for i in 0..draw {
            out.push('\r');
            out.push_str("\x1b[K");
            if let Some(line) = lines.get(i) {
                out.push_str(line);
            }
            out.push('\n');
        }
        self.prev_lines = draw;
        out
    }

    /// End a live session: clear the frame, print `final_lines` once, show the
    /// cursor (`ESC[?25h`).
    pub fn finish(&mut self, final_lines: &[String]) -> String {
        let mut out = String::new();
        if self.prev_lines > 0 {
            out.push_str(&format!("\x1b[{}A", self.prev_lines));
            for _ in 0..self.prev_lines {
                out.push_str("\r\x1b[K\n");
            }
            self.prev_lines = 0;
        }
        for line in final_lines {
            out.push_str(line);
            out.push('\n');
        }
        if self.hidden {
            out.push_str("\x1b[?25h");
            self.hidden = false;
        }
        out
    }
}

/// Carriage-return single-line repainter for consoles without cursor addressing
/// (the Pagh framebuffer): completed bars are printed once, the live bar is
/// redrawn in place with `\r` and padded so a shorter frame erases the old one.
#[derive(Clone, Debug, Default)]
pub struct CrEmitter {
    /// Visible width of the currently drawn live line.
    prev_width: usize,
}

impl CrEmitter {
    /// A fresh emitter.
    pub fn new() -> CrEmitter {
        CrEmitter::default()
    }

    /// Repaint the live line (no newline).
    pub fn live(&mut self, line: &str) -> String {
        let width = line.chars().count();
        let mut out = String::new();
        out.push('\r');
        out.push_str(line);
        // Erase any tail left by a longer previous line (no ANSI on a dumb console).
        for _ in width..self.prev_width {
            out.push(' ');
        }
        self.prev_width = width.max(self.prev_width);
        out
    }

    /// Erase the live line without printing anything under it.
    pub fn clear(&mut self) -> String {
        let mut out = String::new();
        if self.prev_width > 0 {
            out.push('\r');
            for _ in 0..self.prev_width {
                out.push(' ');
            }
            out.push_str("\r\n");
            self.prev_width = 0;
        }
        out
    }

    /// Finish the live line and print a permanent line under it.
    pub fn permanent(&mut self, line: &str) -> String {
        let mut out = String::new();
        if self.prev_width > 0 {
            out.push('\r');
            for _ in 0..self.prev_width {
                out.push(' ');
            }
            out.push_str("\r\n");
            self.prev_width = 0;
        }
        out.push_str(line);
        out.push('\n');
        out
    }

    /// Reset the live line width (after [`CrEmitter::permanent`]).
    pub fn reset(&mut self) {
        self.prev_width = 0;
    }
}
