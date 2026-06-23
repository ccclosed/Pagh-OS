//! Console rendering: color palette and color-aware line/prompt rendering.
//!
//! The framebuffer has a single global foreground color (set via
//! [`crate::drivers::framebuffer::set_fg_color`]); serial has no color concept.
//! So every helper here colorizes the framebuffer side only and leaves the
//! serial stream plain — never emitting color or escape bytes to serial
//! (R8.5). Each styled framebuffer print is wrapped so the foreground color is
//! always reset back to `Default` afterward, guarding against the console
//! getting stuck in a color (R8.2/8.3/8.4).
//!
//! Because the kernel builds with `panic = "abort"` we cannot catch a panic to
//! run a reset on unwind; instead the contract is a strict
//! set -> call -> set-back sequence, which is the most robust guard available.

#![allow(dead_code)]

use crate::drivers::framebuffer;

/// A console output style. Each maps to a single framebuffer foreground color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// Normal text and user input.
    Default,
    /// The interactive prompt (R8.3).
    Prompt,
    /// Error lines (R8.2).
    Error,
    /// Success / confirmation lines (R8.4).
    Success,
}

/// Foreground color (0xRRGGBB) for normal text and user input. Legible on the
/// existing background (R8.6); also the framebuffer driver's own default.
pub const COLOR_DEFAULT: u32 = 0xFFFFFF;
/// Foreground color for the prompt (R8.3).
pub const COLOR_PROMPT: u32 = 0x55FF55;
/// Foreground color for error lines (R8.2).
pub const COLOR_ERROR: u32 = 0xFF5555;
/// Foreground color for success / confirmation lines (R8.4).
pub const COLOR_SUCCESS: u32 = 0x55FFFF;

impl Style {
    /// The framebuffer foreground color (0xRRGGBB) for this style.
    pub fn color(self) -> u32 {
        match self {
            Style::Default => COLOR_DEFAULT,
            Style::Prompt => COLOR_PROMPT,
            Style::Error => COLOR_ERROR,
            Style::Success => COLOR_SUCCESS,
        }
    }
}

/// Run `f` with the framebuffer foreground color set to `style`'s color, then
/// always reset the color back to `Default` afterward.
///
/// The sequence is set -> call -> reset, executed unconditionally. Since the
/// kernel uses `panic = "abort"` there is no unwinding to catch, so a panic in
/// `f` would abort the kernel regardless; in all non-panicking paths the color
/// is guaranteed to be restored so the console never stays stuck in a color
/// (R8.2/8.3/8.4). Serial is untouched — color is framebuffer-only (R8.5).
pub fn with_style(style: Style, f: impl FnOnce()) {
    framebuffer::set_fg_color(style.color());
    f();
    framebuffer::set_fg_color(COLOR_DEFAULT);
}

/// Print `text` as a full line in `style`'s color on the framebuffer (reset to
/// `Default` afterward) and plain on serial. Serial is emitted first so the
/// serial stream stays clean and never sees color bytes (R8.5).
fn styled_line(style: Style, text: &str) {
    crate::kprintln!("{}", text);
    with_style(style, || {
        crate::fb_println!("{}", text);
    });
}

/// Print `text` as a line in [`Style::Error`] color on the framebuffer (reset
/// afterward) and plain on serial (R8.2, R8.5).
pub fn error_line(text: &str) {
    styled_line(Style::Error, text);
}

/// Print `text` as a line in [`Style::Success`] color on the framebuffer (reset
/// afterward) and plain on serial (R8.4, R8.5).
pub fn success_line(text: &str) {
    styled_line(Style::Success, text);
}

/// Print the shell prompt `pagh:{cwd}> ` (no trailing newline) in
/// [`Style::Prompt`] color on the framebuffer (reset afterward) and plain on
/// serial (R8.3, R8.5).
pub fn prompt(cwd: &str) {
    crate::kprint!("pagh:{}> ", cwd);
    with_style(Style::Prompt, || {
        crate::fb_print!("pagh:{}> ", cwd);
    });
}
