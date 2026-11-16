//! Effectful console driver for the `ppkg install` progress UI.
//!
//! The pure model/renderers live in [`super::ppkg_progress`] (host-tested
//! snapshots); this module is the thin I/O half: it selects a [`ProgressMode`]
//! from the console's capabilities, feeds [`crate::pkg::apt::InstallObserver`]
//! call-backs in, and writes the returned strings to serial + framebuffer.
//!
//! ## Capability model
//!
//! The Pagh framebuffer console understands `\n`, `\r` and `\x08` only — it has
//! no cursor addressing (see [`crate::net::progress`], which documents the same
//! constraint). Therefore:
//!
//!   * interactive framebuffer/VT session -> [`ProgressMode::CarriageReturn`]:
//!     one live line redrawn with `\r`, completed package bars printed once above
//!     it, no escape bytes ever written to the framebuffer;
//!   * serial capture / background provisioning / `--quiet` ->
//!     [`ProgressMode::Plain`]: no live UI at all, only the normal `ppkg:` lines
//!     (acceptance runs assert on those and must not see ANSI noise);
//!   * an ANSI-capable terminal can opt into the multi-line VT100 renderer with
//!     `--progress=ansi` (cursor hide/show, `ESC[nA\rESC[K` repaint at 10 FPS).
//!
//! Nothing here changes install semantics: it only observes the transaction.

use crate::pkg::apt::{InstallObserver, InstallPhase};

use super::ppkg_progress::{
    plain_phase_line, plain_plan_line, AnsiEmitter, CrEmitter, Phase, ProgressMode, ProgressModel,
};

/// Map the package layer's phase enum onto the UI model's (kept separate so the
/// pure renderer has no dependency on `pkg::apt` and stays host-testable).
fn ui_phase(phase: InstallPhase) -> Phase {
    match phase {
        InstallPhase::Download => Phase::Download,
        InstallPhase::Verify => Phase::Verify,
        InstallPhase::Unpack => Phase::Unpack,
    }
}

/// Drives one `ppkg install` transaction's progress display.
pub struct ProgressDriver {
    mode: ProgressMode,
    model: ProgressModel,
    ansi: AnsiEmitter,
    cr: CrEmitter,
    /// True between the first unpack frame and [`ProgressDriver::finish`].
    live: bool,
    /// Guards against double-finalisation (success then error path, etc.).
    finished: bool,
}

impl ProgressDriver {
    /// Start a driver in `mode`; the transaction clock starts now.
    pub fn new(mode: ProgressMode) -> ProgressDriver {
        let now = crate::task::scheduler::ticks();
        ProgressDriver {
            mode,
            model: ProgressModel::new(mode, now),
            ansi: AnsiEmitter::new(),
            cr: CrEmitter::new(),
            live: false,
            finished: false,
        }
    }

    /// The selected mode (for diagnostics/tests of the selection logic).
    pub fn mode(&self) -> ProgressMode {
        self.mode
    }

    /// Write raw progress bytes to both consoles, mirroring the rest of the
    /// shell I/O (framebuffer updates are skipped while the background mirror is
    /// paused, exactly like [`crate::net::progress`]).
    fn out(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        crate::kprint!("{}", text);
        if !crate::log::fb_mirror_paused() {
            crate::fb_print!("{}", text);
        }
    }

    /// Start the live block (ANSI mode hides the cursor once).
    fn start_live(&mut self) {
        if self.live || self.mode == ProgressMode::Plain {
            return;
        }
        if self.mode == ProgressMode::Ansi {
            let seq = self.ansi.begin();
            self.out(&seq);
        }
        self.live = true;
    }

    /// Repaint the live frame if the 10 FPS gate allows it.
    fn redraw(&mut self) {
        if !self.live {
            return;
        }
        let now = crate::task::scheduler::ticks();
        if !self.model.redraw_due(now) {
            return;
        }
        match self.mode {
            ProgressMode::Ansi => {
                let lines = self.model.frame(now);
                let seq = self.ansi.frame(&lines);
                self.out(&seq);
            }
            ProgressMode::CarriageReturn => {
                let line = self.model.cr_line(now);
                let seq = self.cr.live(&line);
                self.out(&seq);
            }
            ProgressMode::Plain => {}
        }
    }

