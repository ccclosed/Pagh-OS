//! Terminal customization: colour themes, wallpaper, and the live `settings` panel.
//!
//! Everything here is cosmetic and reversible. The console has one knob that
//! actually matters — the foreground colour (`framebuffer::set_fg_color`) — plus a
//! background knob and a wallpaper layer, and this module owns the choices made on
//! top of them. Three rules keep it from becoming a source of "hardware" bugs:
//!
//! * **State is one small struct in one lock.** [`settings()`] hands out a guard;
//!   anything that needs several fields reads them together, so a half-applied
//!   theme cannot be observed.
//! * **Applying is explicit.** Mutating the struct does not touch the screen;
//!   [`apply`] pushes the palette into the drivers, and callers repaint. A panel
//!   that changed five colours and then hit an error would otherwise leave the
//!   console striped.
//! * **No allocation on the paint path.** Wallpapers are computed from the pixel
//!   coordinates, not from a stored image, so a 1280×800 gradient costs arithmetic
//!   and a `settings` change costs nothing.
//!
//! The panel ([`run_panel`]) is a modal screen, not a shell command: it owns the
//! keyboard until the user leaves, because every keystroke in it means "change
//! something and show me", which is exactly what the shell's line editor would
//! otherwise consume. `paint` takes the same approach.

use crate::shell::keys::{Decoder, KeyEvent};
use crate::sync::spinlock::Spinlock;

/// Built-in colour schemes. The index into this table is what the `theme` command
/// and the panel store, so adding a scheme is a single row here.
pub static THEMES: &[Theme] = &[
    Theme {
        name: "classic",
        text: 0xFF_FFFF,
        prompt: 0x55_FF55,
        error: 0xFF_5555,
        success: 0x55_FFFF,
        background: 0x00_0000,
        status_bar: 0x1B_3A5B,
        status_text: 0xE6_F0FF,
    },
    Theme {
        name: "amber",
        text: 0xFF_B000,
        prompt: 0xFF_D75F,
        error: 0xFF_3B30,
        success: 0xFF_E08A,
        background: 0x14_0C00,
        status_bar: 0x3A_2400,
        status_text: 0xFF_E0A0,
    },
    Theme {
        name: "green",
        text: 0x33_FF66,
        prompt: 0x9A_FF33,
        error: 0xFF_4444,
        success: 0x33_FFCC,
        background: 0x00_0A00,
        status_bar: 0x10_3A18,
        status_text: 0xCC_FFCC,
    },
    Theme {
        name: "paper",
        text: 0x1A_1A1A,
        prompt: 0x00_55AA,
        error: 0xC0_2A20,
        success: 0x00_7744,
        background: 0xF2_EFE6,
        status_bar: 0xD8_D2C4,
        status_text: 0x2A_2A2A,
    },
    Theme {
        name: "synth",
        text: 0xF8_F8F2,
        prompt: 0xFF_2A9D,
        error: 0xFF_4D6D,
        success: 0x2A_FFE0,
        background: 0x1A_1030,
        status_bar: 0x3A_1A5A,
        status_text: 0xFF_D0F0,
    },
];

/// One colour scheme. All values are `0xRRGGBB`, matching the framebuffer driver.
#[derive(Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    pub text: u32,
    pub prompt: u32,
    pub error: u32,
    pub success: u32,
    pub background: u32,
    pub status_bar: u32,
    pub status_text: u32,
}

/// A wallpaper layer painted behind the text.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Wallpaper {
    /// Plain background colour.
    None,
    /// Vertical gradient between two colours, derived from the theme.
    Gradient,
    /// Horizontal gradient.
    GradientX,
    /// Gradient plus a darker band every third scanline (CRT-ish).
    Scanlines,
    /// Deterministic starfield from a fixed hash — no RNG state to keep.
    Stars,
    /// A coarser checkerboard, useful to see exactly which cells are repainted.
    Grid,
}

impl Wallpaper {
    pub const ALL: [Wallpaper; 6] = [
        Wallpaper::None,
        Wallpaper::Gradient,
        Wallpaper::GradientX,
        Wallpaper::Scanlines,
        Wallpaper::Stars,
        Wallpaper::Grid,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Wallpaper::None => "none",
            Wallpaper::Gradient => "gradient",
            Wallpaper::GradientX => "gradient-x",
            Wallpaper::Scanlines => "scanlines",
            Wallpaper::Stars => "stars",
            Wallpaper::Grid => "grid",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|w| w.name() == s)
    }
}

