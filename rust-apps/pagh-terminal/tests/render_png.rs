//! Renderer, PNG and committed-screenshot tests.

use pagh_terminal::cell::Rgb;
use pagh_terminal::demo;
use pagh_terminal::png;
use pagh_terminal::render::{
    render_screen, render_window, Canvas, RenderOptions, Theme, BORDER, TITLE_H, VIEWPORT_PADDING,
};
use pagh_terminal::screen::{Screen, SelectionMode};
use pagh_terminal::Terminal;

fn theme() -> Theme {
    Theme::default()
}

#[test]
fn png_roundtrip_preserves_every_pixel() {
    let mut canvas = Canvas::new(37, 19, Rgb(1, 2, 3));
    canvas.fill_rect(3, 4, 10, 5, Rgb(200, 100, 50));
    canvas.blend_rect(6, 2, 8, 8, Rgb(0, 255, 0), 128);
    let encoded = pagh_terminal::png::encode(&canvas);
    assert_eq!(
        &encoded[..8],
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
    );
    let decoded = png::decode(&encoded).expect("decode own PNG");
    assert_eq!(decoded.width, canvas.width);
    assert_eq!(decoded.height, canvas.height);
    assert_eq!(decoded.pixels, canvas.pixels);
}

#[test]
fn empty_screen_is_theme_background() {
    let screen = Screen::new(4, 2);
    let theme = theme();
    let canvas = render_screen(&screen, &theme, &RenderOptions::default());
    assert_eq!(canvas.width, 4 * 8 + VIEWPORT_PADDING * 2);
    assert_eq!(canvas.height, 2 * 18 + VIEWPORT_PADDING * 2);
    assert_eq!(&canvas.pixel(0, 0)[..3], &theme.bg.to_array());
    assert_eq!(
        &canvas.pixel(canvas.width - 1, canvas.height - 1)[..3],
        &theme.bg.to_array()
    );
}

#[test]
fn text_cursor_and_selection_paint_expected_pixels() {
    let mut terminal = Terminal::new(10, 3);
    terminal.feed_str("hello");
    terminal
        .screen
        .begin_selection_abs(0, 1, SelectionMode::Char);
    terminal.screen.extend_selection_abs(0, 2);
    let theme = theme();
    let canvas = render_screen(&terminal.screen, &theme, &RenderOptions::default());

    // Selected cell background.
    assert_eq!(
        &canvas.pixel(VIEWPORT_PADDING + 8 + 2, VIEWPORT_PADDING + 2)[..3],
        &theme.selection.to_array()
    );
    // Cursor block at cell 5.
    assert_eq!(
        &canvas.pixel(VIEWPORT_PADDING + 5 * 8 + 3, VIEWPORT_PADDING + 3)[..3],
        &theme.cursor.to_array()
    );
    // A glyph pixel is not the background.
    let mut non_bg = 0;
    for y in 0..18 {
        for x in 0..8 {
            if canvas.pixel(VIEWPORT_PADDING + x, VIEWPORT_PADDING + y)[..3] != theme.bg.to_array()
            {
                non_bg += 1;
            }
        }
    }
    assert!(non_bg > 10);
}

#[test]
fn box_drawing_has_no_seams_between_cells() {
    let mut terminal = Terminal::new(10, 3);
    terminal.feed_str("┌─┐");
    let theme = theme();
    let canvas = render_screen(&terminal.screen, &theme, &RenderOptions::default());
    let y = VIEWPORT_PADDING + 9;
    // From the centre of the left corner to the centre of the right corner the
    // top border must be continuous, including the cell boundaries.
    for x in 4..(2 * 8 + 4) {
        assert_eq!(
            &canvas.pixel(VIEWPORT_PADDING + x, y)[..3],
            &theme.fg.to_array(),
            "gap at x={x}"
        );
    }
}