    /// Erase the live block, leaving the cursor on a fresh line.
    fn clear_live(&mut self) {
        if !self.live {
            return;
        }
        match self.mode {
            ProgressMode::Ansi => {
                let seq = self.ansi.finish(&[]);
                self.out(&seq);
            }
            ProgressMode::CarriageReturn => {
                let seq = self.cr.clear();
                self.out(&seq);
            }
            ProgressMode::Plain => {}
        }
        self.live = false;
    }

    /// End the transaction: erase any live block and print the final total once.
    ///
    /// Idempotent and safe on both the success and the error path (the shell calls
    /// it unconditionally after `install_observed` returns).
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if !self.live {
            return;
        }
        let now = crate::task::scheduler::ticks();
        match self.mode {
            ProgressMode::Ansi => {
                let lines = self.model.frame(now);
                let seq = self.ansi.finish(&lines);
                self.out(&seq);
            }
            ProgressMode::CarriageReturn => {
                let seq = self.cr.clear();
                self.out(&seq);
                // The final total as a permanent line (the live line held it while
                // the transaction was running).
                let line = self.model.cr_line(now);
                let seq = self.cr.permanent(&line);
                self.out(&seq);
            }
            ProgressMode::Plain => {}
        }
        self.live = false;
    }
}

impl InstallObserver for ProgressDriver {
    fn on_plan(&mut self, packages: &[(alloc::string::String, u64)]) {
        self.model.plan(packages);
        if self.mode == ProgressMode::Plain && !packages.is_empty() {
            let total: u64 = packages.iter().map(|(_, size)| *size).sum();
            self.out(&alloc::format!(
                "{}\n",
                plain_plan_line(packages.len(), total)
            ));
        }
    }

    fn on_phase(&mut self, pkg: &str, phase: InstallPhase) {
        self.model.begin_phase(pkg, ui_phase(phase));
        match self.mode {
            ProgressMode::Plain => {
                let size = self
                    .model
                    .packages
                    .iter()
                    .find(|p| p.name == pkg)
                    .map(|p| p.size)
                    .unwrap_or(0);
                if let Some(line) = plain_phase_line(pkg, ui_phase(phase), size) {
                    self.out(&alloc::format!("{}\n", line));
                }
            }
            _ => {
                // Only the unpack phase is long and log-free (the fetch draws its
                // own transport bar), so the live block covers exactly that span.
                if phase == InstallPhase::Unpack {
                    self.start_live();
                    self.redraw();
                }
            }
        }
    }

    fn on_unpack_progress(&mut self, pkg: &str, done: u64, total: u64) {
        self.model.unpack_progress(pkg, done, total);
        self.redraw();
    }

    fn on_package_done(&mut self, pkg: &str, files: usize) {
        if self.mode == ProgressMode::Plain {
            return;
        }
        self.clear_live();
        if let Some(line) = self.model.complete_package(pkg, files) {
            match self.mode {
                ProgressMode::Ansi => self.out(&alloc::format!("{}\n", line)),
                ProgressMode::CarriageReturn => {
                    let seq = self.cr.permanent(&line);
                    self.out(&seq);
                }
                ProgressMode::Plain => {}
            }
        }
    }
}

/// Select the progress mode for an interactive session from console capability.
///
/// Called by the shell when no explicit `--progress=` was given: the framebuffer
/// (and any VT built on it) gets the carriage-return renderer; a serial-only or
/// mirrored/paused console gets plain lines so captured logs stay clean.
pub fn auto_mode() -> ProgressMode {
    if crate::drivers::framebuffer::is_initialized() && !crate::log::fb_mirror_paused() {
        ProgressMode::CarriageReturn
    } else {
        ProgressMode::Plain
    }
}