/// The whole customization state.
#[derive(Clone, Copy)]
pub struct Settings {
    pub theme: usize,
    pub wallpaper: Wallpaper,
    /// Status bar text/background come from the theme; this hides the bar instead.
    pub status_bar: bool,
    /// Show the mouse position in the status bar (`mouse(x,y)`).
    pub status_mouse: bool,
    /// Whether the blinking caret is drawn in the shell.
    pub caret: bool,
}

impl Settings {
    const fn new() -> Self {
        Settings {
            theme: 0,
            wallpaper: Wallpaper::None,
            status_bar: true,
            status_mouse: true,
            caret: true,
        }
    }

    /// The active theme.
    pub fn theme(&self) -> &'static Theme {
        &THEMES[self.theme.min(THEMES.len() - 1)]
    }

    /// Cycle to the next scheme and return its name.
    pub fn next_theme(&mut self) -> &'static str {
        self.theme = (self.theme + 1) % THEMES.len();
        self.theme().name
    }
}

static SETTINGS: Spinlock<Settings> = Spinlock::new(Settings::new());

/// Set when the palette changed under the shell's feet.
///
/// The console model remembers the colour each cell was drawn in, so changing the
/// palette makes the pixels *behind* the text wrong, not the text itself: a pixel
/// repaint shows the new background under the old colours. The shell consumes this
/// flag and reprints its own line, which is the only place that can re-emit the
/// prompt in the new colours.
static FORCE_REDRAW: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Ask the shell to reprint its prompt and re-read the status bar on its next pass.
pub fn request_redraw() {
    FORCE_REDRAW.store(true, core::sync::atomic::Ordering::SeqCst);
}

/// Take the pending redraw request (and clear it).
pub fn take_force_redraw() -> bool {
    FORCE_REDRAW.swap(false, core::sync::atomic::Ordering::SeqCst)
}

/// Run `f` with the settings. Keep the closure short: the lock masks interrupts.
pub fn with<R>(f: impl FnOnce(&mut Settings) -> R) -> R {
    let mut guard = SETTINGS.lock();
    f(&mut guard)
}

/// A snapshot, for callers that need several fields at once without holding the lock.
pub fn snapshot() -> Settings {
    *SETTINGS.lock()
}

/// Push the palette into the drivers. Does not repaint: the caller decides when
/// the screen catches up, so a multi-step change is never shown half-applied.
pub fn apply() {
    let s = snapshot();
    let theme = s.theme();
    crate::drivers::framebuffer::set_fg_color(theme.text);
    crate::drivers::framebuffer::set_bg_color(theme.background);
    // The strip has its own two colours in the theme; deriving them from the text
    // colours made every scheme's bar look like the default one.
    crate::drivers::framebuffer::set_status_colors(theme.status_bar, theme.status_text);
    // A wallpaper only shows *through* text when the console stops stamping its
    // background behind every glyph. On a plain background that stamp is what keeps
    // a re-printed cell clean, so it goes back on.
    crate::drivers::framebuffer::set_opaque_bg(s.wallpaper == Wallpaper::None);
    crate::shell::render::set_colors(theme.text, theme.prompt, theme.error, theme.success);
    // The status strip is drawn with its own two colours and no text of its own
    // changes when the theme does, so it has to be repainted here — the shell only
    // redraws it when its *text* changes, which may be seconds later.
    let mouse = crate::drivers::ps2_mouse::poll();
    crate::drivers::framebuffer::draw_status_bar(&status_left(), &status_right(&mouse));
    // Anything else already on screen was drawn with the old palette and is wrong.
    request_redraw();
}

/// `pagh OS   <cwd>` — the strip's left half. Public because the shell's own
/// repaint path shares it (see `shell::shell_status_text`).
pub fn status_left() -> alloc::string::String {
    alloc::format!("pagh OS   {}", crate::shell::path::cwd())
}

/// `up Ns   mouse(x,y)` — the strip's right half, with the mouse readout optional.
pub fn status_right(mouse: &crate::drivers::ps2_mouse::MouseState) -> alloc::string::String {
    let secs = crate::task::scheduler::ticks() / crate::arch::x86_64::apic::TICK_HZ;
    let s = snapshot();
    if !s.status_mouse {
        return alloc::format!("up {}s", secs);
    }
    alloc::format!(
        "up {}s   mouse({},{})",
        secs,
        mouse.x / crate::drivers::framebuffer::CHAR_WIDTH,
        mouse.y / crate::drivers::framebuffer::CHAR_HEIGHT
    )
}

