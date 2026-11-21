//! Renderer, PNG and committed-screenshot tests.

use pagh_terminal::cell::Rgb;
use pagh_terminal::demo;
use pagh_terminal::png;
use pagh_terminal::render::{
    render_screen, render_screen_into, render_screen_into_cleared, render_window, Canvas,
    RenderOptions, Theme, BORDER, TITLE_H, VIEWPORT_PADDING,
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

/// The fast frame (background already painted `theme.bg`) must be byte-identical
/// to the full path that repaints every cell: otherwise removing the second
/// background pass would change the picture. Both surfaces are checked — RGBA
/// (`Canvas`) and the mmap client one (`MappedBuffer`, XRGB8888 wire order).
fn assert_cleared_matches_full(terminal: &Terminal, opts: &RenderOptions, label: &str) {
    let theme = theme();
    let (cols, rows) = (terminal.screen.cols(), terminal.screen.rows());
    let pad = opts.padding;
    let (width, height) = (pad * 2 + cols * 8, pad * 2 + rows * 18);

    // Full path: every cell repaints its own background.
    let mut full = Canvas::new(width, height, theme.bg);
    render_screen_into(&mut full, pad, pad, &terminal.screen, &theme, opts);

    // Fast path on an already-painted canvas.
    let mut fast = Canvas::new(width, height, theme.bg);
    fast.fill(theme.bg);
    render_screen_into_cleared(&mut fast, pad, pad, &terminal.screen, &theme, opts);
    assert_eq!(
        full.pixels, fast.pixels,
        "{label}: owned canvas pixels differ"
    );

    // ...and on a real MAP_SHARED memfd, as in the live client.
    let fd = pagh_ui::shm::create_memfd("render-pixel-identical").expect("memfd");
    let len = width * height * 4;
    pagh_ui::shm::set_len(std::os::fd::AsRawFd::as_raw_fd(&fd), len).expect("ftruncate");
    let mapping = pagh_ui::shm::MappedBuffer::map_shared(std::os::fd::AsRawFd::as_raw_fd(&fd), len)
        .expect("mmap");
    let mut mapped = Canvas::from_mapped(mapping, width, height, width * 4).expect("canvas");
    mapped.fill(theme.bg);
    render_screen_into_cleared(&mut mapped, pad, pad, &terminal.screen, &theme, opts);
    let mapped_rgba = mapped.clone();
    assert_eq!(
        full.pixels, mapped_rgba.pixels,
        "{label}: mapped canvas pixels differ"
    );

    // A second fast-path pass over the same canvas changes nothing: the
    // background skip is idempotent.
    let mut again = fast.clone();
    again.fill(theme.bg);
    render_screen_into_cleared(&mut again, pad, pad, &terminal.screen, &theme, opts);
    assert_eq!(fast.pixels, again.pixels, "{label}: not idempotent");
}

#[test]
fn cleared_background_path_is_pixel_identical() {
    // 1. Empty grid: every cell is the default background.
    assert_cleared_matches_full(
        &Terminal::new(12, 4),
        &RenderOptions::default(),
        "empty grid",
    );

    // 2. Text, colours, bold/dim/hidden/blink, underline/overline/strike.
    let mut colored = Terminal::new(24, 6);
    colored.feed_str(
        "\x1b[1;31mred bold\x1b[0m \x1b[2;32mdim\x1b[0m \x1b[4munder\x1b[0m\r\n\
         \x1b[9mstrike\x1b[0m \x1b[53mover\x1b[0m \x1b[7mreverse\x1b[0m\r\n\
         \x1b[38;5;208mindexed\x1b[0m \x1b[48;5;53mbg53\x1b[0m \x1b[8mhidden\x1b[0m",
    );
    assert_cleared_matches_full(&colored, &RenderOptions::default(), "attrs");

    // 3. Block cursor over a cell that holds a glyph.
    let mut cursor = Terminal::new(10, 3);
    cursor.feed_str("abc");
    assert_cleared_matches_full(&cursor, &RenderOptions::default(), "block cursor");

    // 4. Underline and bar cursor styles.
    let mut bar = Terminal::new(10, 3);
    bar.feed_str("\x1b[3 qabc");
    let mut opts = RenderOptions::default();
    opts.padding = VIEWPORT_PADDING;
    assert_cleared_matches_full(&bar, &opts, "bar cursor");

    // 5. Mouse selection (cells painted with theme.selection).
    let mut selected = Terminal::new(10, 3);
    selected.feed_str("hello world");
    selected
        .screen
        .begin_selection_abs(0, 1, SelectionMode::Char);
    selected.screen.extend_selection_abs(0, 6);
    assert_cleared_matches_full(&selected, &RenderOptions::default(), "selection");

    // 6. Reverse video (the whole grid is inverted, so cell bg = theme.fg).
    let mut reversed = Terminal::new(10, 3);
    reversed.feed_str("\x1b[7mreversed\x1b[0m");
    assert_cleared_matches_full(&reversed, &RenderOptions::default(), "reverse video");

    // 7. Wide glyph: the continuation cell must not paint over it.
    let mut wide = Terminal::new(8, 2);
    wide.feed_str("\u{1F600}a\u{754C}");
    assert_cleared_matches_full(&wide, &RenderOptions::default(), "wide glyph");

    // 8. Scrollback and the scrollbar thumb.
    let mut scrolled = Terminal::new(10, 3);
    scrolled.feed_str("1\r\n2\r\n3\r\n4\r\n5\r\n6");
    assert_cleared_matches_full(&scrolled, &RenderOptions::default(), "scrollback");
}

#[test]
fn cleared_path_repaints_non_default_backgrounds() {
    // The opposite guard: an unconditional skip would leave a cell with an
    // explicit background as the canvas colour. Start from a *foreign* canvas
    // colour and require the explicit cell background to cover it.
    let theme = theme();
    let mut terminal = Terminal::new(6, 2);
    terminal.feed_str("\x1b[48;5;53mxx\x1b[0m");
    let opts = RenderOptions::default();
    let (width, height) = (opts.padding * 2 + 6 * 8, opts.padding * 2 + 2 * 18);
    let mut canvas = Canvas::new(width, height, Rgb(9, 9, 9));
    canvas.fill(theme.bg);
    render_screen_into_cleared(
        &mut canvas,
        opts.padding,
        opts.padding,
        &terminal.screen,
        &theme,
        &opts,
    );
    let cell = canvas.pixel(VIEWPORT_PADDING + 1, VIEWPORT_PADDING + 1);
    assert_ne!(
        cell[..3],
        theme.bg.to_array(),
        "a cell with an explicit background must be painted"
    );
}

/// The row-store rewrite must write exactly the pixels the per-pixel path wrote,
/// including clipped rectangles and zero-size requests.
#[test]
fn bulk_fills_match_the_per_pixel_reference() {
    let theme = theme();
    let (width, height) = (23, 17);
    // An offset stride exercises the mapped path's row arithmetic.
    let stride = width * 4 + 12;
    let cases = [
        (0, 0, width, height, Rgb(10, 20, 30)),
        (3, 4, 5, 1, Rgb(200, 100, 50)),
        (width - 2, height - 1, 8, 8, Rgb(1, 2, 3)), // clipped at the edge
        (width, 0, 4, 4, Rgb(9, 9, 9)),              // fully outside
        (0, 0, 0, 5, Rgb(9, 9, 9)),                  // empty
        (2, 2, 0, 0, Rgb(9, 9, 9)),                  // empty
        (7, 7, 1, 1, Rgb(255, 255, 255)),
    ];
    for (x, y, w, h, color) in cases {
        // Reference: the per-pixel `put` path (blend_pixel at alpha 255).
        let mut reference = Canvas::new(width, height, theme.bg);
        for py in y..(y + h).min(height) {
            for px in x..(x + w).min(width) {
                reference.blend_pixel(px, py, color, 255);
            }
        }
        let mut fast = Canvas::new(width, height, theme.bg);
        fast.fill_rect(x, y, w, h, color);
        assert_eq!(
            reference.pixels, fast.pixels,
            "owned fill_rect({x},{y},{w},{h}) diverges from the per-pixel path"
        );

        // Same through a mapped surface: wire order must stay B,G,R,0.
        let fd = pagh_ui::shm::create_memfd("bulk-fill-reference").expect("memfd");
        pagh_ui::shm::set_len(std::os::fd::AsRawFd::as_raw_fd(&fd), stride * height).expect("len");
        let mapping = pagh_ui::shm::MappedBuffer::map_shared(
            std::os::fd::AsRawFd::as_raw_fd(&fd),
            stride * height,
        )
        .expect("mmap");
        let mut mapped = Canvas::from_mapped(mapping, width, height, stride).expect("canvas");
        mapped.fill(theme.bg);
        mapped.fill_rect(x, y, w, h, color);
        let rgba = mapped.clone();
        assert_eq!(
            fast.pixels, rgba.pixels,
            "mapped fill_rect({x},{y},{w},{h}) diverges from the RGBA surface"
        );
    }

    // Whole-surface fill, both storages, against a per-pixel reference.
    let mut reference = Canvas::new(width, height, theme.bg);
    for py in 0..height {
        for px in 0..width {
            reference.blend_pixel(px, py, Rgb(4, 5, 6), 255);
        }
    }
    let mut filled = Canvas::new(width, height, theme.bg);
    filled.fill(Rgb(4, 5, 6));
    assert_eq!(reference.pixels, filled.pixels, "owned fill diverges");

    let fd = pagh_ui::shm::create_memfd("bulk-fill-whole").expect("memfd");
    pagh_ui::shm::set_len(std::os::fd::AsRawFd::as_raw_fd(&fd), stride * height).expect("len");
    let mapping = pagh_ui::shm::MappedBuffer::map_shared(
        std::os::fd::AsRawFd::as_raw_fd(&fd),
        stride * height,
    )
    .expect("mmap");
    let mut mapped = Canvas::from_mapped(mapping, width, height, stride).expect("canvas");
    mapped.fill(Rgb(4, 5, 6));
    assert_eq!(
        reference.pixels,
        mapped.clone().pixels,
        "mapped fill diverges (wire order or stride)"
    );
}
