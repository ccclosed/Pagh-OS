//! `lgbt`: paint the six-stripe pride flag over the whole framebuffer.

use crate::drivers::framebuffer;

/// Flag stripes, top to bottom.
const STRIPES: [u32; 6] = [
    0x00E4_0303,
    0x00FF_8C00,
    0x00FF_ED00,
    0x0000_8026,
    0x0000_4DFF,
    0x0075_0787,
];

const CAPTION: &str = "made by chat LGBT & Dickpick";

/// Draw the flag, wait for a key, restore the console.
pub fn run() {
    let (w, h) = framebuffer::dimensions();
    if w == 0 || h == 0 {
        crate::kprintln!("lgbt: no framebuffer");
        return;
    }

    crate::kprintln!("lgbt: pride flag on the framebuffer; press any key to return");

    framebuffer::with(|fb| {
        for (i, color) in STRIPES.iter().enumerate() {
            let top = h * i / STRIPES.len();
            let bottom = h * (i + 1) / STRIPES.len();
            fb.fill_rect(0, top, w, bottom - top, *color);
        }

        // Caption on a black band, so it stays readable across the stripes.
        let band_h = 32;
        let text_w = CAPTION.len() * 8;
        let band_w = core::cmp::min(w, text_w + 24);
        let band_x = (w - band_w) / 2;
        let band_y = (h - band_h) / 2;
        fb.fill_rect(band_x, band_y, band_w, band_h, 0);
        fb.draw_text_px(
            band_x + 12,
            band_y + (band_h - 16) / 2,
            CAPTION,
            0x00FF_FFFF,
            0,
        );

        // Below the caption: the requisite dick pic, in white.
        draw_dick(fb, w / 2, band_y + band_h + 46, 0x00FF_FFFF);
    });

    // Drain whatever is still queued — the Enter that submitted this command
    // and its break codes — or the flag would be dismissed instantly.
    while crate::drivers::get_char("keyboard")
        .and_then(|kbd| kbd.read_char())
        .is_some()
    {}

    loop {
        crate::arch::cpu::halt();
        match crate::drivers::get_char("keyboard").and_then(|kbd| kbd.read_char()) {
            // Key presses only: ignore releases (bit 7) and extended prefix.
            Some(sc) if sc & 0x80 == 0 && sc != 0xE0 => break,
            _ => {}
        }
    }
    framebuffer::clear_screen();
}

/// A small pixel-art phallus: shaft, glans and two balls, centred on `(cx, cy)`
/// which is the top of the shaft.
fn draw_dick(fb: &mut framebuffer::FramebufferWriter, cx: usize, cy: usize, color: u32) {
    let shaft_w = 26usize;
    let shaft_h = 74usize;
    let r = (shaft_w / 2) as isize;

    fb.fill_rect(cx - shaft_w / 2, cy, shaft_w, shaft_h, color);
    fb.fill_circle(cx as isize, cy as isize, r + 6, color);
    fb.fill_circle(cx as isize - r - 6, (cy + shaft_h) as isize, r + 1, color);
    fb.fill_circle(cx as isize + r + 6, (cy + shaft_h) as isize, r + 1, color);
}