/// Repaint the whole surface for the current settings: wallpaper, then the console
/// grid that was on screen. Called after a theme or wallpaper change.
pub fn repaint() {
    let s = snapshot();
    let theme = s.theme();
    // The wallpaper owns the text area only; the status strip keeps its own two
    // colours, so it is repainted explicitly after the background is replaced.
    // Hide the arrow for the whole repaint: it would otherwise be drawn over by the
    // wallpaper/grid and its saved background would describe a frame that no longer
    // exists (see drivers::cursor's ordering contract).
    crate::drivers::cursor::hide();
    // Order matters. `repaint_grid` re-emits every cell, and a cell carries the
    // background it was last drawn with (black on a fresh console) — so it puts the
    // old, opaque background back over the picture. Painting the wallpaper *last*
    // is what makes it visible inside the text grid instead of only in the margins
    // the grid does not cover; the glyphs printed from here on are transparent
    // (`set_opaque_bg(false)` in `apply`), so text keeps sitting on the picture.
    crate::drivers::framebuffer::repaint_grid();
    crate::drivers::framebuffer::with(|fb| paint_wallpaper(fb, s.wallpaper, theme));
    crate::drivers::framebuffer::repaint_grid();
    let mouse = crate::drivers::ps2_mouse::poll();
    crate::drivers::framebuffer::draw_status_bar(&status_left(), &status_right(&mouse));
    crate::drivers::cursor::show();
}

/// Fill the text area (everything above the status bar) with the wallpaper.
///
/// Wallpapers are computed per pixel from `(x, y)` and the theme, so this needs no
/// stored image and cannot drift out of sync with the palette.
fn paint_wallpaper(
    fb: &mut crate::drivers::framebuffer::FramebufferWriter,
    wallpaper: Wallpaper,
    theme: &Theme,
) {
    let (w, h) = fb.dimensions();
    let bottom = h.saturating_sub(crate::drivers::framebuffer::STATUS_BAR_HEIGHT);
    let bg = theme.background;
    match wallpaper {
        Wallpaper::None => fb.fill_rect(0, 0, w, bottom, bg),
        Wallpaper::Gradient | Wallpaper::Scanlines => {
            // The ramp runs from a tinted shade to the background colour itself.
            // Scaling the background (the obvious implementation) does nothing on a
            // black-based theme — 0x000000 times anything is still black, so classic
            // showed no wallpaper at all. Blending *towards* a fixed tint works on
            // every theme; on a light one the same tint darkens instead, which is
            // what "gradient" should look like there.
            // The bright end has to be clearly brighter than the background, or the
            // "gradient" is a two-colour ramp nobody can see: blending 21% of a
            // grey into black is 0x0A0C0F, which reads as black on any monitor.
            let tint = if brightness(bg) < 400 {
                0x8A_9CC8
            } else {
                0x14_1820
            };
            let top = mix(bg, tint, 96);
            let floor = if brightness(bg) < 400 {
                mix(bg, tint, 12)
            } else {
                bg
            };
            for y in 0..bottom {
                let t = y as u32 * 256 / bottom.max(1) as u32;
                let mut color = mix(top, floor, t);
                if wallpaper == Wallpaper::Scanlines && y % 3 == 2 {
                    color = shade(color, 0.55);
                }
                fb.fill_rect(0, y, w, 1, color);
            }
        }
        Wallpaper::GradientX => {
            let tint = if brightness(bg) < 400 {
                0x8A_9CC8
            } else {
                0x14_1820
            };
            let left = mix(bg, tint, 96);
            for x in 0..w {
                let t = x as u32 * 256 / w.max(1) as u32;
                fb.fill_rect(x, 0, 1, bottom, mix(left, bg, t));
            }
        }
        Wallpaper::Stars => {
            fb.fill_rect(0, 0, w, bottom, bg);
            let tint = 0xB0_BCD8;
            let bright = mix(bg, tint, 200);
            let dim = mix(bg, tint, 96);
            // A hash, not an RNG: the field is identical after every repaint, so a
            // theme change does not make the sky jump.
            let mut i = 0u32;
            while i < 900 {
                let hx = hash(i.wrapping_mul(2654435761));
                let hy = hash(hx ^ 0x9E3779B9);
                let x = (hx >> 8) as usize % w.max(1);
                let y = (hy >> 8) as usize % bottom.max(1);
                let c = if hx & 0x40 == 0 { bright } else { dim };
                fb.set_pixel(x, y, c);
                i += 1;
            }
        }
        Wallpaper::Grid => {
            // A faint field first, then the lines: on a black-based theme lines drawn
            // straight onto the background are 0x0A0A0C and simply invisible.
            fb.fill_rect(0, 0, w, bottom, mix(bg, 0x8A_9CC8, 40));
            let line = mix(bg, 0xB0_BCD8, 130);
            let (cw, ch) = (
                crate::drivers::framebuffer::CHAR_WIDTH,
                crate::drivers::framebuffer::CHAR_HEIGHT,
            );
            let mut x = 0;
            while x < w {
                fb.fill_rect(x, 0, 1, bottom, line);
                x += cw;
            }
            let mut y = 0;
            while y < bottom {
                fb.fill_rect(0, y, w, 1, line);
                y += ch;
            }
        }
    }
}

