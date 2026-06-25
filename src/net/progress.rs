//! Single-line, in-place download progress bar for the package fetchers.
//!
//! Both the cleartext HTTP ([`super::http_fetch`]) and the TLS
//! ([`super::tls`]) download loops call [`show`] as body bytes arrive and
//! [`finish`] once the transfer ends (success or failure). The line is redrawn
//! in place using a leading carriage return (`\r`) and **no** trailing newline,
//! so it overwrites itself instead of scrolling: the framebuffer console honours
//! `\r` (it resets the column) and repaints each glyph cell opaquely, and a host
//! serial terminal does the same. [`finish`] prints the closing newline exactly
//! once, and only if a bar was actually drawn, so callers can invoke it
//! unconditionally on every exit path without leaving a stray blank line.
//!
//! Output goes to both serial and the framebuffer (mirroring the rest of the
//! kernel's console I/O); no ANSI/escape bytes are emitted, only printable ASCII
//! plus `\r`/`\n`, so the 8x16 bitmap font renders it verbatim.

#![allow(dead_code)]

use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, Ordering};

/// Visible width, in cells, of the `[####    ]` bar.
const BAR_WIDTH: usize = 28;

/// Whether a progress line is currently drawn (so [`finish`] knows to terminate
/// it). Downloads are serialized (one fetch at a time, driven by the single net
/// pump), so a plain flag is sufficient.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Format a byte count as a short `B`/`KiB`/`MiB` string with one decimal place.
/// Integer-only and panic-free. Local copy (kept tiny) so the low-level `net`
/// layer does not depend on the higher-level `pkg` module for formatting.
fn human(n: u64) -> String {
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

/// Build the progress line for `received` bytes out of an optional `total` (the
/// parsed `Content-Length`). With a known total it renders a filled bar plus a
/// percentage and `recv / total`; before the head is parsed (`None`) it shows
/// just the running byte count. Trailing spaces pad the line so a shorter redraw
/// fully erases the previous, longer one.
pub fn line(received: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => {
            let recv = received.min(total);
            let filled = (BAR_WIDTH as u64 * recv / total) as usize;
            let pct = 100 * recv / total;
            let mut bar = String::with_capacity(BAR_WIDTH);
            for i in 0..BAR_WIDTH {
                bar.push(if i < filled { '#' } else { ' ' });
            }
            format!(
                "  [{}] {:>3}%  {} / {}    ",
                bar,
                pct,
                human(recv),
                human(total)
            )
        }
        _ => format!("  downloading {} ...    ", human(received)),
    }
}

/// Redraw the progress `text` in place (leading `\r`, no newline) on serial and
/// the framebuffer. Marks a bar as active so [`finish`] will terminate it.
pub fn show(text: &str) {
    ACTIVE.store(true, Ordering::Relaxed);
    crate::kprint!("\r{}", text);
    crate::fb_print!("\r{}", text);
}

/// Terminate the current progress line with a newline, if one was drawn. A no-op
/// otherwise, so it is safe to call on both success and failure exit paths.
pub fn finish() {
    if ACTIVE.swap(false, Ordering::Relaxed) {
        crate::kprintln!("");
        crate::fb_println!("");
    }
}