#[test]
fn scrollbar_appears_only_with_scrollback() {
    let mut terminal = Terminal::new(8, 3);
    terminal.feed_str("1\r\n2\r\n3\r\n4\r\n5");
    let theme = theme();
    let with_sb = render_screen(&terminal.screen, &theme, &RenderOptions::default());
    let x = VIEWPORT_PADDING + 8 * 8;
    let found = (0..with_sb.height).any(|y| with_sb.pixel(x, y)[..3] == theme.scrollbar.to_array());
    assert!(found, "scrollback thumb should be painted");

    let empty = Screen::new(8, 3);
    let no_sb = render_screen(&empty, &theme, &RenderOptions::default());
    let found = (0..no_sb.height).any(|y| no_sb.pixel(x, y)[..3] == theme.scrollbar.to_array());
    assert!(!found);
}

#[test]
fn missing_wide_glyph_gets_two_cell_placeholder() {
    let mut terminal = Terminal::new(6, 2);
    terminal.feed_str("\u{1F600}a");
    assert_eq!(terminal.screen.cursor.x, 3);
    assert!(terminal.screen.cell(0, 0).is_wide());
    assert!(terminal.screen.cell(1, 0).is_wide_continuation());
    let canvas = render_screen(&terminal.screen, &theme(), &RenderOptions::default());
    // The placeholder box is wider than one cell: verify at least one lit
    // pixel in the second (continuation) cell area.
    let mut lit = 0;
    for y in 0..18 {
        for x in 8..16 {
            if canvas.pixel(VIEWPORT_PADDING + x, VIEWPORT_PADDING + y)[..3]
                != theme().bg.to_array()
            {
                lit += 1;
            }
        }
    }
    assert!(lit > 4);
}

#[test]
fn window_chrome_dimensions_and_title_bar() {
    let mut terminal = Terminal::new(20, 5);
    terminal.feed_str("\x1b]0;My Title\x07hello");
    let theme = theme();
    let canvas = render_window(
        &terminal.screen,
        &theme,
        &terminal.screen.title,
        &RenderOptions::default(),
    );
    assert_eq!(canvas.width, VIEWPORT_PADDING * 2 + 20 * 8 + BORDER * 2);
    assert_eq!(
        canvas.height,
        VIEWPORT_PADDING * 2 + 5 * 18 + BORDER * 2 + TITLE_H
    );
    // Thin title strip, distinct from the grid background; a hairline separates
    // it from the content, and there is no status bar or button cluster.
    assert_eq!(&canvas.pixel(20, 10)[..3], &theme.title_bg.to_array());
    assert_eq!(
        &canvas.pixel(20, BORDER + TITLE_H - 1)[..3],
        &theme.border.to_array()
    );
    assert_eq!(
        &canvas.pixel(4, canvas.height - 1)[..3],
        &theme.bg.to_array()
    );
}

#[test]
fn headless_script_renders_png_that_decodes() {
    let mut terminal = Terminal::new(40, 6);
    terminal.feed_str("\x1b[1;31mred\x1b[0m\r\nПривет, мир! 界");
    let canvas = render_screen(&terminal.screen, &theme(), &RenderOptions::default());
    let png = pagh_terminal::png::encode(&canvas);
    let decoded = pagh_terminal::png::decode(&png).unwrap();
    assert_eq!(decoded.pixels, canvas.pixels);
}

#[test]
fn committed_screenshots_are_current() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("screenshots");
    let theme = theme();
    for name in demo::SCREENSHOTS {
        let path = dir.join(format!("{name}.png"));
        let committed = std::fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "missing {}: {error} (run `cargo run -- --screenshots screenshots`)",
                path.display()
            )
        });
        let generated = demo::demo_png(name, &theme).expect("demo exists");
        assert_eq!(
            committed, generated,
            "committed screenshot {name} is stale; regenerate with `cargo run -- --screenshots screenshots`"
        );
        // And it must be a valid PNG that our decoder understands.
        let canvas = pagh_terminal::png::decode(&committed).expect("decode screenshot");
        assert!(canvas.width > 300 && canvas.height > 200);
    }
}