/// Perceived brightness (0..765), used to decide whether a wallpaper should lighten
/// or darken its theme's background.
fn brightness(color: u32) -> u32 {
    ((color >> 16) & 0xFF) + ((color >> 8) & 0xFF) + (color & 0xFF)
}

/// Multiply a colour's channels, clamping at 0xFF. `f < 1` darkens, `f > 1` lightens.
fn shade(color: u32, f: f32) -> u32 {
    let ch = |shift: u32| {
        let v = ((color >> shift) & 0xFF) as f32 * f;
        let v = if v > 255.0 { 255.0 } else { v };
        v as u32
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// Linear blend: `t = 0` gives `a`, `t = 256` gives `b`.
fn mix(a: u32, b: u32, t: u32) -> u32 {
    let ch = |shift: u32| {
        let (x, y) = ((a >> shift) & 0xFF, (b >> shift) & 0xFF);
        (x * (256 - t) + y * t) >> 8
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// Cheap integer hash for the starfield.
fn hash(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7FEB_352D);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846C_A68B);
    x ^= x >> 16;
    x
}

// ─── the `settings` panel ───

/// One row of the panel: a label, its current value, and how to change it.
struct Row {
    label: &'static str,
    value: alloc::string::String,
}

/// Run the interactive settings panel until the user leaves.
///
/// Keys: `↑`/`↓` or `k`/`j` move, `←`/`→` or `h`/`l` change, `t` cycles the theme,
/// `w` the wallpaper, `r` resets, `q`/`Esc`/`Enter` leaves. Every change is applied
/// and repainted immediately, so the panel is the preview.
pub fn run_panel() {
    let mut sel = 0usize;
    let mut decoder = Decoder::new();
    loop {
        // The panel is a full-screen frame: hide the arrow while it is painted, then
        // put it back on top of the finished frame (the same contract as `paint`).
        crate::drivers::cursor::hide();
        draw_panel(sel);
        crate::drivers::framebuffer::flush();
        let mouse = crate::drivers::ps2_mouse::poll();
        crate::drivers::cursor::move_to(mouse.x, mouse.y);
        // Sleep until a scancode actually arrives. A bare `halt()` returns on
        // every timer tick, and redrawing a full-screen panel plus flushing the
        // whole surface at 1 kHz would spend the CPU on frames nobody asked for.
        while !crate::drivers::ps2_kbd::has_scancode() {
            crate::arch::cpu::halt();
            // The pointer keeps working over the panel while waiting for a key.
            let mouse = crate::drivers::ps2_mouse::poll();
            crate::drivers::cursor::move_to(mouse.x, mouse.y);
        }
        let Some(bytes) = crate::drivers::get_char("keyboard").and_then(|k| k.read_char()) else {
            continue;
        };
        let Some(ev) = decoder.feed(bytes) else {
            continue;
        };
        let mut changed = false;
        match ev {
            KeyEvent::Up | KeyEvent::Char('k') => sel = sel.saturating_sub(1),
            KeyEvent::Down | KeyEvent::Char('j') => sel = (sel + 1).min(ROW_COUNT - 1),
            KeyEvent::Left | KeyEvent::Char('h') => changed = adjust(sel, -1),
            KeyEvent::Right | KeyEvent::Char('l') => changed = adjust(sel, 1),
            KeyEvent::Char('t') => {
                with(|s| s.next_theme());
                changed = true;
            }
            KeyEvent::Char('w') => {
                with(|s| {
                    let i = Wallpaper::ALL
                        .iter()
                        .position(|w| *w == s.wallpaper)
                        .unwrap_or(0);
                    s.wallpaper = Wallpaper::ALL[(i + 1) % Wallpaper::ALL.len()];
                });
                changed = true;
            }
            KeyEvent::Char('r') => {
                with(|s| *s = Settings::new());
                changed = true;
            }
            KeyEvent::Escape | KeyEvent::Char('q') | KeyEvent::Enter => break,
            _ => {}
        }
        if changed {
            apply();
            repaint();
        }
    }
    // Leaving the panel: put the console back the way the shell expects it, with a
    // fresh prompt on a cleared screen.
    apply();
    crate::drivers::framebuffer::clear_screen();
    crate::shell::render::prompt(&crate::shell::path::cwd());
}

const ROW_COUNT: usize = 5;

fn adjust(row: usize, dir: i32) -> bool {
    with(|s| match row {
        0 => {
            let n = THEMES.len();
            s.theme = if dir > 0 {
                (s.theme + 1) % n
            } else {
                (s.theme + n - 1) % n
            };
            true
        }
        1 => {
            let all = Wallpaper::ALL;
            let i = all.iter().position(|w| *w == s.wallpaper).unwrap_or(0);
            let n = all.len();
            s.wallpaper = all[if dir > 0 {
                (i + 1) % n
            } else {
                (i + n - 1) % n
            }];
            true
        }
        2 => {
            s.status_bar = !s.status_bar;
            true
        }
        3 => {
            s.status_mouse = !s.status_mouse;
            true
        }
        4 => {
            s.caret = !s.caret;
            true
        }
        _ => false,
    })
}

fn rows() -> [Row; ROW_COUNT] {
    let s = snapshot();
    [
        Row {
            label: "Theme",
            value: alloc::string::String::from(s.theme().name),
        },
        Row {
            label: "Wallpaper",
            value: alloc::string::String::from(s.wallpaper.name()),
        },
        Row {
            label: "Status bar",
            value: on_off(s.status_bar),
        },
        Row {
            label: "Status: mouse",
            value: on_off(s.status_mouse),
        },
        Row {
            label: "Caret",
            value: on_off(s.caret),
        },
    ]
}

fn on_off(v: bool) -> alloc::string::String {
    alloc::string::String::from(if v { "on" } else { "off" })
}

/// Draw the panel: a framed list with the selected row highlighted.
fn draw_panel(sel: usize) {
    let s = snapshot();
    let theme = s.theme();
    let rows = rows();
    let (w, h) = crate::drivers::framebuffer::dimensions();
    if w == 0 || h == 0 {
        return;
    }
    let cw = crate::drivers::framebuffer::CHAR_WIDTH;
    let chh = crate::drivers::framebuffer::CHAR_HEIGHT;
    let panel_w = (cw * 34).min(w.saturating_sub(8));
    let panel_h = (chh * (ROW_COUNT + 4)).min(h.saturating_sub(8));
    let x = (w - panel_w) / 2;
    let y = (h - panel_h) / 2;

    crate::drivers::framebuffer::with(|fb| {
        fb.fill_rect(x, y, panel_w, panel_h, shade(theme.background, 1.5));
        fb.draw_rect(x, y, panel_w, panel_h, theme.prompt);
        fb.draw_text_px(
            x + cw / 2,
            y + chh / 2,
            "terminal settings",
            theme.prompt,
            shade(theme.background, 1.5),
        );
        for (i, row) in rows.iter().enumerate() {
            let ry = y + chh * (i + 2);
            let bg = if i == sel {
                shade(theme.prompt, 0.25)
            } else {
                shade(theme.background, 1.5)
            };
            fb.fill_rect(x + cw / 2, ry, panel_w - cw, chh, bg);
            fb.draw_text_px(x + cw / 2, ry, row.label, theme.text, bg);
            fb.draw_text_px(
                x + panel_w - cw / 2 - row.value.len() * cw,
                ry,
                &row.value,
                theme.prompt,
                bg,
            );
        }
        let help_y = y + panel_h - chh - chh / 2;
        fb.draw_text_px(
            x + cw / 2,
            help_y,
            "up/down pick - left/right change - t theme - w wallpaper - r reset - q quit",
            theme.text,
            shade(theme.background, 1.5),
        );
    });
}
